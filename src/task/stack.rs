//! Чистый кодировщик начального стека процесса System V x86_64 и auxv.
//!
//! **Чистая половина** компонента `Stack_Initializer` (design §5): вычисляет
//! точный байтовый образ начального пользовательского стека, совместимого с
//! Linux — `argc`, массивы указателей `argv`/`envp`, ELF auxiliary vector,
//! 16 байт `AT_RANDOM` и NUL-терминированные строки аргументов/окружения, —
//! вместе с входным `rsp`. Отображения страниц здесь **нет**; эффектное
//! отображение — в `task::stack_map` (обвязка `run_linux_binary`).
//!
//! Только `core` + `alloc` (использует [`alloc::vec::Vec`]), поэтому
//! проверяется host-харнессом `proptest` (R11.6). Ядро даёт глобальный
//! аллокатор, host-test крейт линкует `std` (дающий `alloc`), так что один и
//! тот же исходник компилируется в обоих местах.
//!
//! Покрытые требования: R6.1–R6.8 (раскладка стека/auxv) и R7.5 (гейт аргументов).
#![allow(dead_code)]

use alloc::vec::Vec;

/// Теги типов ELF auxiliary vector, используемые кодировщиком.
///
/// Значения точно соответствуют ABI Linux x86_64 (design Data Models → `at`).
pub mod at {
    /// Маркер конца вектора.
    pub const NULL: u64 = 0;
    /// Адрес program headers в загруженном образе.
    pub const PHDR: u64 = 3;
    /// Размер одной записи program header в байтах.
    pub const PHENT: u64 = 4;
    /// Число записей program header.
    pub const PHNUM: u64 = 5;
    /// Размер системной страницы.
    pub const PAGESZ: u64 = 6;
    /// Базовый адрес интерпретатора (здесь не используется, определён для полноты).
    pub const BASE: u64 = 7;
    /// Адрес точки входа программы.
    pub const ENTRY: u64 = 9;
    /// Адрес 16 байт случайных данных.
    pub const RANDOM: u64 = 25;
}

/// Входы auxiliary vector, передаваемые ELF-загрузчиком.
///
/// `random_ptr` — выход, а не вход: кодировщик сам *вычисляет* адрес 16
/// случайных байт в стеке (он владеет их размещением) и игнорирует переданное
/// значение. Поле оставлено, чтобы вызывающий мог прочитать выбранный адрес,
/// если строит структуру как заготовку, но [`build_initial_stack`] его никогда
/// не читает.
pub struct AuxInputs {
    /// `AT_PHDR`: адрес program headers в пользовательском образе.
    pub phdr: u64,
    /// `AT_PHENT`: размер одной записи program header.
    pub phent: u64,
    /// `AT_PHNUM`: число записей program header.
    pub phnum: u64,
    /// `AT_ENTRY`: адрес точки входа программы.
    pub entry: u64,
    /// `AT_PAGESZ`: размер системной страницы.
    pub pagesz: u64,
    /// `AT_BASE`: адрес загрузки ELF-интерпретатора, ноль для статических образов.
    pub base: u64,
    /// `AT_RANDOM`: игнорируется на входе; кодировщик сам заполняет случайный блок.
    pub random_ptr: u64,
}

/// Закодированный начальный стек: байты для копирования и ключевые
/// пользовательские адреса.
///
/// `bytes` — непрерывный образ, занимающий `[initial_rsp, stack_top)`; индекс 0
/// в `bytes` соответствует пользовательскому адресу `argc_addr` (==
/// `initial_rsp`). Вызывающий (или property-тест) находит любой адрес в стеке `a`
/// по `bytes[a - argc_addr]`.
pub struct StackImage {
    /// Сырой образ стека, от младшего адреса. `bytes[0]` живёт по `argc_addr`.
    pub bytes: Vec<u8>,
    /// Пользовательский виртуальный адрес слова `argc` (выровнен на 16, R6.5).
    pub argc_addr: u64,
    /// Начальный `rsp` процесса — равен `argc_addr`.
    pub initial_rsp: u64,
}

