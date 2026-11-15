//! CLI desktop-сервера: AF_UNIX-сокет + headless backend.
//!
//! Пример:
//! ```text
//! cargo run --bin pagh-desktop -- --socket /tmp/pagh-desktop.sock --screenshot-dir /tmp/frames
//! ```

use pagh_desktop::demo;
use pagh_desktop::server::{LaunchTarget, Server, ServerOptions};
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
    let mut options = ServerOptions::default();
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
        "pagh-desktop — userspace-композитор Pagh-OS (PDP/1)\n\
         \n\
         Использование:\n\
           pagh-desktop [--socket PATH] [--width N] [--height N]\n\
                        [--screenshot-dir DIR] [--max-frames N]\n\
                        [--spawn-demo] [--demo-binary PATH]\n\
                        [--launch-target APP=PROGRAM[|ARG…]]\n\
                        [--clock now|fixed] [--quiet]\n\
         \n\
         По умолчанию: /tmp/pagh-desktop.sock, 1280x720, часы 23:38 / 15.11.2026,\n\
         headless-backend (PNG только при --screenshot-dir).\n\
         \n\
         --launch-target задаёт приложение для PDP/1 launch (type 13):\n\
           --launch-target 'terminal=/usr/bin/pagh-terminal|--socket|{{socket}}'\n\
         Токен {{socket}} заменяется путём сокета; без него добавится --connect.\n\
         Протокол: PROTOCOL.md; демо-клиент: pagh-desktop-demo."
    );
}
