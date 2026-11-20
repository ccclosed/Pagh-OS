//! PR1: проверка собранного бинаря `tinygl-probe`.
//!
//! Это не unit-тест обёртки, а проверка **артефакта**: бинарь должен печатать
//! эталонный дайджест спеки и корректный ELF-класс (`interp`/`elf`). На musl
//! тот же код дополнительно требует отсутствия `PT_INTERP` (критерий №2);
//! на host-gnu динамический ELF — норма.

use std::process::Command;

const REFERENCE: &str =
    "TINYGL PROBE digest=0x3f7e5ece33386f9b center=0x003c4c7a corner=0x00000000";

#[test]
fn probe_binary_prints_reference_digest() {
    let output = Command::new(env!("CARGO_BIN_EXE_tinygl-probe"))
        .output()
        .expect("tinygl-probe must be built for integration tests");
    assert!(
        output.status.success(),
        "tinygl-probe failed: status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(REFERENCE),
        "expected `{REFERENCE}` in probe output, got:\n{stdout}"
    );
    assert!(
        stdout.contains("TINYGL PROBE interp="),
        "probe must report PT_INTERP/ELF class, got:\n{stdout}"
    );
    // cc=<id> — доказательство работы детекции C-компилятора (build.rs).
    assert!(
        stdout.contains(" cc="),
        "probe must report the detected C compiler, got:\n{stdout}"
    );
}

#[test]
fn probe_interp_check_mode_is_consistent() {
    let exe = env!("CARGO_BIN_EXE_tinygl-probe");
    let output = Command::new(exe)
        .arg("--check-interp")
        .output()
        .expect("tinygl-probe must be built for integration tests");
    assert!(
        output.status.success(),
        "--check-interp failed: status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("TINYGL PROBE interp="),
        "--check-interp must print the ELF class, got:\n{stdout}"
    );
    assert!(
        stdout.contains("elf=static-pie")
            || stdout.contains("elf=static-exec")
            || stdout.contains("elf=dynamic"),
        "--check-interp must classify the ELF, got:\n{stdout}"
    );
}
