//! Чистая классификация ELF и выбор load-bias для static-PIE.
//!
//! Чистая, без аллокаций, `core`-only логика, общая с крейтом `host-tests`
//! (R11.6). Модуль решает — *не отображая ничего, не аллоцируя таблицы страниц и
//! не трогая глобальное состояние* — является ли буфер байт Linux static
//! `ET_EXEC` или static-PIE `ET_DYN` x86_64 ELF, который это ядро умеет
//! загружать, и (для `ET_DYN`) выбирает выровненный по странице load-bias,
//! держащий все сегменты в нижней половине пользовательского адресного
//! пространства.
//!
//! Эффектный загрузчик (`ElfLoader::load`) использует [`classify_elf`] и
//! [`choose_bias`] до аллокации пользовательского PML4 или маппинга сегментов,
//! поэтому битый/непригодный бинарь отвергается описательным `&'static str` до
//! касания памяти (R5.5, R5.6, R5.9, R11.3, R12.1).
//!
//! Разбор идёт явными little-endian чтениями байт из среза (а не
//! переинтерпретацией буфера как `#[repr(C)]`-структуры), поэтому логика
//! полностью переносима, безопасна по выравниванию и одинакова на хосте и на
//! цели.
#![allow(dead_code)]

/// x86_64 page size (4 KiB).
const PAGE_SIZE: u64 = 4096;

/// Длина ELF64-заголовка (`Ehdr`) в байтах.
///
/// Экспортируется, чтобы PATH-поиск `lxrun` мог прочитать только заголовок
/// кандидата (см. [`looks_like_linux_elf`]) и не тянуть в память весь образ
/// ради решения «это вообще ELF?».
pub const HEADER_LEN: usize = EHDR_SIZE;

/// Исключающая верхняя граница нижней половины канонического
/// пользовательского диапазона (`User_Addr_Max`). Сегмент допустим, только если
/// его диапазон, округлённый по страницам, строго ниже неё.
///
/// Копия той же константы из `src/vfs/elf.rs` сделана намеренно: тот модуль
/// тянет зависимости только для ядра (VMM/PMM и крейт пейджинга `x86_64`),
/// поэтому импорт сделал бы этот чистый модуль — и включающий его через `#[path]`
/// крейт `host-tests` — несостоявшимся на хосте. Значение задано архитектурой.
pub const USER_ADDR_MAX: u64 = 0x0000_8000_0000_0000;

/// База по умолчанию, выровненная по странице, для load-bias static-PIE
/// (`ET_DYN`).
///
/// Детерминирована, чтобы два запуска одного бинаря загружались одинаково.
/// Подойдёт любое выровненное по странице значение ниже `USER_ADDR_MAX`;
/// `0x1_0000` (64 KiB) оставляет NULL-страницу незамапленной и всю
/// пользовательскую половину доступной выше.
pub const PIE_BASE: u64 = 0x1_0000;

// ── идентификация ELF64 ─────────────────────────────────────────────────────
const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;

// ── e_type / e_machine ──────────────────────────────────────────────────────
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 0x3E;

// ── p_type ──────────────────────────────────────────────────────────────────
const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;

// ── размеры структур ELF64 ──────────────────────────────────────────────────
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;

// ── смещения полей заголовка ELF64 ──────────────────────────────────────────
const E_TYPE_OFF: usize = 16;
const E_MACHINE_OFF: usize = 18;
const E_PHOFF_OFF: usize = 32;
const E_PHENTSIZE_OFF: usize = 54;
const E_PHNUM_OFF: usize = 56;

// ── смещения полей program header ELF64 (относительно phdr) ─────────────────
const P_TYPE_OFF: usize = 0;
const P_OFFSET_OFF: usize = 8;
const P_VADDR_OFF: usize = 16;
const P_FILESZ_OFF: usize = 32;
const P_MEMSZ_OFF: usize = 40;

/// Вид ELF-образа, который принимает загрузчик.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ElfKind {
    /// Классический статический исполняемый файл (`ET_EXEC`); грузится по
    /// абсолютному `p_vaddr` (bias 0). (R5.1)
    Exec,
    /// Позиционно-независимый исполняемый файл или динамический интерпретатор
    /// (`ET_DYN`); грузится по `p_vaddr + bias` с bias, выбранным ядром. (R5.2)
    Dyn,
}

/// Результат классификации буфера-кандидата ELF.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ElfVerdict {
    /// Буфер непригоден; `&'static str` внутри называет причину (служить
    /// единственной диагностикой отказа загрузчика — R12.1).
    Reject(&'static str),
    /// Буфер — загружаемый статический бинарь.
    Load {
        /// Это `ET_EXEC` или static-PIE `ET_DYN`.
        kind: ElfKind,
        /// `true` для `ET_DYN` (load bias выбирается через [`choose_bias`]),
        /// `false` для `ET_EXEC` (bias всегда 0).
        bias_required: bool,
    },
}

