//! Чистый (`core`+`alloc`) рендеринг текстов синтетического `/proc`.
//!
//! Всё, что файлу `/proc` нужно *сказать*, живёт здесь как данные на входе и
//! байты на выходе: без VFS, без состояния ядра, без побочных эффектов,
//! видимых аллокатору. Ядро-ориентированный слой узлов ([`super::procfs`])
//! только собирает входы (счётчики PMM, тик-клок, `CompatState`, CPUID) и
//! отдаёт их форматтерам; точные байтовые форматы и таблица путей/inode — здесь,
//! чтобы `host-tests` включали файл дословно через `#[path]` и проверяли
//! свойства (контракт `docs/procfs.md` §7).
//!
//! Форматы намеренно Linux-совместимы, а несущие детали взяты из парсеров самих
//! потребителей (см. `docs/procfs.md` §4):
//!   * `meminfo` — libuv `strstr("MemTotal:")` + `sscanf("%llu kB")`;
//!   * `cpuinfo` — libuv `fscanf("processor\t: %u\n")` + литерал
//!     `"model name\t: "`;
//!   * `uptime`  — libuv/vmstat `sscanf("%lf")`;
//!   * `maps`    — разбор диапазонов адресов и прав в readelf/санитайзерах;
//!   * `status`  — строки key/tab/value, как в Linux `fs/proc/array.c`.

#![allow(dead_code)]

use alloc::fmt;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Дописывает форматированную строку в `out`.
///
/// `Vec<u8>` в этой конфигурации не реализует `core::fmt::Write`, поэтому текст
/// готовится через `alloc::fmt::format` и копируется — одна небольшая аллокация
/// на строку, зато байтовая раскладка очевидна и одинакова в ядре и в
/// `host-tests`.
fn push(out: &mut Vec<u8>, args: fmt::Arguments<'_>) {
    let s = fmt::format(args);
    out.extend_from_slice(s.as_bytes());
}

// ─── таблица путей / inode ──────────────────────────────────────────────────
//
// Номера inode — константы, чтобы путь сохранял одну идентичность во всех
// `lookup()`: `d_ino` из `getdents64`, `st_ino` из `stat` и повторные open
// согласованы. База лежит выше диапазона ext2 (счётчик ramfs стартует с
// 0x0054_0000 именно поэтому, `src/vfs/ramfs.rs`) и ниже бита 63, который
// `synth_ino` резервирует под FNV-идентичность узлов без настоящего inode.

/// Базовый номер inode для всех узлов procfs.
pub const PROC_INO_BASE: u64 = 0x00F0_0000_0000;
/// Сам `/proc` (внутренний узел; `MountNode` сообщает FNV-идентичность имени
/// монтирования `proc`, см. `docs/procfs.md` §2.3).
pub const PROC_ROOT_DIR_INO: u64 = PROC_INO_BASE;
/// `/proc/self`.
pub const PROC_SELF_DIR_INO: u64 = PROC_INO_BASE + 1;
/// `/proc/sys` (`docs/procfs.md` §2.3).
pub const PROC_SYS_DIR_INO: u64 = PROC_INO_BASE + 10;
/// `/proc/sys/kernel`.
pub const PROC_SYS_KERNEL_DIR_INO: u64 = PROC_INO_BASE + 11;
/// `/proc/net` — каталог сетевой статистики (`docs/procfs.md` §15).
pub const PROC_NET_DIR_INO: u64 = PROC_INO_BASE + 23;

/// Какой файл `/proc` представляет узел.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcKind {
    /// `/proc/self/exe` — симлинк на запущенный образ.
    SelfExe,
    /// `/proc/self/cmdline`.
    SelfCmdline,
    /// `/proc/self/status`.
    SelfStatus,
    /// `/proc/self/maps`.
    SelfMaps,
    /// `/proc/self/stat` — Linux-строка `stat` читателя.
    SelfStat,
    /// `/proc/self/statm` — сводка памяти читателя в страницах.
    SelfStatm,
    /// `/proc/cpuinfo`.
    CpuInfo,
    /// `/proc/meminfo`.
    MemInfo,
    /// `/proc/uptime`.
    Uptime,
    /// `/proc/stat` — тиковые/событийные счётчики ядра.
    Stat,
    /// `/proc/version` — строка идентичности этого ядра.
    Version,
    /// `/proc/filesystems` — драйверы ФС, которые реализует эта сборка.
    FileSystems,
    /// `/proc/loadavg` — средние нагрузки плюс живые счётчики задач.
    LoadAvg,
    /// `/proc/mounts` — таблица монтирования VFS (порядок полей Linux).
    Mounts,
    /// `/proc/self/mounts` — та же таблица, per-process путь.
    SelfMounts,
    /// `/proc/self/cgroup` — корневая cgroup (без контроллеров).
    SelfCgroup,
    /// `/proc/<pid>/cgroup`.
    PidCgroup,
    /// `/proc/self/mountinfo`.
    SelfMountInfo,
    /// `/proc/<pid>/mountinfo`.
    PidMountInfo,
    /// `/proc/sys/kernel/ostype`.
    SysOstype,
    /// `/proc/sys/kernel/osrelease`.
    SysOsrelease,
    /// `/proc/sys/kernel/hostname`.
    SysHostname,
    /// `/proc/<pid>/cmdline`.
    PidCmdline,
    /// `/proc/<pid>/stat`.
    PidStat,
    /// `/proc/<pid>/status`.
    PidStatus,
    /// `/proc/<pid>/maps`.
    PidMaps,
    /// `/proc/<pid>/limits` и `/proc/self/limits`.
    PidLimits,
    /// `/proc/self/limits`.
    SelfLimits,
    /// `/proc/<pid>/fdinfo/<n>` (pid цели и fd несёт узел).
    FdInfo,
    /// `/proc/net/dev` — счётчики канала в Linux-формате.
    NetDev,
    /// `/proc/partitions` — блочные устройства USB в Linux-подобном формате.
    Partitions,
}

/// Одна именованная запись: имя для `readdir`, стабильный inode и вид
/// содержимого.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcEntry {
    pub name: &'static str,
    pub ino: u64,
    pub kind: ProcKind,
}

/// Обычные файлы непосредственно под `/proc` (в порядке `readdir`).
pub const PROC_FILES: [ProcEntry; 8] = [
    ProcEntry {
        name: "cpuinfo",
        ino: PROC_INO_BASE + 6,
        kind: ProcKind::CpuInfo,
    },
    ProcEntry {
        name: "meminfo",
        ino: PROC_INO_BASE + 7,
        kind: ProcKind::MemInfo,
    },
    ProcEntry {
        name: "uptime",
        ino: PROC_INO_BASE + 8,
        kind: ProcKind::Uptime,
    },
    ProcEntry {
        name: "stat",
        ino: PROC_INO_BASE + 9,
        kind: ProcKind::Stat,
    },
    ProcEntry {
        name: "version",
        ino: PROC_INO_BASE + 15,
        kind: ProcKind::Version,
    },
    ProcEntry {
        name: "filesystems",
        ino: PROC_INO_BASE + 16,
        kind: ProcKind::FileSystems,
    },
    ProcEntry {
        name: "loadavg",
        ino: PROC_INO_BASE + 17,
        kind: ProcKind::LoadAvg,
    },
    ProcEntry {
        name: "mounts",
        ino: PROC_INO_BASE + 18,
        kind: ProcKind::Mounts,
    },
];

/// Обычные файлы под `/proc/sys/kernel` (в порядке `readdir`).
pub const PROC_SYS_FILES: [ProcEntry; 3] = [
    ProcEntry {
        name: "ostype",
        ino: PROC_INO_BASE + 12,
        kind: ProcKind::SysOstype,
    },
    ProcEntry {
        name: "osrelease",
        ino: PROC_INO_BASE + 13,
        kind: ProcKind::SysOsrelease,
    },
    ProcEntry {
        name: "hostname",
        ino: PROC_INO_BASE + 14,
        kind: ProcKind::SysHostname,
    },
];

/// Обычные файлы под `/proc/net` (порядок `readdir`; сейчас один — `dev`).
pub const PROC_NET_FILES: [ProcEntry; 1] = [ProcEntry {
    name: "dev",
    ino: PROC_INO_BASE + 24,
    kind: ProcKind::NetDev,
}];

