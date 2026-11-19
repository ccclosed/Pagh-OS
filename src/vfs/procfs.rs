//! Синтетический `/proc` (контракт: `docs/procfs.md`).
//!
//! `/proc` — поддерево VFS, у файлов которого нет хранилища: их текст
//! рендерится из состояния ядра при чтении. Существенные решения:
//!
//!   * **Ленивое содержимое на экземпляр.** При монтировании ничего не
//!     рендерится. Узел создаётся `lookup()`/`open()` и рендерит текст на первом
//!     `size()`/`read()`. Чтение со смещения 0 перерисовывает, поэтому
//!     программа, опрашивающая `/proc/uptime` или `/proc/meminfo`, видит
//!     актуальные значения, а многоблочный читатель одного файла держит один
//!     рендер и не склеивает два разных (`docs/procfs.md` §3.2).
//!   * **`size()` — длина рендера**, а не ноль, как в Linux: путь чтения этого
//!     ядра выводит EOF из `size()` (`plan_read`), поэтому procfs-файл нулевого
//!     размера читался бы как мгновенно пустой (DEVIATION-1).
//!   * **Порядок локов — сначала состояние, потом узел.** Рендеринг
//!     (`COMPAT_STATES`, PMM, планировщик) идёт до взятия спинлока узла; спинлок
//!     узла никогда не держится через вызов `with_pid_compat`.
//!   * **`/proc/self` невидим задаче без `CompatState`** (ядро-поток, boot
//!     selftest): `lookup` возвращает `NotFound` → `ENOENT`, а не выдумывает
//!     процесс (`docs/procfs.md` §5.5).
//!   * **Неизвестные пути — `NotFound`**, wildcard'а нет. Числовой компонент
//!     резолвится *только* когда pid зарегистрирован в `COMPAT_STATES`, поэтому
//!     `/proc/<dead-pid>` — `ENOENT`; `/proc/self/fd`, `/proc/loadavg`,
//!     `/proc/version` и т. п. ведут себя так же.
//!
//! Реализовано: `/proc/{cpuinfo,meminfo,uptime,stat,version,filesystems,loadavg,
//! mounts,cgroup,mountinfo,limits}`; `/proc/sys/kernel/{ostype,osrelease,
//! hostname}`; per-pid `/proc/<pid>/{status,stat,statm,cmdline,maps,limits,
//! cgroup,mountinfo,fd/*,fdinfo/*,task/*}`; `/proc/self/*`; per-thread view
//! `/proc/<pid>/task/` — по каталогу на каждый живой тред группы.
//!
//! Per-pid-файлы снимают состояние ЦЕЛИ (не читателя) под `COMPAT_STATES`,
//! отпускают лок реестра и только потом форматируют текст. В этом смысл структур
//! `*Snap` ниже: `COMPAT_STATES` — нереентерабельный спинлок, его нельзя держать
//! через рендер (AGENTS.md, инвариант 4), а cpu-bound цель может параллельно
//! менять свой `VmRegionSet`.

use alloc::borrow::Cow;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::x86_64::apic::TICK_HZ;
use crate::arch::x86_64::cpuid;
use crate::arch::x86_64::linux::misc;
use crate::arch::x86_64::linux::signal_frame::{SIG_DFL, SIG_IGN};
use crate::memory::layout::{PAGE_SIZE, USER_STACK_PAGES, USER_STACK_TOP};
use crate::memory::pmm;
use crate::sync::spinlock::Spinlock;
use crate::task::compat::{self, CompatState};
use crate::task::fd::OpenObject;
use crate::task::{scheduler, stat};

use super::elf::LoadSegment;
use super::procfs_format::{self as fmt, ProcKind};
use super::{VfsError, VfsNode, VfsResult};

// `/proc/stat` отдаёт единицы USER_HZ; счётчики — сырые тики, поэтому обе
// частоты обязаны быть одной константой, иначе каждое число тихо разойдётся.
const _: () = assert!(TICK_HZ == stat::USER_HZ);

/// Настоящая версия крейта (единственный источник — `Cargo.toml`), то же
/// значение `provision.rs` пишет в `/mnt/etc/pagh-release`.
const KERNEL_VERSION: &str = env!("CARGO_PKG_VERSION");

// `/proc/.../limits` рендерит строку на каждый Linux resource id; таблица
// rlimit и syscall-слой обязаны совпадать по ширине, иначе `limits` тихо
// потеряет или выдумает строку.
const _: () = assert!(fmt::RLIMIT_COUNT == misc::RLIMIT_NLIMITS as usize);

/// Корень поддерева `/proc`, готовый для `vfs::mount_at("/proc", …)`.
pub fn root() -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: String::from("proc"),
        ino: fmt::PROC_ROOT_DIR_INO,
        kind: DirKind::Root,
    })
}

// ─── каталоги ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum DirKind {
    /// `/proc`.
    Root,
    /// `/proc/self`.
    SelfDir,
    /// `/proc/sys`.
    SysDir,
    /// `/proc/sys/kernel`.
    SysKernelDir,
    /// `/proc/net` — каталог сетевой статистики (`docs/procfs.md` §15).
    NetDir,
    /// `/proc/<pid>` с pid цели.
    Pid(u64),
    /// `/proc/self/fd` или `/proc/<pid>/fd` с pid цели.
    FdDir(u64),
    /// `/proc/self/fdinfo` или `/proc/<pid>/fdinfo` с pid цели.
    FdInfoDir(u64),
    /// `/proc/self/task` или `/proc/<pid>/task` с id **группы тредов** (процесса,
    /// чьи треды листаются), а не компонента пути, через который пришли: у
    /// не-лидера `/proc/<tid>/task` — та же группа, что у лидера.
    TaskDir(u64),
}

struct ProcDir {
    /// `"proc"`, `"self"` или десятичный pid. Owned, потому что имя pid-каталога
    /// динамическое (`VfsNode::name` возвращает `&str`).
    name: String,
    ino: u64,
    kind: DirKind,
}