// ── little-endian читатели полей (с проверкой границ, тотальные) ────────────

#[inline]
fn rd_u16(data: &[u8], off: usize) -> Option<u16> {
    let b = data.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

#[inline]
fn rd_u32(data: &[u8], off: usize) -> Option<u32> {
    let b = data.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

#[inline]
fn rd_u64(data: &[u8], off: usize) -> Option<u64> {
    let b = data.get(off..off.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
    ]))
}

/// Округляет `x` вверх до границы страницы; `None` при переполнении.
#[inline]
fn page_up(x: u64) -> Option<u64> {
    x.checked_add(PAGE_SIZE - 1).map(|v| v & !(PAGE_SIZE - 1))
}

/// Чистая классификация буфера-кандидата ELF (без маппинга и аллокаций).
///
/// Возвращает [`ElfVerdict::Load`] **тогда и только тогда**, когда буфер —
/// 64-битный little-endian `EM_X86_64` ELF типа `ET_EXEC` или `ET_DYN`, чей
/// необязательный `PT_INTERP` резолвит загрузчик процесса, и каждый `PT_LOAD`:
///   * `p_filesz <= p_memsz` (R5.9);
///   * `p_offset + p_filesz <= data.len()` (R5.9);
///   * диапазон `[p_vaddr, p_vaddr + p_memsz)`, округлённый по страницам, строго
///     ниже [`USER_ADDR_MAX`] (R5.5). Для `ET_DYN` это нижняя граница при нулевом
///     bias; реальный bias выбирает позже [`choose_bias`].
///
/// Любой другой буфер даёт [`ElfVerdict::Reject`] с описательной причиной, и
/// ничего не мапится (R5.3, R5.6, R11.3). Вся арифметика overflow-safe.
pub fn classify_elf(data: &[u8]) -> ElfVerdict {
    // ── наличие и идентичность заголовка (R5.6) ──
    if data.len() < EHDR_SIZE {
        return ElfVerdict::Reject("ELF: data too small for header");
    }
    if data[0] != 0x7F || &data[1..4] != b"ELF" {
        return ElfVerdict::Reject("ELF: invalid magic");
    }
    if data[EI_CLASS] != ELFCLASS64 {
        return ElfVerdict::Reject("ELF: not 64-bit");
    }
    if data[EI_DATA] != ELFDATA2LSB {
        return ElfVerdict::Reject("ELF: not little-endian");
    }

    let e_machine = match rd_u16(data, E_MACHINE_OFF) {
        Some(v) => v,
        None => return ElfVerdict::Reject("ELF: truncated header"),
    };
    if e_machine != EM_X86_64 {
        return ElfVerdict::Reject("ELF: not x86_64");
    }

    let e_type = match rd_u16(data, E_TYPE_OFF) {
        Some(v) => v,
        None => return ElfVerdict::Reject("ELF: truncated header"),
    };
    let kind = match e_type {
        ET_EXEC => ElfKind::Exec,
        ET_DYN => ElfKind::Dyn,
        _ => return ElfVerdict::Reject("ELF: not ET_EXEC or ET_DYN"),
    };

    // ── границы таблицы program headers ──
    let phnum = match rd_u16(data, E_PHNUM_OFF) {
        Some(v) => v as usize,
        None => return ElfVerdict::Reject("ELF: truncated header"),
    };
    if phnum == 0 {
        // Сегментов нет. Инварианты сегментов нарушить нечем; образ
        // классифицируется только по типу.
        return ElfVerdict::Load {
            kind,
            bias_required: kind == ElfKind::Dyn,
        };
    }

    let phentsize = match rd_u16(data, E_PHENTSIZE_OFF) {
        Some(v) => v as usize,
        None => return ElfVerdict::Reject("ELF: truncated header"),
    };
    if phentsize < PHDR_SIZE {
        return ElfVerdict::Reject("ELF: invalid program header size");
    }
    let phoff = match rd_u64(data, E_PHOFF_OFF) {
        Some(v) => v as usize,
        None => return ElfVerdict::Reject("ELF: truncated header"),
    };

    let table_bytes = match phnum.checked_mul(phentsize) {
        Some(v) => v,
        None => return ElfVerdict::Reject("ELF: program header table size overflow"),
    };
    let table_end = match phoff.checked_add(table_bytes) {
        Some(v) => v,
        None => return ElfVerdict::Reject("ELF: program header table offset overflow"),
    };
    if table_end > data.len() {
        return ElfVerdict::Reject("ELF: program header table beyond data");
    }

    // ── проверка каждого сегмента ──
    for i in 0..phnum {
        // В границах: i < phnum и phnum*phentsize не переполнилось.
        let ph = phoff + i * phentsize;

        let p_type = match rd_u32(data, ph + P_TYPE_OFF) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: truncated program header"),
        };

        // PT_INTERP проверяет и резолвит эффектный загрузчик процесса; в этом
        // проходе это не загружаемый сегмент.

        if p_type != PT_LOAD {
            continue;
        }

        let p_offset = match rd_u64(data, ph + P_OFFSET_OFF) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: truncated program header"),
        };
        let p_vaddr = match rd_u64(data, ph + P_VADDR_OFF) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: truncated program header"),
        };
        let p_filesz = match rd_u64(data, ph + P_FILESZ_OFF) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: truncated program header"),
        };
        let p_memsz = match rd_u64(data, ph + P_MEMSZ_OFF) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: truncated program header"),
        };

        // filesz не больше memsz (отрицательный bss-хвост невозможен). (R5.9)
        if p_filesz > p_memsz {
            return ElfVerdict::Reject("ELF: p_filesz exceeds p_memsz");
        }

        // Диапазон данных файла обязан лежать в переданном буфере. (R5.9)
        let file_end = match p_offset.checked_add(p_filesz) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: segment file range overflow"),
        };
        if file_end > data.len() as u64 {
            return ElfVerdict::Reject("ELF: file data beyond input");
        }

        // Виртуальный диапазон обязан быть каноническим нижней половиной и не
        // переполняться. Для ET_DYN это нижняя граница при нулевом bias;
        // choose_bias перепроверяет диапазон с bias. (R5.5)
        let vaddr_end = match p_vaddr.checked_add(p_memsz) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: vaddr range overflow"),
        };
        let page_end = match page_up(vaddr_end) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: vaddr page-round overflow"),
        };
        if page_end >= USER_ADDR_MAX {
            return ElfVerdict::Reject("ELF: segment outside user address space");
        }
    }

    ElfVerdict::Load {
        kind,
        bias_required: kind == ElfKind::Dyn,
    }
}

