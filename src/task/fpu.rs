// task/fpu.rs — FPU/SSE-состояние на задачу (FXSAVE/FXRSTOR).
//
// Фрейм переключения контекста (см. `task::switch`) несёт только GPR и RFLAGS;
// регистровый файл x87/SSE (ST/MM, XMM, MXCSR, FCW/FSW) архитектурно отделён и
// путями pushfq/iretq НЕ сохраняется. Пользовательский код (glibc, cpython)
// постоянно использует SSE, поэтому две FP-задачи, ротируясь на CPU, иначе
// портили бы XMM-регистры друг друга при каждом вытеснении.
//
// Ядро собрано без FP-кодогенерации (подмножество target features `-soft-float`)
// и никогда не исполняет FP/SSE-инструкции, поэтому переключениям ядро→ядро
// сохранение не нужно. Жадная политика:
//
//   * каждая задача получает 512-байтную, выровненную на 64 байта, FXSAVE-область
//     при spawn;
//   * когда планировщик снимает задачу, чей сохранённый фрейм — ring-3 ИЛИ у
//     которой есть Linux-compat-состояние (compat-задачу могут вытеснить или она
//     уступит CPU внутри syscall, и её пользовательское FP-состояние живо), FP-
//     состояние CPU делается FXSAVE в область этой задачи;
//   * когда планировщик запускает такую задачу, состояние восстанавливается
//     FXRSTOR (область никогда не запускавшейся задачи заранее заполнена образом
//     сброса FINIT, поэтому первое восстановление даёт чистый FPU).
//
// CR0/CR4 настраиваются один раз при загрузке `arch::cpu::enable_sse` (EM=0,
// MP=1, OSFXSR=1, OSXMMEXCPT=1); машинерия TS/lazy-switch не нужна.

use crate::sync::spinlock::Spinlock;
use alloc::collections::BTreeMap;

/// Размер образа FXSAVE/FXRSTOR (x87 + SSE, 64-битный режим).
const FXSAVE_SIZE: usize = 512;
/// Образы FXSAVE должны быть выровнены на 16 байт; берём 64 для надёжности (и
/// дружелюбно к кэш-линии).
const FXSAVE_ALIGN: usize = 64;

/// Значение сброса control word x87: все исключения замаскированы, расширенная точность.
const FCW_INIT: u16 = 0x037F;
/// Значение сброса MXCSR: все исключения замаскированы, округление к ближайшему.
const MXCSR_INIT: u32 = 0x1F80;

/// FXSAVE-область на задачу. `#[repr(align(64))]` сохраняет выравнивание
/// внутреннего буфера независимо от того, где лежит значение (или указатель).
#[repr(C, align(64))]
pub struct FxArea([u8; FXSAVE_SIZE]);

impl FxArea {
    /// Заполнить область образом сброса FPU (что оставляют FINIT + LDMXCSR).
    /// FXRSTOR из этого образа даёт чистый FPU со всеми замаскированными
    /// исключениями.
    fn init_image(&mut self) {
        self.0 = [0u8; FXSAVE_SIZE];
        self.0[0..2].copy_from_slice(&FCW_INIT.to_le_bytes());
        self.0[24..28].copy_from_slice(&MXCSR_INIT.to_le_bytes());
    }
}

/// Выделить FX-область, обнулённую и заполненную образом сброса, напрямую из
/// глобального аллокатора (не через `Box`), чтобы выравнивание на 64 было явным.
fn alloc_area() -> *mut FxArea {
    use alloc::alloc::{alloc_zeroed, handle_alloc_error, Layout};
    let layout = Layout::from_size_align(FXSAVE_SIZE, FXSAVE_ALIGN).expect("FxArea layout");
    // SAFETY: layout ненулевого размера; возвращённый указатель выровнен и
    // валиден для чтения/записи `FXSAVE_SIZE` байт на всё время жизни программы
    // (области освобождает только exit-reaper, после того как задачу больше
    // нельзя запланировать).
    let ptr = unsafe { alloc_zeroed(layout) };
    if ptr.is_null() {
        handle_alloc_error(layout);
    }
    let area = unsafe { &mut *(ptr as *mut FxArea) };
    area.init_image();
    ptr as *mut FxArea
}

/// Освободить область, ранее возвращённую [`alloc_area`] (тот же layout).
fn free_area(ptr: *mut FxArea) {
    use alloc::alloc::{dealloc, Layout};
    let layout = Layout::from_size_align(FXSAVE_SIZE, FXSAVE_ALIGN).expect("FxArea layout");
    // SAFETY: `ptr` получен из `alloc_area` с этим точным layout, и вызывающий
    // гарантирует, что дальше он не используется.
    unsafe { dealloc(ptr as *mut u8, layout) }
}