/// Свежий узел `/proc/self` (дети пересоздаются на каждый lookup, чтобы каждый
/// рендерился для спрашивающего процесса).
fn self_dir() -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: String::from("self"),
        ino: fmt::PROC_SELF_DIR_INO,
        kind: DirKind::SelfDir,
    })
}

/// `/proc/sys` — read-only пространство sysctl.
fn sys_dir() -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: String::from("sys"),
        ino: fmt::PROC_SYS_DIR_INO,
        kind: DirKind::SysDir,
    })
}

/// `/proc/sys/kernel`.
fn sys_kernel_dir() -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: String::from("kernel"),
        ino: fmt::PROC_SYS_KERNEL_DIR_INO,
        kind: DirKind::SysKernelDir,
    })
}

/// `/proc/net` — read-only каталог сетевой статистики (Linux-совместимый путь).
fn net_dir() -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: String::from("net"),
        ino: fmt::PROC_NET_DIR_INO,
        kind: DirKind::NetDir,
    })
}

/// Свежий узел `/proc/<pid>`. Создаётся, только когда pid наблюдён живым;
/// `lookup` перепроверяет это при каждом доступе к ребёнку, поэтому кэшированный
/// каталог не переживёт свой процесс.
fn pid_dir(pid: u64) -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: format!("{}", pid),
        ino: fmt::pid_dir_ino(pid),
        kind: DirKind::Pid(pid),
    })
}

/// Свежий узел `/proc/<pid>/task`.
///
/// `tgid` — группа тредов, чьи задачи листает каталог. Она резолвится один раз из
/// компонента пути, который использовал вызывающий (`pid_dir(pid)` хранит свой
/// pid, а узел спрашивает у реестра `tgid` этого pid), поэтому
/// `/proc/<tid>/task` у не-лидера листает его группу, а не пустой каталог с
/// именем его tid.
fn task_dir(tgid: u64) -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: String::from("task"),
        ino: fmt::pid_task_dir_ino(tgid),
        kind: DirKind::TaskDir(tgid),
    })
}

/// Симлинк `/proc/<pid>/fd/<n>`.
struct ProcFdLink {
    name: String,
    target: String,
    ino: u64,
}

impl VfsNode for ProcFdLink {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn is_symlink(&self) -> bool {
        true
    }
    fn read_link(&self) -> Option<String> {
        Some(self.target.clone())
    }
    fn fs_ino(&self) -> u64 {
        self.ino
    }
    fn size(&self) -> u64 {
        self.target.len() as u64
    }
    fn mode(&self) -> u32 {
        0o777
    }
}

/// Цель в человеко-/Linux-виде для одного открытого дескриптора.
fn fd_target(fd: u32, object: &OpenObject) -> String {
    match object {
        OpenObject::Stdin | OpenObject::Console => String::from("/dev/serial"),
        OpenObject::File { path, .. } | OpenObject::Dir { path, .. } => path.clone(),
        OpenObject::PipeRead(end) | OpenObject::PipeWrite(end) => {
            alloc::format!("pipe:[{}]", Arc::as_ptr(end) as usize)
        }
        OpenObject::Socket { rx, .. } => alloc::format!("socket:[{}]", Arc::as_ptr(rx) as usize),
        OpenObject::InetTcp(_) | OpenObject::InetUdp(_) => alloc::format!("socket:[{fd}]"),
        OpenObject::UnixListener(_) | OpenObject::UnixSocketUnbound { .. } => {
            alloc::format!("socket:[{fd}]")
        }
        OpenObject::Timerfd(_) => String::from("anon_inode:[timerfd]"),
        OpenObject::Signalfd { .. } => String::from("anon_inode:[signalfd]"),
        OpenObject::Eventfd { .. } => String::from("anon_inode:[eventfd]"),
        OpenObject::Epoll { .. } => String::from("anon_inode:[eventpoll]"),
    }
}

fn fd_snapshot(pid: u64) -> Vec<(u32, String)> {
    compat::with_pid_compat(pid, |cs| {
        let capacity = cs.fds.capacity() as u32;
        (0..capacity)
            .filter_map(|fd| cs.fds.get(fd).map(|obj| (fd, fd_target(fd, obj))))
            .collect()
    })
    .unwrap_or_default()
}

fn fd_dir(pid: u64) -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: String::from("fd"),
        ino: fmt::pid_dir_ino(pid) + 0x10,
        kind: DirKind::FdDir(pid),
    })
}

/// `/proc/self/fdinfo` или `/proc/<pid>/fdinfo`: обычный файл на каждый
/// открытый fd.
fn fdinfo_dir(pid: u64) -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: String::from("fdinfo"),
        ino: fmt::pid_dir_ino(pid) + 0x11,
        kind: DirKind::FdInfoDir(pid),
    })
}

/// Открытые дескрипторы цели только номерами (рендерер fdinfo прочитает сам
/// объект позже, вне лока реестра).
fn open_fds(pid: u64) -> Vec<u32> {
    compat::with_pid_compat(pid, |cs| {
        (0..cs.fds.capacity() as u32)
            .filter(|fd| cs.fds.get(*fd).is_some())
            .collect()
    })
    .unwrap_or_default()
}

fn fd_is_open(pid: u64, fd: u32) -> bool {
    compat::with_pid_compat(pid, |cs| cs.fds.get(fd).is_some()).unwrap_or(false)
}

impl VfsNode for ProcDir {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_directory(&self) -> bool {
        true
    }

