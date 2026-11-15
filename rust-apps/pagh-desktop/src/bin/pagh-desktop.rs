//! CLI desktop-сервера: AF_UNIX-сокет + headless backend.
//!
//! Пример:
//! ```text
//! cargo run --bin pagh-desktop -- --socket /tmp/pagh-desktop.sock --screenshot-dir /tmp/frames
//! ```

use pagh_desktop::demo;
use pagh_desktop::server::{InputChoice, LaunchTarget, OutputChoice, Server, ServerOptions};
use std::env;
use std::io;
use std::path::PathBuf;

fn main() {
    if let Err(err) = run() {
        eprintln!("pagh-desktop: {err}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let mut options = ServerOptions {
        backend: OutputChoice::Auto,
        input: InputChoice::Auto,
        ..ServerOptions::default()
    };
    let mut clock_now = false;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => options.socket = PathBuf::from(value(&mut args, "--socket")?),
            "--width" => options.width = parse_u32(value(&mut args, "--width")?, "--width")?,
            "--height" => options.height = parse_u32(value(&mut args, "--height")?, "--height")?,
            "--screenshot-dir" => {
                options.screenshot_dir = Some(PathBuf::from(value(&mut args, "--screenshot-dir")?))
            }
            "--max-frames" => {
                options.max_frames = Some(parse_u64(
                    value(&mut args, "--max-frames")?,
                    "--max-frames",
                )?)
            }
            "--backend" => {
                let value = value(&mut args, "--backend")?;
                options.backend = match value.as_str() {
                    "auto" => OutputChoice::Auto,
                    "headless" => OutputChoice::Headless,
                    "fbdev" => OutputChoice::Fbdev,
                    other => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("--backend принимает auto|headless|fbdev, получено {other:?}"),
                        ))
                    }
                };
            }
            "--fbdev" => options.fbdev_path = PathBuf::from(value(&mut args, "--fbdev")?),
            "--input" => {
                let value = value(&mut args, "--input")?;
                options.input = match value.as_str() {
                    "auto" => InputChoice::Auto,
                    "none" => InputChoice::None,
                    "evdev" => InputChoice::Evdev,
                    other => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("--input принимает auto|none|evdev, получено {other:?}"),
                        ))
                    }
                };
            }
            "--evdev" => options.evdev_path = PathBuf::from(value(&mut args, "--evdev")?),
            "--input-trace" => options.input_trace = true,
            "--full-render" => options.full_render = true,
            // Однокнопочный режим для QEMU e2e: auto backend/input, демо-окно,
            // ограниченное время жизни и selfcheck. Остальные флаги могут
            // переопределить значения после --e2e.
            "--e2e" => {
                options.backend = OutputChoice::Auto;
                options.input = InputChoice::Auto;
                options.demo_window = true;
                options.input_trace = true;
                // QEMU screendump читает поверхность дисплея; полный present
                // делает обновление детерминированным (production damage не
                // меняется, --full-render остаётся явным флагом).
                options.full_render = true;
                if options.exit_after_ms.is_none() {
                    options.exit_after_ms = Some(15_000);
                }
                if options.fbdev_selfcheck_ms.is_none() {
                    options.fbdev_selfcheck_ms = Some(3_000);
                }
            }
            "--exit-after-ms" => {
                options.exit_after_ms = Some(parse_u64(
                    value(&mut args, "--exit-after-ms")?,
                    "--exit-after-ms",
                )?)
            }
            "--selfcheck-ms" => {
                options.fbdev_selfcheck_ms = Some(parse_u64(
                    value(&mut args, "--selfcheck-ms")?,
                    "--selfcheck-ms",
                )?)
            }
            "--demo-window" => options.demo_window = true,
            "--spawn-demo" => options.spawn_demo = true,
            "--demo-binary" => {
                options.demo_binary = Some(PathBuf::from(value(&mut args, "--demo-binary")?))
            }
            "--launch-target" => {
                let spec = value(&mut args, "--launch-target")?;
                options.launch_targets.push(parse_launch_target(&spec)?);
            }
            "--clock" => {
                let value = value(&mut args, "--clock")?;
                match value.as_str() {
                    "now" => clock_now = true,
                    "fixed" => clock_now = false,
                    other => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("--clock принимает now|fixed, получено {other:?}"),
                        ))
                    }
                }
            }
            "--quiet" => options.quiet = true,
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("неизвестный аргумент {other:?} (см. --help)"),
                ))
            }
        }
    }

    let mut server = Server::bind(options)?;
    if clock_now {
        let (time, date) = demo::clock_now();
        server.compositor_mut().set_clock(time, date);
    }
    server.run()
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> io::Result<String> {
    args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("после {flag} требуется значение"),
        )
    })
}

fn parse_launch_target(spec: &str) -> io::Result<LaunchTarget> {
    let (app, command) = spec.split_once('=').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "--launch-target ожидает APP=PROGRAM[|ARG…]",
        )
    })?;
    if app.is_empty() || app.len() > 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--launch-target: пустой или слишком длинный app id",
        ));
    }
    let tokens: Vec<String> = command
        .split('|')
        .filter(|token| !token.is_empty())
        .map(|token| token.to_string())
        .collect();
    if tokens.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--launch-target: пустая команда",
        ));
    }
    Ok(LaunchTarget::new(app, tokens))
}

fn parse_u32(value: String, flag: &str) -> io::Result<u32> {
    value.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{flag}: ожидается число, получено {value:?}"),
        )
    })
}

fn parse_u64(value: String, flag: &str) -> io::Result<u64> {
    value.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{flag}: ожидается число, получено {value:?}"),
        )
    })
}

fn print_help() {
    println!(
        "pagh-desktop — userspace-композитор Pagh-OS (PDP/1)

Использование:
  pagh-desktop [--socket PATH] [--width N] [--height N]
               [--backend auto|headless|fbdev] [--fbdev PATH]
               [--input auto|none|evdev] [--evdev PATH] [--input-trace]
               [--screenshot-dir DIR] [--max-frames N] [--exit-after-ms N]
               [--selfcheck-ms N] [--demo-window] [--e2e] [--full-render]
               [--spawn-demo] [--demo-binary PATH]
               [--launch-target APP=PROGRAM[|ARG…]]
               [--clock now|fixed] [--quiet]

По умолчанию: /tmp/pagh-desktop.sock, backend=auto (fbdev, иначе headless),
input=auto (evdev при fbdev), часы 23:38 / 15.11.2026.
fbdev забирает экран у kernel-консоли (takeover односторонний, см.
docs/desktop-b.md); --input-trace печатает evdev-события в serial.
--e2e = auto backend/input + demo-окно + exit-after-ms 15000 + selfcheck 3000 +
input-trace + full-render (для QEMU-приёмки tools/e2e.py desktop).

--launch-target задаёт приложение для PDP/1 launch (type 13):
  --launch-target 'terminal=/usr/bin/pagh-terminal|--socket|{{socket}}'
Токен {{socket}} заменяется путём сокета; без него добавится --connect.
Протокол: PROTOCOL.md; демо-клиент: pagh-desktop-demo."
    );
}
