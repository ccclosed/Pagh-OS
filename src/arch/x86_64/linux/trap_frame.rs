//! Чистое зеркало фрейма, который `task::switch::irq32_stub` оставляет на стеке
//! ядра, плюс байтовый план доставки сигнала с пути возврата тика таймера.
//!
//! ## Зачем модуль (и почему приведение к `SavedRegs` запрещено)
//!
//! Обработчик тика получает `current_rsp` — RSP сразу после того, как
//! `irq32_stub` положил свой фрейм:
//!
//! ```text
//!   [rsp+0]    RFLAGS, снимаемый `popfq` (0x002: IF замаскирован до `iretq`)
//!   [+8..+127] 15 GPR в порядке полей SavedRegs: r15@+8 … rax@+120
//!   [+128]     RIP     (прерванная инструкция)
//!   [+136]     CS      (RPL 3 = задача была в ring 3, когда её прервали)
//!   [+144]     RFLAGS  (прерванный пользовательский RFLAGS)
//!   [+152]     RSP     (прерванный пользовательский RSP)
//!   [+160]     SS
//! ```
//!
//! Слово по `+120` — это `rax`, а НЕ per-task слот user-RSP syscall-фрейма,
//! который живёт по `SavedRegs + 120` (то есть `+128` здесь). Приведение
//! `(current_rsp + 8) as *mut SavedRegs` и вызов syscall-пути доставки прочитали
//! бы пользовательский `rax` как указатель на пользовательский стек, записали бы
//! базу фрейма в `rax` и переписали `rcx`/`r11` — которые `iretq`, в отличие от
//! `sysretq`, никогда не потребляет: обработчик не запустился бы, а `rt_sigframe`
//! лёг бы по случайному адресу из `rax`. Этот модуль — типизированная, проверяемая
//! альтернатива; два адаптера (syscall-фрейм и IRQ-фрейм) делят только чистый
//! кодировщик `signal_frame`.
//!
//! ## Контракт с `src/test.rs`
//!
//! `scheduler_layout_tests` проверяет ту же раскладку побайтно на сентинел-
//! значениях (индексы слов 0 = popfq, 10 = rdi, 16 = RIP, 19 = RSP); `const`
//! проверки в конце файла закрепляют ТЕ ЖЕ смещения против реального типа
//! `SavedRegs`, поэтому перестановка любой стороны ломает сборку.
#![allow(dead_code)]

use super::regs::SavedRegs;
use super::signal_frame::{SigFrameRegs, SIGINFO_OFFSET, UC_OFFSET};

/// Полный 21-словный фрейм, который строит `irq32_stub` (168 байт).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IrqFrame {
    /// `+0`: слово RFLAGS, снимаемое путём восстановления через `popfq`. Оно
    /// остаётся `0x002` (IF замаскирован) на весь хвост восстановления — `iretq`
    /// восстанавливает настоящий пользовательский IF из [`IrqFrame::rflags`].
    /// Никогда не ставить IF здесь: окно между `popfq` и `iretq` не должно быть
    /// прерываемым.
    pub popfq_rflags: u64,
    /// `+8 .. +127`: 15 GPR в порядке `SavedRegs` (`r15` младший, `rax` по
    /// абсолютному `+120`).
    pub gpr: SavedRegs,
    /// `+128`: прерванный RIP.
    pub rip: u64,
    /// `+136`: прерванный CS.
    pub cs: u64,
    /// `+144`: прерванный RFLAGS.
    pub rflags: u64,
    /// `+152`: прерванный RSP.
    pub rsp: u64,
    /// `+160`: прерванный SS.
    pub ss: u64,
}

impl IrqFrame {
    /// Была ли задача прервана в ring 3? Только тогда фрейм несёт пользовательский
    /// контекст, из которого можно построить `rt_sigframe`: у задачи, вытесненной
    /// внутри syscall, CS — ядерный селектор, а пользовательский контекст живёт в
    /// фрейме входа syscall выше по стеке и достижим только через эпилог возврата
    /// этого syscall (или его проверки `EINTR`).
    #[inline]
    pub fn is_user_frame(&self) -> bool {
        self.cs & 3 == 3 && self.cs != 0
    }