    fn fs_ino(&self) -> u64 {
        self.ino
    }

    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        let mut out: Vec<Arc<dyn VfsNode>> = Vec::new();
        match self.kind {
            // `readdir` листает пространство имён, а не видимое в этот момент
            // подмножество: `self` показывается даже вызывающему, который не
            // может его резолвить (Linux так же поступает с исчезнувшим
            // pid-каталогом). Числовые pid — живой снимок: один короткий лок
            // реестра.
            DirKind::Root => {
                out.push(self_dir());
                out.push(sys_dir());
                for (pid, _tgid) in compat::compat_pid_tgid_snapshot() {
                    out.push(pid_dir(pid));
                }
                for e in fmt::PROC_FILES.iter().copied() {
                    out.push(file_for(e));
                }
                out.push(net_dir());
            }
            DirKind::SelfDir => {
                for e in fmt::PROC_SELF_FILES.iter().copied() {
                    out.push(file_for(e));
                }
                if compat::current_has_compat() {
                    let pid = scheduler::current_pid();
                    out.push(fd_dir(pid));
                    out.push(fdinfo_dir(pid));
                    out.push(task_dir(compat::current_tgid()));
                }
            }
            DirKind::SysDir => {
                out.push(sys_kernel_dir());
            }
            DirKind::SysKernelDir => {
                for e in fmt::PROC_SYS_FILES.iter().copied() {
                    out.push(file_for(e));
                }
            }
            DirKind::NetDir => {
                for e in fmt::PROC_NET_FILES.iter().copied() {
                    out.push(file_for(e));
                }
            }
            DirKind::Pid(pid) => {
                for e in fmt::PROC_PID_FILES.iter().copied() {
                    out.push(pid_file(pid, e));
                }
                out.push(fd_dir(pid));
                out.push(fdinfo_dir(pid));
                // У каждой живой задачи есть каталог `task` (Linux): у
                // не-лидера он листает всю группу тредов, а не только его самого.
                if let Some(tgid) = compat::with_pid_compat(pid, |cs| cs.tgid) {
                    out.push(task_dir(tgid));
                }
            }
            DirKind::TaskDir(tgid) => {
                // По числовому каталогу на каждую живую задачу группы по
                // возрастанию (реестр — `BTreeMap`). Каждая запись ЕСТЬ
                // pid-каталог задачи, поэтому `/proc/<tgid>/task/<tid>` и
                // `/proc/<tid>` — один VFS-объект.
                for tid in compat::thread_group_pids(tgid) {
                    out.push(pid_dir(tid));
                }
            }
            DirKind::FdDir(pid) => {
                for (fd, target) in fd_snapshot(pid) {
                    out.push(Arc::new(ProcFdLink {
                        name: alloc::format!("{fd}"),
                        target,
                        ino: fmt::pid_fd_ino(pid, fd),
                    }));
                }
            }
            DirKind::FdInfoDir(pid) => {
                for fd in open_fds(pid) {
                    out.push(fdinfo_file(pid, fd));
                }
            }
        }
        Ok(out)
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        match self.kind {
            DirKind::Root => {
                if name == "self" {
                    if compat::current_has_compat() {
                        return Ok(self_dir());
                    }
                    // Своего процесса нет: описывать нечего.
                    return Err(VfsError::NotFound);
                }
                if name == "sys" {
                    return Ok(sys_dir());
                }
                if name == "net" {
                    return Ok(net_dir());
                }
                // Числовые компоненты — per-pid дерево. Именно проверка живости
                // делает это пространством имён, а не wildcard'ом: мёртвый (или
                // никогда не использованный) pid — ENOENT, не пустой файл.
                if let Some(pid) = fmt::parse_pid_component(name) {
                    return if compat::compat_exists(pid) {
                        Ok(pid_dir(pid))
                    } else {
                        Err(VfsError::NotFound)
                    };
                }
                fmt::proc_file(name).map(file_for).ok_or(VfsError::NotFound)
            }
            DirKind::SelfDir => {
                if name == "fd" {
                    return if compat::current_has_compat() {
                        Ok(fd_dir(scheduler::current_pid()))
                    } else {
                        Err(VfsError::NotFound)
                    };
                }
                if name == "fdinfo" {
                    return if compat::current_has_compat() {
                        Ok(fdinfo_dir(scheduler::current_pid()))
                    } else {
                        Err(VfsError::NotFound)
                    };
                }
                if name == "task" {
                    // Собственная группа тредов читателя; у вызывающего без
                    // `CompatState` нет тредов для описания (§5.5).
                    return if compat::current_has_compat() {
                        Ok(task_dir(compat::current_tgid()))
                    } else {
                        Err(VfsError::NotFound)
                    };
                }
                let e = fmt::proc_self_file(name).ok_or(VfsError::NotFound)?;
                if e.kind == ProcKind::SelfExe && current_exe().is_none() {
                    // Указывать не на что: образа нет (`readlink` в этом случае
                    // отвечает ENOENT).
                    return Err(VfsError::NotFound);
                }
                Ok(file_for(e))
            }
            DirKind::SysDir => {
                if name == "kernel" {
                    Ok(sys_kernel_dir())
                } else {
                    Err(VfsError::NotFound)
                }
            }
            DirKind::SysKernelDir => fmt::proc_sys_file(name)
                .map(file_for)
                .ok_or(VfsError::NotFound),
            DirKind::NetDir => fmt::proc_net_file(name)
                .map(file_for)
                .ok_or(VfsError::NotFound),
            DirKind::Pid(pid) => {
                if !compat::compat_exists(pid) {
                    return Err(VfsError::NotFound);
                }
                if name == "fd" {
                    return Ok(fd_dir(pid));
                }
                if name == "fdinfo" {
                    return Ok(fdinfo_dir(pid));
                }
                if name == "task" {
                    // Резолвится через `tgid` цели, поэтому не-лидер получает
                    // здесь всю свою группу тредов (Linux делает так же), а имя
                    // каталога остаётся свойством группы, а не компонента пути,
                    // через который пришли.
                    return match compat::with_pid_compat(pid, |cs| cs.tgid) {
                        Some(tgid) => Ok(task_dir(tgid)),
                        None => Err(VfsError::NotFound),
                    };
                }
                fmt::proc_pid_file(name)
                    .map(|e| pid_file(pid, e))
                    .ok_or(VfsError::NotFound)
            }
            DirKind::TaskDir(tgid) => {
                // Числовой компонент резолвится, только когда pid — ЖИВАЯ задача
                // ЭТОЙ группы тредов; зарегистрированная задача другого процесса,
                // мёртвый pid и нечисловое имя — ENOENT. Проверка живости и
                // членства держит каталог пространством имён, а не wildcard'ом
                // (`docs/procfs.md` §5.6).
                let tid = fmt::parse_pid_component(name).ok_or(VfsError::NotFound)?;
                let actual = compat::with_pid_compat(tid, |cs| cs.tgid);
                if actual != Some(tgid) {
                    return Err(VfsError::NotFound);
                }
                // Тот же VFS-объект, что `/proc/<tid>`: одна задача, один inode,
                // один набор файлов (`docs/procfs.md` §13).
                Ok(pid_dir(tid))
            }
            DirKind::FdDir(pid) => {
                let fd: u32 = name.parse().map_err(|_| VfsError::NotFound)?;
                let target = fd_snapshot(pid)
                    .into_iter()
                    .find(|(open_fd, _)| *open_fd == fd)
                    .map(|(_, target)| target)
                    .ok_or(VfsError::NotFound)?;
                Ok(Arc::new(ProcFdLink {
                    name: String::from(name),
                    target,
                    ino: fmt::pid_fd_ino(pid, fd),
                }))
            }
            DirKind::FdInfoDir(pid) => {
                if !compat::compat_exists(pid) {
                    return Err(VfsError::NotFound);
                }
                let fd: u32 = name.parse().map_err(|_| VfsError::NotFound)?;
                if !fd_is_open(pid, fd) {
                    return Err(VfsError::NotFound);
                }
                Ok(fdinfo_file(pid, fd))
            }
        }
    }
}