/// Обычные файлы под `/proc/self` (в порядке `readdir`).
///
/// `stat` и `statm` описывают *вызывающий* процесс, поэтому их inode, как и у
/// остальных записей `self`, фиксированы здесь. `/proc/<pid>/stat` использует
/// ниже идентичность от pid; пути описывают одно состояние, но являются разными
/// VFS-узлами (`self` здесь — каталог, а не симлинк на pid-каталог).
pub const PROC_SELF_FILES: [ProcEntry; 10] = [
    ProcEntry {
        name: "exe",
        ino: PROC_INO_BASE + 2,
        kind: ProcKind::SelfExe,
    },
    ProcEntry {
        name: "cmdline",
        ino: PROC_INO_BASE + 3,
        kind: ProcKind::SelfCmdline,
    },
    ProcEntry {
        name: "status",
        ino: PROC_INO_BASE + 4,
        kind: ProcKind::SelfStatus,
    },
    ProcEntry {
        name: "maps",
        ino: PROC_INO_BASE + 5,
        kind: ProcKind::SelfMaps,
    },
    ProcEntry {
        name: "stat",
        ino: PROC_INO_BASE + 10,
        kind: ProcKind::SelfStat,
    },
    ProcEntry {
        name: "statm",
        ino: PROC_INO_BASE + 11,
        kind: ProcKind::SelfStatm,
    },
    ProcEntry {
        name: "mounts",
        ino: PROC_INO_BASE + 19,
        kind: ProcKind::SelfMounts,
    },
    ProcEntry {
        name: "cgroup",
        ino: PROC_INO_BASE + 20,
        kind: ProcKind::SelfCgroup,
    },
    ProcEntry {
        name: "mountinfo",
        ino: PROC_INO_BASE + 21,
        kind: ProcKind::SelfMountInfo,
    },
    ProcEntry {
        name: "limits",
        ino: PROC_INO_BASE + 22,
        kind: ProcKind::SelfLimits,
    },
];

/// Порядок `readdir` **статических** записей `/proc`: сначала `self`/`sys`, затем
/// обычные файлы. Живые числовые pid-каталоги вставляет между `self` и `cpuinfo`
/// ядро-ориентированный `ProcDir::readdir` (pid по возрастанию, снимок реестра
/// compat); они динамические и здесь не перечисляются.
pub const PROC_ROOT_ORDER: [&str; 11] = [
    "self",
    "sys",
    "cpuinfo",
    "meminfo",
    "uptime",
    "stat",
    "version",
    "filesystems",
    "loadavg",
    "mounts",
    "net",
];

// ─── идентичность пути / inode для /proc/<pid> ──────────────────────────────
//
// У pid-каталога нет фиксированной строки таблицы: его идентичность выводится из
// pid, поэтому `getdents64` (`d_ino`), `stat` и повторные `lookup` согласованы на
// всё время жизни каталога. База выше статического диапазона (`PROC_INO_BASE +
// 11`), а шаг оставляет место файлам каталога. pid маскируется до младших 32
// бит: планировщик pagh выдаёт небольшие последовательные pid, а 32-битная
// маска держит результат ниже бита 63 при любом входе.

/// Первый inode диапазона каталогов `/proc/<pid>`.
pub const PROC_PID_DIR_INO_BASE: u64 = PROC_INO_BASE + 0x0000_0000_0010_0000;
/// Inode, резервируемые на один pid-каталог. Шаг оставляет место и для
/// динамических идентичностей `/proc/<pid>/fd/<n>` (`+0x100`) и
/// `/proc/<pid>/fdinfo/<n>` (`+0x1000`), чей fd ограничен `NOFILE_MAX` (1024) —
/// заметно ниже `0x10000`.
pub const PROC_PID_INO_STRIDE: u64 = 0x1_0000;
/// pid маскируются до стольких бит перед выводом inode.
pub const PROC_PID_INO_MASK: u64 = 0xFFFF_FFFF;

/// Стабильный inode каталога `/proc/<pid>`.
pub fn pid_dir_ino(pid: u64) -> u64 {
    PROC_PID_DIR_INO_BASE + (pid & PROC_PID_INO_MASK) * PROC_PID_INO_STRIDE
}

/// Стабильный inode файла под `/proc/<pid>` (`slot` — его запись таблицы).
pub fn pid_file_ino(pid: u64, slot: u64) -> u64 {
    pid_dir_ino(pid) + slot
}

/// Стабильный inode симлинка `/proc/<pid>/fd/<fd>` (используется и для
/// `/proc/self/fd/<fd>`).
pub fn pid_fd_ino(pid: u64, fd: u32) -> u64 {
    pid_dir_ino(pid) + 0x100 + fd as u64
}

/// Стабильный inode обычного файла `/proc/<pid>/fdinfo/<fd>`.
///
/// Отдельный диапазон от [`pid_fd_ino`]: симлинк fd и файл fdinfo — разные записи
/// каталога с разными inode, как в Linux.
pub fn pid_fdinfo_ino(pid: u64, fd: u32) -> u64 {
    pid_dir_ino(pid) + 0x1000 + fd as u64
}

/// Смещение каталога `/proc/<pid>/task` внутри inode-шага pid.
///
/// Выбрано ниже диапазона симлинков fd (`+0x100`) и не пересекается ни со слотами
/// статических файлов (`+1..=7`), ни со смещениями каталогов fd/fdinfo (`+0x10`,
/// `+0x11`), поэтому каждая запись под одним pid-каталогом сохраняет уникальную,
/// стабильную по пути идентичность.
pub const PROC_PID_TASK_DIR_OFFSET: u64 = 0x30;

/// Стабильный inode каталога `/proc/<pid>/task`.
///
/// Собственный каталог треда `/proc/<tgid>/task/<tid>` намеренно **не** получает
/// новый inode: это тот же VFS-объект, что `/proc/<tid>`, как в Linux, где у
/// задачи один inode, достижимый обоими путями. Поэтому в модуле нет
/// `pid_task_tid_ino` — ядро-ориентированный слой узлов резолвит `<tid>` в
/// существующий pid-каталог ([`pid_dir_ino`]), и оба пути `stat` одинаково.
pub fn pid_task_dir_ino(pid: u64) -> u64 {
    pid_dir_ino(pid) + PROC_PID_TASK_DIR_OFFSET
}

/// Один файл под `/proc/<pid>`: статическое имя, слот inode и вид.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcPidEntry {
    pub name: &'static str,
    pub slot: u64,
    pub kind: ProcKind,
}

/// Файлы каждого каталога `/proc/<pid>` в порядке `readdir`.
pub const PROC_PID_FILES: [ProcPidEntry; 7] = [
    ProcPidEntry {
        name: "cmdline",
        slot: 1,
        kind: ProcKind::PidCmdline,
    },
    ProcPidEntry {
        name: "stat",
        slot: 2,
        kind: ProcKind::PidStat,
    },
    ProcPidEntry {
        name: "status",
        slot: 3,
        kind: ProcKind::PidStatus,
    },
    ProcPidEntry {
        name: "maps",
        slot: 4,
        kind: ProcKind::PidMaps,
    },
    ProcPidEntry {
        name: "cgroup",
        slot: 5,
        kind: ProcKind::PidCgroup,
    },
    ProcPidEntry {
        name: "mountinfo",
        slot: 6,
        kind: ProcKind::PidMountInfo,
    },
    ProcPidEntry {
        name: "limits",
        slot: 7,
        kind: ProcKind::PidLimits,
    },
];

/// Порядок `readdir` для `/proc/<pid>`.
pub const PROC_PID_ORDER: [&str; 7] = [
    "cmdline",
    "stat",
    "status",
    "maps",
    "cgroup",
    "mountinfo",
    "limits",
];

/// Ищет файл `/proc/<pid>` по имени.
pub fn proc_pid_file(name: &str) -> Option<ProcPidEntry> {
    PROC_PID_FILES.iter().copied().find(|e| e.name == name)
}

/// Разбирает компонент пути `/proc` как Linux pid.
pub fn parse_pid_component(name: &str) -> Option<u64> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || !bytes.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let pid: u64 = name.parse().ok()?;
    if pid == 0 || pid > i32::MAX as u64 {
        return None;
    }
    Some(pid)
}

/// Ищет обычный файл `/proc` по имени (`self`/`sys` — каталоги).
pub fn proc_file(name: &str) -> Option<ProcEntry> {
    PROC_FILES.iter().copied().find(|e| e.name == name)
}

/// Ищет файл `/proc/self` по имени.
pub fn proc_self_file(name: &str) -> Option<ProcEntry> {
    PROC_SELF_FILES.iter().copied().find(|e| e.name == name)
}

/// Ищет файл `/proc/sys/kernel` по имени.
pub fn proc_sys_file(name: &str) -> Option<ProcEntry> {
    PROC_SYS_FILES.iter().copied().find(|e| e.name == name)
}

/// Ищет файл `/proc/net` по имени (`docs/procfs.md` §15).
pub fn proc_net_file(name: &str) -> Option<ProcEntry> {
    PROC_NET_FILES.iter().copied().find(|e| e.name == name)
}

// ─── /proc/meminfo ──────────────────────────────────────────────────────────

/// Счётчики памяти ядра для `/proc/meminfo`, уже в КиБ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemInfoKb {
    pub total_kb: u64,
    pub free_kb: u64,
}

