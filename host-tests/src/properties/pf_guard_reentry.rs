// Feature: надёжность окна #PF под compat-локом (fix/pf-guard-reentry).
//
// Дефект, который закрепляют эти свойства:
//
//   * `mem_sys::handle_user_page_fault` отказывался обслуживать отказ, если
//     глобальный счётчик `COMPAT_DEPTH` (глубина `with_current_compat`) был > 0;
//   * обработчики syscall'ов идут с включёнными прерываниями, тик вытесняет
//     задачу, и тик между `fetch_add`/`lock()` оставлял глубину >0, пока CPU уже
//     исполнял ДРУГУЮ задачу в ring 3;
//   * её законная запись в ленивую страницу кучи/anon-ммапа получала SIGSEGV
//     («refusing re-entry»), а kill-путь сбрасывал счётчик, после чего
//     возобновлённый владелец делал `fetch_sub` из нуля — счётчик переворачивался
//     в `usize::MAX`, и отказывали уже ВСЕ отказы подряд.
//
// Ядерная половина — `src/arch/x86_64/linux/{compat_guard,trap_frame,mem_sys}.rs`.
// Здесь проверяются ровно те два чистых решения, на которые она опирается:
// учёт «кто внутри» с владельцем (`compat_guard::CompatEntry`) и политика
// «отказывать или обслуживать» (`trap_frame::pf_registry_must_refuse`).

use crate::compat_guard::CompatEntry;
use crate::trap_frame::pf_registry_must_refuse;
use proptest::prelude::*;

/// Задачи, между которыми разыгрываются чередования (сервер и два клиента —
/// ровно та тройка, что гибла в тяжёлой desktop-сессии).
const PIDS: [u64; 3] = [6, 7, 8];

// ─── чистая политика отказа ────────────────────────────────────────────────

#[test]
fn ring3_lazy_fault_is_never_a_reentry() {
    // Ring 3 не может быть внутри замыкания: чтобы исполнять пользовательскую
    // инструкцию, задача уже вышла из него. Оба значения `inside` обязаны
    // приводить к обслуживанию — иначе законная demand-page запись в кучу
    // превращается в SIGSEGV (наблюдалось как `[EXC #14] SIGSEGV pid=8`).
    assert!(!pf_registry_must_refuse(true, false));
    assert!(!pf_registry_must_refuse(true, true));
}

#[test]
fn kernel_fault_inside_closure_is_still_guarded() {
    // Настоящий повторный вход: kernel-контекст ЭТОЙ задачи внутри
    // `with_current_compat`, второй `COMPAT_STATES.lock()` повесил бы CPU с
    // замаскированными прерываниями. Такой отказ обязан остаться отказом.
    assert!(pf_registry_must_refuse(false, true));
    assert!(!pf_registry_must_refuse(false, false));
}

proptest! {
    /// Политика — ровно «kernel-mode И внутри»: отказ ring 3 не отвергается
    /// ни при каком состоянии учёта, отказ ring 0 отвергается только внутри
    /// собственного замыкания.
    #[test]
    fn pf_guard_policy_is_exact(user_mode in any::<bool>(), inside in any::<bool>()) {
        prop_assert_eq!(
            pf_registry_must_refuse(user_mode, inside),
            !user_mode && inside
        );
    }

    /// Регрессия тяжёлой сессии: посторонняя задача (pid из `PIDS`, отличный от
    /// владельца) делает тысячи ленивых записей, пока владелец вытеснен между
    /// `enter` и `lock()`. Ни одна не должна быть отвергнута, а собственный
    /// kernel-отказ владельца — по-прежнему отвергается.
    #[test]
    fn preempted_owner_never_poisons_other_tasks(
        owner in prop::sample::select(PIDS.to_vec()),
        faults in 1usize..5_000,
    ) {
        let entry = CompatEntry::new();
        entry.enter(owner); // тик вытеснил владельца до lock()
        for pid in PIDS {
            if pid == owner {
                continue;
            }
            // compat::compat_lock_held() для этой задачи — ответ учёта.
            let inside = entry.is_inside(pid);
            prop_assert!(!inside, "чужая глубина выдана за своё замыкание");
            for _ in 0..faults {
                prop_assert!(
                    !pf_registry_must_refuse(true, inside),
                    "законная ленивая страница ring 3 отвергнута"
                );
            }
        }
        // Владелец, у которого отказ случился ВНУТРИ замыкания, остаётся
        // защищённым — это настоящий re-entry.
        prop_assert!(pf_registry_must_refuse(false, entry.is_inside(owner)));
    }
}