/// Режим отказа [`build_initial_stack`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StackError {
    /// Закодированный образ не помещается в `[stack_low, stack_top)` (R6.8).
    TooLarge,
}

/// Число байт случайных данных, на которые указывает `AT_RANDOM` (R6.6).
const RANDOM_LEN: u64 = 16;

/// Округлить `x` вниз до ближайшего кратного 16.
#[inline]
fn align_down_16(x: u64) -> u64 {
    x & !15
}

/// Построить образ начального стека процесса System V x86_64 (чисто; R6.1–R6.8).
///
/// Раскладка от младшего адреса (`argc_addr`, куда указывает `rsp` на входе)
/// вверх к `stack_top`:
///
/// ```text
/// argc_addr -> [ argc (== argv.len()) ]
///              [ argv[0] .. argv[N-1] ][ NULL ]
///              [ envp[0] .. envp[M-1] ][ NULL ]
///              [ (AT_PHDR,..)(AT_PHENT,..)(AT_PHNUM,..)
///                (AT_ENTRY,..)(AT_PAGESZ,..)(AT_RANDOM,&rand)(AT_NULL,0) ]
///              [ ... нулевой паддинг ... ]
///              [ 16 случайных байт ]   <- сюда указывает AT_RANDOM
///              [ строки argv/envp, каждая с NUL ]   (кончается на stack_top)
/// ```
///
/// Каждый указатель `argv`/`envp` ссылается на первый байт своей
/// NUL-терминированной копии в этом образе (R6.7), а `&argc` выровнен на 16
/// (R6.5). Возвращает [`StackError::TooLarge`], если образ не помещается в
/// `[stack_low, stack_top)` (R6.8).
pub fn build_initial_stack(
    stack_top: u64,
    stack_low: u64,
    argv: &[&[u8]],
    envp: &[&[u8]],
    aux: &AuxInputs,
    random16: [u8; 16],
) -> Result<StackImage, StackError> {
    // ── Фиксированный размер таблицы указателей/auxv (в байтах), всё 8-байтные
    // слова: argc (1) + указатели argv (N) + NULL (1) + указатели envp (M) + NULL
    // (1) + auxv: 6 реальных записей + 1 AT_NULL = 7 пар = 14 слов.
    let n = argv.len() as u64;
    let m = envp.len() as u64;
    let table_words = 1u64
        .checked_add(n)
        .and_then(|w| w.checked_add(1)) // NULL argv
        .and_then(|w| w.checked_add(m))
        .and_then(|w| w.checked_add(1)) // NULL envp
        .and_then(|w| w.checked_add(16)) // 8 пар auxv
        .ok_or(StackError::TooLarge)?;
    let table_size = table_words.checked_mul(8).ok_or(StackError::TooLarge)?;

    // ── Блоб строк: сначала строки argv, затем envp, каждая с NUL ────────
    // Запоминаем смещение каждой строки в блобе, чтобы вычислить её итоговый
    // абсолютный пользовательский адрес, когда `argc_addr` зафиксирован.
    let mut str_blob: Vec<u8> = Vec::new();
    let mut argv_str_off: Vec<u64> = Vec::with_capacity(argv.len());
    let mut envp_str_off: Vec<u64> = Vec::with_capacity(envp.len());
    for s in argv {
        argv_str_off.push(str_blob.len() as u64);
        str_blob.extend_from_slice(s);
        str_blob.push(0);
    }
    for s in envp {
        envp_str_off.push(str_blob.len() as u64);
        str_blob.extend_from_slice(s);
        str_blob.push(0);
    }
    let str_total = str_blob.len() as u64;

    // ── Вычисление адресов (сверху вниз) ─────────────────────────────────
    // Строки прилегают к stack_top; 16 случайных байт — сразу под ними;
    // фиксированная таблица — ниже, с argc, выровненным на 16. Любой зазор
    // становится нулевым паддингом между таблицей и случайными байтами.
    let str_start = stack_top
        .checked_sub(str_total)
        .ok_or(StackError::TooLarge)?;
    let rand_start = str_start
        .checked_sub(RANDOM_LEN)
        .ok_or(StackError::TooLarge)?;
    let table_floor = rand_start
        .checked_sub(table_size)
        .ok_or(StackError::TooLarge)?;
    let argc_addr = align_down_16(table_floor);

    // Проверка вписывания (R6.8): должны остаться внутри отображённого региона стека.
    if argc_addr < stack_low || stack_top < argc_addr {
        return Err(StackError::TooLarge);
    }

    // ── Материализовать непрерывный образ [argc_addr, stack_top) ──────────
    let image_len = (stack_top - argc_addr) as usize;
    let mut bytes = alloc::vec![0u8; image_len];

    // Хелпер: записать u64 (LE) по байтовому смещению абсолютного адреса `addr`.
    let put_u64 = |buf: &mut [u8], addr: u64, val: u64| {
        let off = (addr - argc_addr) as usize;
        buf[off..off + 8].copy_from_slice(&val.to_le_bytes());
    };

    // Обойти таблицу, записывая слова в последовательные 8-байтные слоты от argc_addr.
    let mut cur = argc_addr;
    // argc (R6.2)
    put_u64(&mut bytes, cur, n);
    cur += 8;
    // Указатели argv, затем один NULL (R6.1)
    for off in &argv_str_off {
        put_u64(&mut bytes, cur, str_start + off);
        cur += 8;
    }
    put_u64(&mut bytes, cur, 0);
    cur += 8;
    // Указатели envp, затем один NULL (R6.1)
    for off in &envp_str_off {
        put_u64(&mut bytes, cur, str_start + off);
        cur += 8;
    }
    put_u64(&mut bytes, cur, 0);
    cur += 8;
    // auxv: каждый тег один раз (R6.3), PHENT/PHNUM/ENTRY из входов (R6.4),
    // AT_RANDOM указывает на случайный блок в стеке (R6.6), AT_NULL последним (R6.1).
    let auxv: [(u64, u64); 8] = [
        (at::PHDR, aux.phdr),
        (at::PHENT, aux.phent),
        (at::PHNUM, aux.phnum),
        (at::ENTRY, aux.entry),
        (at::PAGESZ, aux.pagesz),
        (at::BASE, aux.base),
        (at::RANDOM, rand_start),
        (at::NULL, 0),
    ];
    for (tag, val) in auxv {
        put_u64(&mut bytes, cur, tag);
        cur += 8;
        put_u64(&mut bytes, cur, val);
        cur += 8;
    }
    // `cur` теперь равен argc_addr + table_size; зазор до rand_start остаётся нулевым.

    // ── Случайный блок (R6.6) ────────────────────────────────────────────────
    let rand_off = (rand_start - argc_addr) as usize;
    bytes[rand_off..rand_off + 16].copy_from_slice(&random16);

    // ── Байты строк (R6.7) ──────────────────────────────────────────────────
    let str_off = (str_start - argc_addr) as usize;
    bytes[str_off..str_off + str_total as usize].copy_from_slice(&str_blob);

    Ok(StackImage {
        bytes,
        argc_addr,
        initial_rsp: argc_addr,
    })
}

/// Гейт аргументов run-запроса (R7.5).
///
/// `true` тогда и только тогда, когда в `argv` не больше 256 записей **и**
/// суммарная длина всех аргументов не больше 4096 байт.
pub fn arg_gate(argv: &[&[u8]]) -> bool {
    if argv.len() > 256 {
        return false;
    }
    let mut total: usize = 0;
    for a in argv {
        total = match total.checked_add(a.len()) {
            Some(t) => t,
            None => return false,
        };
        if total > 4096 {
            return false;
        }
    }
    total <= 4096
}
