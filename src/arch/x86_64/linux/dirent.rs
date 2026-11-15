//! Чистая упаковка записей `getdents64`.
//!
//! Только `core`+`alloc`, общая с крейтом `host-tests` (R11.6). `getdents64`
//! (217) сериализует записи каталога в буфер пользователя как последовательность
//! упакованных `struct linux_dirent64`:
//!
//! ```c
//! struct linux_dirent64 {
//!     __u64        d_ino;     // 64-битный номер inode
//!     __s64        d_off;     // смещение к следующей записи
//!     unsigned short d_reclen; // длина этой записи
//!     unsigned char  d_type;   // тип файла
//!     char         d_name[];   // имя с NUL на конце
//! };
//! ```
//!
//! Фиксированный заголовок — 19 байт (`8 + 8 + 2 + 1`); длина записи — заголовок
//! плюс имя плюс его NUL, округлённая **вверх до границы 8 байт**, чтобы каждая
//! следующая запись начиналась с выравниванием на 8 (все поля остаются
//! естественно выровненными, на что Linux и рассчитывает). Эффектный обработчик в
//! `io_sys` двигает per-fd курсор каталога и добавляет по такой записи на ребёнка,
//! пока следующая не перестанет помещаться в буфер вызывающего.
//!
//! Модуль без ядерных зависимостей, поэтому упаковка проверяется свойствами на
//! хосте (размеры, выравнивание, NUL-терминация имени, круговой обход полей).
#![allow(dead_code)]

use alloc::vec;
use alloc::vec::Vec;

/// Значение `d_type`: неизвестный тип.
pub const DT_UNKNOWN: u8 = 0;
/// Значение `d_type`: каталог.
pub const DT_DIR: u8 = 4;
/// Значение `d_type`: regular-файл.
pub const DT_REG: u8 = 8;

/// Значение `d_type`: символическая ссылка.
pub const DT_LNK: u8 = 10;

/// Фиксированный заголовок `linux_dirent64` перед именем переменной длины:
/// `d_ino` (8) + `d_off` (8) + `d_reclen` (2) + `d_type` (1) = 19 байт.
pub const DIRENT_HEADER: usize = 19;

/// `d_type` для записи каталога по виду узла в VFS.
///
/// Проверка симлинка идёт первой: `/proc/self/exe` — ссылка, а не каталог, и
/// `ls -l` рисует `l`, только когда `d_type` это говорит (`docs/procfs.md` §5.3).
#[inline]
pub fn d_type_for(is_directory: bool, is_symlink: bool) -> u8 {
    if is_symlink {
        DT_LNK
    } else if is_directory {
        DT_DIR
    } else {
        DT_REG
    }
}

/// Вычислить `d_reclen`, выровненный на 8, для записи с именем длиной `name_len`
/// байт (хранимое имя всегда с NUL, отсюда `+ 1`).
///
/// `reclen = align_up(DIRENT_HEADER + name_len + 1, 8)`. Всегда положительное
/// кратное 8 и всегда `> DIRENT_HEADER`, поэтому запись вмещает хотя бы один
/// байт имени плюс терминатор.
#[inline]
pub fn dirent_reclen(name_len: usize) -> usize {
    (DIRENT_HEADER + name_len + 1 + 7) & !7
}

/// Упаковать одну запись `linux_dirent64` в свежевыделенный, полностью
/// обнулённый `Vec<u8>` длины [`dirent_reclen`]`(name.len())`.
///
/// Раскладка полей (little-endian, как в ABI x86_64):
///   * `[0..8)`   `d_ino`
///   * `[8..16)`  `d_off`
///   * `[16..18)` `d_reclen`
///   * `[18]`     `d_type`
///   * `[19..]`   `d_name`, скопированное как есть, с неявным NUL (буфер
///                обнулён) и хвостовым выравнивающим паддингом.
///
/// Буфер стартует обнулённым, поэтому байт сразу за именем гарантированно `0`,
/// имя всегда NUL-терминировано, а паддинг до `d_reclen` нулевой.
pub fn encode_dirent64(d_ino: u64, d_off: i64, d_type: u8, name: &[u8]) -> Vec<u8> {
    let reclen = dirent_reclen(name.len());
    let mut rec = vec![0u8; reclen];
    rec[0..8].copy_from_slice(&d_ino.to_le_bytes());
    rec[8..16].copy_from_slice(&d_off.to_le_bytes());
    rec[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
    rec[18] = d_type;
    rec[DIRENT_HEADER..DIRENT_HEADER + name.len()].copy_from_slice(name);
    // rec[DIRENT_HEADER + name.len()] остаётся 0 (NUL-терминатор).
    rec
}

/// Прочитать обратно поле `d_reclen` закодированной записи (чистый аксессор для
/// host-property-тестов, восстанавливающих границу записи).
#[inline]
pub fn record_reclen(rec: &[u8]) -> u16 {
    u16::from_le_bytes([rec[16], rec[17]])
}
