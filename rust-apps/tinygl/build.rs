// Derived from TinyGL 0.4.1 (c) 1997-2022 Fabrice Bellard, MIT — see LICENSE-TinyGL.
//! Build script **только этого крейта** (PR1 build-probe).
//!
//! Что делает:
//! 1. Компилирует 23 C-файла upstream TinyGL 0.4.1 (`c_src/`) в статическую
//!    библиотеку для `$TARGET` с `-O2 -fno-strict-aliasing`
//!    `-Wno-implicit-fallthrough` (три `implicit-fallthrough` в `get.c`).
//! 2. Для `*-linux-musl` ищет C-компилятор в фиксированном порядке:
//!    `CC_<target>` → `<target>-gcc` → `musl-gcc` → `clang --target=<target>`
//!    (+`--sysroot`, если задан `TINYGL_MUSL_SYSROOT`). Если ничего не найдено —
//!    падает с **инструкцией** (`musl-tools` / clang+sysroot), а не с
//!    невнятным «cc not found». Это и есть суть PR1: выяснить, заводится ли
//!    C-тулчейн, до интеграции с композитором.
//!
//! Чего НЕ делает: не трогает системную сборку. `tools/build-rust-app.sh`
//! остаётся rustup+cargo; C-компилятор нужен только крейту `tinygl`.
//! static-pie/`INTERP`-гейт финального бинаря — в `tinygl-probe` (ELF-разбор
//! собственного `/proc/self/exe`) и, снаружи, в `readelf`-проверке
//! `tools/build-rust-app.sh` (не меняется этим PR).

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 23 файла ядра upstream 0.4.1: все `src/*.c`, кроме X11/NanoX/BeOS-обвязки
/// (`glx.c`, `nglx.c`, `oscontext.c`; каталог `BeOS/` не вендорится).
const CORE_SOURCES: &[&str] = &[
    "api",
    "arrays",
    "clear",
    "clip",
    "error",
    "get",
    "image_util",
    "init",
    "light",
    "list",
    "matrix",
    "memory",
    "misc",
    "msghandling",
    "select",
    "specbuf",
    "texture",
    "vertex",
    "zbuffer",
    "zdither",
    "zline",
    "zmath",
    "ztriangle",
];

fn main() {
    println!("cargo:rerun-if-changed=c_src/src");
    println!("cargo:rerun-if-changed=c_src/include/GL/gl.h");
    // cc-rs читает CC_<target> сам; объявляем зависимости, чтобы cargo
    // перезапускал build.rs при смене компилятора.
    for name in [
        "CC_x86_64_unknown_linux_musl",
        "CC_x86_64-unknown-linux-musl",
        "TINYGL_MUSL_SYSROOT",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }

    let target = env::var("TARGET").expect("cargo always sets TARGET");
    let musl = target.ends_with("-linux-musl");

    let mut build = cc::Build::new();
    for source in CORE_SOURCES {
        build.file(format!("c_src/src/{source}.c"));
    }
    build
        .include("c_src/include")
        .include("c_src/src")
        .opt_level(2)
        .flag("-fno-strict-aliasing")
        .flag_if_supported("-Wno-implicit-fallthrough")
        .warnings(true);

    let kind = if musl {
        detect_musl_cc(&target, &mut build)
    } else {
        let compiler = env::var("CC")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "cc".to_string());
        if find_tool(&compiler).is_none() {
            panic!(
                "tinygl: не найден C-компилятор `{compiler}` для TARGET={target}.\n\
                 Установите gcc/clang (host-сборка): sudo apt install build-essential"
            );
        }
        format!("host:{compiler}")
    };

    build.compile("tinygl_c");
    println!("cargo:rustc-env=TINYGL_CC_KIND={kind}");
    println!("cargo:rustc-env=TINYGL_TARGET={target}");
    if musl {
        // На musl видно, какой именно компилятор выбран: это доказательство
        // работы детекции в логе сборки.
        println!("cargo:warning=tinygl: C compiler for {target}: {kind}");
    } else {
        // libm нужна только glibc-хосту: в musl математика уже в libc.a,
        // а `-lm` в self-contained musl-таргете рискует не найти файл.
        println!("cargo:rustc-link-lib=m");
    }
}

/// Детекция C-компилятора для musl-таргета. Возвращает строку `cc=<id>`,
/// которую печатает `tinygl-probe`.
fn detect_musl_cc(target: &str, build: &mut cc::Build) -> String {
    // (1) CC_<target> — cc-rs подхватит сам; поддерживаем оба написания.
    let under = target.replace('-', "_");
    for var in [format!("CC_{target}"), format!("CC_{under}")] {
        if let Ok(value) = env::var(&var) {
            if !value.trim().is_empty() {
                return format!("env:{value}");
            }
        }
    }

    // (2) канонический cross-gcc для этого таргета.
    let target_gcc = format!("{target}-gcc");
    if let Some(path) = find_tool(&target_gcc) {
        build.compiler(&path);
        return format!("{target_gcc}:{}", path.display());
    }

    // (3) musl-gcc (Debian/Ubuntu `musl-tools`, Fedora `musl-gcc`).
    if let Some(path) = find_tool("musl-gcc") {
        build.compiler(&path);
        return format!("musl-gcc:{}", path.display());
    }

    // (4) clang + `--target`; sysroot — только если задан.
    if let Some(path) = find_tool("clang") {
        build.compiler(&path);
        build.flag(format!("--target={target}"));
        if let Ok(sysroot) = env::var("TINYGL_MUSL_SYSROOT") {
            if !sysroot.trim().is_empty() {
                build.flag(format!("--sysroot={sysroot}"));
            }
        }
        return format!("clang:{}", path.display());
    }

    panic!(
        "tinygl: не найден C-компилятор для TARGET={target}.\n\
         Проверены (по порядку):\n\
           1. CC_{target} / CC_{under}\n\
           2. {target}-gcc\n\
           3. musl-gcc\n\
           4. clang --target={target}\n\
         \n\
         Установите один из вариантов:\n\
           Debian/Ubuntu: sudo apt install musl-tools          # даст musl-gcc\n\
           Fedora:        sudo dnf install musl-gcc\n\
           clang+sysroot: sudo apt install clang musl-dev\n\
                          export TINYGL_MUSL_SYSROOT=/usr/x86_64-linux-musl\n\
         \n\
         Host-сборка/тесты musl-C не требуют: `cargo test` (target gnu).\n\
         PR1 специально проверяет наличие этого тулчейна: без него musl-\n\
         критерии (сборка C под musl, static-pie/INTERP) не подтверждены, и\n\
         это честный остаток, а не «собралось». НЕ подменяйте musl-сборку\n\
         host-gcc: ABI-объекты glibc-хедера не доказывают musl-сборку."
    );
}

/// Найти исполняемый файл в PATH (или вернуть путь как есть, если он с `/`).
fn find_tool(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return is_executable(path).then(|| path.to_path_buf());
    }
    let paths = env::var_os("PATH")?;
    for dir in env::split_paths(&paths) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Проверка «файл исполняемый» без внешних зависимостей: пробуем `--version`
/// через `Command`, чтобы не тащить `libc`/`std::os::unix` в build-скрипт.
fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    Command::new(path)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
