//! Чистый маршаллинг ABI syscall'ов Linux x86_64 и проверка принадлежности
//! supported-set.
//!
//! Только `core`, без аллокаций, общая с крейтом `host-tests` (R11.6). Здесь две
//! обязанности:
//!
//!   * [`marshal_args`] — декодировать соглашение вызова Linux x86_64 из
//!     сохранённых GPR в пару `(nr, args)`, которую потребляет диспетчер
//!     (R1.1, R1.8).
//!   * [`is_supported`] — точная проверка принадлежности фиксированному
//!     `Supported_Syscall_Set` (R2.1); гейт диспетчеризации и `-ENOSYS` для
//!     всего остального (R1.4, R11.4, R11.5).
//!
//! Числовые константы syscall'ов экспонируются как `pub const`, чтобы позже
//! обработчики io/mem/misc и `linux_dispatch` сопоставлялись с общими именами, а
//! не голыми литералами.
#![allow(dead_code)]

/// Биты RFLAGS, которые `syscall` сбрасывает на входе в ядро (IA32_FMASK /
/// `SFMask`), как в Linux: `X86_EFLAGS_TF|DF|IF|IOPL|AC|NT`.
///
/// F-r4-arch-3: pagh маскировал только `IF|TF`, поэтому AC, DF, NT и IOPL
/// переживали вход в ring 0. AC опаснее всех: interrupt gates его не чистят, и
/// на железе с `CR0.AM=1` первый невыровненный доступ ядра в syscall-окне
/// (обработчики читают user-указатели `read_unaligned`) даёт #AC с ядерным RIP,
/// то есть остановку машины. QEMU #AC не моделирует, поэтому здесь закреплён
/// сам битсет — Linux-значение, проверяемое host-тестом.
pub const SYSCALL_SFMASK: u64 =
    RFLAGS_TF | RFLAGS_DF | RFLAGS_IF | RFLAGS_IOPL | RFLAGS_AC | RFLAGS_NT;
/// Trap Flag: пошаговое исполнение пользователя не должно протекать в ядро.
pub const RFLAGS_TF: u64 = 1 << 8;
/// Interrupt Flag: окно `syscall` идёт с выключенными прерываниями.
pub const RFLAGS_IF: u64 = 1 << 9;
/// Direction Flag: Rust/LLVM требуют DF=0 на входе в любой код.
pub const RFLAGS_DF: u64 = 1 << 10;
/// IOPL (два бита): ring 3 не должен влиять на привилегию I/O в ядре.
pub const RFLAGS_IOPL: u64 = (1 << 12) | (1 << 13);
/// Nested Task: у long mode нет аппаратного task switch, но бит обязан быть
/// сброшен, как в Linux.
pub const RFLAGS_NT: u64 = 1 << 14;
/// Alignment Check: ядро не должно получать #AC из-за флага пользователя.
pub const RFLAGS_AC: u64 = 1 << 18;