/// Дешёвый префиксный тест: начинается ли буфер с ELF64-заголовка Linux
/// x86_64 `ET_EXEC`/`ET_DYN`, то есть может ли [`classify_elf`] вообще его
/// принять.
///
/// Проверяются только поля `Ehdr` (магия, класс, порядок байт, `e_machine`,
/// `e_type`) — таблица program headers и сегменты **не** просматриваются, для
/// этого нужен весь образ. Поэтому `true` — это «кандидат для PATH-поиска
/// `lxrun`», а не гарантия загрузки: окончательный вердикт по-прежнему даёт
/// [`classify_elf`] на полном буфере внутри загрузчика, и негодный кандидат
/// отвергается там с описательной причиной, а не запускается.
///
/// Нужен поиску, чтобы прочитать [`HEADER_LEN`] байт каждого кандидата, а не
/// весь образ (несколько МиБ) до того, как имя найдено.
pub fn looks_like_linux_elf(data: &[u8]) -> bool {
    if data.len() < EHDR_SIZE {
        return false;
    }
    if data[0] != 0x7F || &data[1..4] != b"ELF" {
        return false;
    }
    if data[EI_CLASS] != ELFCLASS64 || data[EI_DATA] != ELFDATA2LSB {
        return false;
    }
    if rd_u16(data, E_MACHINE_OFF) != Some(EM_X86_64) {
        return false;
    }
    matches!(rd_u16(data, E_TYPE_OFF), Some(ET_EXEC) | Some(ET_DYN))
}

/// Выбирает выровненный по странице load-bias для static-PIE (`ET_DYN`) образа.
///
/// `max_vaddr_end` — максимум `p_vaddr + p_memsz` по всем `PT_LOAD` (наивысший
/// несмещённый виртуальный адрес образа). Возвращает выровненный по 4096 `bias`,
/// при котором `bias + page_up(max_vaddr_end) < USER_ADDR_MAX`, поэтому каждый
/// смещённый сегмент строго внутри нижней половины пользовательского диапазона
/// (R5.2 / Property 16). `None`, если такой bias при детерминированной
/// [`PIE_BASE`] не помещается. Вся арифметика overflow-safe.
pub fn choose_bias(max_vaddr_end: u64) -> Option<u64> {
    let bias = PIE_BASE; // уже выровнен по 4096
    let end = page_up(max_vaddr_end)?;
    let top = bias.checked_add(end)?;
    if top < USER_ADDR_MAX {
        Some(bias)
    } else {
        None
    }
}
