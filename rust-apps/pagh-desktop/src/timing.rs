//! Тайминг кадра: rdtsc, калибровка и агрегация по стадиям.
//!
//! Композитор — CPU-рендер, поэтому «сколько стоит кадр» измеряется счётчиком
//! тактов (`rdtsc`) вокруг стадий горячего пути. В userspace нет доступа к
//! kernel APIC, поэтому TSC калибруется против `CLOCK_MONOTONIC` (`Instant`) —
//! это тот же источник времени, что и тик ядра, только через libc/syscall.
//! Калибровка занимает ~60 мс один раз, при первом обращении к переводу тактов
//! в наносекунды; замеры хранятся в тактах (целочисленно) и переводятся только
//! при печати.
//!
//! Стадии вложены, а не дизъюнктны — это важно читать правильно:
//!
//! * `Render` ⊇ `Blend`: `render()` целиком, внутри него операции холста;
//! * `Present` ⊇ `Copy`: `backend.present()` целиком, внутри него blit scanout;
//! * `frame = render + present + прочее` (диспетчеризация, take damage).
//!
//! Поэтому `blend` и `copy` не складываются с `render`/`present`: это доли.
//!
//! Стоимость инструментирования: `Scope` — два `rdtsc` и один `lock xadd` на
//! *вызов* операции холста/blit (не на пиксель), сотни вызовов на кадр, то есть
//! доли процента бюджета кадра. Попиксельный `Canvas::put`/`blend` намеренно не
//! инструментирован: там счётчик стоил бы дороже полезной работы.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// Стадии горячего пути кадра.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Stage {
    Render = 0,
    Blend = 1,
    Copy = 2,
    Present = 3,
    /// Попиксельный проход тени окна: отдельная стадия, потому что в P0 она
    /// оказалась доминирующей и её нельзя размазывать по общему `Blend`.
    Shadow = 4,
}

/// Сколько стадий знает счётчик.
pub const STAGE_COUNT: usize = 5;

static CYCLES_PER_NS: OnceLock<f64> = OnceLock::new();

// Счётчики — глобальные атомарные, НЕ `thread_local!`.
//
// Почему не TLS: пер-поточный вариант (два `Cell<[u64; N]>`) добавляет 56 байт
// в `.tbss`, и гостевая сборка (static-pie musl под Pagh) после этого
// детерминированно падает в `__libc_free` на выходе клиента (`[EXC #14]`
// addr=0x100000035 RIP=0x60545, то есть free() богусного указателя). С
// TLS-объёмом как у baseline (0x80) всё чисто; это баг ядра/ABI, а не рендера,
// и он вне скоупа frame time — здесь просто не растим `.tbss`.
//
// Цена: счётчики общие на процесс. Горячий путь однопоточный (сервер рисует в
// одном потоке, bench — тоже), поэтому в бою это точно; параллельные тесты
// одного процесса делят счётчики, и тест бенчмарка не сравнивает стадии между
// собой (см. комментарий в `bench::tests`).
static STAGE_CYCLES: [AtomicU64; STAGE_COUNT] = [const { AtomicU64::new(0) }; STAGE_COUNT];
/// [непрозрачные, с альфа-смешиванием] пиксели текущего кадра.
static PIXELS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

#[inline]
fn bump_stage(slot: usize, cycles: u64) {
    STAGE_CYCLES[slot].fetch_add(cycles, Ordering::Relaxed);
}

#[inline]
fn bump_pixels(opaque: u64, blended: u64) {
    PIXELS[0].fetch_add(opaque, Ordering::Relaxed);
    PIXELS[1].fetch_add(blended, Ordering::Relaxed);
}

/// Счётчик тактов. На x86_64 — `rdtsc` (инвариантный TSC), иначе монотонное
/// время в наносекундах (чтобы модуль собирался и без x86).
#[inline]
pub fn now_cycles() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: `rdtsc` доступен на всём x86_64 (baseline) и не имеет
        // побочных эффектов в памяти; в ring 3 он читается без привилегий.
        unsafe { core::arch::x86_64::_rdtsc() }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }
}

/// Источник времени для отчётов: `rdtsc` или `Instant`.
pub fn clock_source() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        "rdtsc"
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        "instant"
    }
}

