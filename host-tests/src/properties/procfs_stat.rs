// Feature: procfs (issue #11), slice 2 — `/proc/stat` (contract
// `docs/procfs.md` §4.8). The file is parsed by libuv's `uv_cpu_info`
// (`fgets` discards the aggregate line, then `fscanf("cpu%u %llu …")` needs a
// `cpu0` line with at least six numbers), by procps/htop and by `vmstat`.
//
// The renderer receives counters and formats them; it never reads a clock.
// These properties pin the Linux column order/spacing, the honest zeros, the
// one-CPU `cpu0` line and the numeric monotonicity that `/proc/stat` must have.

use crate::procfs_format::{format_stat, StatInputs};
use crate::task_stat::{classify_tick, TickClass, USER_HZ};
use proptest::prelude::*;

fn inputs() -> impl Strategy<Value = StatInputs> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(|(user, system, idle, ctxt, btime, processes, running, blocked)| {
            StatInputs {
                user,
                system,
                idle,
                ctxt,
                btime,
                processes,
                procs_running: running,
                procs_blocked: blocked,
            }
        })
}

/// Parse the numeric columns of one CPU line (the label is discarded).
fn columns(line: &str) -> Vec<u64> {
    line.split_whitespace()
        .skip(1)
        .map(|f| f.parse::<u64>().expect("stat columns are decimal"))
        .collect()
}

fn lines(out: &[u8]) -> Vec<&str> {
    core::str::from_utf8(out)
        .expect("stat is ASCII")
        .trim_end_matches('\n')
        .split('\n')
        .collect()
}

