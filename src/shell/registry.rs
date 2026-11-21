//! Реестр команд: единственная статическая таблица `CommandSpec`, ведущая
//! диспетчеризацию и `help`.
//!
//! Модуль — каркас registry-driven модели диспетчеризации (R6). Он определяет
//! форму [`CommandSpec`], [`ShellCtx`], который получают обработчики, таблицу
//! [`COMMANDS`] и хелперы [`lookup`] / [`command_names`], используемые
//! диспетчеризацией, `help` и Tab-дополнением.
//!
//! Таблица [`COMMANDS`] заполнена; сами тела команд по-прежнему в
//! `mod.rs::execute_command`, а диспетчеризация идёт через [`lookup`].
//! Дополнение потребляет [`command_names`]. Эти элементы не упоминаются внутри
//! иначе, отсюда `#[allow(dead_code)]`, чтобы сборка `#![no_std]` оставалась без
//! предупреждений.

/// Общее изменяемое состояние, с которым обработчик команды работает за один
/// вызов.

/// По замыслу здесь собраны сервисы, нужные обработчикам, чтобы они оставались
/// развязаны с глобальными. В этой версии каркаса полей нет; он будет наполняться
/// по мере миграции команд.
///
/// Обработчики получают текущий рабочий каталог через
/// [`crate::shell::path::cwd`] / [`crate::shell::path::set_cwd`] и рендерят вывод
/// через хелперы `crate::shell::render`. Ничего в этой структуре не должно
/// вызывать `render` — этот модуль подключается отдельно.
#[allow(dead_code)]
pub struct ShellCtx;

impl ShellCtx {
    /// Создать свежий контекст на один вызов.
    #[allow(dead_code)]
    pub fn new() -> Self {
        ShellCtx
    }
}

impl Default for ShellCtx {
    fn default() -> Self {
        ShellCtx::new()
    }
}

/// Метаданные и обработчик одной команды, единица реестра.
///
/// Одна строка `CommandSpec` ведёт и диспетчеризацию (через `handler`), и `help`
/// (через `name` / `description` / `usage`), так что добавление команды — правка
/// одной строки без других изменений (R6.5, R9.5).
#[allow(dead_code)]
pub struct CommandSpec {
    /// Слово команды, набираемое в промпте (например `"ls"`).
    pub name: &'static str,
    /// Однострочная сводка, показываемая `help`.
    pub description: &'static str,
    /// Строка usage для `help <cmd>` и ошибок аргументов.
    pub usage: &'static str,
    /// Обработчик, вызываемый с контекстом вызова и разобранными аргументами
    /// (токены после имени команды).
    pub handler: fn(ctx: &mut ShellCtx, args: &[&str]),
}

