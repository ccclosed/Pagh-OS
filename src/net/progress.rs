//! Однострочный прогресс-бар загрузки на месте для загрузчиков пакетов.
//!
//! И cleartext HTTP ([`super::http_fetch`]), и TLS ([`super::tls`]) циклы загрузки
//! вызывают [`show`] по мере прихода байтов тела и [`finish`] по окончании
//! передачи (успех или сбой). Строка перерисовывается на месте ведущим возвратом
//! каретки (`\r`) и **без** завершающего newline, поэтому перезаписывает себя, а
//! не прокручивается: framebuffer-консоль понимает `\r` (сбрасывает колонку) и
//! перекрашивает каждую глиф-клетку непрозрачно, host-serial-терминал делает то
//! же. [`finish`] печатает закрывающий newline ровно один раз и только если бар
//! реально рисовался, так что вызывающие могут звать его безусловно на каждом
//! пути выхода, не оставляя лишней пустой строки.
//!
//! Вывод идёт и в serial, и во framebuffer (как остальной консольный I/O ядра);
//! ANSI/escape-байты не эмитятся, только печатный ASCII плюс `\r`/`\n`, так что
//! bitmap-шрифт консоли (Fira Code 8×18, см. [`crate::drivers::framebuffer`])
//! рендерит это дословно.

#![allow(dead_code)]

use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering};

/// Видимая ширина в клетках бара `[####    ]`.
const BAR_WIDTH: usize = 28;

/// Нарисована ли сейчас строка прогресса (чтобы [`finish`] знал, что её надо
/// завершить). Загрузки сериализованы (одна за раз, ведомая одним сетевым
/// насосом), так что простого флага достаточно.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Форматировать число байт как короткую строку `B`/`KiB`/`MiB` с одним знаком
/// после точки. Только целые числа, panic-free. Локальная копия (держится
/// крошечной), чтобы низкоуровневый слой `net` не зависел от более высокого
/// модуля `pkg` ради форматирования.
fn human(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if n >= MIB {
        format!("{}.{} MiB", n / MIB, (n % MIB) * 10 / MIB)
    } else if n >= KIB {
        format!("{}.{} KiB", n / KIB, (n % KIB) * 10 / KIB)
    } else {
        format!("{} B", n)
    }
}

/// Построить строку прогресса для `received` байт из необязательного `total`
/// (разобранный `Content-Length`). При известном total рисуется заполненный бар
/// плюс процент и `recv / total`; до разбора головы (`None`) показывается только
/// текущий счётчик байт. Хвостовые пробелы добивают строку, чтобы более короткая
/// перерисовка полностью стёрла предыдущую длинную.
pub fn line(received: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => {
            let recv = received.min(total);
            let filled = (BAR_WIDTH as u64 * recv / total) as usize;
            let pct = 100 * recv / total;
            let mut bar = String::with_capacity(BAR_WIDTH);
            for i in 0..BAR_WIDTH {
                bar.push(if i < filled { '#' } else { ' ' });
            }
            format!(
                "  [{}] {:>3}%  {} / {}    ",
                bar,
                pct,
                human(recv),
                human(total)
            )
        }
        _ => format!("  downloading {} ...    ", human(received)),
    }
}

/// Перерисовать прогресс `text` на месте (ведущий `\r`, без newline) в serial и
/// framebuffer. Помечает бар активным, чтобы [`finish`] его завершил.
pub fn show(text: &str) {
    ACTIVE.store(true, Ordering::Relaxed);
    crate::kprint!("\r{}", text);
    // Пока зеркало лога framebuffer на паузе (фоновый provisioning), бар
    // остаётся только в serial — интерактивная консоль не перерисовывается
    // сотни раз фоновой загрузкой.
    if !crate::log::fb_mirror_paused() {
        crate::fb_print!("\r{}", text);
    }
}

/// Завершить текущую строку прогресса newline, если она рисовалась. Иначе no-op,
/// так что безопасно звать на путях и успеха, и сбоя.
pub fn finish() {
    if ACTIVE.swap(false, Ordering::Relaxed) {
        crate::kprintln!("");
        if !crate::log::fb_mirror_paused() {
            crate::fb_println!("");
        }
    }
}