// ─── файлы ──────────────────────────────────────────────────────────────────

fn file_for(e: fmt::ProcEntry) -> Arc<dyn VfsNode> {
    Arc::new(ProcFile {
        name: Cow::Borrowed(e.name),
        ino: e.ino,
        kind: e.kind,
        pid: 0,
        fd: 0,
        buf: Spinlock::new(None),
    })
}

fn pid_file(pid: u64, e: fmt::ProcPidEntry) -> Arc<dyn VfsNode> {
    Arc::new(ProcFile {
        name: Cow::Borrowed(e.name),
        ino: fmt::pid_file_ino(pid, e.slot),
        kind: e.kind,
        pid,
        fd: 0,
        buf: Spinlock::new(None),
    })
}

/// `/proc/<pid>/fdinfo/<fd>`: текстовый узел создаётся только для живого pid и
/// открытого дескриптора (вызывающий уже проверил оба).
fn fdinfo_file(pid: u64, fd: u32) -> Arc<dyn VfsNode> {
    Arc::new(ProcFile {
        name: Cow::Owned(alloc::format!("{fd}")),
        ino: fmt::pid_fdinfo_ino(pid, fd),
        kind: fmt::ProcKind::FdInfo,
        pid,
        fd,
        buf: Spinlock::new(None),
    })
}

/// Путь образа текущего процесса, если он есть.
fn current_exe() -> Option<String> {
    compat::with_current_compat(|cs| cs.exe_path.clone()).filter(|p| !p.is_empty())
}

struct ProcFile {
    /// Имя из статической таблицы или десятичный fd для узла `fdinfo`.
    name: Cow<'static, str>,
    ino: u64,
    kind: ProcKind,
    /// Цель узла `ProcKind::Pid*`/`FdInfo`; у `Self*`/статических видов не
    /// используется.
    pid: u64,
    /// Дескриптор узла `ProcKind::FdInfo` (0 для остальных видов).
    fd: u32,
    /// Рендер текущего прохода чтения. Заполняется по требованию; чтение со
    /// смещения 0 его обновляет (см. документацию модуля).
    buf: Spinlock<Option<Vec<u8>>>,
}