/// Обязательные ключи `meminfo` по порядку: `(метка, значение)`.
///
/// `MemAvailable` равно `MemFree`, потому что в этом ядре нет page cache и
/// reclaimable slab: это честное число, а не заглушка (`docs/procfs.md`
/// DEVIATION-3). Остальные счётчики — настоящие нули.
fn meminfo_rows(m: &MemInfoKb) -> [(&'static str, u64); 18] {
    [
        ("MemTotal:", m.total_kb),
        ("MemFree:", m.free_kb),
        ("MemAvailable:", m.free_kb),
        ("Buffers:", 0),
        ("Cached:", 0),
        ("SwapCached:", 0),
        ("Active:", 0),
        ("Inactive:", 0),
        ("SwapTotal:", 0),
        ("SwapFree:", 0),
        ("Dirty:", 0),
        ("Writeback:", 0),
        ("AnonPages:", 0),
        ("Mapped:", 0),
        ("Shmem:", 0),
        ("Slab:", 0),
        ("SReclaimable:", 0),
        ("SUnreclaim:", 0),
    ]
}

/// Рендерит `/proc/meminfo`: `label` дополняется до 16 колонок, значение
/// выровнено вправо в 8, затем `" kB\n"` — байт-в-байт по форме как Linux
/// `show_val_kb`.
pub fn format_meminfo(m: &MemInfoKb) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);
    for (label, kb) in meminfo_rows(m) {
        push(&mut out, format_args!("{:<16}{:>8} kB\n", label, kb));
    }
    out
}

// ─── /proc/uptime ───────────────────────────────────────────────────────────

/// Рендерит `/proc/uptime`: `"<сек>.<сотые> <idle_сек>.<idle_сотые>\n"`.
///
/// Поле idle — `0.00`: это ядро не учитывает простой (`docs/procfs.md`
/// DEVIATION-4), и выдуманный ненулевой idle был бы хуже правды. Потребители
/// (`libuv`, `uptime(1)`, `htop`) читают только первое поле.
pub fn format_uptime(ticks: u64, tick_hz: u64) -> Vec<u8> {
    let hz = if tick_hz == 0 { 1 } else { tick_hz };
    let secs = ticks / hz;
    let centis = (ticks % hz) * 100 / hz;
    let mut out = Vec::with_capacity(32);
    push(&mut out, format_args!("{}.{:02} 0.00\n", secs, centis));
    out
}

// ─── таблица монтирования ───────────────────────────────────────────────────

/// Рендерит `/proc/*/mountinfo` (Linux-подмножество 36 полей mountinfo).
///
/// В pagh нет отдельных блочных устройств и строк опций на монтирование, поэтому
/// секция необязательных полей пуста (`-`), major:minor — `0:0`, root — `/`, а
/// опции и super-options — `rw`. ID назначаются в порядке сортировки точек
/// монтирования; корень — родитель самому себе, у остальных родитель 1.
pub fn format_mountinfo(mounts: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, (point, fs)) in mounts.iter().enumerate() {
        let id = index + 1;
        let parent = if index == 0 { id } else { 1 };
        push(
            &mut out,
            format_args!("{id} {parent} 0:0 / {} rw - {} {} rw\n", point, fs, fs),
        );
    }
    out
}

/// Рендерит `/proc/mounts` / `/proc/self/mounts`.
///
/// Порядок полей Linux: `device mount_point fstype options dump pass`. В pagh нет
/// опций на монтирование и отдельных блочных устройств, поэтому device — сама
/// точка монтирования, options — `rw`, dump/pass — `0 0`.
pub fn format_mounts(mounts: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (point, fs) in mounts {
        push(
            &mut out,
            format_args!("{} {} {} rw 0 0\n", point, point, fs),
        );
    }
    out
}

/// `/proc/*/cgroup` — pagh держит каждый процесс в одной корневой cgroup и не
/// реализует контроллеров. В Linux v2 одна строка на иерархию: `0::/`.
pub fn format_cgroup() -> Vec<u8> {
    Vec::from(&b"0::/\n"[..])
}

// ─── /proc/<pid>/fdinfo/<fd> ────────────────────────────────────────────────

/// Настоящие, принадлежащие ядру поля одного открытого дескриптора — как их
/// показывает `/proc/.../fdinfo`. `mnt_id`/`ino` равны `0` у объектов без VFS-
/// монтирования или VFS-inode (пайпы, сокеты, event-объекты, stdio): VFS этого
/// ядра определяет `fs_ino() == 0` как «нет настоящей идентичности inode», а за
/// такими объектами нет монтирования, поэтому `0` — честный сентинел, а не
/// выдуманный id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FdInfo {
    /// Текущая позиция файла (`f_pos`); 0 для объектов без неё.
    pub pos: u64,
    /// Linux `f_flags`, включая `O_CLOEXEC`, если дескриптор его несёт.
    pub flags: u64,
    /// Mount id из таблицы монтирования VFS, либо 0, если за объектом нет
    /// монтирования.
    pub mnt_id: u64,
    /// Номер VFS-inode, либо 0, если у объекта нет VFS-inode.
    pub ino: u64,
}

/// Рендерит `/proc/<pid>/fdinfo/<fd>` в порядке Linux `fs/proc/fd.c`.
///
/// Ведущий `0` у `flags` — это Linux-формат `0%o`: поле в C-стиле восьмеричное,
/// поэтому `O_RDONLY` рендерится как `00`, а `O_RDWR` — как `02`.
pub fn format_fdinfo(i: &FdInfo) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    push(
        &mut out,
        format_args!(
            "pos:\t{}\nflags:\t0{:o}\nmnt_id:\t{}\nino:\t{}\n",
            i.pos, i.flags, i.mnt_id, i.ino
        ),
    );
    out
}

// ─── /proc/<pid>/limits ─────────────────────────────────────────────────────

/// Число resource id, определённых в Linux (`RLIMIT_NLIMITS`); таблица rlimit
/// этого ядра ровно такой ширины (`misc::RLIMIT_NLIMITS`).
pub const RLIMIT_COUNT: usize = 16;
/// Linux `RLIM_INFINITY`: значение «без предела».
pub const RLIM_INFINITY: u64 = u64::MAX;

/// Одна строка `/proc/<pid>/limits`: имя и единица из Linux. `units: None`
/// означает, что у строки нет колонки единицы (Linux печатает хвостовой пробел).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitRow {
    pub name: &'static str,
    pub units: Option<&'static str>,
}

/// Строки Linux `RLIMIT_*` в порядке resource id 0..15
/// (`fs/proc/base.c::lnames`). Порядок — это ABI: `limits` позиционен для
/// скриптов, сравнивающих его с `ulimit -a`.
pub const LIMIT_ROWS: [LimitRow; RLIMIT_COUNT] = [
    LimitRow {
        name: "Max cpu time",
        units: Some("seconds"),
    },
    LimitRow {
        name: "Max file size",
        units: Some("bytes"),
    },
    LimitRow {
        name: "Max data size",
        units: Some("bytes"),
    },
    LimitRow {
        name: "Max stack size",
        units: Some("bytes"),
    },
    LimitRow {
        name: "Max core file size",
        units: Some("bytes"),
    },
    LimitRow {
        name: "Max resident set",
        units: Some("bytes"),
    },
    LimitRow {
        name: "Max processes",
        units: Some("processes"),
    },
    LimitRow {
        name: "Max open files",
        units: Some("files"),
    },
    LimitRow {
        name: "Max locked memory",
        units: Some("bytes"),
    },
    LimitRow {
        name: "Max address space",
        units: Some("bytes"),
    },
    LimitRow {
        name: "Max file locks",
        units: Some("locks"),
    },
    LimitRow {
        name: "Max pending signals",
        units: Some("signals"),
    },
    LimitRow {
        name: "Max msgqueue size",
        units: Some("bytes"),
    },
    LimitRow {
        name: "Max nice priority",
        units: None,
    },
    LimitRow {
        name: "Max realtime priority",
        units: None,
    },
    LimitRow {
        name: "Max realtime timeout",
        units: Some("us"),
    },
];

/// `unlimited` или десятичное значение — ровно как печатает Linux.
fn limit_value(value: u64) -> String {
    if value == RLIM_INFINITY {
        String::from("unlimited")
    } else {
        fmt::format(format_args!("{}", value))
    }
}

