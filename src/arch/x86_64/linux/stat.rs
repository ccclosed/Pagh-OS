//! Кодирование `struct stat` для слоя совместимости с x86_64-бинарями.
//!
//! Только `core`, без аллокаций, общая с крейтом `host-tests` (R11.6).
//! `fstat`/`newfstatat` заполняют [`LinuxStat`] из размера и режима файла и
//! копируют его в буфер пользователя; кодирование здесь — чистое ядро, которое
//! переиспользуют эти обработчики (R2.8).
#![allow(dead_code)]

/// Биты типа файла для regular-файла (`S_IFREG`).
///
/// Комбинируются с битами прав в значение `st_mode`, которого ждёт Linux-бинарь
/// (R2.8).
pub const S_IFREG: u32 = 0o100000;

/// Биты типа файла для каталога (`S_IFDIR`).
pub const S_IFDIR: u32 = 0o040000;

/// Биты типа файла для символической ссылки (`S_IFLNK`).
///
/// `/proc/self/exe` — первый узел в этом VFS, который их сообщает: `lstat`
/// (`newfstatat` с `AT_SYMLINK_NOFOLLOW`) использует `S_IFLNK | 0o777` и длину
/// цели ссылки как `st_size`, точно как Linux.
pub const S_IFLNK: u32 = 0o120000;

/// Разумный размер блока по умолчанию, сообщаемый в `st_blksize`.
pub const DEFAULT_BLKSIZE: i64 = 4096;

/// Раскладка Linux `struct stat` x86_64 (заполняется подмножество).
///
/// Порядок полей, размеры и паддинг точно соответствуют архитектурной
/// раскладке, поэтому побайтовая копия в буфер пользователя правильно
/// интерпретируется Linux-бинарём. `fstat`/`newfstatat` заполняют `st_size`,
/// `st_mode` (биты типа | права) и `st_blksize`; остальные поля нулевые (R2.8).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinuxStat {
    /// Id устройства, содержащего файл.
    pub st_dev: u64,
    /// Номер inode.
    pub st_ino: u64,
    /// Число жёстких ссылок.
    pub st_nlink: u64,
    /// Тип и режим файла (например, `S_IFREG | 0o644`).
    pub st_mode: u32,
    /// Id владеющего пользователя.
    pub st_uid: u32,
    /// Id владеющей группы.
    pub st_gid: u32,
    /// Паддинг для выравнивания `st_rdev`.
    pub __pad0: u32,
    /// Id устройства (если файл — специальный).
    pub st_rdev: u64,
    /// Полный размер в байтах.
    pub st_size: i64,
    /// Предпочтительный размер блока для I/O файловой системы.
    pub st_blksize: i64,
    /// Число выделенных 512-байтных блоков.
    pub st_blocks: i64,
    /// Время последнего доступа (секунды).
    pub st_atime: i64,
    /// Время последнего доступа (наносекунды).
    pub st_atime_nsec: i64,
    /// Время последней модификации (секунды).
    pub st_mtime: i64,
    /// Время последней модификации (наносекунды).
    pub st_mtime_nsec: i64,
    /// Время последнего изменения статуса (секунды).
    pub st_ctime: i64,
    /// Время последнего изменения статуса (наносекунды).
    pub st_ctime_nsec: i64,
    /// Зарезервированные неиспользуемые поля.
    pub __unused: [i64; 3],
}

impl LinuxStat {
    /// Полностью нулевой [`LinuxStat`]. Внутренняя заготовка для [`encode_stat`].
    const fn zeroed() -> Self {
        LinuxStat {
            st_dev: 0,
            st_ino: 0,
            st_nlink: 0,
            st_mode: 0,
            st_uid: 0,
            st_gid: 0,
            __pad0: 0,
            st_rdev: 0,
            st_size: 0,
            st_blksize: 0,
            st_blocks: 0,
            st_atime: 0,
            st_atime_nsec: 0,
            st_mtime: 0,
            st_mtime_nsec: 0,
            st_ctime: 0,
            st_ctime_nsec: 0,
            __unused: [0; 3],
        }
    }

    /// Прочитать закодированный размер по его архитектурному смещению (`st_size`).
    ///
    /// Чистый аксессор для Property P8, восстанавливающий вход. (R2.8)
    pub fn stat_size(&self) -> i64 {
        self.st_size
    }

    /// Прочитать закодированный режим по его архитектурному смещению (`st_mode`).
    ///
    /// Чистый аксессор для Property P8, восстанавливающий вход. (R2.8)
    pub fn stat_mode(&self) -> u32 {
        self.st_mode
    }
}

/// Закодировать [`LinuxStat`] из `size` и `mode` файла.
///
/// `size` кладётся в `st_size`, `mode` (уже несущий биты типа файла вроде
/// [`S_IFREG`]) — в `st_mode`. `st_blksize` ставится разумным значением, все
/// остальные поля обнуляются (R2.8).
pub fn encode_stat(size: u64, mode: u32, uid: u32, gid: u32) -> LinuxStat {
    let mut stat = LinuxStat::zeroed();
    stat.st_size = size as i64;
    stat.st_mode = mode;
    stat.st_uid = uid;
    stat.st_gid = gid;
    stat.st_blksize = DEFAULT_BLKSIZE;
    stat
}
