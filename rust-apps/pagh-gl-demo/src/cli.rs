//! Разбор CLI `pagh-gl-demo`.
//!
//! Спека §6.1: `pagh-gl-demo <triangle|cube|gears> [--connect sock]
//! [--frames N] [--size 640x480] [--out DIR] [--hash]`.
//! Сверх спеки добавлен `--lighting on|off` — он выбирает из двух путей
//! освещения, которые §6.1 требует для `gears` (по умолчанию `on`).

use std::path::PathBuf;

use crate::scenes::SceneKind;

pub const DEFAULT_WIDTH: u32 = 640;
pub const DEFAULT_HEIGHT: u32 = 480;
pub const MAX_DIM: u32 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub scene: SceneKind,
    /// `None` — headless (`Vec<u32>`), `Some` — живой PDP/1 по этому сокету.
    pub connect: Option<String>,
    /// Сколько кадров отрисовать; `0` — до закрытия окна (только live).
    pub frames: u32,
    pub width: u32,
    pub height: u32,
    /// Куда писать PPM-кадры (headless — все, live — финальный).
    pub out: Option<PathBuf>,
    /// Печатать `TINYGL`-строку с дайджестом для **каждого** кадра.
    pub hash: bool,
    /// `gears`: освещение on/off (для остальных сцен игнорируется).
    pub lighting: bool,
}

pub fn usage() -> &'static str {
    "usage: pagh-gl-demo <triangle|cube|gears> [options]\n\
     \n\
     options:\n\
       --connect <sock>     живой PDP/1 (pagh-desktop); без него — headless\n\
       --frames <N>         кадров (0 = до закрытия окна; headless: >= 1)\n\
       --size <WxH>         размер клиентской области (по умолчанию 640x480,\n\
                            W кратно 4)\n\
       --out <dir>          писать кадры как PPM (P6) в каталог\n\
       --hash               печатать дайджест каждого кадра\n\
       --lighting <on|off>  освещение gears (по умолчанию on)\n\
       -h, --help           эта справка"
}

/// Разобрать аргументы **без** имени программы (как `env::args().skip(1)`).
pub fn parse(args: &[String]) -> Result<Options, String> {
    let mut scene = None;
    let mut connect = None;
    let mut frames = None;
    let mut width = DEFAULT_WIDTH;
    let mut height = DEFAULT_HEIGHT;
    let mut out = None;
    let mut hash = false;
    let mut lighting = true;

    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-h" | "--help" => return Err(String::new()),
            "--hash" => hash = true,
            "--connect" => {
                index += 1;
                connect = Some(value(args, index, "--connect")?.to_string());
            }
            "--frames" => {
                index += 1;
                let raw = value(args, index, "--frames")?;
                frames =
                    Some(raw.parse::<u32>().map_err(|_| {
                        format!("--frames: ожидалось целое 0..=u32, получено `{raw}`")
                    })?);
            }
            "--size" => {
                index += 1;
                let raw = value(args, index, "--size")?;
                let (w, h) = parse_size(raw)?;
                width = w;
                height = h;
            }
            "--out" => {
                index += 1;
                out = Some(PathBuf::from(value(args, index, "--out")?));
            }
            "--lighting" => {
                index += 1;
                lighting = match value(args, index, "--lighting")? {
                    "on" | "1" => true,
                    "off" | "0" => false,
                    other => {
                        return Err(format!(
                            "--lighting: ожидалось `on` или `off`, получено `{other}`"
                        ))
                    }
                };
            }
            other if other.starts_with("--") => {
                return Err(format!("неизвестный флаг `{other}`"));
            }
            other => {
                if scene.is_some() {
                    return Err(format!("лишний позиционный аргумент `{other}`"));
                }
                scene =
                    Some(SceneKind::parse(other).ok_or_else(|| {
                        format!("неизвестная сцена `{other}` (triangle|cube|gears)")
                    })?);
            }
        }
        index += 1;
    }

    let scene = scene.ok_or_else(|| "не указана сцена".to_string())?;
    let frames = frames.unwrap_or(if connect.is_some() { 0 } else { 1 });
    if connect.is_none() && frames == 0 {
        return Err("--frames 0 имеет смысл только с --connect (headless нужен >= 1 кадр)".into());
    }
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return Err(format!("--size {width}x{height} вне 1..={MAX_DIM}"));
    }
    if width % 4 != 0 {
        return Err(format!(
            "--size: ширина {width} не кратна 4 (TinyGL молча срезал бы xsize & ~3)"
        ));
    }

    Ok(Options {
        scene,
        connect,
        frames,
        width,
        height,
        out,
        hash,
        lighting,
    })
}

fn value<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str, String> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("{flag}: не хватает значения"))
}

fn parse_size(raw: &str) -> Result<(u32, u32), String> {
    let (width, height) = raw
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("--size: ожидалось WxH, получено `{raw}`"))?;
    let width = width
        .parse::<u32>()
        .map_err(|_| format!("--size: ширина `{width}` — не целое"))?;
    let height = height
        .parse::<u32>()
        .map_err(|_| format!("--size: высота `{height}` — не целое"))?;
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> Result<Options, String> {
        parse(
            &args
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn headless_defaults_to_one_frame_at_640x480() {
        let options = opts(&["triangle"]).expect("triangle");
        assert_eq!(options.scene, SceneKind::Triangle);
        assert_eq!(options.frames, 1);
        assert_eq!((options.width, options.height), (640, 480));
        assert!(options.connect.is_none());
        assert!(options.lighting);
    }

    #[test]
    fn connect_defaults_to_infinite_frames() {
        let options = opts(&["gears", "--connect", "/tmp/pagh-desktop.sock"]).expect("gears");
        assert_eq!(options.frames, 0);
        assert_eq!(options.connect.as_deref(), Some("/tmp/pagh-desktop.sock"));
    }

    #[test]
    fn accepts_size_frames_out_hash_and_lighting() {
        let options = opts(&[
            "cube",
            "--frames",
            "7",
            "--size",
            "320x240",
            "--out",
            "/tmp/frames",
            "--hash",
            "--lighting",
            "off",
        ])
        .expect("all flags");
        assert_eq!(options.frames, 7);
        assert_eq!((options.width, options.height), (320, 240));
        assert_eq!(options.out, Some(PathBuf::from("/tmp/frames")));
        assert!(options.hash);
        assert!(!options.lighting);
    }

    #[test]
    fn rejects_bad_width_and_unknown_scene_and_flag() {
        assert!(opts(&["triangle", "--size", "641x480"]).is_err());
        assert!(opts(&["sphere"]).is_err());
        assert!(opts(&["cube", "--bogus"]).is_err());
        assert!(opts(&["cube", "--frames"]).is_err());
        assert!(opts(&[]).is_err());
    }

    #[test]
    fn frames_zero_requires_connect() {
        assert!(opts(&["cube", "--frames", "0"]).is_err());
        assert!(opts(&["cube", "--frames", "0", "--connect", "x"]).is_ok());
    }
}