/// Номера syscall'ов Linux x86_64 из `Supported_Syscall_Set` (R2.1).
pub mod nr {
    /// `read` — чтение из дескриптора файла.
    pub const READ: u64 = 0;
    /// `write` — запись в дескриптор файла.
    pub const WRITE: u64 = 1;
    /// `open` — открыть/создать файл.
    pub const OPEN: u64 = 2;
    /// `close` — закрыть дескриптор файла.
    pub const CLOSE: u64 = 3;
    /// `fstat` — статус файла по дескриптору.
    pub const FSTAT: u64 = 5;
    pub const POLL: u64 = 7;
    /// `select` — синхронное мультиплексирование fd (таймаут timeval).
    pub const SELECT: u64 = 23;
    /// `pselect6` — select с timespec + sigmask (на нём ждёт readline).
    pub const EPOLL_WAIT: u64 = 232;
    pub const EPOLL_CTL: u64 = 233;
    pub const PSELECT6: u64 = 270;
    /// `ppoll` — poll с timespec + sigmask.
    pub const PPOLL: u64 = 271;
    pub const EVENTFD2: u64 = 290;
    pub const EPOLL_CREATE1: u64 = 291;
    /// `epoll_pwait` — epoll_wait с sigmask (маска принимается и игнорируется).
    pub const EPOLL_PWAIT: u64 = 281;
    /// `socketpair` — пара связанных AF_UNIX stream-сокетов.
    pub const SOCKETPAIR: u64 = 53;
    /// `statx` — расширенный статус файла.
    pub const STATX: u64 = 332;
    /// AF_UNIX stream-сокеты (msgpack-rpc-сервер nvim).
    pub const SOCKET: u64 = 41;
    pub const CONNECT: u64 = 42;
    pub const ACCEPT: u64 = 43;
    pub const BIND: u64 = 49;
    pub const LISTEN: u64 = 50;
    pub const GETSOCKNAME: u64 = 51;
    pub const ACCEPT4: u64 = 288;
    pub const SETSOCKOPT: u64 = 54;
    pub const GETSOCKOPT: u64 = 55;
    /// Заглушки сессий/прав/локов.
    pub const SETSID: u64 = 112;
    /// Семейство job control, которое bash пробует при старте.
    pub const SETPGID: u64 = 109;
    pub const GETPGRP: u64 = 111;
    pub const GETPGID: u64 = 121;
    pub const UMASK: u64 = 95;
    pub const FLOCK: u64 = 73;
    /// Vectored / позиционно-vectored I/O + синхронизация файлов. Читатель
    /// ShaDa в nvim получал здесь ENOSYS (E886 "function not implemented").
    pub const READV: u64 = 19;
    pub const PREADV: u64 = 295;
    pub const PWRITEV: u64 = 296;
    pub const FSYNC: u64 = 74;
    pub const FDATASYNC: u64 = 75;
    /// Семейство rename. nvim пишет ShaDa во временный файл и переименовывает
    /// его на место; отсутствие rename давало «E136: Can't rename ShaDa» и
    /// вечный экран Press-ENTER при каждом выходе.
    pub const RENAME: u64 = 82;
    pub const RENAMEAT: u64 = 264;
    pub const RENAMEAT2: u64 = 316;
    /// Реальные номера x86_64 (asm/unistd_64.h): settime=227, gettime=228,
    /// getres=229, nanosleep=230, exit_group=231. Проверено по
    /// /usr/include/asm/unistd_64.h после ошибочного «off by one», который
    /// столкнул nanosleep с exit_group и сделал выход процесса невозможным.
    pub const CLOCK_SETTIME: u64 = 227;
    pub const CLOCK_GETTIME: u64 = 228;
    pub const CLOCK_GETRES: u64 = 229;
    /// `lseek` — переместить смещение дескриптора.
    pub const LSEEK: u64 = 8;
    /// `mmap` — отобразить память.
    pub const MMAP: u64 = 9;
    /// `mprotect` — сменить защиту памяти.
    pub const MPROTECT: u64 = 10;
    /// `munmap` — снять отображение памяти.
    pub const MUNMAP: u64 = 11;
    /// `mremap` — изменить размер отображения (быстрый путь realloc в glibc).
    pub const MREMAP: u64 = 25;
    /// `brk` — изменить program break.
    pub const BRK: u64 = 12;
    /// `rt_sigaction` — прочитать/сменить действие сигнала.
    pub const RT_SIGACTION: u64 = 13;
    /// `rt_sigprocmask` — прочитать/сменить маску заблокированных сигналов.
    pub const RT_SIGPROCMASK: u64 = 14;
    /// `rt_sigreturn` — вернуться из обработчика сигнала (восстанавливает
    /// контекст, сохранённый в `rt_sigframe`, на котором стоит restorer).
    pub const RT_SIGRETURN: u64 = 15;
    /// `ioctl` — управление устройством.
    pub const IOCTL: u64 = 16;
    /// `pread64` — позиционное чтение (смещение не сдвигается).
    pub const PREAD64: u64 = 17;
    /// `pwrite64` — позиционная запись (смещение не сдвигается).
    pub const PWRITE64: u64 = 18;
    /// `writev` — gather-запись.
    pub const WRITEV: u64 = 20;
    /// `access` — проверить доступность файла.
    pub const ACCESS: u64 = 21;
    /// `sendfile` (40) — скопировать regular-файл в поток/дескриптор файла.
    pub const SENDFILE: u64 = 40;
    /// `copy_file_range` (326) — скопировать диапазон байт между двумя regular-
    /// файлами.
    pub const COPY_FILE_RANGE: u64 = 326;
    /// `faccessat2` (439) — faccessat с флагами.
    pub const FACCESSAT2: u64 = 439;
    /// `readahead` (187) — рекомендательная предвыборка.
    pub const READAHEAD: u64 = 187;
    /// `sync` (162) — сбросить все файловые системы.
    pub const SYNC: u64 = 162;
    /// `syncfs` (306) — сбросить файловую систему за fd.
    pub const SYNCFS: u64 = 306;
    /// `getpriority` (140).
    pub const GETPRIORITY: u64 = 140;
    /// `setpriority` (141).
    pub const SETPRIORITY: u64 = 141;
    /// `personality` (135) — единственная personality.
    pub const PERSONALITY: u64 = 135;
    /// `sched_getparam` (143).
    pub const SCHED_GETPARAM: u64 = 143;
    /// `sched_setscheduler` (144).
    pub const SCHED_SETSCHEDULER: u64 = 144;
    /// `sched_getscheduler` (145).
    pub const SCHED_GETSCHEDULER: u64 = 145;
    /// `sched_get_priority_max` (146).
    pub const SCHED_GET_PRIORITY_MAX: u64 = 146;
    /// `sched_get_priority_min` (147).
    pub const SCHED_GET_PRIORITY_MIN: u64 = 147;
    /// `sched_rr_get_interval` (148).
    pub const SCHED_RR_GET_INTERVAL: u64 = 148;
    /// `rt_sigpending` (127).
    pub const RT_SIGPENDING: u64 = 127;
    /// `sync_file_range` (277).
    pub const SYNC_FILE_RANGE: u64 = 277;
    /// `fallocate` (285), только режим 0.
    pub const FALLOCATE: u64 = 285;
    /// `memfd_create` (319) — анонимный файл в памяти.
    pub const MEMFD_CREATE: u64 = 319;
    /// `timerfd_create` (283).
    pub const TIMERFD_CREATE: u64 = 283;
    /// `timerfd_settime` (286).
    pub const TIMERFD_SETTIME: u64 = 286;
    /// `timerfd_gettime` (287).
    pub const TIMERFD_GETTIME: u64 = 287;
    /// `signalfd` (282).
    pub const SIGNALFD: u64 = 282;
    /// `signalfd4` (289).
    pub const SIGNALFD4: u64 = 289;
    /// `sched_setaffinity` (203) — маска одного CPU.
    pub const SCHED_SETAFFINITY: u64 = 203;
    /// `sched_getaffinity` (204) — маска одного CPU.
    pub const SCHED_GETAFFINITY: u64 = 204;
    /// `epoll_create` (213) — legacy-аргумент размера, тот же объект, что create1.
    pub const EPOLL_CREATE: u64 = 213;
    /// `fadvise64` (221) — только рекомендательно.
    pub const FADVISE64: u64 = 221;
    /// `eventfd` (284) — legacy eventfd без флагов.
    pub const EVENTFD: u64 = 284;
    /// `getcpu` (309) — единственный CPU и NUMA-узел.
    pub const GETCPU: u64 = 309;
    pub const PIPE: u64 = 22;
    /// `sched_yield` — уступить CPU.
    pub const SCHED_YIELD: u64 = 24;
    /// `dup` — продублировать дескриптор (наименьший свободный).
    pub const DUP: u64 = 32;
    /// `dup2` — продублировать дескриптор в явную цель.
    pub const DUP2: u64 = 33;
    /// `nanosleep` — высокоточный сон.
    pub const NANOSLEEP: u64 = 35;
    /// `getpid` — получить id процесса.
    pub const GETPID: u64 = 39;
    /// `clone` — создать поток, разделяющий адресное пространство.
    pub const CLONE: u64 = 56;
    /// `execve` — заменить образ текущего процесса.
    pub const EXECVE: u64 = 59;
    /// `exit` — завершить вызывающую задачу.
    pub const EXIT: u64 = 60;
    /// wait4 — ждать дочерний процесс.
    pub const WAIT4: u64 = 61;
    /// `kill` — послать сигнал процессу, группе процессов или всем процессам.
    /// Декодирование аргументов и классификация целей — в [`super::kill`]
    /// (host-тестируется `p51`); эффектный обработчик —
    /// [`super::signal::sys_kill`].
    pub const KILL: u64 = 62;
    /// `uname` — идентификация системы.
    pub const UNAME: u64 = 63;
    /// `fcntl` — управление дескриптором файла.
    pub const FCNTL: u64 = 72;
    /// `getcwd` — получить текущий рабочий каталог.
    pub const GETCWD: u64 = 79;
    /// `chdir` — сменить текущий рабочий каталог.
    pub const CHDIR: u64 = 80;
    /// `fchdir` — сменить cwd на путь дескриптора каталога.
    pub const FCHDIR: u64 = 81;
    /// `readlink` — прочитать цель символической ссылки.
    /// `mkdir` — создать каталог.
    pub const MKDIR: u64 = 83;
    /// `rmdir` (84) — git удаляет пустые временные каталоги при уборке.
    pub const RMDIR: u64 = 84;
    /// `sendto` (44) — DNS-запросы резолвера glibc по UDP.
    pub const SENDTO: u64 = 44;
    /// `recvfrom` (45) — DNS-ответы.
    pub const RECVFROM: u64 = 45;
    /// `unlink` (87) — git удаляет свои `*.lock`-файлы.
    pub const UNLINK: u64 = 87;
    /// `mkdirat` (258) — mkdir относительно AT_FDCWD/dirfd.
    pub const MKDIRAT: u64 = 258;
    /// `fchownat` (260) — chown относительно AT_FDCWD/dirfd.
    pub const FCHOWNAT: u64 = 260;
    /// `unlinkat` (263) — unlink/rmdir относительно AT_FDCWD/dirfd.
    pub const UNLINKAT: u64 = 263;
    /// `chmod` (90) — git меняет режим `.git/config.lock` при init.
    pub const CHMOD: u64 = 90;
    /// `fchmod` (91) — сменить режим открытого дескриптора файла.
    pub const FCHMOD: u64 = 91;
    /// `fchmodat` (268) — `chmod` с базой AT_FDCWD/dirfd и флагами.
    pub const FCHMODAT: u64 = 268;
    /// `faccessat` (269) — `access` с базой AT_FDCWD/dirfd и AT_EACCESS.
    pub const FACCESSAT: u64 = 269;
    /// `chown` (92) — сменить владельца пути (только root).
    pub const CHOWN: u64 = 92;
    /// `fchown` (93) — сменить владельца открытого дескриптора (только root).
    pub const FCHOWN: u64 = 93;
    pub const READLINK: u64 = 89;
    /// `gettimeofday` — получить стенное время как `timeval`.
    pub const GETTIMEOFDAY: u64 = 96;
    /// `getrlimit` — получить лимит ресурса.
    pub const GETRLIMIT: u64 = 97;
    /// `sysinfo` — статистика системы.
    pub const SYSINFO: u64 = 99;
    /// `getuid` — real user id.
    pub const GETUID: u64 = 102;
    /// `getgid` — real group id.
    pub const GETGID: u64 = 104;
    /// `setuid` — задать real/effective/saved user id.
    pub const SETUID: u64 = 105;
    /// `setgid` — задать real/effective/saved group id.
    pub const SETGID: u64 = 106;
    /// `geteuid` — effective user id.
    pub const GETEUID: u64 = 107;
    /// `getegid` — effective group id.
    pub const GETEGID: u64 = 108;
    /// `setreuid` — задать real/effective user id.
    pub const SETREUID: u64 = 113;
    /// `setregid` — задать real/effective group id.
    pub const SETREGID: u64 = 114;
    /// `setresuid` — задать любое подмножество real/effective/saved user id.
    pub const SETRESUID: u64 = 117;
    /// `getresuid` — прочитать real/effective/saved user id.
    pub const GETRESUID: u64 = 118;
    /// `setresgid` — задать любое подмножество real/effective/saved group id.
    pub const SETRESGID: u64 = 119;
    /// `getresgid` — прочитать real/effective/saved group id.
    pub const GETRESGID: u64 = 120;
    /// `getppid` — id родительского процесса.
    pub const GETPPID: u64 = 110;
    /// `statfs` — статистика файловой системы по пути.
    pub const STATFS: u64 = 137;
    /// `fstatfs` — статистика файловой системы по fd.
    pub const FSTATFS: u64 = 138;
    /// `arch_prctl` — задать/прочитать архитектурное состояние потока.
    pub const ARCH_PRCTL: u64 = 158;
    /// `gettid` — id потока.
    pub const GETTID: u64 = 186;
    /// `time` — стенные секунды.
    pub const TIME: u64 = 201;
    /// `futex` — примитив синхронизации userspace.
    pub const FUTEX: u64 = 202;
    /// `getdents64` — прочитать записи каталога.
    pub const GETDENTS64: u64 = 217;
    /// `set_tid_address` — задать указатель на thread id.
    pub const SET_TID_ADDRESS: u64 = 218;
    /// `clock_gettime` — прочитать POSIX-часы.
    /// `clock_nanosleep` — высокоточный сон относительно часов.
    pub const CLOCK_NANOSLEEP: u64 = 230;
    /// `exit_group` — завершить вызывающую задачу (форма группы потоков).
    pub const EXIT_GROUP: u64 = 231;
    /// `openat` — открыть относительно дескриптора каталога.
    pub const OPENAT: u64 = 257;
    /// `newfstatat` — статус файла относительно дескриптора каталога.
    pub const NEWFSTATAT: u64 = 262;
    /// `readlinkat` — прочитать цель симлинка относительно дескриптора каталога.
    pub const READLINKAT: u64 = 267;
    /// `set_robust_list` / `get_robust_list` — владение robust futex.
    pub const SET_ROBUST_LIST: u64 = 273;
    pub const GET_ROBUST_LIST: u64 = 274;
    /// `dup3` — продублировать дескриптор в явную цель с флагами.
    pub const DUP3: u64 = 292;
    pub const PIPE2: u64 = 293;
    /// `madvise` — рекомендательные подсказки памяти (принимаются и игнорируются).
    pub const MADVISE: u64 = 28;
    /// `tgkill` — послать сигнал конкретному потоку (модель одного потока: себе).
    pub const TGKILL: u64 = 234;
    /// `prlimit64` — получить/задать лимит ресурса.
    pub const PRLIMIT64: u64 = 302;
    /// `setrlimit` (160) — prlimit64 с вызывающим в качестве цели.
    pub const SETRLIMIT: u64 = 160;
    /// `getrusage` (98) — обнулённый учёт CPU/fault.
    pub const GETRUSAGE: u64 = 98;
    /// `times` (100) — счётчик тиков плюс обнулённый tms.
    pub const TIMES: u64 = 100;
    /// `getrandom` — заполнить буфер случайными байтами.
    pub const GETRANDOM: u64 = 318;
    /// `rseq` — регистрация restartable sequences (заглушка).
    pub const RSEQ: u64 = 334;
    /// `sigaltstack` — задать/прочитать стек сигналов (заглушка).
    pub const SIGALTSTACK: u64 = 131;
}