impl ProcFile {
    /// Рендерит текущий текст файла. Вызывается **без** спинлока узла и сам его
    /// не берёт, поэтому может свободно обращаться к состоянию ядра.
    fn render(&self) -> Vec<u8> {
        match self.kind {
            ProcKind::CpuInfo => fmt::format_cpuinfo(&[cpuid::read()]),
            ProcKind::MemInfo => fmt::format_meminfo(&fmt::MemInfoKb {
                total_kb: pmm::total_frames() as u64 * PAGE_SIZE / 1024,
                free_kb: pmm::free_frames() as u64 * PAGE_SIZE / 1024,
            }),
            ProcKind::Uptime => fmt::format_uptime(scheduler::ticks(), TICK_HZ),
            // Только счётчики: ни чтения wall-clock, ни арифметики в рендерере
            // (`docs/procfs.md` §4.8). `procs_blocked` — честный ноль: у этого
            // планировщика нет состояния blocked (блокирующие ожидания уступают
            // и остаются runnable), считать нечего.
            ProcKind::Stat => {
                let cpu = stat::cpu_ticks();
                fmt::format_stat(&fmt::StatInputs {
                    user: cpu.user,
                    system: cpu.system,
                    idle: cpu.idle,
                    ctxt: stat::context_switches(),
                    btime: stat::boot_unix(),
                    processes: stat::processes(),
                    procs_running: scheduler::ready_count(),
                    procs_blocked: 0,
                })
            }
            ProcKind::SelfCmdline => self.render_cmdline(scheduler::current_pid()),
            ProcKind::SelfStatus => self.render_status(scheduler::current_pid()),
            ProcKind::SelfMaps => self.render_maps(scheduler::current_pid()),
            ProcKind::SelfStat => self.render_stat(scheduler::current_pid()),
            ProcKind::SelfStatm => self.render_statm(scheduler::current_pid()),
            ProcKind::PidCmdline => self.render_cmdline(self.pid),
            ProcKind::PidStatus => self.render_status(self.pid),
            ProcKind::PidMaps => self.render_maps(self.pid),
            ProcKind::PidStat => self.render_stat(self.pid),
            // Настоящая версия крейта, а не выдуманная строка Linux-релиза.
            ProcKind::Version => fmt::format_version(KERNEL_VERSION),
            ProcKind::FileSystems => fmt::format_filesystems(&fmt::FILESYSTEMS),
            ProcKind::LoadAvg => {
                let (one, five, fifteen) = scheduler::load_averages();
                let (runnable, total, last_pid) = scheduler::loadavg_counts();
                fmt::format_loadavg(&fmt::LoadAvg {
                    one,
                    five,
                    fifteen,
                    runnable,
                    total,
                    last_pid,
                })
            }
            ProcKind::SysOstype => fmt::format_sys_ostype(),
            ProcKind::SysOsrelease => fmt::format_sys_osrelease(KERNEL_VERSION),
            ProcKind::SysHostname => fmt::format_sys_hostname(),
            ProcKind::SelfCgroup | ProcKind::PidCgroup => fmt::format_cgroup(),
            ProcKind::FdInfo => self.render_fdinfo(self.pid, self.fd),
            ProcKind::SelfLimits => self.render_limits(scheduler::current_pid()),
            ProcKind::PidLimits => self.render_limits(self.pid),
            ProcKind::SelfMountInfo | ProcKind::PidMountInfo => {
                let snapshot = crate::vfs::mount_snapshot();
                let owned: Vec<(String, &'static str)> = snapshot
                    .into_iter()
                    .map(|entry| (entry.point, entry.fs))
                    .collect();
                let refs: Vec<(&str, &str)> = owned
                    .iter()
                    .map(|(point, fs)| (point.as_str(), *fs))
                    .collect();
                fmt::format_mountinfo(&refs)
            }
            ProcKind::Mounts | ProcKind::SelfMounts => {
                let snapshot = crate::vfs::mount_snapshot();
                let owned: Vec<(String, &'static str)> = snapshot
                    .into_iter()
                    .map(|entry| (entry.point, entry.fs))
                    .collect();
                let refs: Vec<(&str, &str)> = owned
                    .iter()
                    .map(|(point, fs)| (point.as_str(), *fs))
                    .collect();
                fmt::format_mounts(&refs)
            }
            // Сетевые счётчики — живой снимок канала; канала нет — только две
            // шапки (Linux так же печатает таблицу без интерфейсов). Формат и
            // ширины колонок живут в чистом форматтере.
            ProcKind::NetDev => {
                let rows: Vec<fmt::NetDevRow> = crate::net::iface_stats()
                    .map(|s| fmt::NetDevRow {
                        name: s.name,
                        rx_bytes: s.counters.rx_bytes,
                        rx_packets: s.counters.rx_packets,
                        rx_errs: s.rx_errors,
                        rx_drop: s.rx_dropped,
                        tx_bytes: s.counters.tx_bytes,
                        tx_packets: s.counters.tx_packets,
                        tx_errs: s.tx_errors,
                        tx_drop: s.tx_dropped,
                    })
                    .into_iter()
                    .collect();
                fmt::format_net_dev(&rows)
            }
            // Узел `exe` — симлинк, а не читаемый файл.
            ProcKind::SelfExe => Vec::new(),
        }
    }

    /// Рендерит в буфер сессии (идемпотентно).
    fn refresh(&self) {
        let fresh = self.render();
        *self.buf.lock() = Some(fresh);
    }

    /// `/proc/<pid>/cmdline` — снимок argv, который `CompatState` уже держит в
    /// Linux-форме с разделителями NUL (`format_cmdline`). Копируем под локом
    /// реестра, ничего не форматируя.
    fn render_cmdline(&self, pid: u64) -> Vec<u8> {
        compat::with_pid_compat(pid, |cs| cs.cmdline.clone()).unwrap_or_default()
    }

    fn render_status(&self, pid: u64) -> Vec<u8> {
        let Some(s) = compat::with_pid_compat(pid, |cs| StatusSnap::from(cs)) else {
            // Цель завершилась между lookup и рендером: пустой файл — единственный
            // честный ответ VFS-узла (сам путь уже исчез).
            return Vec::new();
        };
        // Threads = целевой тред плюс все другие треды его группы. Запрашивается
        // вне лока реестра (`group_member_pids` берёт его сам).
        let threads = compat::group_member_pids(s.tgid, pid).len() as u64 + 1;

        fmt::format_status(&fmt::StatusInputs {
            name: &s.name,
            state: pid_state(pid),
            umask: s.umask,
            pid,
            tgid: s.tgid,
            ppid: s.ppid,
            threads,
            fd_size: s.fd_size,
            sig_pending: s.sig_pending,
            sig_blocked: s.sig_blocked,
            sig_ign: s.sig_ign,
            sig_cgt: s.sig_cgt,
            uids: s.uids,
            gids: s.gids,
            vm: s.vm,
        })
    }

    fn render_stat(&self, pid: u64) -> Vec<u8> {
        let Some(s) = compat::with_pid_compat(pid, |cs| StatusSnap::from(cs)) else {
            return Vec::new();
        };
        let threads = compat::group_member_pids(s.tgid, pid).len() as u64 + 1;
        let stack_lo = USER_STACK_TOP - USER_STACK_PAGES * PAGE_SIZE;

        fmt::format_pid_stat(&fmt::PidStatInputs {
            name: &s.name,
            pid,
            state: pid_state(pid),
            ppid: s.ppid,
            // Одна группа процессов на группу тредов (сессии pagh не моделирует).
            pgrp: s.tgid,
            session: s.tgid,
            threads,
            sig_blocked: s.sig_blocked,
            sig_ign: s.sig_ign,
            sig_cgt: s.sig_cgt,
            vm: s.vm,
            start_brk: s.current_brk,
            startstack: stack_lo,
            exit_code: s.exit_code,
        })
    }

    fn render_statm(&self, pid: u64) -> Vec<u8> {
        match compat::with_pid_compat(pid, |cs| StatusSnap::from(cs)) {
            Some(s) => fmt::format_statm(&s.vm),
            None => Vec::new(),
        }
    }

    fn render_maps(&self, pid: u64) -> Vec<u8> {
        let Some(s) = compat::with_pid_compat(pid, |cs| MapsSnap::from(cs)) else {
            return Vec::new();
        };
        maps_text(&s)
    }

    /// `/proc/<pid>/limits`: эффективная пара `(soft, hard)` цели для каждого
    /// Linux resource id. Побеждает per-process override из `prlimit64`/
    /// `setrlimit`; иначе отдаётся дефолт ядра (`misc::default_rlimit`) — ровно
    /// то, что `getrlimit(2)` вернёт для этой цели. Ресурсы, которые ядро не
    /// моделирует, дают `RLIM_INFINITY` и рендерятся как `unlimited`, а не
    /// выдуманным числом.
    fn render_limits(&self, pid: u64) -> Vec<u8> {
        let Some(values) = compat::with_pid_compat(pid, |cs| {
            let mut values = [(fmt::RLIM_INFINITY, fmt::RLIM_INFINITY); fmt::RLIMIT_COUNT];
            for (resource, slot) in values.iter_mut().enumerate() {
                *slot = cs
                    .rlimits
                    .get(&(resource as u64))
                    .copied()
                    .unwrap_or_else(|| misc::default_rlimit(resource as u64));
            }
            values
        }) else {
            return Vec::new();
        };
        fmt::format_limits(&values)
    }

    /// `/proc/<pid>/fdinfo/<fd>`: снимает реальные position, flags, mount и
    /// inode дескриптора под локом реестра, затем форматирует после его
    /// освобождения. `mnt_id`/`ino` остаются 0 у объектов без VFS-монтирования/
    /// inode.
    fn render_fdinfo(&self, pid: u64, fd: u32) -> Vec<u8> {
        let Some(s) = compat::with_pid_compat(pid, |cs| FdInfoSnap::from(cs, fd)).flatten() else {
            return Vec::new();
        };
        let mnt_id = s.path.as_deref().map(mount_id_for_path).unwrap_or(0);
        let ino = match &s.node {
            Some(node) => crate::arch::x86_64::linux::io_sys::node_ino(node),
            // Дескриптор каталога хранит только путь; резолвим его вне лока
            // реестра, чтобы показать настоящий inode каталога.
            None => s
                .path
                .as_deref()
                .and_then(|path| super::lookup_path(path).ok())
                .map(|node| crate::arch::x86_64::linux::io_sys::node_ino(&node))
                .unwrap_or(0),
        };
        fmt::format_fdinfo(&fmt::FdInfo {
            pos: s.pos,
            flags: s.flags,
            mnt_id,
            ino,
        })
    }
}

/// Настоящие поля дескриптора, скопированные из-под `COMPAT_STATES`.
struct FdInfoSnap {
    pos: u64,
    flags: u64,
    /// Абсолютный VFS-путь для дескриптора `File`/`Dir`; `None` для stdio,
    /// пайпов, сокетов и прочих анонимных объектов.
    path: Option<String>,
    /// VFS-узел за дескриптором `File` (клонируется, чтобы `fs_ino()`
    /// выполнился после освобождения лока реестра).
    node: Option<Arc<dyn VfsNode>>,
}

impl FdInfoSnap {
    fn from(cs: &CompatState, fd: u32) -> Option<Self> {
        let flags = cs.fds.fdinfo_flags(fd)?;
        match cs.fds.get(fd)? {
            OpenObject::File { node, path, offset } => Some(FdInfoSnap {
                pos: *offset,
                flags,
                path: Some(path.clone()),
                node: Some(Arc::clone(node)),
            }),
            // Единственная позиция дескриптора каталога в этом ядре — курсор
            // детей `getdents64` (`lseek(SEEK_SET)` ставит ровно этот индекс),
            // поэтому отдаём его как `pos`, не выдумывая байтовое смещение, как
            // в Linux.
            OpenObject::Dir { path, index, .. } => Some(FdInfoSnap {
                pos: *index as u64,
                flags,
                path: Some(path.clone()),
                node: None,
            }),
            _ => Some(FdInfoSnap {
                pos: 0,
                flags,
                path: None,
                node: None,
            }),
        }
    }
}

/// Отображает путь VFS ядра в id, который назначает `/proc/*/mountinfo`:
/// 1-based индекс самой длинной подходящей точки монтирования в отсортированном
/// [`super::mount_snapshot`]. Таблица и порядок те же, поэтому `mnt_id` файла и
/// соответствующая строка mountinfo согласованы. Пути `memfd` нигде не
/// смонтированы и дают 0.
fn mount_id_for_path(path: &str) -> u64 {
    if path.starts_with("/memfd:") {
        return 0;
    }
    let mut best: Option<(usize, u64)> = None;
    for (index, entry) in super::mount_snapshot().iter().enumerate() {
        let point = entry.point.as_str();
        let matches = point == "/"
            || path == point
            || path
                .strip_prefix(point)
                .is_some_and(|rest| rest.starts_with('/'));
        if matches && best.map_or(true, |(len, _)| point.len() > len) {
            best = Some((point.len(), index as u64 + 1));
        }
    }
    best.map(|(_, id)| id).unwrap_or(0)
}

/// Запаркована ли целевая задача stop-сигналом? Для `/proc` это ядро различает
/// ровно два наблюдаемых состояния: `R` (running/ready) и `T` (stopped;
/// `scheduler::STOPPED_TASKS`). Спящие задачи не моделируются.
fn pid_state(pid: u64) -> u8 {
    if scheduler::is_stopped(pid) {
        b'T'
    } else {
        b'R'
    }
}

/// Всё, что нужно `/proc/<pid>/{status,stat,statm}`, скопированное из-под лока
/// реестра, чтобы форматирование его никогда не держало (AGENTS.md, инвариант 4).
struct StatusSnap {
    name: Vec<u8>,
    umask: u32,
    tgid: u64,
    ppid: u64,
    fd_size: u64,
    sig_pending: u64,
    sig_blocked: u64,
    sig_ign: u64,
    sig_cgt: u64,
    uids: [u32; 4],
    gids: [u32; 4],
    vm: fmt::VmKb,
    /// Текущий program break (поле 47 `/proc/<pid>/stat`).
    current_brk: u64,
    /// Записанный код выхода (поле 52 `/proc/<pid>/stat`), 0 пока жива.
    exit_code: i64,
}

impl StatusSnap {
    fn from(cs: &CompatState) -> Self {
        let argv0: &[u8] = match cs.cmdline.iter().position(|b| *b == 0) {
            Some(end) => &cs.cmdline[..end],
            None => &cs.cmdline[..],
        };
        let name = fmt::task_name(&cs.exe_path, argv0);

        // Маски диспозиций: `SIG_IGN` для игнорируемых сигналов, «установлен
        // обработчик» для всего, что не default и не ignore.
        let mut sig_ign = 0u64;
        let mut sig_cgt = 0u64;
        for (i, action) in cs.sig.lock().handlers.iter().enumerate() {
            if action.handler == SIG_IGN {
                sig_ign |= 1u64 << i;
            } else if action.handler != SIG_DFL {
                sig_cgt |= 1u64 << i;
            }
        }

        let (vm, current_brk) = vm_kb(cs);

        StatusSnap {
            name,
            umask: cs.umask,
            tgid: cs.tgid,
            ppid: cs.ppid,
            fd_size: cs.fds.capacity() as u64,
            sig_pending: cs.sig_pending,
            sig_blocked: cs.sig_blocked,
            sig_ign,
            sig_cgt,
            // Четыре колонки Uid/Gid в Linux — real, effective, saved и
            // filesystem. В pagh нет отдельных fsuid/fsgid, поэтому последняя
            // колонка повторяет effective id.
            uids: [cs.creds.uid, cs.creds.euid, cs.creds.suid, cs.creds.euid],
            gids: [cs.creds.gid, cs.creds.egid, cs.creds.sgid, cs.creds.egid],
            vm,
            current_brk,
            exit_code: cs.exit_code.map(|c| c as i64).unwrap_or(0),
        }
    }
}

/// Входы `/proc/<pid>/maps`, снятые под локом реестра.
struct MapsSnap {
    exe_path: String,
    /// `st_ino` VFS-узла образа (идентичность `stat(2)`), 0 если неизвестен.
    image_ino: u64,
    interp_path: String,
    /// `st_ino` VFS-узла интерпретатора, 0 для статических образов.
    interp_ino: u64,
    image_segments: Vec<LoadSegment>,
    interp_segments: Vec<LoadSegment>,
    initial_brk: u64,
    current_brk: u64,
    mmaps: Vec<crate::arch::x86_64::linux::mem::MmapRegion>,
}

impl MapsSnap {
    fn from(cs: &CompatState) -> Self {
        let guard = cs.vm.lock();
        MapsSnap {
            exe_path: cs.exe_path.clone(),
            image_ino: cs.image_ino,
            interp_path: cs.interp_path.clone(),
            interp_ino: cs.interp_ino,
            image_segments: cs.image_segments.clone(),
            interp_segments: cs.interp_segments.clone(),
            initial_brk: guard.initial_brk,
            current_brk: guard.current_brk,
            mmaps: guard.mmaps.clone(),
        }
    }
}

/// Рендерит снимок `/proc/<pid>/maps` (общий для `self` и pid-узлов).
fn maps_text(s: &MapsSnap) -> Vec<u8> {
    // Строки file-backed несут настоящие `st_dev`/`st_ino`, которые `stat(2)`
    // отдаёт для того же VFS-узла, а file-backed `mmap` ещё и pathname и file
    // offset (`docs/procfs.md` §14). Узел без VFS-идентичности сохраняет честный
    // `00:00 0`.
    let mut regions: Vec<fmt::MapsRegion> = Vec::new();
    let (image_dev, image_ino) = file_identity(s.image_ino);
    for seg in &s.image_segments {
        regions.push(seg_region(
            seg,
            Some(s.exe_path.as_str()),
            image_dev,
            image_ino,
        ));
    }
    let (interp_dev, interp_ino) = file_identity(s.interp_ino);
    for seg in &s.interp_segments {
        regions.push(seg_region(
            seg,
            Some(s.interp_path.as_str()),
            interp_dev,
            interp_ino,
        ));
    }

    let heap_lo = page_down(s.initial_brk);
    let heap_hi = page_up(s.current_brk);
    if heap_hi > heap_lo {
        regions.push(fmt::MapsRegion {
            start: heap_lo,
            end: heap_hi,
            prot: fmt::PROT_READ | fmt::PROT_WRITE,
            file_offset: 0,
            dev_major: 0,
            dev_minor: 0,
            ino: 0,
            path: Some("[heap]"),
        });
    }

    for m in &s.mmaps {
        let (maj, min, ino, offset, path) = match &m.backing {
            Some(b) => {
                let (maj, min) = fmt::dev_bytes(vfs_dev());
                (maj, min, b.ino, b.file_offset, Some(b.path.as_str()))
            }
            // Анонимный маппинг: файла за ним нет, печатать идентичность нечего.
            None => (0, 0, 0, 0, None),
        };
        regions.push(fmt::MapsRegion {
            start: m.base,
            end: m.base.saturating_add(m.pages * PAGE_SIZE),
            prot: m.prot,
            file_offset: offset,
            dev_major: maj,
            dev_minor: min,
            ino,
            path,
        });
    }

    let stack_lo = USER_STACK_TOP - USER_STACK_PAGES * PAGE_SIZE;
    regions.push(fmt::MapsRegion {
        start: stack_lo,
        end: USER_STACK_TOP,
        prot: fmt::PROT_READ | fmt::PROT_WRITE,
        file_offset: 0,
        dev_major: 0,
        dev_minor: 0,
        ino: 0,
        path: Some("[stack]"),
    });

    fmt::format_maps(&regions)
}

/// `st_dev`, который каждый VFS-узел отдаёт через `stat(2)` (`08:00`); строка
/// maps для file-backed-маппинга обязана печатать то же устройство, что `stat`.
fn vfs_dev() -> u64 {
    crate::arch::x86_64::linux::io_sys::STAT_DEV_VFS
}

/// `(dev, ino)` для file-backed-региона: настоящая идентичность `stat(2)`, если
/// у узла она есть, иначе честный сентинел `00:00 0`.
fn file_identity(ino: u64) -> (u64, u64) {
    if ino == 0 {
        (0, 0)
    } else {
        (vfs_dev(), ino)
    }
}

/// Одна строка `/proc/<pid>/maps` для загруженного `PT_LOAD`-сегмента.
fn seg_region<'a>(
    seg: &LoadSegment,
    path: Option<&'a str>,
    dev: u64,
    ino: u64,
) -> fmt::MapsRegion<'a> {
    let (dev_major, dev_minor) = fmt::dev_bytes(dev);
    fmt::MapsRegion {
        start: seg.start,
        end: seg.end,
        prot: prot_bits(seg.prot),
        file_offset: seg.file_offset,
        dev_major,
        dev_minor,
        ino,
        path,
    }
}

