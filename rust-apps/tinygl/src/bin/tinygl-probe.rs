// Derived from TinyGL 0.4.1 (c) 1997-2022 Fabrice Bellard, MIT — see LICENSE-TinyGL.
//! `tinygl-probe` — PR1 build-probe: детерминированный треугольник TinyGL и
//! ELF-самопроверка static-pie/`INTERP`.
//!
//! Печатает ровно те маркеры, на которые опирается приёмка PR1 (спека §5.2):
//!
//! ```text
//! TINYGL PROBE digest=0x3f7e5ece33386f9b center=0x003c4c7a corner=0x00000000 w=64 h=64 cc=<id>
//! TINYGL PROBE interp=none elf=static-pie
//! ```
//!
//! Сцена — эталон спеки: `fb[64*64]` (заполнен `0xAB`), `ZB_open(64,64,
//! ZB_MODE_RGBA, …, fb)`, `glClearColor(0,0,0,0)`, `glClear`, frustum
//! `(-1,1,-1,1,1,10)`, `glTranslatef(0,0,-3)`, RGB-треугольник
//! `(-1,-1) (1,-1) (0,1)`. Дайджест — [`tinygl::pixel_digest`] (FNV-1a 64 с
//! offset basis эталонной пробы). Любое расхождение — сигнал, что C-код
//! собран/связан не так, как эталон; PR1 в этом случае блокируется.

use std::fs;
use std::process::ExitCode;

use tinygl::{pixel_digest, Context, MatrixMode, Primitive, Surface};

const W: u32 = 64;
const H: u32 = 64;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let interp_only = args.iter().any(|arg| arg == "--check-interp");

    if !interp_only {
        let (digest, center, corner) = match render_probe() {
            Ok(result) => result,
            Err(err) => {
                eprintln!("tinygl-probe: {err}");
                return ExitCode::FAILURE;
            }
        };
        println!(
            "TINYGL PROBE digest=0x{digest:016x} center=0x{center:08x} corner=0x{corner:08x} w={W} h={H} cc={}",
            env!("TINYGL_CC_KIND")
        );
    }

    match inspect_self() {
        Ok(info) => {
            println!(
                "TINYGL PROBE interp={} elf={}",
                info.interp.as_deref().unwrap_or("none"),
                info.elf_kind()
            );
            // Критерий PR1 №2: у musl-бинаря не должно быть PT_INTERP.
            // Для host-сборки (gnu) динамический ELF — норма, гейт не применяем.
            let target = env!("TINYGL_TARGET");
            if target.contains("linux-musl") && info.interp.is_some() {
                eprintln!(
                    "tinygl-probe: FAIL: TARGET={target}, но в ELF есть PT_INTERP ({}); \
                     бинарь не static-pie",
                    info.interp.unwrap_or_default()
                );
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("tinygl-probe: не удалось разобрать /proc/self/exe: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Эталонная сцена PR1. Возвращает `(digest, center, corner)`; ошибка
/// содержит контекст вызова (сообщение печатается в `main`).
fn render_probe() -> Result<(u64, u32, u32), String> {
    let mut fb = vec![0xABu32; (W * H) as usize];
    let (digest, center, corner) = {
        let surface = Surface::new(&mut fb, W, H).map_err(|err| format!("Surface::new: {err}"))?;
        let ctx = Context::new(surface).map_err(|err| format!("Context::new: {err}"))?;

        ctx.set_viewport(0, 0, W, H)
            .map_err(|err| err.to_string())?;
        ctx.clear(0x0000_0000, 0);

        ctx.matrix_mode(MatrixMode::Projection);
        ctx.load_identity();
        ctx.frustum(-1.0, 1.0, -1.0, 1.0, 1.0, 10.0)
            .map_err(|err| err.to_string())?;
        ctx.matrix_mode(MatrixMode::ModelView);
        ctx.load_identity();
        ctx.translate(0.0, 0.0, -3.0)
            .map_err(|err| err.to_string())?;

        ctx.begin(Primitive::Triangles)
            .map_err(|err| err.to_string())?;
        ctx.color3f(1.0, 0.0, 0.0);
        ctx.vertex3f(-1.0, -1.0, 0.0);
        ctx.color3f(0.0, 1.0, 0.0);
        ctx.vertex3f(1.0, -1.0, 0.0);
        ctx.color3f(0.0, 0.0, 1.0);
        ctx.vertex3f(0.0, 1.0, 0.0);
        ctx.end().map_err(|err| err.to_string())?;

        (
            pixel_digest(ctx.pixels()),
            ctx.pixels()[(32 * W + 32) as usize],
            ctx.pixels()[0],
        )
    };
    // Буфер снова доступен: Context закрыл glClose + ZB_close.
    Ok((digest, center, corner))
}

struct ElfInfo {
    /// Содержимое `PT_INTERP` (None — статический ELF).
    interp: Option<String>,
    /// `e_type` ELF-заголовка.
    e_type: u16,
}

impl ElfInfo {
    fn elf_kind(&self) -> &'static str {
        match (self.e_type, self.interp.is_some()) {
            (3, false) => "static-pie",  // ET_DYN без интерпретатора
            (2, false) => "static-exec", // ET_EXEC без интерпретатора
            (_, true) => "dynamic",
            _ => "other",
        }
    }
}

/// Разобрать собственный ELF64 little-endian: `e_type` и `PT_INTERP`.
///
/// Гейт критерия №2 живёт здесь, а не в отдельном скрипте: проверяется
/// **финальный** бинарь (readelf-проверка в `tools/build-rust-app.sh` остаётся
/// внешним, не изменяемым этим PR рубежом).
fn inspect_self() -> Result<ElfInfo, String> {
    let data = fs::read("/proc/self/exe").map_err(|err| err.to_string())?;
    if data.len() < 64 || &data[0..4] != b"\x7fELF" {
        return Err("not an ELF file".to_string());
    }
    if data[4] != 2 {
        return Err("not ELF64".to_string());
    }
    if data[5] != 1 {
        return Err("not little-endian ELF".to_string());
    }

    let read_u16 = |offset: usize| -> Result<u16, String> {
        data.get(offset..offset + 2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .ok_or_else(|| format!("truncated ELF at {offset}"))
    };
    let read_u32 = |offset: usize| -> Result<u32, String> {
        data.get(offset..offset + 4)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            .ok_or_else(|| format!("truncated ELF at {offset}"))
    };
    let read_u64 = |offset: usize| -> Result<u64, String> {
        data.get(offset..offset + 8)
            .map(|bytes| {
                u64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ])
            })
            .ok_or_else(|| format!("truncated ELF at {offset}"))
    };

    let e_type = read_u16(16)?;
    let phoff = read_u64(32)? as usize;
    let phentsize = read_u16(54)? as usize;
    let phnum = read_u16(56)? as usize;
    if phentsize < 56 {
        return Err(format!("unexpected e_phentsize={phentsize}"));
    }

    let mut interp = None;
    for index in 0..phnum {
        let header = phoff + index * phentsize;
        let p_type = read_u32(header)?;
        if p_type != 3 {
            // PT_INTERP
            continue;
        }
        let p_offset = read_u64(header + 8)? as usize;
        let p_filesz = read_u64(header + 32)? as usize;
        let bytes = data
            .get(p_offset..p_offset.saturating_add(p_filesz))
            .ok_or_else(|| "PT_INTERP out of bounds".to_string())?;
        let text = bytes
            .iter()
            .copied()
            .take_while(|byte| *byte != 0)
            .map(char::from)
            .collect::<String>();
        interp = Some(text);
        break;
    }

    Ok(ElfInfo { interp, e_type })
}