/// Смаршаллить сохранённые регистры syscall Linux x86_64 в `(nr, args)`.
///
/// По соглашению Linux x86_64 номер syscall в `rax`, а шесть аргументов — в
/// `rdi, rsi, rdx, r10, r8, r9`; заметьте, четвёртый аргумент лежит в `r10`, а не
/// `rcx`. Функция применяет ровно эту перестановку и всегда копирует все шесть
/// регистров-аргументов, сколько бы из них ни потреблял названный syscall
/// (R1.1, R1.8).
///
/// Возвращает `(nr, [a1, a2, a3, a4, a5, a6])`, где
/// `[a1..a6] == [rdi, rsi, rdx, r10, r8, r9]`.
#[inline]
pub fn marshal_args(
    rax: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    r10: u64,
    r8: u64,
    r9: u64,
) -> (u64, [u64; 6]) {
    (rax, [rdi, rsi, rdx, r10, r8, r9])
}

/// Наибольшая одиночная пользовательская полезная нагрузка, которую слой сокетов
/// валидирует, выделяет и копирует для `sendto`/`recvfrom` (1 МиБ).
///
/// Обход указателя для этих syscall'ов всегда ограничивался этим значением
/// (`check_user_ptr(buf, len.min(MAX_SOCKET_IO))`), но аллокация и копирование
/// использовали сырой `len`: процессу хватало 1 МиБ отображённых страниц, чтобы
/// заставить ядро попросить у аллокатора `len` байт, а `len` — контролируемый
/// атакующим `u64`: многогигабайтная аллокация, то есть abort/OOM DoS (тот же
/// класс, что аллокации по crafted-ext2 `i_size`). Всё, что трогает полезную
/// нагрузку, обязано согласоваться с пределом; см. [`socket_payload_len`].
pub const MAX_SOCKET_IO: u64 = 1 << 20;

