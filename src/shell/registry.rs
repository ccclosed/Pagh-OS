//! Command registry: the single static `CommandSpec` table driving dispatch
//! and `help`.
//!
//! This module is the scaffolding for the registry-driven dispatch model
//! described in the design (R6). It defines the [`CommandSpec`] shape, the
//! [`ShellCtx`] handlers receive, the [`COMMANDS`] table, and the [`lookup`] /
//! [`command_names`] helpers used by dispatch, `help`, and Tab completion.
//!
//! The [`COMMANDS`] table starts empty; migrating the existing command bodies
//! out of `mod.rs::execute_command` into registry handlers (and rewiring
//! dispatch through [`lookup`]) is a later task. Likewise the completion code
//! consumes [`command_names`] in a later task. Until then these items are not
//! referenced internally, hence the `#[allow(dead_code)]` annotations that keep
//! the `#![no_std]` build warning-free.

/// Shared, mutable state a command handler operates on for a single
/// invocation.
///
/// Per the design this bundles the services handlers need so they stay
/// decoupled from globals. In this scaffolding version it carries no fields;
/// it will be fleshed out as commands migrate.
///
/// Handlers access the current working directory through
/// [`crate::shell::path::cwd`] / [`crate::shell::path::set_cwd`], and (once it
/// exists) render output through the `crate::shell::render` helpers. Nothing in
/// this struct should be used to call into `render` yet — that module is
/// landed by a later task.
#[allow(dead_code)]
pub struct ShellCtx;

impl ShellCtx {
    /// Construct a fresh per-invocation context.
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

/// A single command's metadata and handler, the unit of the registry.
///
/// One `CommandSpec` row drives both dispatch (via `handler`) and `help` (via
/// `name` / `description` / `usage`), so adding a command is a single-row edit
/// with no other changes (R6.5, R9.5).
#[allow(dead_code)]
pub struct CommandSpec {
    /// The command word typed at the prompt (e.g. `"ls"`).
    pub name: &'static str,
    /// One-line summary shown by `help`.
    pub description: &'static str,
    /// Usage string shown by `help <cmd>` and on argument errors.
    pub usage: &'static str,
    /// Handler invoked with the per-invocation context and the parsed
    /// arguments (the tokens following the command name).
    pub handler: fn(ctx: &mut ShellCtx, args: &[&str]),
}

/// The single source of truth for shell commands.
///
/// Dispatch, `help`, and completion all read from this slice, so adding a row
/// is the only edit needed to expose a command (R6.5, R9.5). Order here is the
/// order `help` lists the commands in.
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
        name: "ls",
        description: "List directory entries",
        usage: "ls [path]",
        handler: super::commands::cmd_ls,
    },
    CommandSpec {
        name: "cat",
        description: "Print file contents",
        usage: "cat <path>",
        handler: super::commands::cmd_cat,
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
        description: "Remove a file or empty directory",
        usage: "rm <path>",
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
        name: "nc",
        description: "TCP connect + echo (nc <ip> <port> [text])",
        usage: "nc <ip> <port> [text]",
        handler: super::commands::cmd_nc,
    },
    CommandSpec {
        name: "selftest",
        description: "Run kernel self-test suite (serial)",
        usage: "selftest",
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
        name: "paint",
        description: "Launch the framebuffer paint app",
        usage: "paint",
        handler: super::commands::cmd_paint,
    },
];

/// Look up a command by its exact name via a linear scan of [`COMMANDS`].
///
/// Returns `None` when no command matches.
#[allow(dead_code)]
pub fn lookup(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|spec| spec.name == name)
}

/// Iterate the registered command names, in table order.
///
/// Used by Tab completion (command-name completion) and `help` to enumerate the
/// available commands without exposing the table layout.
#[allow(dead_code)]
pub fn command_names() -> impl Iterator<Item = &'static str> {
    COMMANDS.iter().map(|spec| spec.name)
}