/// Единственный источник истины для команд shell.
///
/// Диспетчеризация, `help` и дополнение читают этот срез, так что добавление
/// строки — единственная правка, нужная чтобы выставить команду (R6.5, R9.5).
/// Порядок здесь — порядок, в котором `help` перечисляет команды.
pub static COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "help",
        description: "Show this help",
        usage: "help [command]",
        handler: super::commands::cmd_help,
    },
    CommandSpec {
        name: "clear",
        description: "Clear screen",
        usage: "clear",
        handler: super::commands::cmd_clear,
    },
    CommandSpec {
        name: "echo",
        description: "Echo arguments",
        usage: "echo <text>",
        handler: super::commands::cmd_echo,
    },
    CommandSpec {
        name: "uptime",
        description: "Show ticks",
        usage: "uptime",
        handler: super::commands::cmd_uptime,
    },
    CommandSpec {
        name: "date",
        description: "Show the wall clock (UTC) and Unix seconds",
        usage: "date",
        handler: super::commands::cmd_date,
    },
    CommandSpec {
        name: "ls",
        description:
            "List a directory; -l long format, -a accepted (all entries are always listed)",
        usage: "ls [-l] [-a] [path]",
        handler: super::commands::cmd_ls,
    },
    CommandSpec {
        name: "tree",
        description: "Recursive directory listing (depth-limited)",
        usage: "tree [-L DEPTH] [PATH]",
        handler: super::commands::cmd_tree,
    },
    CommandSpec {
        name: "cat",
        description: "Print file contents",
        usage: "cat <path>",
        handler: super::commands::cmd_cat,
    },
    CommandSpec {
        name: "chmod",
        description: "Change permission bits of files (octal mode)",
        usage: "chmod OCTAL-MODE PATH...",
        handler: super::commands::cmd_chmod,
    },
    CommandSpec {
        name: "chown",
        description: "Change owner and group of files (numeric uid[:gid])",
        usage: "chown UID[:GID] PATH...",
        handler: super::commands::cmd_chown,
    },
    CommandSpec {
        name: "mkdir",
        description: "Create a directory (e.g. mkdir /mnt/d)",
        usage: "mkdir <path>",
        handler: super::commands::cmd_mkdir,
    },
    CommandSpec {
        name: "touch",
        description: "Create an empty file",
        usage: "touch <path>",
        handler: super::commands::cmd_touch,
    },
    CommandSpec {
        name: "write",
        description: "Write text to a file (write <path> <text>)",
        usage: "write <path> <text>",
        handler: super::commands::cmd_write,
    },
    CommandSpec {
        name: "rm",
        description: "Remove files/dirs (-r for directories)",
        usage: "rm [-r] <path>...",
        handler: super::commands::cmd_rm,
    },
    CommandSpec {
        name: "sync",
        description: "Flush the filesystem",
        usage: "sync",
        handler: super::commands::cmd_sync,
    },
    CommandSpec {
        name: "fscrash",
        description: "Demo journal replay + persistence",
        usage: "fscrash",
        handler: super::commands::cmd_fscrash,
    },
    CommandSpec {
        name: "pci",
        description: "List PCI devices",
        usage: "pci",
        handler: super::commands::cmd_pci,
    },
    CommandSpec {
        name: "usb",
        description: "xHCI controller state and per-port status",
        usage: "usb",
        handler: super::commands::cmd_usb,
    },
    CommandSpec {
        name: "disk",
        description: "List block devices; format one as ext2 and mount it at /mnt",
        usage: "disk [list|setup|mount <n>|format <n> [--force]|cancel]",
        handler: super::commands::cmd_disk,
    },
    CommandSpec {
        name: "mount",
        description: "Not implemented: kernel mount(2) is deferred (USB S1); use `disk`",
        usage: "mount -t ext2 <device> <point>  (not implemented)",
        handler: super::commands::cmd_mount,
    },
    CommandSpec {
        name: "umount",
        description: "Not implemented: kernel umount2(2) is deferred (USB S1); use `disk`",
        usage: "umount <point>  (not implemented)",
        handler: super::commands::cmd_umount,
    },
    CommandSpec {
        name: "df",
        description: "Show mounted filesystems and capacity; -h binary and -H SI units",
        usage: "df [-h] [-H]",
        handler: super::commands::cmd_df,
    },
    CommandSpec {
        name: "free",
        description: "Show physical memory and kernel heap usage",
        usage: "free",
        handler: super::commands::cmd_free,
    },
    CommandSpec {
        name: "ps",
        description: "List compat processes and kernel threads; -f adds UID and full command line",
        usage: "ps [-e] [-f]",
        handler: super::commands::cmd_ps,
    },
    CommandSpec {
        name: "exec",
        description: "Run the embedded user process",
        usage: "exec",
        handler: super::commands::cmd_exec,
    },
    CommandSpec {
        name: "ifconfig",
        description: "Show network interface config",
        usage: "ifconfig",
        handler: super::commands::cmd_ifconfig,
    },
    CommandSpec {
        name: "ping",
        description: "Send one ICMP echo request and print the RTT",
        usage: "ping <ip>",
        handler: super::commands::cmd_ping,
    },
    CommandSpec {
        name: "nc",
        description: "TCP connect + echo (nc <ip> <port> [text])",
        usage: "nc <ip> <port> [text]",
        handler: super::commands::cmd_nc,
    },
    CommandSpec {
        name: "jobs",
        description: "List stopped/running jobs",
        usage: "jobs",
        handler: super::commands::cmd_jobs,
    },
    CommandSpec {
        name: "fg",
        description: "Resume the most recent stopped job in the foreground",
        usage: "fg",
        handler: super::commands::cmd_fg,
    },
    CommandSpec {
        name: "bg",
        description: "Resume the most recent stopped job in the background",
        usage: "bg",
        handler: super::commands::cmd_bg,
    },
    CommandSpec {
        name: "selftest",
        description: "Run kernel self-test suite (serial; N passes = idempotency check)",
        usage: "selftest [passes]",
        handler: super::commands::cmd_selftest,
    },
    CommandSpec {
        name: "cd",
        description: "Change the current directory (cd [path])",
        usage: "cd [path]",
        handler: super::commands::cmd_cd,
    },
    CommandSpec {
        name: "pwd",
        description: "Print the current directory",
        usage: "pwd",
        handler: super::commands::cmd_pwd,
    },
    CommandSpec {
        name: "id",
        description: "Show uid/gid/groups of the session or a user; -u/-g/-G select, -n names",
        usage: "id [-u|-g|-G] [-n] [user]",
        handler: super::commands::cmd_id,
    },
    CommandSpec {
        name: "groups",
        description: "List group names of the session or a user",
        usage: "groups [user]",
        handler: super::commands::cmd_groups,
    },
    CommandSpec {
        name: "su",
        description: "Switch the session identity, authenticating when the session is not root",
        usage: "su [-p] [user]",
        handler: super::commands::cmd_su,
    },
    CommandSpec {
        name: "login",
        description:
            "Start a session identity for a /etc/passwd user (-p forces a password prompt)",
        usage: "login [-p] <user>",
        handler: super::commands::cmd_login,
    },
    CommandSpec {
        name: "passwd",
        description: "Set a password in /etc/shadow (no echo; root may target any user)",
        usage: "passwd [user]",
        handler: super::commands::cmd_passwd,
    },
    CommandSpec {
        name: "whoami",
        description: "Print the user name of the session's effective uid",
        usage: "whoami",
        handler: super::commands::cmd_whoami,
    },
    CommandSpec {
        name: "head",
        description: "Print the first lines of a file",
        usage: "head [-n LINES] FILE",
        handler: super::texttools::cmd_head,
    },
    CommandSpec {
        name: "tail",
        description: "Print the last lines of a file",
        usage: "tail [-n LINES] FILE",
        handler: super::texttools::cmd_tail,
    },
    CommandSpec {
        name: "wc",
        description: "Count lines, words and bytes in a file",
        usage: "wc [-l] [-w] [-c] FILE",
        handler: super::texttools::cmd_wc,
    },
    CommandSpec {
        name: "grep",
        description: "Print the lines of a file that contain PATTERN",
        usage: "grep [-i] [-n] [-c] [-v] PATTERN FILE",
        handler: super::texttools::cmd_grep,
    },
    CommandSpec {
        name: "cp",
        description: "Copy a file (cp <src> <dst>)",
        usage: "cp <src> <dst>",
        handler: super::commands::cmd_cp,
    },
    CommandSpec {
        name: "mv",
        description: "Move/rename a file (mv <src> <dst>)",
        usage: "mv <src> <dst>",
        handler: super::commands::cmd_mv,
    },
    CommandSpec {
        name: "stat",
        description: "Show file/directory info",
        usage: "stat <path>",
        handler: super::commands::cmd_stat,
    },
    CommandSpec {
        name: "sleep",
        description: "Sleep for N seconds",
        usage: "sleep <seconds>",
        handler: super::commands::cmd_sleep,
    },
    CommandSpec {
        name: "nano",
        description: "Configurable editor: undo, replace, clipboard and backups",
        usage: "nano [path] | nano --settings [show|reset|set <key> <value>]",
        handler: super::commands::cmd_nano,
    },
    CommandSpec {
        name: "lua",
        description: "Run Lua 5.5",
        usage: "lua [file|-e code]",
        handler: super::commands::cmd_lua,
    },
    CommandSpec {
        name: "python",
        description: "Run CPython 3.16",
        usage: "python [file|-c code]",
        handler: super::commands::cmd_python,
    },
    CommandSpec {
        name: "python3",
        description: "Run CPython 3.16",
        usage: "python3 [file|-c code]",
        handler: super::commands::cmd_python,
    },
    CommandSpec {
        name: "pythonc",
        description: "Compile Python source to bytecode",
        usage: "pythonc <file.py> [...]",
        handler: super::commands::cmd_pythonc,
    },
    CommandSpec {
        name: "cargo",
        description: "Run the installed Rust cargo toolchain through Linux compatibility",
        usage: "cargo <args...>",
        handler: super::commands::cmd_cargo,
    },
    CommandSpec {
        name: "rustc",
        description: "Run the installed Rust compiler through Linux compatibility",
        usage: "rustc <file.rs> [args...]",
        handler: super::commands::cmd_rustc,
    },
    CommandSpec {
        name: "rust",
        description:
            "Run a static Linux ELF (e.g. a native Rust musl binary) through Linux compatibility",
        usage: "rust <static-elf> [args...]",
        handler: super::commands::cmd_rust,
    },
    CommandSpec {
        name: "paint",
        description: "Launch the framebuffer paint app",
        usage: "paint",
        handler: super::commands::cmd_paint,
    },
    CommandSpec {
        name: "pkg",
        description: "Download & install a .deb by URL (use `ppkg` for by-name)",
        usage: "pkg <host> <path> [port]",
        handler: super::commands::cmd_pkg,
    },
    CommandSpec {
        name: "ppkg",
        description: "Pagh package manager (update/install/show/list/setmirror)",
        usage: "ppkg <update|install|show|list|setmirror> [args]",
        handler: super::commands::cmd_apt,
    },
    CommandSpec {
        name: "lxrun",
        description: "Run a Linux ELF by path or by bare name searched in PATH",
        usage: "lxrun [path|program] [args...]",
        handler: super::commands::cmd_lxrun,
    },
    CommandSpec {
        name: "warn",
        description: "Show/hide kernel [WARN] lines on screen (serial always logs)",
        usage: "warn <on|off>",
        handler: super::commands::cmd_warn,
    },
    CommandSpec {
        name: "settings",
        description: "Customize the terminal (themes, wallpaper, status bar, caret)",
        usage: "settings",
        handler: super::commands::cmd_settings,
    },
    CommandSpec {
        name: "theme",
        description: "List or switch console colour schemes",
        usage: "theme [name|next|list]",
        handler: super::commands::cmd_theme,
    },
    CommandSpec {
        name: "wallpaper",
        description: "Set the background layer behind the text",
        usage: "wallpaper <name|list>",
        handler: super::commands::cmd_wallpaper,
    },
];

/// Найти команду по точному имени линейным сканом [`COMMANDS`].
///
/// Возвращает `None`, когда ни одна команда не совпала.
#[allow(dead_code)]
pub fn lookup(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|spec| spec.name == name)
}

/// Итерировать зарегистрированные имена команд в порядке таблицы.
///
/// Используется Tab-дополнением (имена команд) и `help`, чтобы перечислять
/// доступные команды, не выставляя раскладку таблицы.
#[allow(dead_code)]
pub fn command_names() -> impl Iterator<Item = &'static str> {
    COMMANDS.iter().map(|spec| spec.name)
}