/// Рендерит `/proc/<pid>/limits`.
///
/// Ширины заголовка и строк — Linux байт-в-байт
/// (`fs/proc/base.c::proc_pid_limits`): имя 25 + пробел, soft 20 + пробел, hard
/// 20 + пробел, затем единица шириной 10. Заголовок — 78 байт, включая пять
/// хвостовых пробелов после `Units`; строка без единицы ("Max nice priority")
/// заканчивается одним разделяющим пробелом после колонки hard, как в Linux.
pub fn format_limits(limits: &[(u64, u64); RLIMIT_COUNT]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 * 80);
    out.extend_from_slice(
        b"Limit                     Soft Limit           Hard Limit           Units     \n",
    );
    for (row, (cur, max)) in LIMIT_ROWS.iter().zip(limits.iter()) {
        let cur = limit_value(*cur);
        let max = limit_value(*max);
        if let Some(unit) = row.units {
            push(
                &mut out,
                format_args!("{:<25} {:<20} {:<20} {:<10}\n", row.name, cur, max, unit),
            );
        } else {
            push(
                &mut out,
                format_args!("{:<25} {:<20} {:<20} \n", row.name, cur, max),
            );
        }
    }
    out
}

// ─── /proc/version ──────────────────────────────────────────────────────────

/// Рендерит `/proc/version`.
///
/// pagh — не Linux, поэтому это **не** поддельная строка Linux-релиза: первое
/// слово — имя ядра, второе — настоящая версия крейта
/// (`env!("CARGO_PKG_VERSION")`, тот же источник, что `/mnt/etc/pagh-release`),
/// затем машина и фиксированный build-тег.
///
/// Хвостовой `#1 SMP` — форма, которую grep-ят скрипты-потребители; `#1` —
/// постоянный номер сборки этого ядра, а `SMP` — часть запрошенного
/// фиксированного тега: pagh — **однопроцессорное** ядро, и эта строка не
/// заявляет поддержку SMP. Версия передаётся аргументом, а не читается здесь,
/// чтобы форматтер оставался чистым и host-тестируемым; ядро-сторона узла
/// подставляет версию крейта.
pub fn format_version(version: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + version.len());
    push(&mut out, format_args!("pagh {} (x86_64) #1 SMP\n", version));
    out
}

// ─── /proc/filesystems ──────────────────────────────────────────────────────

/// Строка `/proc/filesystems`: ФС, которую эта сборка действительно реализует.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileSystemEntry {
    /// Linux-маркер `nodev`: ФС не нужен блочный диск.
    pub nodev: bool,
    /// Имя ФС из Linux (не имя Rust-модуля).
    pub name: &'static str,
}

/// Драйверы ФС, присутствующие в этой сборке, в задокументированном порядке.
///
/// «Присутствует» означает **реализован и зарегистрирован в VFS**, а не
/// «смонтирован»: Linux листает драйверы здесь независимо от наличия
/// смонтированного экземпляра. В pagh нет рантайм-реестра ФС, поэтому эта
/// таблица *и есть* реестр — добавление или удаление драйвера обязано обновлять
/// её в том же коммите (`docs/procfs.md` §4.10).
///
/// * `proc` — `src/vfs/procfs.rs`, синтетическая, без блочного устройства;
/// * `ramfs` — `src/vfs/ramfs.rs`, in-memory `/tmp`, без блочного устройства;
/// * `ext2` — `src/fs/ext2/`, on-disk корень, монтируемый в `/mnt` при наличии.
pub const FILESYSTEMS: [FileSystemEntry; 3] = [
    FileSystemEntry {
        nodev: true,
        name: "proc",
    },
    FileSystemEntry {
        nodev: true,
        name: "ramfs",
    },
    FileSystemEntry {
        nodev: false,
        name: "ext2",
    },
];

/// Рендерит `/proc/filesystems`: `nodev\t<имя>\n` для ФС без устройства,
/// `\t<имя>\n` для ФС с блочным устройством.
///
/// Ведущий таб в строке блочной ФС — точная раскладка Linux (`seq_puts(m,
/// "\text4\n")` без слова `nodev`); все парсеры делят по пробельным символам,
/// но байтовая форма зафиксирована здесь, чтобы будущий читатель `mount -t` мог
/// на неё положиться.
pub fn format_filesystems(entries: &[FileSystemEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 * entries.len());
    for e in entries {
        if e.nodev {
            out.extend_from_slice(b"nodev\t");
        } else {
            out.push(b'\t');
        }
        out.extend_from_slice(e.name.as_bytes());
        out.push(b'\n');
    }
    out
}

// ─── /proc/loadavg ──────────────────────────────────────────────────────────

/// Входы `/proc/loadavg`.
///
/// Три средние нагрузки не входят во входы, потому что pagh не ведёт их учёт:
/// форматтер всегда печатает `0.00 0.00 0.00` — честное значение. `uv_loadavg`
/// из libuv откатывается к `sysinfo(2)`, который уже реализован с
/// `loads = [0; 3]` (`docs/procfs.md` §4.11), так что ниже по потоку никто не
/// обманут. Остальные поля настоящие: runnable и total — живые счётчики
/// планировщика, `last_pid` — последний выданный pid (`0` до первой выдачи).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadAvg {
    /// Средняя за 1 минуту в сотых (`123` = `1.23`).
    pub one: u32,
    /// Средняя за 5 минут в сотых.
    pub five: u32,
    /// Средняя за 15 минут в сотых.
    pub fifteen: u32,
    /// Очередь ready + выполняющаяся задача (idle исключён).
    pub runnable: u64,
    /// Живые задачи: runnable + parked/stopped (блокирующие ожидания остаются
    /// runnable).
    pub total: u64,
    /// Последний выданный планировщиком pid, либо `0`, если не выдавался.
    pub last_pid: u64,
}

/// Рендерит `/proc/loadavg`:
/// `"<1m> <5m> <15m> <runnable>/<total> <last_pid>\n"`, средние в сотых.
///
/// Четвёртое поле Linux — `runnable/total`, где `total` — число тредов. В pagh
/// нет состояния blocked (uninterruptible) и списка спящих задач, поэтому
/// `total` — все живые задачи в состояниях ротации планировщика: runnable плюс
/// stopped. Счётчики создания задач намеренно не используются: они росли бы
/// вечно и не являются живым счётом.
pub fn format_loadavg(l: &LoadAvg) -> Vec<u8> {
    let mut out = Vec::with_capacity(48);
    push(
        &mut out,
        format_args!(
            "{}.{:02} {}.{:02} {}.{:02} {}/{} {}\n",
            l.one / 100,
            l.one % 100,
            l.five / 100,
            l.five % 100,
            l.fifteen / 100,
            l.fifteen % 100,
            l.runnable,
            l.total,
            l.last_pid
        ),
    );
    out
}

// ─── /proc/stat ─────────────────────────────────────────────────────────────

/// Тиковые и событийные счётчики ядра за `/proc/stat`, уже в единицах `USER_HZ`
/// (один тик = одна единица; `USER_HZ == TICK_HZ == 1000`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatInputs {
    /// Тики ring 3.
    pub user: u64,
    /// Тики ring 0, не idle.
    pub system: u64,
    /// Тики idle-задачи / `hlt`.
    pub idle: u64,
    /// Переключения контекста с момента загрузки.
    pub ctxt: u64,
    /// Wall-clock время загрузки в Unix-секундах, `0`, если RTC не дал валидного
    /// времени.
    pub btime: u64,
    /// Задачи, созданные с загрузки (аналог Linux `total_forks`).
    pub processes: u64,
    /// Задачи ready + running (idle исключён).
    pub procs_running: u64,
    /// Заблокированные задачи (в этом ядре состояния blocked нет: всегда `0`).
    pub procs_blocked: u64,
}

/// Рендерит `/proc/stat` для однопроцессорной машины.
///
/// Порядок полей Linux, ровно 10 числовых колонок на строку CPU:
///
/// ```text
/// cpu  user nice system idle iowait irq softirq steal guest guest_nice
/// cpu0 …
/// ctxt …
/// btime …
/// processes …
/// procs_running …
/// procs_blocked …
/// ```
///
/// `nice`, `iowait`, `irq`, `softirq`, `steal`, `guest` и `guest_nice` — настоящие
/// нули: ядро их не учитывает, а выдуманные значения были бы хуже молчания. По
/// той же причине опущены строки `intr` и `softirq`: нет per-IRQ счётчика, чтобы
/// заполнить их правдиво (`docs/procfs.md` §4.8, DEVIATION-15).
///
/// У агрегатной строки `cpu` **два** пробела после метки: Linux пишет `"cpu "`, а
/// затем каждое значение с одним пробелом перед ним (`show_val`), тогда как
/// per-CPU строка `cpu0` пишет метку без хвостового пробела. `uv_cpu_info` из
/// libuv отбрасывает первую строку и разбирает `cpu%u` + шесть чисел, поэтому
/// именно строка `cpu0` заставляет `os.cpus()` в Node работать на однопроцессорной
/// машине.
pub fn format_stat(s: &StatInputs) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    push(
        &mut out,
        format_args!("cpu  {} 0 {} {} 0 0 0 0 0 0\n", s.user, s.system, s.idle),
    );
    push(
        &mut out,
        format_args!("cpu0 {} 0 {} {} 0 0 0 0 0 0\n", s.user, s.system, s.idle),
    );
    push(&mut out, format_args!("ctxt {}\n", s.ctxt));
    push(&mut out, format_args!("btime {}\n", s.btime));
    push(&mut out, format_args!("processes {}\n", s.processes));
    push(
        &mut out,
        format_args!("procs_running {}\n", s.procs_running),
    );
    push(
        &mut out,
        format_args!("procs_blocked {}\n", s.procs_blocked),
    );
    out
}