/// Тактов в наносекунду. Калибруется один раз (сумма трёх окон по 20 мс, чтобы
/// проигнорировать округление часов и джиттер планировщика).
pub fn cycles_per_ns() -> f64 {
    *CYCLES_PER_NS.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        {
            let mut cycles = 0u64;
            let mut nanos = 0u64;
            for _ in 0..3 {
                let c0 = now_cycles();
                let t0 = Instant::now();
                std::thread::sleep(std::time::Duration::from_millis(20));
                let t1 = Instant::now();
                let c1 = now_cycles();
                let elapsed = t1.duration_since(t0).as_nanos() as u64;
                if elapsed > 0 && c1 > c0 {
                    cycles += c1 - c0;
                    nanos += elapsed;
                }
            }
            if cycles == 0 || nanos == 0 {
                1.0
            } else {
                cycles as f64 / nanos as f64
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            1.0
        }
    })
}

/// Такты → миллисекунды.
pub fn cycles_to_ms(cycles: u64) -> f64 {
    cycles as f64 / cycles_per_ns() / 1_000_000.0
}

/// Наносекунды → миллисекунды (для замеров через `Instant`).
pub fn ns_to_ms(nanos: u64) -> f64 {
    nanos as f64 / 1_000_000.0
}

/// RAII-замер: в `Drop` добавляет такты в счётчик стадии.
///
/// `Drop` (а не явный `end()`) выбран специально: в горячих функциях есть
/// ранние `return`, и замер не должен теряться вместе с ними.
pub struct Scope {
    start: u64,
    slot: usize,
}

impl Scope {
    #[inline]
    pub fn new(stage: Stage) -> Self {
        Self {
            start: now_cycles(),
            slot: stage as usize,
        }
    }

