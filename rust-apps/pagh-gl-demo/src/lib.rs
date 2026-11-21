//! `pagh-gl-demo` — CPU-демо TinyGL для Pagh-OS (PR5 спеки
//! `docs/tinygl-port-spec.md`).
//!
//! Три сцены (`triangle`, `cube`, `gears`) рендерятся **прямо** в
//! `mmap(MAP_SHARED)` memfd-поверхность через `tinygl::Context` (upstream C
//! 0.4.1 + thin FFI, MIT). Клиент шлёт композитору только `COMMIT`+`DAMAGE`
//! (PDP/1 `ShmSurface`), без копии кадра; без `--connect` — headless-рендер в
//! `Vec<u32>` с FNV-1a-дайджестом.
//!
//! # Честные ограничения
//!
//! * CPU-only, fixed-function GL 1.1 subset; **не** GPU/NVIDIA/OpenGL 2+,
//!   без shaders/GLSL/FBO/VBO/EGL/GLX/Wayland.
//! * 16-битный Z, `stride == width*4`, `width % 4 == 0`; `glClear` заливает Z
//!   нулём независимо от `clear_depth` (upstream).
//! * один `Context` на процесс (`gl_ctx` глобальный).
//! * нет full-frame fallback: без `welcome.flags` bit 2 (`ShmSurface`) демо
//!   падает с внятной ошибкой (спека §4.5).
//! * `gl_fatal_error` upstream — `exit(1)`; наш слой отвергает известные
//!   опасные входы, но внутренний fatal upstream остаётся смертью процесса.
//!
//! # Сборка/тесты
//!
//! ```text
//! cd rust-apps/pagh-gl-demo
//! cargo test                                   # host: сцены, дайджесты, CLI
//! cargo run --bin pagh-gl-demo -- triangle --hash
//! cargo build --release --target x86_64-unknown-linux-musl   # static-pie
//! ```
//!
//! Musl-сборке нужен C-компилятор для `x86_64-unknown-linux-musl`; его ищет
//! `rust-apps/tinygl/build.rs` (`CC_<target>` → `<target>-gcc` → `musl-gcc` →
//! `clang --target=...`).

pub mod cli;
pub mod client;
pub mod render;
pub mod scenes;
pub mod surface;
pub mod timing;

pub use render::{render_headless, GlRenderer, HeadlessFrames};
pub use scenes::SceneKind;
pub use tinygl::pixel_digest;

/// Версия демо (не версия ядра/`pagh`; MINOR бампается только в PR6).
pub const DEMO_VERSION: &str = env!("CARGO_PKG_VERSION");
