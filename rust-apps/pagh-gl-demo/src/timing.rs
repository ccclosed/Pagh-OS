//! Замеры кадра: wall-clock (`Instant`) и циклы (`rdtsc`).
//!
//! `Instant` участвует **только** в замерах: логика кадра — это счётчик
//! кадров, а не время (спека §6.2). В QEMU TCG `rdtsc` виртуален, поэтому
//! абсолютные числа честны лишь внутри одной конфигурации QEMU; отчёт печатает
//! оба ряда и называет это верхней границей, а не бенчмарком GPU.

use std::time::{Duration, Instant};

/// `rdtsc` — счётчик тактов хоста/гостя. Только x86_64 (у Pagh-OS и host-тестов
/// другой архитектуры нет).
pub fn now_cycles() -> u64 {
    // SAFETY: rdtsc доступен в ring 3 на x86_64 и не трогает память; побочный
    // эффект — только чтение счётчика.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Один ряд измерений.
#[derive(Debug, Default, Clone)]
pub struct Series {
    values: Vec<f64>,
}

impl Series {
    pub fn push(&mut self, value: f64) {
        self.values.push(value);
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// `min/median/max`; пустой ряд — нули.
    pub fn summary(&self) -> (f64, f64, f64) {
        if self.values.is_empty() {
            return (0.0, 0.0, 0.0);
        }
        let mut sorted = self.values.clone();
        sorted.sort_by(f64::total_cmp);
        let min = sorted[0];
        let max = sorted[sorted.len() - 1];
        let median = if sorted.len() % 2 == 1 {
            sorted[sorted.len() / 2]
        } else {
            let mid = sorted.len() / 2;
            (sorted[mid - 1] + sorted[mid]) / 2.0
        };
        (min, median, max)
    }

    /// Строка `min/med/max` с фиксированной точностью.
    pub fn format_ms(&self) -> String {
        let (min, med, max) = self.summary();
        format!("{min:.3}/{med:.3}/{max:.3}")
    }
}

/// Измерение одного кадра: wall-clock и циклы вокруг одной операции.
#[derive(Debug)]
pub struct Stopwatch {
    start: Instant,
    cycles: u64,
}

impl Stopwatch {
    pub fn start() -> Stopwatch {
        Stopwatch {
            start: Instant::now(),
            cycles: now_cycles(),
        }
    }

    /// Вернуть `(wall, cycles)` с момента старта.
    pub fn stop(&self) -> (Duration, u64) {
        (
            self.start.elapsed(),
            now_cycles().saturating_sub(self.cycles),
        )
    }
}
