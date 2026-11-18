//! `pagh-desktop` — userspace-композитор и desktop server для Pagh-OS (вариант B).
//!
//! Композитор принимает кадры клиентов по AF_UNIX-протоколу PDP/1, управляет окнами
//! (z-order, фокус, перетаскивание, resize), рисует рабочий стол с панелью и умеет
//! отдавать кадры в headless-backend (PNG/PPM). Модули крейта не требуют внешних
//! зависимостей и собираются как для хоста, так и статически под `x86_64-unknown-linux-musl`.
//!
//! См. `PROTOCOL.md` (ABI), `README.md` (сборка/запуск) и `docs/desktop-server.md`
//! в корне репозитория (архитектура и остаток работ).

#![deny(unsafe_op_in_unsafe_fn)]

pub mod color;
pub mod context_menu;
pub mod geometry;
pub mod glyph;
pub mod icons;
pub mod panel;
pub mod png;
pub mod protocol;
pub mod theme;
pub mod window;

pub mod backend;
pub mod canvas;
pub mod client;
pub mod compositor;
pub mod deflate;
pub mod demo;
pub mod evdev;
pub mod fbdev;
pub mod font;
pub mod scenario;
pub mod server;
pub mod shm;
pub mod surface;

pub use backend::{Backend, BackendKind, HeadlessBackend, OutputBackend};
pub use canvas::Canvas;
pub use client::{Client, Welcome};
pub use color::Color;
pub use compositor::{
    AppKind, Compositor, DragState, FlyoutKind, HostEvent, MouseButton, WindowError,
};
pub use evdev::{EvdevInput, InputAction};
pub use fbdev::{FbFixInfo, FbLayout, FbVarInfo, FbdevBackend};
pub use geometry::{Point, Rect, Size};
pub use panel::{Panel, PanelHit, PanelIcon};
pub use protocol::{InputEvent, Message};
pub use scenario::ScenarioReport;
pub use server::{Server, ServerCommand, ServerControl, ServerHandle, ServerOptions};
pub use surface::{PixelFormat, Surface};
pub use theme::Theme;
pub use window::Window;