/// Эффективная длина пользовательской полезной нагрузки для сокетного syscall из
/// сырого счётчика.
///
/// `send = true` (`sendto`): счётчик выше [`MAX_SOCKET_IO`] отвергается с
/// `EMSGSIZE` ДО любой аллокации — молча отправить усечённую датаграмму было бы
/// отдельным багом корректности.
/// `send = false` (`recvfrom`): счётчик — *ёмкость* буфера вызывающего; Linux
/// принимает ёмкость больше датаграммы, поэтому ядро урезает её до предела и
/// затем валидирует, выделяет и копирует ровно этот диапазон.
pub fn socket_payload_len(count: u64, send: bool) -> Result<u64, super::errno::Errno> {
    if send && count > MAX_SOCKET_IO {
        return Err(super::errno::Errno::EMSGSIZE);
    }
    Ok(if count > MAX_SOCKET_IO {
        MAX_SOCKET_IO
    } else {
        count
    })
}

/// Полный `Supported_Syscall_Set` (R2.1) — единственный источник правды и для
/// гейта диспетчеризации, и для теста точности.
///
/// Записан ИМЕНАМИ `nr::`, а не голыми числами, чтобы список был
/// самодокументируемым и не мог разойтись с константами, по которым
/// сопоставляется диспетчер. У каждой записи есть реальная ветка в
/// `linux::dispatch_supported` (три особые записи, маршрутизируемые самим
/// `linux_dispatch` — `execve`, `clone`, `rt_sigreturn` — тоже члены, потому что
/// проходят гейт по пути туда); число НЕ отсюда возвращает `-ENOSYS` до
/// инспекции любого указателя-аргумента (R1.4, R11.4, R11.5). Список строго
/// возрастает, поэтому [`is_supported`] может искать двоичным поиском.
///
/// ## Контракт сопровождения (три правки в одном коммите)
///
/// Добавить syscall — значит: константа `nr::`, ветка `dispatch_supported` и
/// этот список. Затем обе видимые в рантайме половины проверяются с двух
/// сторон:
///
///   * тест `supported_set_is_exact` сравнивает список с НЕЗАВИСИМЫМ литеральным
///     перечислением номеров (так опечатка в константе или пропущенная/лишняя
///     запись падает на хосте), требует строгого возрастания (дубликат или
///     перестановка падают) и проходит `0..=600` в ОБЕ стороны (принято здесь ⇒
///     пропущено гейтом; нет здесь ⇒ `-ENOSYS`);
///   * запасная ветка `dispatch_supported` логирует `nr has NO dispatch arm` и
///     возвращает `ENOSYS` — обратный дрейф (гейт пустил, ветки нет) заявляет о
///     себе, а не молча превращает syscall в вечный `ENOSYS`.
///
/// Ветки match не интроспектируются из теста, поэтому список — ручное
/// перечисление покрытия диспетчера; тест закрепляет числа, а запасная растяжка —
/// ветки.
pub const SUPPORTED_SYSCALLS: [u64; 159] = [
    nr::READ,
    nr::WRITE,
    nr::OPEN,
    nr::CLOSE,
    nr::FSTAT,
    nr::POLL,
    nr::LSEEK,
    nr::MMAP,
    nr::MPROTECT,
    nr::MUNMAP,
    nr::BRK,
    nr::RT_SIGACTION,
    nr::RT_SIGPROCMASK,
    nr::RT_SIGRETURN,
    nr::IOCTL,
    nr::PREAD64,
    nr::PWRITE64,
    nr::READV,
    nr::WRITEV,
    nr::ACCESS,
    nr::PIPE,
    nr::SELECT,
    nr::SCHED_YIELD,
    nr::MREMAP,
    nr::MADVISE,
    nr::DUP,
    nr::DUP2,
    nr::NANOSLEEP,
    nr::GETPID,
    nr::SENDFILE,
    nr::SOCKET,
    nr::CONNECT,
    nr::ACCEPT,
    nr::SENDTO,
    nr::RECVFROM,
    nr::BIND,
    nr::LISTEN,
    nr::GETSOCKNAME,
    nr::SOCKETPAIR,
    nr::SETSOCKOPT,
    nr::GETSOCKOPT,
    nr::CLONE,
    nr::EXECVE,
    nr::EXIT,
    nr::WAIT4,
    nr::KILL,
    nr::UNAME,
    nr::FCNTL,
    nr::FLOCK,
    nr::FSYNC,
    nr::FDATASYNC,
    nr::GETCWD,
    nr::CHDIR,
    nr::FCHDIR,
    nr::RENAME,
    nr::MKDIR,
    nr::RMDIR,
    nr::UNLINK,
    nr::READLINK,
    nr::CHMOD,
    nr::FCHMOD,
    nr::CHOWN,
    nr::FCHOWN,
    nr::UMASK,
    nr::GETTIMEOFDAY,
    nr::GETRLIMIT,
    nr::GETRUSAGE,
    nr::SYSINFO,
    nr::TIMES,
    nr::GETUID,
    nr::GETGID,
    nr::SETUID,
    nr::SETGID,
    nr::GETEUID,
    nr::GETEGID,
    nr::SETPGID,
    nr::GETPPID,
    nr::GETPGRP,
    nr::SETSID,
    nr::SETREUID,
    nr::SETREGID,
    nr::SETRESUID,
    nr::GETRESUID,
    nr::SETRESGID,
    nr::GETRESGID,
    nr::GETPGID,
    nr::RT_SIGPENDING,
    nr::SIGALTSTACK,
    nr::PERSONALITY,
    nr::STATFS,
    nr::FSTATFS,
    nr::GETPRIORITY,
    nr::SETPRIORITY,
    nr::SCHED_GETPARAM,
    nr::SCHED_SETSCHEDULER,
    nr::SCHED_GETSCHEDULER,
    nr::SCHED_GET_PRIORITY_MAX,
    nr::SCHED_GET_PRIORITY_MIN,
    nr::SCHED_RR_GET_INTERVAL,
    nr::ARCH_PRCTL,
    nr::SETRLIMIT,
    nr::SYNC,
    nr::GETTID,
    nr::READAHEAD,
    nr::TIME,
    nr::FUTEX,
    nr::SCHED_SETAFFINITY,
    nr::SCHED_GETAFFINITY,
    nr::EPOLL_CREATE,
    nr::GETDENTS64,
    nr::SET_TID_ADDRESS,
    nr::FADVISE64,
    nr::CLOCK_SETTIME,
    nr::CLOCK_GETTIME,
    nr::CLOCK_GETRES,
    nr::CLOCK_NANOSLEEP,
    nr::EXIT_GROUP,
    nr::EPOLL_WAIT,
    nr::EPOLL_CTL,
    nr::TGKILL,
    nr::OPENAT,
    nr::MKDIRAT,
    nr::FCHOWNAT,
    nr::NEWFSTATAT,
    nr::UNLINKAT,
    nr::RENAMEAT,
    nr::READLINKAT,
    nr::FCHMODAT,
    nr::FACCESSAT,
    nr::PSELECT6,
    nr::PPOLL,
    nr::SET_ROBUST_LIST,
    nr::GET_ROBUST_LIST,
    nr::SYNC_FILE_RANGE,
    nr::EPOLL_PWAIT,
    nr::SIGNALFD,
    nr::TIMERFD_CREATE,
    nr::EVENTFD,
    nr::FALLOCATE,
    nr::TIMERFD_SETTIME,
    nr::TIMERFD_GETTIME,
    nr::ACCEPT4,
    nr::SIGNALFD4,
    nr::EVENTFD2,
    nr::EPOLL_CREATE1,
    nr::DUP3,
    nr::PIPE2,
    nr::PREADV,
    nr::PWRITEV,
    nr::PRLIMIT64,
    nr::SYNCFS,
    nr::GETCPU,
    nr::RENAMEAT2,
    nr::GETRANDOM,
    nr::MEMFD_CREATE,
    nr::COPY_FILE_RANGE,
    nr::STATX,
    nr::RSEQ,
    nr::FACCESSAT2,
];