// ─── учёт владельца: чередования задач ─────────────────────────────────────

/// Один шаг чередования: задача `pid` входит в замыкание, выходит из него или
/// аварийно сбрасывает учёт (kill-путь исключения).
#[derive(Debug, Clone, Copy)]
enum Op {
    Enter(usize),
    Exit(usize),
    Abandon(usize),
}

fn arb_op() -> impl Strategy<Value = Op> {
    (0usize..PIDS.len(), 0u8..3).prop_map(|(i, kind)| match kind {
        0 => Op::Enter(i),
        1 => Op::Exit(i),
        _ => Op::Abandon(i),
    })
}

fn apply(entry: &CompatEntry, op: Op) {
    match op {
        Op::Enter(i) => entry.enter(PIDS[i]),
        Op::Exit(i) => entry.exit(PIDS[i]),
        Op::Abandon(i) => {
            entry.abandon(PIDS[i]);
        }
    }
}

proptest! {
    /// Произвольные межзадачные чередования никогда не переворачивают учёт:
    /// глубина насыщается, ноль означает «никто не внутри», и любой остаток
    /// можно снять выходами владельца.
    #[test]
    fn interleavings_never_poison_the_guard(
        ops in prop::collection::vec(arb_op(), 0..200)
    ) {
        let entry = CompatEntry::new();
        for op in ops {
            apply(&entry, op);
            // Инвариант представления: «кто-то внутри» и «глубина > 0» — одно.
            prop_assert_eq!(entry.depth() == 0, entry.owner() == 0);
            prop_assert!(entry.depth() < usize::MAX / 2, "underflow");
            for pid in PIDS {
                if entry.is_inside(pid) {
                    prop_assert_eq!(entry.owner(), pid);
                    prop_assert!(entry.depth() > 0);
                }
            }
        }
        // Кто бы ни остался владельцем, его собственные выходы обнуляют учёт —
        // протухшая глубина не превращается в вечный отказ.
        for _ in 0..300 {
            if entry.owner() == 0 {
                break;
            }
            entry.exit(entry.owner());
        }
        prop_assert_eq!(entry.depth(), 0);
        prop_assert_eq!(entry.owner(), 0);
    }

    /// Вложенные замыкания одной задачи считаются, а выход не-владельца
    /// игнорируется — ровно так kill-путь постороннего процесса больше не
    /// ломает баланс владельца.
    #[test]
    fn nesting_counts_and_foreign_exit_is_ignored(
        owner in prop::sample::select(PIDS.to_vec()),
        depth in 1usize..32,
        other in prop::sample::select(PIDS.to_vec()),
    ) {
        let entry = CompatEntry::new();
        for _ in 0..depth {
            entry.enter(owner);
        }
        prop_assert_eq!(entry.depth(), depth);
        if other != owner {
            entry.exit(other);
            prop_assert_eq!(entry.depth(), depth, "выход чужой задачи не вычитает глубину");
        }
        for _ in 0..depth {
            entry.exit(owner);
        }
        prop_assert_eq!(entry.depth(), 0);
        prop_assert!(!entry.is_inside(owner));
    }
}

#[test]
fn abandon_is_scoped_to_the_owner() {
    // Точный сценарий двух подряд SIGSEGV: pid 8 гибнет первым, пока глубина
    // принадлежит вытесненному pid 7. Kill-путь pid 8 не имеет права сбросить
    // чужой учёт, иначе возобновлённый pid 7 сделает fetch_sub из нуля.
    let entry = CompatEntry::new();
    entry.enter(7);
    assert!(!entry.abandon(8), "чужой учёт не сбрасывается");
    assert_eq!(entry.depth(), 1);
    entry.exit(7);
    assert_eq!(entry.depth(), 0, "баланс владельца сохранён");
    assert!(!entry.is_inside(8));
}