    /// Явно закрыть замер (иначе это сделает `Drop`).
    #[inline]
    fn finish(&mut self) {
        if self.slot == usize::MAX {
            return;
        }
        let end = now_cycles();
        let slot = self.slot;
        self.slot = usize::MAX;
        if end > self.start {
            bump_stage(slot, end - self.start);
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Учесть пиксели, записанные непрозрачным путём.
#[inline]
pub fn count_opaque(pixels: u64) {
    bump_pixels(pixels, 0);
}

/// Учесть пиксели, прошедшие альфа-смешивание.
#[inline]
pub fn count_blended(pixels: u64) {
    bump_pixels(0, pixels);
}

/// Снимок счётчиков за один кадр (такты и пиксели).
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStages {
    pub render: u64,
    pub blend: u64,
    pub copy: u64,
    pub present: u64,
    pub shadow: u64,
    pub opaque_px: u64,
    pub blended_px: u64,
}

impl FrameStages {
    pub fn render_ms(&self) -> f64 {
        cycles_to_ms(self.render)
    }
    pub fn blend_ms(&self) -> f64 {
        cycles_to_ms(self.blend)
    }
    pub fn copy_ms(&self) -> f64 {
        cycles_to_ms(self.copy)
    }
    pub fn present_ms(&self) -> f64 {
        cycles_to_ms(self.present)
    }
    /// Тень окна — подмножество `blend`/`render`, но измеряется отдельно.
    pub fn shadow_ms(&self) -> f64 {
        cycles_to_ms(self.shadow)
    }
    /// Доля непрозрачных пикселей среди всех записанных (0..=1).
    pub fn opaque_share(&self) -> f64 {
        let total = self.opaque_px + self.blended_px;
        if total == 0 {
            0.0
        } else {
            self.opaque_px as f64 / total as f64
        }
    }
}

/// Прочитать и обнулить счётчики стадий (граница кадра).
pub fn take_frame() -> FrameStages {
    let stages = [
        STAGE_CYCLES[0].swap(0, Ordering::Relaxed),
        STAGE_CYCLES[1].swap(0, Ordering::Relaxed),
        STAGE_CYCLES[2].swap(0, Ordering::Relaxed),
        STAGE_CYCLES[3].swap(0, Ordering::Relaxed),
        STAGE_CYCLES[4].swap(0, Ordering::Relaxed),
    ];
    let pixels = [
        PIXELS[0].swap(0, Ordering::Relaxed),
        PIXELS[1].swap(0, Ordering::Relaxed),
    ];
    FrameStages {
        render: stages[Stage::Render as usize],
        blend: stages[Stage::Blend as usize],
        copy: stages[Stage::Copy as usize],
        present: stages[Stage::Present as usize],
        shadow: stages[Stage::Shadow as usize],
        opaque_px: pixels[0],
        blended_px: pixels[1],
    }
}

/// Обнулить счётчики, не читая их.
pub fn reset_frame() {
    for counter in &STAGE_CYCLES {
        counter.store(0, Ordering::Relaxed);
    }
    for counter in &PIXELS {
        counter.store(0, Ordering::Relaxed);
    }
}

/// Агрегация выборки (миллисекунды): avg / p50 / p95 / max.
#[derive(Clone, Debug, Default)]
pub struct Samples {
    values: Vec<f64>,
}

impl Samples {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            values: Vec::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, value: f64) {
        self.values.push(value);
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn avg(&self) -> f64 {
        if self.values.is_empty() {
            return 0.0;
        }
        self.values.iter().sum::<f64>() / self.values.len() as f64
    }

    pub fn max(&self) -> f64 {
        self.values.iter().copied().fold(0.0_f64, f64::max)
    }

    /// Перцентиль по ближайшему рангу (0..=1); p50 = медиана.
    pub fn percentile(&self, q: f64) -> f64 {
        if self.values.is_empty() {
            return 0.0;
        }
        let mut sorted = self.values.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let last = sorted.len() - 1;
        let index = ((q.clamp(0.0, 1.0) * last as f64).floor() as usize).min(last);
        sorted[index]
    }

    pub fn p50(&self) -> f64 {
        self.percentile(0.50)
    }

    pub fn p95(&self) -> f64 {
        self.percentile(0.95)
    }

    /// Стереть выборку, сохранив ёмкость.
    pub fn clear(&mut self) {
        self.values.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_and_averages_are_sane() {
        let mut samples = Samples::with_capacity(100);
        for value in 1..=100 {
            samples.push(value as f64);
        }
        assert_eq!(samples.len(), 100);
        assert!((samples.avg() - 50.5).abs() < 1e-9);
        assert_eq!(samples.percentile(0.5), 50.0);
        assert!(samples.percentile(0.95) >= 95.0);
        assert_eq!(samples.max(), 100.0);
        assert_eq!(samples.percentile(0.0), 1.0);
        assert_eq!(samples.percentile(1.0), 100.0);
    }

    #[test]
    fn empty_samples_are_zero() {
        let samples = Samples::with_capacity(1);
        assert!(samples.is_empty());
        assert_eq!(samples.avg(), 0.0);
        assert_eq!(samples.p50(), 0.0);
        assert_eq!(samples.p95(), 0.0);
    }

    #[test]
    fn calibration_is_positive_and_stable() {
        let cpns = cycles_per_ns();
        assert!(cpns > 0.0, "cycles_per_ns должен быть positive: {cpns}");
        // Инвариантный TSC на нормальном CPU — от 0.5 до 10 тактов на нс.
        assert!(
            cpns >= 0.1 && cpns <= 20.0,
            "неправдоподобная калибровка: {cpns}"
        );
        assert_eq!(cycles_per_ns(), cpns, "калибровка кэшируется");
        assert!(cycles_to_ms(0) == 0.0);
    }

    #[test]
    fn scope_accumulates_into_its_stage() {
        reset_frame();
        {
            let _render = Scope::new(Stage::Render);
            let _blend = Scope::new(Stage::Blend);
        }
        let stages = take_frame();
        assert!(stages.render > 0, "render-спан должен накопить такты");
        assert!(stages.blend > 0, "blend-спан должен накопить такты");
        assert_eq!(take_frame().render, 0, "take_frame обнуляет счётчик");
    }

    #[test]
    fn pixel_counters_split_opaque_and_blended() {
        reset_frame();
        count_opaque(10);
        count_blended(30);
        let stages = take_frame();
        assert_eq!(stages.opaque_px, 10);
        assert_eq!(stages.blended_px, 30);
        assert!((stages.opaque_share() - 0.25).abs() < 1e-9);
    }
}
