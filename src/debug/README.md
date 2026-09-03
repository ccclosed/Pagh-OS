# `src/debug/` — отладочные утилиты

Беспамятный (heap-free) трейсинг стека для контекстов panic/фолтов.

## Файлы

| Файл | Роль |
|---|---|
| `mod.rs` | Корень: `pub mod unwind;` |
| `unwind.rs` | RBP-chain walk + RSP stack scan |

## Ключевые символы

- `stack_trace()` — читает текущий RBP (`asm!("mov {}, rbp")`), делегирует в
  `stack_trace_from`. Используется panic-хендлером в `lib.rs`.
- `stack_trace_from(rbp: u64)` — идёт по цепочке фреймов (`[rbp+0]` = saved RBP,
  `[rbp+8]` = адрес возврата), `read_volatile` на каждый слот, печать адресов возврата.
- `stack_scan_backtrace(rsp, max_qwords)` — когда цепочка RBP мусор (например, `RIP=0x1`
  после corruption control flow): скан стека вверх на 8-байтовые значения внутри
  `[kernel_start(), kernel_end())`, cap 24 фрейма. Используется kernel-mode
  page-fault-хендлером в `idt.rs` перед halt.

## Детали

- `MAX_FRAMES = 32` (chain walk).
- Гарды: RBP обязан быть ≥ `0xFFFF8000_00000000` (higher half), иначе trace стопается;
  guard от бесконечного цикла (`saved_rbp == rbp`); null return address — стоп.
- Верхняя граница скана: если `rsp >= KERNEL_STACK_REGION_BASE`, лимит клампится на верх
  текущего per-PID слота (`KERNEL_STACK_STRIDE`) — скан не может зафолтить на соседней
  unmapped guard-странице.
- **Никаких heap-аллокаций** — безопасно для panic-хендлера (`panic = "abort"`).

## Зависимости

- **От:** `kprintln!` (serial), `memory::layout`.
- **На неё:** panic-хендлер `lib.rs`, page-fault-хендлер `arch/x86_64/idt.rs`. Больше никто.