/// Владеющий указатель на FXSAVE-область задачи (Send-обёртка, чтобы жить в
/// глобальной карте; ядро однопроцессорное, весь доступ под локом).
struct AreaPtr(*mut FxArea);
unsafe impl Send for AreaPtr {}

/// FXSAVE-области по pid, выделяются `scheduler::spawn`, освобождаются
/// exit-reaper'ом. Лок держится только на поиск/вставку/удаление в карте.
static FP_AREAS: Spinlock<BTreeMap<u64, AreaPtr>> = Spinlock::new(BTreeMap::new());

/// Убедиться, что у `pid` есть FXSAVE-область. Вызывается из `scheduler::spawn`
/// (контекст задачи, возможно с выключенными прерываниями — никогда из IRQ).
///
/// Идёт с замаскированными прерываниями: пути save/restore берут тот же лок из
/// тика таймера (`save_if_user`/`restore_if_user`), поэтому IRQ, пришедший сюда
/// на удержанном локе, заблокировал бы (единственное) ядро.
pub fn area_for(pid: u64) {
    crate::arch::cpu::without_interrupts(|| {
        let mut areas = FP_AREAS.lock();
        if !areas.contains_key(&pid) {
            areas.insert(pid, AreaPtr(alloc_area()));
        }
    });
}

/// Освободить FXSAVE-область `pid`. Вызывается exit-reaper'ом после того, как
/// задача ушла из ротации.
pub fn free(pid: u64) {
    if let Some(area) = FP_AREAS.lock().remove(&pid) {
        free_area(area.0);
    }
}

/// Принадлежит ли сохранённый фрейм по `rsp` (каноническая раскладка, CS по
/// +136) задаче, которая может держать пользовательское FP-состояние в CPU?
///
/// Ring-3 фреймы (CS RPL=3) — сразу да. Фреймы yield всегда несут ядерный CS,
/// даже когда уступающая задача — Linux-compat-процесс, прерванный внутри
/// syscall; для них решает compat-реестр. Ядерные потоки не имеют
/// compat-состояния и имеют ядерный CS: нет.
fn frame_uses_fp(pid: u64, rsp: u64) -> bool {
    if rsp == 0 || (rsp as i64) < 0 || rsp < 0xffff_8000_0000_0000 {
        return false;
    }
    // SAFETY: фрейм живёт в канонической верхней половине; чтение повторяет
    // проверку того же места в `scheduler::check_frame`.
    let cs = unsafe { core::ptr::read_volatile((rsp + 136) as *const u64) };
    (cs & 3) == 3 || crate::task::compat::compat_exists(pid)
}

/// Сохранить FP-состояние CPU в область `pid`, если задача может держать
/// пользовательское FP-состояние (см. [`frame_uses_fp`]). Вызывается
/// планировщиком при остановке задачи (тик таймера или кооперативная уступка).
pub fn save_if_user(pid: u64, rsp: u64) {
    if !frame_uses_fp(pid, rsp) {
        return;
    }
    let ptr = {
        let areas = FP_AREAS.lock();
        match areas.get(&pid) {
            Some(area) => area.0,
            None => {
                crate::warn!("[FPU] no FXSAVE area for user pid={} (state lost)", pid);
                return;
            }
        }
    };
    // SAFETY: `ptr` указывает на выровненную на 64 байта 512-байтную область
    // задачи (запись карты держит её живой); FXSAVE пишет только эту память и
    // читает только регистровый файл FPU CPU.
    unsafe {
        core::arch::asm!("fxsave [{0}]", in(reg) ptr, options(nostack));
    }
}

/// Восстановить FP-состояние `pid` в CPU, если задача может держать
/// пользовательское FP-состояние. Вызывается планировщиком прямо перед
/// возобновлением задачи (её фрейм по `rsp`).
pub fn restore_if_user(pid: u64, rsp: u64) {
    if !frame_uses_fp(pid, rsp) {
        return;
    }
    let ptr = {
        let areas = FP_AREAS.lock();
        match areas.get(&pid) {
            Some(area) => area.0,
            None => {
                crate::warn!(
                    "[FPU] no FXSAVE area for user pid={} (restore skipped)",
                    pid
                );
                return;
            }
        }
    };
    // SAFETY: `ptr` указывает на область задачи с ранее сохранённым FXSAVE-
    // образом (или образом сброса, записанным при аллокации) — валидным входом
    // FXRSTOR; загрузка пишет только регистровый файл FPU CPU и читает только
    // 512-байтный буфер.
    unsafe {
        core::arch::asm!("fxrstor [{0}]", in(reg) ptr, options(nostack));
    }
}