proptest! {
    /// The aggregate line is byte-exact: Linux's `"cpu "` label plus one
    /// space-prefixed value per column, ten columns, then a newline.
    #[test]
    fn stat_first_line_is_exact(s in inputs()) {
        let out = format_stat(&s);
        let text = core::str::from_utf8(&out).unwrap();
        let first = text.lines().next().unwrap();
        prop_assert_eq!(
            first,
            format!(
                "cpu  {} 0 {} {} 0 0 0 0 0 0",
                s.user, s.system, s.idle
            )
        );
        let cols = columns(first);
        prop_assert_eq!(cols.len(), 10);
        prop_assert_eq!(&cols[..], &[
            s.user, 0, s.system, s.idle, 0, 0, 0, 0, 0, 0
        ]);
    }

    /// The `cpu0` line is the one libuv's loop actually parses; on a single-CPU
    /// machine it must repeat the aggregate numbers, with six numeric fields
    /// after the index (`user nice sys idle iowait irq`).
    #[test]
    fn stat_cpu0_line_is_parseable_and_equal(s in inputs()) {
        let out = format_stat(&s);
        let ls = lines(&out);
        prop_assert!(ls[1].starts_with("cpu0 "));
        let cols = columns(ls[1]);
        prop_assert_eq!(cols.len(), 10);
        prop_assert_eq!(cols.clone(), columns(ls[0]));
        // libuv's fscanf consumes the index + exactly these six numbers.
        prop_assert!(cols.len() >= 6);
        prop_assert_eq!(cols[0], s.user);
        prop_assert_eq!(cols[1], 0);
        prop_assert_eq!(cols[2], s.system);
        prop_assert_eq!(cols[3], s.idle);
        prop_assert_eq!(cols[4], 0);
        prop_assert_eq!(cols[5], 0);
    }

    /// Fields with no accounting stay exactly zero — the honesty rule.
    #[test]
    fn stat_zero_fields_are_not_fabricated(s in inputs()) {
        let out = format_stat(&s);
        let cols = columns(core::str::from_utf8(&out).unwrap().lines().next().unwrap());
        // nice iowait irq softirq steal guest guest_nice
        for i in [1usize, 4, 5, 6, 7, 8, 9] {
            prop_assert_eq!(cols[i], 0, "column {} must stay 0", i);
        }
    }

    /// Tail lines are Linux's order with exactly one decimal value each; the
    /// counters without a source (`intr`, `softirq`) are omitted rather than
    /// printed as a fake zero.
    #[test]
    fn stat_tail_lines_are_linux_ordered(s in inputs()) {
        let out = format_stat(&s);
        let ls = lines(&out);
        prop_assert_eq!(ls.len(), 7, "cpu, cpu0 + five tail lines");
        let expected = [
            ("ctxt", s.ctxt),
            ("btime", s.btime),
            ("processes", s.processes),
            ("procs_running", s.procs_running),
            ("procs_blocked", s.procs_blocked),
        ];
        for (line, (key, value)) in ls[2..].iter().zip(expected) {
            prop_assert_eq!(*line, format!("{} {}", key, value));
            prop_assert!(!line.starts_with("intr "));
            prop_assert!(!line.starts_with("softirq "));
        }
        prop_assert!(core::str::from_utf8(&out).unwrap().ends_with('\n'));
        prop_assert!(!out.contains(&0));
        prop_assert!(!out.contains(&b'\t'));
    }

    /// Formatting preserves ordering: a counter that did not decrease cannot
    /// render as a smaller number, in either CPU line or the tail.
    #[test]
    fn stat_formatting_is_monotonic(
        base in inputs(),
        d_user in 0u64..1_000_000,
        d_sys in 0u64..1_000_000,
        d_idle in 0u64..1_000_000,
        d_ctxt in 0u64..1_000_000,
        d_proc in 0u64..1_000_000,
    ) {
        let mut later = base;
        later.user = base.user.saturating_add(d_user);
        later.system = base.system.saturating_add(d_sys);
        later.idle = base.idle.saturating_add(d_idle);
        later.ctxt = base.ctxt.saturating_add(d_ctxt);
        later.processes = base.processes.saturating_add(d_proc);

        let a_out = format_stat(&base);
        let b_out = format_stat(&later);
        let a = lines(&a_out);
        let b = lines(&b_out);
        for (x, y) in a.iter().zip(b.iter()) {
            let nx = columns(x);
            let ny = columns(y);
            prop_assert_eq!(nx.len(), ny.len());
            for (old, new) in nx.iter().zip(ny.iter()) {
                prop_assert!(new >= old, "{new} < {old} in {x:?} -> {y:?}");
            }
        }
    }

    /// The whole file stays small (it is read through a heap snapshot), and the
    /// unit constant is the documented USER_HZ.
    #[test]
    fn stat_is_bounded(s in inputs()) {
        let out = format_stat(&s);
        prop_assert!(out.len() < 1024, "got {} bytes", out.len());
        prop_assert!(out.ends_with(b"\n"));
        prop_assert_eq!(USER_HZ, 1000);
    }
}

proptest! {
    /// Tick classification is a total function over the interrupted CS: ring 3
    /// is user, idle/hlt is idle, every other ring-0 frame is system.
    #[test]
    fn tick_classification_is_total(cs in any::<u64>(), idle in any::<bool>(), halt in any::<bool>()) {
        let got = classify_tick(cs, idle, halt);
        let want = if cs & 3 == 3 && cs != 0 {
            TickClass::User
        } else if idle || halt {
            TickClass::Idle
        } else {
            TickClass::System
        };
        prop_assert_eq!(got, want);
    }
}

#[test]
fn tick_classification_truth_table() {
    assert_eq!(classify_tick(0x1b, false, false), TickClass::User);
    assert_eq!(classify_tick(0x1b, true, true), TickClass::User);
    assert_eq!(classify_tick(0x08, true, false), TickClass::Idle);
    assert_eq!(classify_tick(0x08, false, true), TickClass::Idle);
    assert_eq!(classify_tick(0x08, false, false), TickClass::System);
    assert_eq!(classify_tick(0, false, false), TickClass::System);
}