/// ELF `p_flags` (`PF_R`/`PF_W`/`PF_X`) → биты Linux `PROT_*`.
fn prot_bits(p_flags: u32) -> u32 {
    let mut prot = 0;
    if p_flags & 4 != 0 {
        prot |= fmt::PROT_READ;
    }
    if p_flags & 2 != 0 {
        prot |= fmt::PROT_WRITE;
    }
    if p_flags & 1 != 0 {
        prot |= fmt::PROT_EXEC;
    }
    prot
}

/// Округлённый по страницам учёт Vm* загруженных образов, в КиБ.
fn image_kb(image: &[LoadSegment], interp: &[LoadSegment]) -> (u64, u64) {
    let mut total = 0u64;
    for seg in image.iter().chain(interp.iter()) {
        total += seg.end.saturating_sub(seg.start);
    }
    (total / 1024, total / 1024)
}

/// Снимает учёт Vm* цели и текущий break под одним локом `vm`.
///
/// `size` = образ + heap + stack + анонимные маппинги; `rss` считает только
/// заранее выделенные части (образ + stack) — ровно как сообщает
/// `/proc/self/status` (`docs/procfs.md` §4.5.1, DEVIATION-5).
fn vm_kb(cs: &CompatState) -> (fmt::VmKb, u64) {
    let guard = cs.vm.lock();
    let heap_lo = page_down(guard.initial_brk);
    let heap_hi = page_up(guard.current_brk);
    let heap_kb = if heap_hi > heap_lo {
        (heap_hi - heap_lo) / 1024
    } else {
        0
    };
    let stack_kb = USER_STACK_PAGES * PAGE_SIZE / 1024;
    let mmap_kb: u64 = guard.mmaps.iter().map(|m| m.pages * PAGE_SIZE / 1024).sum();
    let (image_kb, rss_kb) = image_kb(&cs.image_segments, &cs.interp_segments);
    (
        fmt::VmKb {
            size: image_kb + heap_kb + stack_kb + mmap_kb,
            rss: rss_kb + stack_kb,
            data: heap_kb,
            stk: stack_kb,
            exe: image_kb,
            lib: 0,
        },
        guard.current_brk,
    )
}