// ─── /proc/sys/kernel ───────────────────────────────────────────────────────

/// `/proc/sys/kernel/ostype`: собственное имя ядра, не Linux.
///
/// Linux отвечает здесь `Linux\n`, потому что читает `init_uts_ns.name.sysname`.
/// `uname(2)` в pagh ради совместимости сообщает `Linux`, но контракт procfs
/// требует идентичность этого ядра, поэтому значение — `Pagh`, то же имя, что
/// использует проект (`docs/procfs.md` §4.12). Имя **ОС** (`Pagh-OS`) живёт в
/// `/etc/os-release`; полная модель — `docs/os-identity.md`.
pub const KERNEL_OSTYPE: &str = "Pagh";

/// `/proc/sys/kernel/hostname`: nodename, который сообщает `uname(2)`.
///
/// В pagh нет переменной hostname и нет `sethostname(2)`; `sys_uname` жёстко
/// возвращает `pagh`, поэтому файл зеркалит эту константу и всегда выглядит
/// записываемым, оставаясь статическим. Будущая поддержка hostname обязана
/// обновить оба места (`src/arch/x86_64/linux/misc.rs` и здесь).
pub const KERNEL_HOSTNAME: &str = "pagh";

/// Рендерит `/proc/sys/kernel/ostype`: `"Pagh\n"`.
pub fn format_sys_ostype() -> Vec<u8> {
    let mut out = Vec::with_capacity(KERNEL_OSTYPE.len() + 1);
    out.extend_from_slice(KERNEL_OSTYPE.as_bytes());
    out.push(b'\n');
    out
}

/// Рендерит `/proc/sys/kernel/osrelease`: настоящая версия крейта плюс перевод
/// строки.
///
/// Это тот же `env!("CARGO_PKG_VERSION")`, который встраивает `/proc/version` и
/// который `provision.rs` пишет в `/mnt/etc/pagh-release`; версия передаётся
/// сюда, чтобы форматтер оставался host-тестируемым.
pub fn format_sys_osrelease(version: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(version.len() + 1);
    out.extend_from_slice(version.as_bytes());
    out.push(b'\n');
    out
}

/// Рендерит `/proc/sys/kernel/hostname`: `KERNEL_HOSTNAME` плюс перевод строки.
pub fn format_sys_hostname() -> Vec<u8> {
    let mut out = Vec::with_capacity(KERNEL_HOSTNAME.len() + 1);
    out.extend_from_slice(KERNEL_HOSTNAME.as_bytes());
    out.push(b'\n');
    out
}

// ─── /proc/cpuinfo ──────────────────────────────────────────────────────────

/// В каком CPUID-слове живёт флаг возможности.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagWord {
    /// `CPUID.01h:EDX`.
    Leaf1Edx,
    /// `CPUID.01h:ECX`.
    Leaf1Ecx,
    /// `CPUID.07h:0:EBX`.
    Leaf7Ebx,
    /// `CPUID.07h:0:ECX`.
    Leaf7Ecx,
    /// `CPUID.80000001h:EDX`.
    Leaf81Edx,
    /// `CPUID.80000001h:ECX`.
    Leaf81Ecx,
}

/// `(имя, слово, бит)` для каждого флага, который ядро готово рекламировать.
///
/// Имена — словарь Linux `flags`; имя выдаётся **тогда и только тогда**, когда
/// выставлен его CPUID-бит, поэтому файл не может заявить отсутствующую у CPU
/// возможность (свойство P52). `rdrand`/`rdseed` включены намеренно: это видимая
/// юзерленду основа источника энтропии.
pub const CPU_FLAG_TABLE: &[(&str, FlagWord, u8)] = &[
    ("fpu", FlagWord::Leaf1Edx, 0),
    ("vme", FlagWord::Leaf1Edx, 1),
    ("de", FlagWord::Leaf1Edx, 2),
    ("pse", FlagWord::Leaf1Edx, 3),
    ("tsc", FlagWord::Leaf1Edx, 4),
    ("msr", FlagWord::Leaf1Edx, 5),
    ("pae", FlagWord::Leaf1Edx, 6),
    ("mce", FlagWord::Leaf1Edx, 7),
    ("cx8", FlagWord::Leaf1Edx, 8),
    ("apic", FlagWord::Leaf1Edx, 9),
    ("sep", FlagWord::Leaf1Edx, 11),
    ("mtrr", FlagWord::Leaf1Edx, 12),
    ("pge", FlagWord::Leaf1Edx, 13),
    ("mca", FlagWord::Leaf1Edx, 14),
    ("cmov", FlagWord::Leaf1Edx, 15),
    ("pat", FlagWord::Leaf1Edx, 16),
    ("pse36", FlagWord::Leaf1Edx, 17),
    ("psn", FlagWord::Leaf1Edx, 18),
    ("clflush", FlagWord::Leaf1Edx, 19),
    ("mmx", FlagWord::Leaf1Edx, 23),
    ("fxsr", FlagWord::Leaf1Edx, 24),
    ("sse", FlagWord::Leaf1Edx, 25),
    ("sse2", FlagWord::Leaf1Edx, 26),
    ("ht", FlagWord::Leaf1Edx, 28),
    ("pbe", FlagWord::Leaf1Edx, 31),
    ("pni", FlagWord::Leaf1Ecx, 0),
    ("pclmulqdq", FlagWord::Leaf1Ecx, 1),
    ("monitor", FlagWord::Leaf1Ecx, 3),
    ("vmx", FlagWord::Leaf1Ecx, 5),
    ("ssse3", FlagWord::Leaf1Ecx, 9),
    ("fma", FlagWord::Leaf1Ecx, 12),
    ("cx16", FlagWord::Leaf1Ecx, 13),
    ("pcid", FlagWord::Leaf1Ecx, 17),
    ("sse4_1", FlagWord::Leaf1Ecx, 19),
    ("sse4_2", FlagWord::Leaf1Ecx, 20),
    ("x2apic", FlagWord::Leaf1Ecx, 21),
    ("movbe", FlagWord::Leaf1Ecx, 22),
    ("popcnt", FlagWord::Leaf1Ecx, 23),
    ("tsc_deadline_timer", FlagWord::Leaf1Ecx, 24),
    ("aes", FlagWord::Leaf1Ecx, 25),
    ("xsave", FlagWord::Leaf1Ecx, 26),
    ("avx", FlagWord::Leaf1Ecx, 28),
    ("f16c", FlagWord::Leaf1Ecx, 29),
    ("rdrand", FlagWord::Leaf1Ecx, 30),
    ("hypervisor", FlagWord::Leaf1Ecx, 31),
    ("fsgsbase", FlagWord::Leaf7Ebx, 0),
    ("bmi1", FlagWord::Leaf7Ebx, 3),
    ("avx2", FlagWord::Leaf7Ebx, 5),
    ("smep", FlagWord::Leaf7Ebx, 7),
    ("bmi2", FlagWord::Leaf7Ebx, 8),
    ("erms", FlagWord::Leaf7Ebx, 9),
    ("invpcid", FlagWord::Leaf7Ebx, 10),
    ("rdseed", FlagWord::Leaf7Ebx, 18),
    ("adx", FlagWord::Leaf7Ebx, 19),
    ("smap", FlagWord::Leaf7Ebx, 20),
    ("clflushopt", FlagWord::Leaf7Ebx, 23),
    ("clwb", FlagWord::Leaf7Ebx, 24),
    ("sha_ni", FlagWord::Leaf7Ebx, 29),
    ("umip", FlagWord::Leaf7Ecx, 2),
    ("pku", FlagWord::Leaf7Ecx, 3),
    ("ospke", FlagWord::Leaf7Ecx, 4),
];

/// Флаги для набора расшифрованных CPUID-слов в порядке таблицы.
pub fn cpu_flags(c: &CpuInfoData) -> Vec<&'static str> {
    let mut out = Vec::new();
    for (name, word, bit) in CPU_FLAG_TABLE.iter().copied() {
        let w = match word {
            FlagWord::Leaf1Edx => c.leaf1_edx,
            FlagWord::Leaf1Ecx => c.leaf1_ecx,
            FlagWord::Leaf7Ebx => c.leaf7_ebx,
            FlagWord::Leaf7Ecx => c.leaf7_ecx,
            FlagWord::Leaf81Edx => c.leaf81_edx,
            FlagWord::Leaf81Ecx => c.leaf81_ecx,
        };
        if w & (1u32 << bit) != 0 {
            out.push(name);
        }
    }
    out
}

