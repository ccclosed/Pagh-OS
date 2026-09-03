# `src/sync/` — синхронизация

Вся подсистема — один файл: спинлок, глушащий прерывания на время удержания.

## Файлы

| Файл | Роль |
|---|---|
| `mod.rs` | Корень: `pub mod spinlock;` |
| `spinlock.rs` | `Spinlock<T>` + RAII-guard `SpinlockGuard<'a, T>` |

## Ключевые символы

- `Spinlock<T>`: `const new(T)`, `lock() -> SpinlockGuard`, `try_lock() -> Option<SpinlockGuard>`.
- `SpinlockGuard`: `Deref/DerefMut`; `Drop` освобождает и восстанавливает IF.

## Как работает

Захват: запомнить `interrupts_enabled()` → `cli` → CAS-цикл
(`compare_exchange_weak(false, true, Acquire, Relaxed)` + `spin_loop()`).
Освобождение при дропе: `Release`-store `false` → `sti` **только если** прерывания были
включены до захвата. Именно вложенность-осведомлённое восстановление IF позволяет
обработчикам прерываний (у которых IF=0) брать локи без перманентного включения прерываний.

## Зависимости

- **От:** `arch::cpu::{interrupts_enabled, disable_interrupts, enable_interrupts}`. Больше ни от чего.
- **На неё:** ~34 файла ядра. Все глобалы — `Spinlock<...>`: планировщик
  (`READY_QUEUE`, `COMPAT_STATES`, `EXITING_PIDS`, `PENDING_REAPS`), FPU-области,
  futex-очереди, pipes, Unix-слушатели, `VmRegionSet`, watchdog syscall'ов, net и драйверы.

## Правила (грабли)

- **Нереентерабелен**: двойной захват на одном CPU = дедлок (см. `task::compat::COMPAT_DEPTH`).
- **Нельзя держать через блокирующее ожидание прерывания устройства** (например, ext2 I/O):
  с замаскированным IF на одном ядре прерывание никогда не придёт. I/O-syscall-обработчики
  делают extract-under-lock → release → block → re-acquire.
- Мьютексов/семафоров/каналов нет; блокирующие ожидания (futex, poll, wait4, epoll_wait)
  реализованы как кооперативные циклы `yield_current()` поверх планировщика.
