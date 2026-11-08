// Feature: procfs (issue #11), slice 3 — `/proc/version`, `/proc/filesystems`,
// `/proc/loadavg` and `/proc/sys/kernel/*` (contract `docs/procfs.md` §4.9–§4.12).
//
// These files are line-oriented and parsed by shell tools, `mount(8)`-style
// readers and libuv. The renderers receive only plain data (the version string,
// the filesystem table, three counts), so the exact bytes are property-testable
// here; the kernel-side node layer only supplies the inputs.

use crate::procfs_format::{
    format_filesystems, format_loadavg, format_sys_hostname, format_sys_osrelease,
    format_sys_ostype, format_version, FileSystemEntry, LoadAvg, FILESYSTEMS, KERNEL_HOSTNAME,
    KERNEL_OSTYPE, PROC_SYS_FILES,
};
use proptest::prelude::*;

/// A semver-ish version: what `CARGO_PKG_VERSION` can actually contain, with the
/// optional pre-release tail. No whitespace/newlines, so the one-line invariant
/// is about the renderer, not a sanitizer it should not have.
fn version_strategy() -> impl Strategy<Value = String> {
    "[0-9]{1,2}\\.[0-9]{1,2}\\.[0-9]{1,2}(-[0-9A-Za-z.]{1,12})?"
}

/// One row from a set of real-looking Linux filesystem names. The names are
/// static because `/proc/filesystems` is a static table, not user data.
fn fs_entry_strategy() -> impl Strategy<Value = FileSystemEntry> {
    (
        any::<bool>(),
        prop_oneof![
            Just("proc"),
            Just("ramfs"),
            Just("ext2"),
            Just("ext4"),
            Just("tmpfs"),
            Just("overlay"),
        ],
    )
        .prop_map(|(nodev, name)| FileSystemEntry { nodev, name })
}

fn lines(out: &[u8]) -> Vec<&str> {
    core::str::from_utf8(out)
        .expect("procfs misc files are ASCII")
        .trim_end_matches('\n')
        .split('\n')
        .collect()
}