    /// Пользовательский контекст, который описывает этот фрейм, в форме, которую
    /// потребляет [`super::signal_frame::encode_rt_sigframe`].
    pub fn user_context(&self) -> SigFrameRegs {
        SigFrameRegs {
            r8: self.gpr.r8,
            r9: self.gpr.r9,
            r10: self.gpr.r10,
            r11: self.gpr.r11,
            r12: self.gpr.r12,
            r13: self.gpr.r13,
            r14: self.gpr.r14,
            r15: self.gpr.r15,
            di: self.gpr.rdi,
            si: self.gpr.rsi,
            bp: self.gpr.rbp,
            bx: self.gpr.rbx,
            dx: self.gpr.rdx,
            ax: self.gpr.rax,
            cx: self.gpr.rcx,
            sp: self.rsp,
            ip: self.rip,
            flags: self.rflags,
        }
    }

    /// Войти в `handler` на ближайшем `iretq`: точный набор мутаций на месте.
    ///
    /// `rax` (живой пользовательский `rax`, он же `sigcontext.rax`, который
    /// восстановит `rt_sigreturn`), `cs`, `ss` и слово для `popfq` НЕ трогаются —
    /// обработчик выполняется в ring 3, на пользовательском стеке, с чистым
    /// пользовательским RFLAGS, а прерванный контекст достижим только через
    /// `rt_sigframe`, записанный по `frame_addr`.
    pub fn enter_handler(&mut self, plan: &IrqDelivery) {
        self.rip = plan.rip;
        self.rflags = plan.rflags;
        self.rsp = plan.rsp;
        self.gpr.rdi = plan.rdi;
        self.gpr.rsi = plan.rsi;
        self.gpr.rdx = plan.rdx;
    }
}

/// Куда входить в обработчик и с какими регистрами-аргументами.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqDelivery {
    /// `sa_handler` — цель `iretq`.
    pub rip: u64,
    /// Чистый пользовательский RFLAGS (`IF` установлен, зарезервированный бит 1
    /// установлен).
    pub rflags: u64,
    /// База `rt_sigframe` (≡ 8 mod 16, `pretcode` в её первом слове).
    pub rsp: u64,
    /// `signo`.
    pub rdi: u64,
    /// `&siginfo`.
    pub rsi: u64,
    /// `&ucontext`.
    pub rdx: u64,
    /// Сигнал, который несёт эта доставка — явно, чтобы план входа в обработчик
    /// можно было сверить с сигналом pending-бита (класс off-by-one: доставка
    /// сигнала N-1 для ожидающего N).
    pub sig: u64,
}

/// Построить план доставки для `sig`, обрабатываемого `handler`, чей
/// `rt_sigframe` начинается по `frame_addr`.
pub fn plan_irq_delivery(frame_addr: u64, sig: u64, handler: u64, user_rflags: u64) -> IrqDelivery {
    IrqDelivery {
        rip: handler,
        rflags: user_rflags,
        rsp: frame_addr,
        rdi: sig,
        rsi: frame_addr + SIGINFO_OFFSET,
        rdx: frame_addr + UC_OFFSET,
        sig,
    }
}

// ─── политика обработки page fault (заслон read-only COW) ───────────────────

/// Что обработчик page fault обязан сделать с отказом, который путь
/// demand-paging не смог обслужить.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageFaultAction {
    /// Завершить только виновный Compat_Process; машина продолжает работать.
    KillProcess,
    /// Напечатать фрейм отказа и остановиться: настоящий отказ ядра должен
    /// оставаться фатальным.
    Halt,
}

/// Решить судьбу page fault, который отказался обслужить
/// `mem_sys::handle_user_page_fault`.
///
/// `user_mode` зеркалит `PageFaultErrorCode::USER_MODE` (отказ пришёл из ring 3),
/// `fault_addr` — CR2, `user_addr_max` — исключающая граница пользовательской
/// половины, а `current_is_compat` говорит, выполняется ли Linux compat-процесс.
///
/// Отказы ring 3 сохраняют прежнее правило: compat-процесс убивается, любая
/// другая задача идёт фатальным путём. Этот политика добавляет случай **отказа
/// ring 0 на пользовательском адресе**: обработчик syscall читал или писал
/// пользовательский буфер, который поток-сосед по `CLONE_VM` снял после проверки
/// на входе. Это никогда не стоит остановки машины — убиваем процесс, который это
/// вызвал. Отказ ring 0 на *ядерном* адресе, напротив, — настоящая ошибка ядра
/// (случай LIEDTKE, для которого обработчик и существует), и он по-прежнему
/// останавливает машину, так что заслон не может молча её проглотить.
pub fn page_fault_action(
    user_mode: bool,
    fault_addr: u64,
    user_addr_max: u64,
    current_is_compat: bool,
) -> PageFaultAction {
    // Первая страница в этом ядре никогда не отображается (`mmap(0,...)`
    // отвергается), поэтому отказ ring 0 по адресу ~0 — баг NULL-указателя ядра,
    // а не гонка за пользовательским буфером: он должен остаться фатальным, а не
    // списан на процесс.
    const MIN_USER_FAULT_ADDR: u64 = 4096;
    let on_user_address = (MIN_USER_FAULT_ADDR..user_addr_max).contains(&fault_addr);
    if (user_mode || on_user_address) && current_is_compat {
        PageFaultAction::KillProcess
    } else {
        PageFaultAction::Halt
    }
}