fn page_down(addr: u64) -> u64 {
    addr & !(PAGE_SIZE - 1)
}

fn page_up(addr: u64) -> u64 {
    page_down(addr.saturating_add(PAGE_SIZE - 1))
}

impl VfsNode for ProcFile {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_directory(&self) -> bool {
        false
    }

    fn fs_ino(&self) -> u64 {
        self.ino
    }

    fn is_symlink(&self) -> bool {
        self.kind == ProcKind::SelfExe
    }

    fn read_link(&self) -> Option<String> {
        if self.kind == ProcKind::SelfExe {
            current_exe()
        } else {
            None
        }
    }

    fn size(&self) -> u64 {
        // Рендерится при первом обращении; дальше буфер сессии
        // переиспользуется, чтобы `size()` и `read()` в одном syscall были
        // согласованы.
        if self.buf.lock().is_none() {
            self.refresh();
        }
        self.buf
            .lock()
            .as_ref()
            .map(|b| b.len() as u64)
            .unwrap_or(0)
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if self.kind == ProcKind::SelfExe {
            // У симлинка нет читаемого содержимого; `open` вместо этого
            // следует по нему.
            return Err(VfsError::NotSupported);
        }
        if offset == 0 {
            // Начало прохода чтения: показываем текущее состояние.
            self.refresh();
        } else if self.buf.lock().is_none() {
            self.refresh();
        }

        let guard = self.buf.lock();
        let data = match guard.as_ref() {
            Some(d) => d,
            None => return Ok(0),
        };
        let off = offset as usize;
        if off >= data.len() {
            return Ok(0);
        }
        let n = core::cmp::min(buf.len(), data.len() - off);
        buf[..n].copy_from_slice(&data[off..off + n]);
        Ok(n)
    }

    // write/truncate/create/remove остаются дефолтами трейта (`NotSupported`):
    // syscall-слой отображает это в `EACCES` для read-only синтетического файла.
    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        Err(VfsError::NotSupported)
    }
}