/// Расшифрованные CPUID-данные одного CPU (вход `/proc/cpuinfo`).
///
/// `Option`-поля есть только у части CPU: лист частоты (`CPUID.16h`) и дескриптор
/// кэша (`CPUID.80000006h`). Отсутствие означает, что строка опускается, а не
/// заполняется выдуманным числом (`docs/procfs.md` §4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuInfoData {
    /// `CPUID.00h` EBX/EDX/ECX, 12 ASCII-байт.
    pub vendor: [u8; 12],
    /// `CPUID.80000002h..80000004h`, 48 ASCII-байт (могут быть все NUL).
    pub brand: [u8; 48],
    pub family: u32,
    pub model: u32,
    pub stepping: u32,
    /// `CPUID.00h` EAX — старший базовый лист.
    pub max_basic: u32,
    /// `CPUID.80000006h` ECX[31:16], КиБ.
    pub cache_kb: Option<u32>,
    /// `CPUID.16h` EAX, МГц.
    pub mhz: Option<u32>,
    pub phys_bits: u8,
    pub virt_bits: u8,
    pub leaf1_edx: u32,
    pub leaf1_ecx: u32,
    pub leaf7_ebx: u32,
    pub leaf7_ecx: u32,
    pub leaf81_edx: u32,
    pub leaf81_ecx: u32,
}

impl Default for CpuInfoData {
    fn default() -> Self {
        CpuInfoData {
            vendor: [0; 12],
            brand: [0; 48],
            family: 0,
            model: 0,
            stepping: 0,
            max_basic: 0,
            cache_kb: None,
            mhz: None,
            phys_bits: 0,
            virt_bits: 0,
            leaf1_edx: 0,
            leaf1_ecx: 0,
            leaf7_ebx: 0,
            leaf7_ecx: 0,
            leaf81_edx: 0,
            leaf81_ecx: 0,
        }
    }
}

/// Литерал, выдаваемый при пустой или полностью NUL-строке brand: не выдумывать
/// имя модели, но сохранить ключ (`docs/procfs.md` §4.2).
pub const UNKNOWN_MODEL: &str = "Unknown x86_64 CPU";

/// Обрезает ASCII-поле с NUL-добивкой, схлопывая внутренние пробельные серии в
/// один пробел.
fn clean_ascii(raw: &[u8]) -> alloc::string::String {
    let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
    let mut out = String::new();
    let mut pending_space = false;
    for &b in &raw[..end] {
        let c = if (0x20..0x7f).contains(&b) {
            b as char
        } else {
            ' '
        };
        if c == ' ' {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    out
}

/// Рендерит `/proc/cpuinfo`: по блоку на запись `cpus`, каждый завершается пустой
/// строкой, с ровно теми разделителями `\t: `, которых ждёт парсер libuv.
pub fn format_cpuinfo(cpus: &[CpuInfoData]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);
    for (i, c) in cpus.iter().enumerate() {
        let vendor = clean_ascii(&c.vendor);
        let brand_raw = clean_ascii(&c.brand);
        let model_name = if brand_raw.is_empty() {
            String::from(UNKNOWN_MODEL)
        } else {
            brand_raw
        };

        push(&mut out, format_args!("processor\t: {}\n", i));
        if !vendor.is_empty() {
            push(&mut out, format_args!("vendor_id\t: {}\n", vendor));
        }
        push(&mut out, format_args!("cpu family\t: {}\n", c.family));
        push(&mut out, format_args!("model\t\t: {}\n", c.model));
        push(&mut out, format_args!("model name\t: {}\n", model_name));
        push(&mut out, format_args!("stepping\t: {}\n", c.stepping));
        if let Some(mhz) = c.mhz {
            push(&mut out, format_args!("cpu MHz\t\t: {}.000\n", mhz));
        }
        if let Some(kb) = c.cache_kb {
            push(&mut out, format_args!("cache size\t: {} KB\n", kb));
        }
        out.extend_from_slice(b"physical id\t: 0\n");
        out.extend_from_slice(b"siblings\t: 1\n");
        out.extend_from_slice(b"core id\t\t: 0\n");
        out.extend_from_slice(b"cpu cores\t: 1\n");
        out.extend_from_slice(b"apicid\t\t: 0\n");
        out.extend_from_slice(b"initial apicid\t: 0\n");
        out.extend_from_slice(b"fpu\t\t: yes\n");
        out.extend_from_slice(b"fpu_exception\t: yes\n");
        push(&mut out, format_args!("cpuid level\t: {}\n", c.max_basic));
        out.extend_from_slice(b"wp\t\t: yes\n");
        out.extend_from_slice(b"flags\t\t: ");
        let flags = cpu_flags(c);
        for (n, f) in flags.iter().enumerate() {
            if n > 0 {
                out.push(b' ');
            }
            out.extend_from_slice(f.as_bytes());
        }
        out.push(b'\n');
        out.extend_from_slice(b"bugs\t\t:\n");
        if let Some(mhz) = c.mhz {
            push(
                &mut out,
                format_args!("bogomips\t: {}.00\n", mhz.saturating_mul(2)),
            );
        }
        out.extend_from_slice(b"clflush size\t: 64\n");
        out.extend_from_slice(b"cache_alignment\t: 64\n");
        if c.phys_bits > 0 && c.virt_bits > 0 {
            push(
                &mut out,
                format_args!(
                    "address sizes\t: {} bits physical, {} bits virtual\n",
                    c.phys_bits, c.virt_bits
                ),
            );
        }
        out.extend_from_slice(b"power management:\n\n");
    }
    out
}

// ─── /proc/self/cmdline ─────────────────────────────────────────────────────

/// Верхняя граница **payload** `/proc/self/cmdline` (суммарные байты аргументов),
/// совпадающая с гейтом аргументов начального стека
/// (`src/task/stack.rs::arg_gate`, 4096 суммарных байт argv). NUL-терминаторы в
/// неё не входят, поэтому максимальный argv, который пропускает гейт, всё ещё
/// представим целиком; рендер занимает не больше `cap + argc` байт.
pub const CMDLINE_CAP: usize = 4096;

/// Рендерит `/proc/self/cmdline`: каждый аргумент с NUL после него, больше
/// ничего.
///
/// Аргумент никогда не режется пополам: как только `cap` был бы превышен, этот
/// аргумент и все следующие отбрасываются (`docs/procfs.md` §4.4). Пустой
/// `argv` поэтому рендерит пустой файл, как в Linux.
pub fn format_cmdline(argv: &[&[u8]], cap: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut payload = 0usize;
    for arg in argv {
        if payload + arg.len() > cap {
            break;
        }
        payload += arg.len();
        out.extend_from_slice(arg);
        out.push(0);
    }
    out
}

// ─── /proc/self/status ──────────────────────────────────────────────────────

/// `TASK_COMM_LEN - 1` — Linux-предел для поля `Name:`.
pub const TASK_NAME_MAX: usize = 15;

/// Linux-имя задачи: basename загруженного образа (не `argv[0]`).
///
/// Откат к basename `argv0`, когда путь образа неизвестен, затем к `pagh`.
/// Усечение побайтное (как в Linux), поэтому не-UTF-8 аргумент проходит без
/// изменений.
pub fn task_name(exe_path: &str, argv0: &[u8]) -> Vec<u8> {
    fn basename(bytes: &[u8]) -> &[u8] {
        match bytes.iter().rposition(|b| *b == b'/') {
            Some(i) => &bytes[i + 1..],
            None => bytes,
        }
    }
    let mut base: &[u8] = &[];
    if !exe_path.is_empty() {
        base = basename(exe_path.as_bytes());
    }
    if base.is_empty() {
        base = basename(argv0);
    }
    if base.is_empty() {
        return b"pagh".to_vec();
    }
    base[..core::cmp::min(base.len(), TASK_NAME_MAX)].to_vec()
}

/// Входы `/proc/self/status`, всё простые скаляры.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmKb {
    pub size: u64,
    pub rss: u64,
    pub data: u64,
    pub stk: u64,
    pub exe: u64,
    pub lib: u64,
}

/// Входы `/proc/self/status`.
#[derive(Clone, Copy, Debug)]
pub struct StatusInputs<'a> {
    /// Имя задачи (≤ 15 байт, намеренно сырые байты).
    pub name: &'a [u8],
    /// Буква состояния Linux: `R` running, `T` stopped (две, которые это ядро
    /// может честно различить).
    pub state: u8,
    pub umask: u32,
    pub pid: u64,
    pub tgid: u64,
    pub ppid: u64,
    pub threads: u64,
    pub fd_size: u64,
    pub sig_pending: u64,
    pub sig_blocked: u64,
    /// Сигналы с диспозицией `SIG_IGN`.
    pub sig_ign: u64,
    /// Сигналы с установленным пользовательским обработчиком.
    pub sig_cgt: u64,
    /// Пользовательские id `[real, effective, saved, filesystem]`. В pagh нет
    /// отдельного fsuid, поэтому четвёртая запись — effective id.
    pub uids: [u32; 4],
    /// Групповые id `[real, effective, saved, filesystem]`.
    pub gids: [u32; 4],
    pub vm: VmKb,
}