/// Проверить, входит ли `nr` в `Supported_Syscall_Set` (R2.1).
///
/// Двоичный поиск по [`SUPPORTED_SYSCALLS`], так что гейт и перечисленная
/// таблица не могут разойтись. `false` для любого номера не из таблицы, включая
/// всё ещё вне области `fork` (57) и `vfork` (58) и любой графический/оконный
/// номер (R1.4, R11.4, R11.5). Диспетчер использует гейт, чтобы вернуть
/// `-ENOSYS` до инспекции любого указателя-аргумента.
#[inline]
pub fn is_supported(nr: u64) -> bool {
    SUPPORTED_SYSCALLS.binary_search(&nr).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marshal_applies_linux_permutation() {
        let (nr, args) = marshal_args(0xAA, 1, 2, 3, 4, 5, 6);
        assert_eq!(nr, 0xAA);
        assert_eq!(args, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn marshal_r10_is_fourth_arg() {
        // r10 (не rcx) — это a4; подтвердить, что позиции ровно rdi,rsi,rdx,r10,r8,r9.
        let (_, args) = marshal_args(0, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60);
        assert_eq!(args, [0x10, 0x20, 0x30, 0x40, 0x50, 0x60]);
    }

    /// Гейт и перечисленный `Supported_Syscall_Set` — одно множество, проверенное
    /// в ОБЕ стороны против независимого литерального перечисления номеров Linux
    /// x86_64, которые реализует диспетчер.
    ///
    /// `EXPECTED` намеренно НЕ выводится из [`SUPPORTED_SYSCALLS`] или
    /// [`is_supported`]: это независимая половина двойной проверки (имена `nr::`
    /// против голых чисел), поэтому неверное значение константы, пропуск или
    /// дубликат падают здесь, а не уходят в релиз. Обратный дрейф (число
    /// пропущено гейтом, а ветки `dispatch_supported` нет) ловит запасная
    /// растяжка той функции в рантайме.
    #[test]
    fn supported_set_is_exact() {
        // Независимое перечисление: номера syscall'ов Linux x86_64 с реальной
        // веткой диспетчера, по возрастанию (держать отсортированным!).
        const EXPECTED: [u64; 159] = [
            0, 1, 2, 3, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
            28, 32, 33, 35, 39, 40, 41, 42, 43, 44, 45, 49, 50, 51, 53, 54, 55, 56, 59, 60, 61, 62,
            63, 72, 73, 74, 75, 79, 80, 81, 82, 83, 84, 87, 89, 90, 91, 92, 93, 95, 96, 97, 98, 99,
            100, 102, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 117, 118, 119, 120,
            121, 127, 131, 135, 137, 138, 140, 141, 143, 144, 145, 146, 147, 148, 158, 160, 162,
            186, 187, 201, 202, 203, 204, 213, 217, 218, 221, 227, 228, 229, 230, 231, 232, 233,
            234, 257, 258, 260, 262, 263, 264, 267, 268, 269, 270, 271, 273, 274, 277, 281, 282,
            283, 284, 285, 286, 287, 288, 289, 290, 291, 292, 293, 295, 296, 302, 306, 309, 316,
            318, 319, 326, 332, 334, 439,
        ];

        // 1. Два независимых перечисления совпадают поэлементно, и оба строго
        //    возрастают (на этом держится двоичный поиск гейта; дубликат или
        //    перестановка не спрячутся за проходящей проверкой).
        assert_eq!(
            SUPPORTED_SYSCALLS, EXPECTED,
            "nr:: names and the literal number list disagree"
        );
        for w in EXPECTED.windows(2) {
            assert!(
                w[0] < w[1],
                "enumeration is not strictly ascending: {} then {}",
                w[0],
                w[1]
            );
        }

        // 2. Двусторонняя точность над надмножеством всех определённых номеров:
        //    гейт принимает `n` тогда и только тогда, когда независимое
        //    перечисление содержит `n`.
        for n in 0..=600u64 {
            assert_eq!(
                is_supported(n),
                EXPECTED.binary_search(&n).is_ok(),
                "gate and enumeration disagree on nr={n}"
            );
        }
        for n in [1000u64, 1 << 20, u64::MAX] {
            assert!(!is_supported(n), "nr={n} must stay unsupported");
        }

        // 3. Точечные проверки номеров, трактовку которых документация путала:
        //    clone(56) и futex(202) ПОДДЕРЖИВАЮТСЯ (прежний doc-комментарий
        //    утверждал обратное), kill(62) — новейший член, а fork/vfork/stat —
        //    никогда.
        for n in [nr::CLONE, nr::FUTEX, nr::KILL] {
            assert!(is_supported(n), "nr={n} must be supported");
        }
        for n in [
            4u64, /* stat */
            57,   /* fork */
            58,   /* vfork */
        ] {
            assert!(!is_supported(n), "nr={n} must stay unsupported");
        }
    }

    #[test]
    fn process_model_syscalls_stay_unsupported() {
        // Syscall'ы модели процессов из «более поздней вехи» должны остаться
        // ENOSYS.
        for nr in [57u64 /* fork */, 58 /* vfork */] {
            assert!(!is_supported(nr), "{nr} must stay unsupported");
        }
    }
}
