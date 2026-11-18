//! Pagh desktop terminal: VT100/ANSI parser, RGBA renderer, and PDP/1
//! protocol client.
//!
//! The crate is deliberately dependency-free so `pagh-terminal` builds as a
//! single statically linked x86_64-musl executable for pagh's Linux ABI layer.
//! It is split into reusable pieces:
//!
//! * [`cell`] / [`screen`] — terminal buffer, scrollback, selection, modes.
//! * [`parser`] — streaming UTF-8 + VT100/ANSI escape-sequence parser.
//! * [`font`] / [`render`] / [`png`] — bitmap font, RGBA software renderer,
//!   window chrome, PNG encoder/decoder.
//! * [`protocol`] / [`client`] / [`keys`] — PDP/1 framing, client surface,
//!   surface, Linux-keyboard-to-VT input encoding.
//! * [`vtconsole`] — parser for the proposed kernel VT console shared text
//!   surface (ABI v1, see `TERMINAL.md`).
//! * [`demo`] — built-in demo shell and deterministic screenshot generator.
//! * [`session`] / [`sh`] — a real shell child behind two pipes for the terminal
//!   window, and the minimal userspace shell it runs (boot-VT mirror is opt-in).

#![forbid(unsafe_code)]

pub mod cell;
pub mod client;
pub mod demo;
pub mod font;
pub mod font_index;
pub mod keys;
pub mod parser;
pub mod png;
pub mod protocol;
pub mod render;
pub mod screen;
pub mod session;
pub mod sh;
pub mod unicode_tables;
pub mod vtconsole;

pub use cell::{Attrs, Cell, Color, Pen, Rgb};
pub use parser::Terminal;
pub use screen::{CursorStyle, MouseMode, Screen, Selection, SelectionMode};