/// Длинное описание Linux-буквы состояния для строки `State:`.
///
/// Это ядро выдаёт только `R` и `T` (учёта спящих/блокированных задач нет,
/// зомби удаляются из реестра compat при выходе). Остальные Linux-буквы тоже
/// отображены, чтобы будущий источник не выдал молча «unknown».
pub fn state_word(state: u8) -> &'static str {
    match state {
        b'R' => "running",
        b'S' => "sleeping",
        b'D' => "disk sleep",
        b'T' => "stopped",
        b't' => "tracing stop",
        b'Z' => "zombie",
        b'X' | b'x' => "dead",
        b'I' => "idle",
        _ => "unknown",
    }
}

/// Рендерит `/proc/self/status`.
///
/// Порядок полей и разделители — как в Linux `fs/proc/array.c`. Capabilities
/// нулевые: в этом ядре нет модели capability, и заявлять root-набор для
/// необеспечиваемой модели было бы ложью (`docs/procfs.md` §4.5).
pub fn format_status(s: &StatusInputs<'_>) -> Vec<u8> {
    let vm = s.vm;
    let mut out = Vec::with_capacity(1024);
    out.extend_from_slice(b"Name:\t");
    out.extend_from_slice(s.name);
    out.push(b'\n');
    push(&mut out, format_args!("Umask:\t{:04o}\n", s.umask));
    push(
        &mut out,
        format_args!("State:\t{} ({})\n", s.state as char, state_word(s.state)),
    );
    push(&mut out, format_args!("Tgid:\t{}\n", s.tgid));
    push(&mut out, format_args!("Pid:\t{}\n", s.pid));
    push(&mut out, format_args!("PPid:\t{}\n", s.ppid));
    out.extend_from_slice(b"TracerPid:\t0\n");
    push(
        &mut out,
        format_args!(
            "Uid:\t{}\t{}\t{}\t{}\n",
            s.uids[0], s.uids[1], s.uids[2], s.uids[3]
        ),
    );
    push(
        &mut out,
        format_args!(
            "Gid:\t{}\t{}\t{}\t{}\n",
            s.gids[0], s.gids[1], s.gids[2], s.gids[3]
        ),
    );
    push(&mut out, format_args!("FDSize:\t{}\n", s.fd_size));
    push(&mut out, format_args!("Threads:\t{}\n", s.threads));
    push(&mut out, format_args!("SigPnd:\t{:016x}\n", s.sig_pending));
    out.extend_from_slice(b"ShdPnd:\t0000000000000000\n");
    push(&mut out, format_args!("SigBlk:\t{:016x}\n", s.sig_blocked));
    push(&mut out, format_args!("SigIgn:\t{:016x}\n", s.sig_ign));
    push(&mut out, format_args!("SigCgt:\t{:016x}\n", s.sig_cgt));
    for key in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
        push(&mut out, format_args!("{}\t0000000000000000\n", key));
    }
    out.extend_from_slice(b"NoNewPrivs:\t0\n");
    out.extend_from_slice(b"Seccomp:\t0\n");
    out.extend_from_slice(b"Cpus_allowed:\t1\n");
    out.extend_from_slice(b"Cpus_allowed_list:\t0\n");
    out.extend_from_slice(b"Mems_allowed:\t1\n");
    out.extend_from_slice(b"Mems_allowed_list:\t0\n");
    push(&mut out, format_args!("VmPeak:\t{:>8} kB\n", vm.size));
    push(&mut out, format_args!("VmSize:\t{:>8} kB\n", vm.size));
    out.extend_from_slice(b"VmLck:\t       0 kB\n");
    out.extend_from_slice(b"VmPin:\t       0 kB\n");
    push(&mut out, format_args!("VmHWM:\t{:>8} kB\n", vm.rss));
    push(&mut out, format_args!("VmRSS:\t{:>8} kB\n", vm.rss));
    push(&mut out, format_args!("VmData:\t{:>8} kB\n", vm.data));
    push(&mut out, format_args!("VmStk:\t{:>8} kB\n", vm.stk));
    push(&mut out, format_args!("VmExe:\t{:>8} kB\n", vm.exe));
    push(&mut out, format_args!("VmLib:\t{:>8} kB\n", vm.lib));
    out.extend_from_slice(b"VmPTE:\t       0 kB\n");
    out.extend_from_slice(b"VmSwap:\t       0 kB\n");
    out.extend_from_slice(b"voluntary_ctxt_switches:\t0\n");
    out.extend_from_slice(b"nonvoluntary_ctxt_switches:\t0\n");
    out
}

// ─── /proc/<pid>/stat и /proc/self/statm ────────────────────────────────────

/// Входы `/proc/<pid>/stat`.
///
/// В строке 52 поля Linux; те, на которые это ядро может ответить правдиво,
/// берутся из снимка `CompatState` цели, остальные — задокументированные нули
/// «не учитывается» (см. рендерер). Полная Linux-ширина нужна, чтобы
/// `ps`/`procps` индексировали нужные поля (18 priority, 20 threads, 22
/// starttime, 23 vsize, 24 rss, 39 processor, …) и не выпадали за конец.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PidStatInputs<'a> {
    /// Имя задачи (сырые байты; обрезается и санитизируется [`stat_comm`]).
    pub name: &'a [u8],
    pub pid: u64,
    /// Буква состояния Linux (здесь `R` или `T`).
    pub state: u8,
    pub ppid: u64,
    /// Id группы процессов. В pagh одна группа на группу тредов, поэтому это
    /// `tgid`.
    pub pgrp: u64,
    /// Id сессии; сессий в pagh нет, поэтому это тоже `tgid`.
    pub session: u64,
    pub threads: u64,
    pub sig_blocked: u64,
    pub sig_ign: u64,
    pub sig_cgt: u64,
    pub vm: VmKb,
    /// Текущий program break (поле 47, `start_brk`).
    pub start_brk: u64,
    /// Младший адрес замапленного пользовательского стека (поле 28,
    /// `startstack`).
    pub startstack: u64,
    /// Поле 52, `exit_code`: 0 до выхода процесса (зомби в этом ядре уже удалён
    /// из реестра compat, поэтому на практике всегда 0).
    pub exit_code: i64,
}

/// Санитизирует имя задачи для поля `comm` в скобках `/proc/<pid>/stat`.
///
/// Linux выдаёт сырые 15 байт `comm`; этот рендерер дополнительно сводит NUL,
/// CR/LF и `)` к видимому ASCII (`)` становится `]`), чтобы имя файла,
/// случайно созданное пользователем, не разорвало строку и не сбило
/// традиционный разбор «найти ПОСЛЕДНЮЮ `)`» в `ps`/`procps`. Усечение побайтное
/// по Linux-пределу `TASK_COMM_LEN - 1`.
pub fn stat_comm(name: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = name
        .iter()
        .take(TASK_NAME_MAX)
        .map(|b| match *b {
            0 | b'\n' | b'\r' => b' ',
            b')' => b']',
            other => other,
        })
        .collect();
    if out.is_empty() {
        out.extend_from_slice(b"pagh");
    }
    out
}

/// Рендерит `/proc/<pid>/stat` (и `/proc/self/stat`): Linux-строка из 52 полей.
///
/// Поля, которые ядро не учитывает, выдаются нулями, а не выдумываются
/// (`utime`/`stime`/`cutime`/`cstime`, счётчики page fault, `starttime`, битмап
/// сигналов, адреса code/data-сегментов и границы `arg/env`). `vsize` — КиБ →
/// байты, `rss` — КиБ → страницы 4 КиБ, оба из того же снимка [`VmKb`], что и
/// `/proc/<pid>/status`, поэтому файлы согласованы. `rsslim` — Linux
/// `RLIM_INFINITY` (все единицы), `exit_signal` — `SIGCHLD`.
pub fn format_pid_stat(s: &PidStatInputs<'_>) -> Vec<u8> {
    let comm = stat_comm(s.name);
    let mut out = Vec::with_capacity(384);

    // Поля 1–9: pid, comm, state, ppid, pgrp, session, tty_nr, tpgid, flags.
    push(&mut out, format_args!("{} (", s.pid));
    out.extend_from_slice(&comm);
    push(
        &mut out,
        format_args!(
            ") {} {} {} {} 0 -1 0",
            s.state as char, s.ppid, s.pgrp, s.session
        ),
    );

    // Поля 10–17: minflt, cminflt, majflt, cmajflt, utime, stime, cutime,
    // cstime — per-process учёта CPU/fault не существует.
    out.extend_from_slice(b" 0 0 0 0 0 0 0 0");

    // Поля 18–52: priority, nice, threads, itrealvalue, starttime, vsize,
    // rss, rsslim, startcode, endcode, startstack, kstkesp, kstkeip, signal,
    // blocked, sigignore, sigcatch, wchan, nswap, cnswap, exit_signal,
    // processor, rt_priority, policy, delayacct_blkio_ticks, guest_time,
    // cguest_time, start_data, end_data, start_brk, arg_start, arg_end,
    // env_start, env_end, exit_code.
    push(
        &mut out,
        format_args!(
            " 20 0 {} 0 0 {} {} {} 0 0 {} 0 0 0 {} {} {} 0 0 0 17 0 0 0 0 0 0 0 0 {} 0 0 0 0 {}\n",
            s.threads,
            s.vm.size.saturating_mul(1024),
            s.vm.rss / 4,
            u64::MAX,
            s.startstack,
            s.sig_blocked,
            s.sig_ign,
            s.sig_cgt,
            s.start_brk,
            s.exit_code,
        ),
    );
    out
}