// ─── политика обработки синхронных исключений CPU ───────────────────────────

/// Что обработчик синхронного исключения CPU (#DE/#DB/#UD/#OF/#BR/#NM/#AC/#MF/#XM)
/// обязан сделать, когда исключение не ожидается самим ядром.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrapAction {
    /// Завершить только виновный Compat_Process; машина продолжает работать.
    KillProcess,
    /// Напечатать, что залогировал обработчик, и остановиться: исключение в
    /// ring 0 — настоящий баг ядра (или отказ железа) и должен оставаться
    /// фатальным.
    Halt,
}

/// Решить судьбу синхронного исключения CPU.
///
/// Только `#GP` и `#PF` реализовывали правило «исключение пользовательского
/// режима — баг userspace, а не отказ машины»; любой другой достижимый из
/// пользователя вектор заканчивался `halt()`, поэтому `1/0`, `ud2` или ring-3
/// `popfq` с TF парковали всю машину из непривилегированного процесса. Правило
/// здесь то же, что у `#GP`/`#PF`: если контекст отказа — ring 3 (или
/// сохранённый RIP — пользовательский адрес) **и** текущая задача — Linux
/// Compat_Process, убить только этот процесс; иначе остановиться.
///
/// `user_mode` выводится из RPL сохранённого CS; `fault_rip < user_addr_max` —
/// та же проверка пользовательского адреса, что применяют пути `#GP`/`#PF`. Нужны
/// обе: исключение в режиме ядра на пользовательском RIP (например, будущий путь
/// эмуляции) не должно ронять машину, а исключение ring 0 на ядерном RIP — ровно
/// случай, который обязан остаться фатальным. Убить можно только compat-задачу;
/// у нативной ядерной задачи нет Compat_Process для завершения, поэтому она
/// остаётся фатальной.
pub fn exception_action(
    user_mode: bool,
    fault_rip: u64,
    user_addr_max: u64,
    current_is_compat: bool,
) -> TrapAction {
    let on_user_rip = fault_rip < user_addr_max;
    if current_is_compat && (user_mode || on_user_rip) {
        TrapAction::KillProcess
    } else {
        TrapAction::Halt
    }
}

// ─── проверки раскладки (контракт `src/test.rs` в типовой форме) ─────────────

const _: () = {
    use core::mem::{offset_of, size_of};
    // Весь фрейм: 1 слово popfq + 15 GPR + RIP/CS/RFLAGS/RSP/SS.
    assert!(size_of::<IrqFrame>() == 21 * 8);
    assert!(offset_of!(IrqFrame, popfq_rflags) == 0);
    assert!(offset_of!(IrqFrame, gpr) == 8);
    assert!(offset_of!(IrqFrame, rip) == 128);
    assert!(offset_of!(IrqFrame, cs) == 136);
    assert!(offset_of!(IrqFrame, rflags) == 144);
    assert!(offset_of!(IrqFrame, rsp) == 152);
    assert!(offset_of!(IrqFrame, ss) == 160);
    // Смещения полей `SavedRegs` внутри блока GPR. `rax` по абсолютному +120 —
    // та самая ловушка, для которой существует этот модуль: это НЕ per-task слот
    // user-RSP syscall-фрейма (тот по `SavedRegs + 120`).
    assert!(size_of::<SavedRegs>() == 15 * 8);
    assert!(offset_of!(SavedRegs, r15) == 0);
    assert!(offset_of!(SavedRegs, rax) == 112);
    assert!(offset_of!(SavedRegs, rdi) == 72);
};