proptest! {
    /// `/proc/version` is one exact line carrying the real version, the machine
    /// and the fixed build tag — never a fabricated Linux release.
    #[test]
    fn version_is_one_exact_line(version in version_strategy()) {
        let out = format_version(&version);
        let text = core::str::from_utf8(&out).unwrap();
        prop_assert_eq!(text, format!("pagh {version} (x86_64) #1 SMP\n"));
        prop_assert_eq!(text.lines().count(), 1);
        prop_assert!(text.starts_with("pagh "));
        prop_assert!(text.contains(&version));
        prop_assert!(!text.contains('\0'));
        // A Linux-looking release is the trap this file avoids.
        prop_assert!(!text.contains("Linux version"));
    }

    /// `nodev\t<name>` for a device-less filesystem, `\t<name>` otherwise; one
    /// newline per row, no padding, no extra fields.
    #[test]
    fn filesystems_rows_are_linux_shaped(entries in prop::collection::vec(fs_entry_strategy(), 0..8)) {
        let out = format_filesystems(&entries);
        let got = if out.is_empty() { Vec::new() } else { lines(&out) };
        prop_assert_eq!(got.len(), entries.len());

        for (line, entry) in got.iter().zip(entries.iter()) {
            let expected = if entry.nodev {
                format!("nodev\t{}", entry.name)
            } else {
                format!("\t{}", entry.name)
            };
            prop_assert_eq!(*line, expected);
            prop_assert!(line.ends_with(entry.name));
        }

        if entries.is_empty() {
            prop_assert!(out.is_empty());
        } else {
            prop_assert!(out.ends_with(b"\n"));
        }
    }

    /// The shipped table lists exactly the drivers this build implements: proc
    /// and ramfs are `nodev`, ext2 takes a block device. Nothing else may appear.
    #[test]
    fn filesystems_table_lists_only_present_drivers(_dummy in 0u8..1) {
        let out = format_filesystems(&FILESYSTEMS);
        prop_assert_eq!(
            out,
            b"nodev\tproc\nnodev\tramfs\n\text2\n".to_vec()
        );
        prop_assert_eq!(FILESYSTEMS.len(), 3);
        prop_assert_eq!(FILESYSTEMS[0].name, "proc");
        prop_assert!(FILESYSTEMS[0].nodev);
        prop_assert_eq!(FILESYSTEMS[1].name, "ramfs");
        prop_assert!(FILESYSTEMS[1].nodev);
        prop_assert_eq!(FILESYSTEMS[2].name, "ext2");
        prop_assert!(!FILESYSTEMS[2].nodev);
    }

    /// `/proc/loadavg`: the three computed averages, then `runnable/total` and
    /// the last allocated pid. The counts parse back to the inputs and
    /// `runnable <= total` is upstream state, not something the renderer clamps.
    #[test]
    fn loadavg_is_exact_and_parseable(
        one in any::<u32>(),
        five in any::<u32>(),
        fifteen in any::<u32>(),
        runnable in any::<u64>(),
        stopped in any::<u64>(),
        last_pid in any::<u64>(),
    ) {
        let total = runnable.saturating_add(stopped);
        let input = LoadAvg { one, five, fifteen, runnable, total, last_pid };
        let out = format_loadavg(&input);
        let text = core::str::from_utf8(&out).unwrap();
        prop_assert_eq!(
            text,
            format!(
                "{}.{:02} {}.{:02} {}.{:02} {runnable}/{total} {last_pid}\n",
                one / 100, one % 100, five / 100, five % 100,
                fifteen / 100, fifteen % 100
            )
        );
        prop_assert_eq!(text.lines().count(), 1);

        let fields: Vec<&str> = text.trim_end_matches('\n').split(' ').collect();
        prop_assert_eq!(fields.len(), 5);

        let (run, tot) = fields[3].split_once('/').expect("runnable/total field");
        prop_assert_eq!(run.parse::<u64>().unwrap(), runnable);
        prop_assert_eq!(tot.parse::<u64>().unwrap(), total);
        prop_assert!(runnable <= total);
        prop_assert_eq!(fields[4].parse::<u64>().unwrap(), last_pid);
        prop_assert!(!text.contains('\0'));
    }

    /// The zero case is the pre-allocation truth: `0/0` and pid `0`, never a
    /// made-up pid 1.
    #[test]
    fn loadavg_zero_case_is_honest(_dummy in 0u8..1) {
        prop_assert_eq!(
            format_loadavg(&LoadAvg::default()),
            b"0.00 0.00 0.00 0/0 0\n".to_vec()
        );
    }

    /// The sys files are one constant line each; `osrelease` is the real version.
    /// The exact names/order of `/proc/sys/kernel` are part of the path table.
    #[test]
    fn sys_files_are_constant_single_lines(version in version_strategy()) {
        prop_assert_eq!(format_sys_ostype(), b"Pagh\n".to_vec());
        prop_assert_eq!(KERNEL_OSTYPE, "Pagh");
        prop_assert_eq!(format_sys_hostname(), b"pagh\n".to_vec());
        prop_assert_eq!(KERNEL_HOSTNAME, "pagh");
        prop_assert_eq!(
            format_sys_osrelease(&version),
            format!("{version}\n").into_bytes()
        );

        for out in [
            format_sys_ostype(),
            format_sys_hostname(),
            format_sys_osrelease(&version),
        ] {
            let text = core::str::from_utf8(&out).unwrap();
            prop_assert_eq!(text.lines().count(), 1);
            prop_assert!(text.ends_with('\n'));
            prop_assert!(!text.contains('\0'));
        }

        let names: Vec<&str> = PROC_SYS_FILES.iter().map(|e| e.name).collect();
        prop_assert_eq!(names, ["ostype", "osrelease", "hostname"]);
    }
}