/// Рендерит `/proc/self/statm`: `size resident shared text lib data dt` в
/// страницах.
///
/// `shared` и `dt` — настоящие нули (учёта разделяемой памяти нет). `lib` на
/// практике 0, потому что `VmKb::lib` равен 0 (сегменты интерпретатора сложены в
/// `exe`). `size`/`resident`/`text`/`data` — те же КиБ, что в
/// `/proc/<pid>/status`, делённые на размер страницы 4 КиБ.
pub fn format_statm(vm: &VmKb) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    push(
        &mut out,
        format_args!(
            "{} {} 0 {} {} {} 0\n",
            vm.size / 4,
            vm.rss / 4,
            vm.exe / 4,
            vm.lib / 4,
            vm.data / 4
        ),
    );
    out
}

// ─── /proc/self/maps ────────────────────────────────────────────────────────

/// Бит защиты `mmap`/`mprotect`: чтение (то же значение, что в `linux::mem`).
pub const PROT_READ: u32 = 1;
/// Бит защиты `mmap`/`mprotect`: запись.
pub const PROT_WRITE: u32 = 2;
/// Бит защиты `mmap`/`mprotect`: исполнение.
pub const PROT_EXEC: u32 = 4;

/// Данные региона на одну строку `/proc/self/maps`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapsRegion<'a> {
    pub start: u64,
    pub end: u64,
    /// Биты Linux `PROT_*`.
    pub prot: u32,
    pub file_offset: u64,
    pub dev_major: u8,
    pub dev_minor: u8,
    pub ino: u64,
    /// Колонка pathname: абсолютный путь или метка в скобках вроде `[stack]`.
    pub path: Option<&'a str>,
}

/// Рендерит `/proc/self/maps`.
///
/// Строки сортируются по начальному адресу, перекрывающиеся входы объединяются,
/// поэтому вывод всегда разбираем инструментами, бинарно ищущими по списку
/// диапазонов (санитайзеры, `addr2line`). Регион без pathname получает **без**
/// хвостового пробела — как анонимные маппинги Linux.
pub fn format_maps(regions: &[MapsRegion<'_>]) -> Vec<u8> {
    let mut sorted: Vec<MapsRegion<'_>> = regions.to_vec();
    sorted.sort_by_key(|r| (r.start, r.end));

    // Объединяем перекрывающиеся диапазоны: сохраняем первое (с младшим стартом)
    // имя и берём объединение защит. В настоящем адресном пространстве адреса не
    // перекрываются, так что это страховка от испорченного входа.
    let mut merged: Vec<MapsRegion<'_>> = Vec::with_capacity(sorted.len());
    for r in sorted {
        if let Some(last) = merged.last_mut() {
            if r.start < last.end {
                last.end = core::cmp::max(last.end, r.end);
                last.prot |= r.prot;
                continue;
            }
        }
        merged.push(r);
    }

    let mut out = Vec::with_capacity(256 + 64 * merged.len());
    for r in merged {
        let r_bit = if r.prot & PROT_READ != 0 { 'r' } else { '-' };
        let w_bit = if r.prot & PROT_WRITE != 0 { 'w' } else { '-' };
        let x_bit = if r.prot & PROT_EXEC != 0 { 'x' } else { '-' };
        push(
            &mut out,
            format_args!(
                "{:08x}-{:08x} {}{}{}p {:08x} {:02x}:{:02x} {}",
                r.start, r.end, r_bit, w_bit, x_bit, r.file_offset, r.dev_major, r.dev_minor, r.ino
            ),
        );
        if let Some(path) = r.path {
            push(&mut out, format_args!(" {}", path));
        }
        out.push(b'\n');
    }
    out
}

/// Формирует `dev_t`-байты `major:minor` для строки maps (hex, по 2 цифры).
pub fn dev_bytes(dev: u64) -> (u8, u8) {
    let major = ((dev >> 8) & 0xfff) as u8;
    let minor = ((dev & 0xff) | ((dev >> 12) & 0xfff00)) as u8;
    (major, minor)
}

// ─── /proc/net/dev ──────────────────────────────────────────────────────────

/// Первая шапка `/proc/net/dev` — дословно как `net/core/net-procfs.c`.
const NET_DEV_HEADER_1: &[u8] =
    b"Inter-|   Receive                                                |  Transmit\n";
/// Вторая шапка: имена 16 колонок (`rx` 8 + `tx` 8), дословно как в Linux.
const NET_DEV_HEADER_2: &[u8] =
    b" face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n";

/// Одна строка `/proc/net/dev`.
///
/// Реальными бывают только байты/пакеты и — у RNDIS — ошибки/потери; остальные
/// колонки Linux (`fifo`/`frame`/`compressed`/`multicast`, `colls`/`carrier`)
/// этот стек не ведёт и печатает нулями, как Linux для драйвера без статистики.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetDevRow {
    pub name: &'static str,
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errs: u64,
    pub rx_drop: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errs: u64,
    pub tx_drop: u64,
}

/// Рендерит `/proc/net/dev`: две шапки и по строке на интерфейс.
///
/// Ширины колонок копируют Linux (`%6s: %7llu ...`): `ifconfig`/`/proc/net/dev`
/// парсеры (net-tools, busybox, libuv) полагаются на позиционное чтение
/// whitespace-разделённых полей, а не на фиксированные столбцы, но совпадать
/// с эталоном дешевле, чем объяснять расхождение. Нет интерфейсов — остаются
/// только шапки (так Linux печатает пустую таблицу).
pub fn format_net_dev(rows: &[NetDevRow]) -> Vec<u8> {
    let mut out =
        Vec::with_capacity(NET_DEV_HEADER_1.len() + NET_DEV_HEADER_2.len() + rows.len() * 160);
    out.extend_from_slice(NET_DEV_HEADER_1);
    out.extend_from_slice(NET_DEV_HEADER_2);
    for r in rows {
        // 16 чисел: rx bytes/packets/errs/drop/fifo/frame/compressed/multicast,
        // затем tx bytes/packets/errs/drop/fifo/colls/carrier/compressed.
        push(
            &mut out,
            format_args!(
                "{:>6}: {:>7} {:>7} {:>4} {:>4} {:>4} {:>5} {:>10} {:>9} {:>8} {:>7} {:>4} {:>4} {:>4} {:>5} {:>7} {:>10}\n",
                r.name,
                r.rx_bytes,
                r.rx_packets,
                r.rx_errs,
                r.rx_drop,
                0u64,
                0u64,
                0u64,
                0u64,
                r.tx_bytes,
                r.tx_packets,
                r.tx_errs,
                r.tx_drop,
                0u64,
                0u64,
                0u64,
                0u64,
            ),
        );
    }
    out
}

// ─── /proc/partitions ───────────────────────────────────────────────────────

/// Одна строка `/proc/partitions`. `major` синтетический (240 — USB), `minor`
/// `16*N + (0 для целого, M для pM)`, `blocks` — в 1024-байтовых единицах.
pub struct PartitionRow<'a> {
    pub major: u32,
    pub minor: u32,
    pub blocks: u64,
    pub name: &'a str,
}

/// Отрендерить таблицу `/proc/partitions`. Формат Linux-подобный, но major/minor
/// синтетические — это диагностика, а не Linux-совместимые номера.
pub fn format_partitions(rows: &[PartitionRow<'_>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + rows.len() * 40);
    out.extend_from_slice(b"major minor  #blocks  name\n");
    for row in rows {
        out.extend_from_slice(
            format!(
                "{:>5} {:>5} {:>9}  {}\n",
                row.major, row.minor, row.blocks, row.name
            )
            .as_bytes(),
        );
    }
    out
}
