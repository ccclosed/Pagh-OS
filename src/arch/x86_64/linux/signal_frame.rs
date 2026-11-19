//! Чистый ABI сигналов x86_64 Linux: номера сигналов, записи диспозиций,
//! алгебра sigset, классификация действий по умолчанию и encode/decode
//! `rt_sigframe`, используемые доставкой сигналов (`super::signal`) и
//! `rt_sigreturn`.
//!
//! Модуль намеренно только `core` и без ядерных зависимостей (R11.6): он
//! включается в `host-tests` через `#[path]` (`p42`), поэтому property-тесты
//! упражняют ТОЧНЫЕ байты, которые ядро пишет в стеки ring 3.
//!
//! ## Контракт раскладки фрейма (x86_64 Linux, `arch/x86/kernel/signal.c`)
//!
//! Доставленный сигнал строит на пользовательском стеке ниже прерванного RSP:
//!
//! ```text
//! frame+0    : pretcode  = sa_restorer (трамплин sigreturn glibc)
//! frame+8    : struct ucontext uc        (304 байта)
//! frame+312  : struct siginfo info       (128 байт, нули + si_signo/si_code)
//! total      : RT_SIGFRAME_SIZE = 440
//! ```
//!
//! Прерванное состояние GPR хранится в `uc.uc_mcontext` (`sigcontext_64`,
//! 256 байт по `frame+48`), а маска сигналов для восстановления на
//! `rt_sigreturn` — в `uc.uc_sigmask` (`frame+304`).
//!
//! Контракт регистров при входе в обработчик (как в Linux на пути возврата из
//! syscall): `RSP = frame`, `RIP = sa_handler`, `RDI = signo`, `RSI = &info`,
//! `RDX = &uc`. `rcx`/`r11` при доставке несут прерванный user RIP / RFLAGS
//! (собственный clobber инструкции `syscall`), поэтому `sigcontext.cx` и
//! `sigcontext.ip` несут одно значение — так же, как показывает Linux.
//!
//! ## Контракт возврата к фрейму для `rt_sigreturn`
//!
//! Restorer работает с `RSP = frame + 8` (`ret` снял `pretcode`), поэтому
//! `rt_sigreturn` находит `uc` ровно по входному RSP: mcontext по `+40`,
//! sigmask по `+296`.
#![allow(dead_code)]

/// Наибольший номер сигнала, который покрывает таблица (Linux `_NSIG/64-bit` —
/// 64).
pub const SIGNAL_COUNT: usize = 64;

/// Биты RFLAGS, которые может восстановить ring-3 `rt_sigreturn`-фрейм.
///
/// Фрейм полностью контролируется пользователем (ядро не может его
/// аутентифицировать и не должно: Linux тоже доверяет), поэтому каждый
/// *привилегированный* бит нужно вымаскировать до того, как значение дойдёт до
/// пути возврата `sysret`, который не санирует RFLAGS:
///
/// * `IOPL` (0x3000) — ring-3 `in`/`out` и PCI config space. Поддельный фрейм
///   когда-то ставил его, отдавая ring 3 порты I/O.
/// * `NT` (0x4000), `VM` (0x20000) — состояние задачи/VM86, здесь бессмысленное.
/// * `RF` (0x10000), `AC` (0x40000) — переопределения resume/alignment-check;
///   `AC=1` свёл бы на нет SMAP, если его когда-нибудь включат.
///
/// Всё, чем пользовательская программа может законно владеть, сохраняется:
/// CF/PF/AF/ZF/SF/TF/IF/DF/OF плюс бит 1 EFLAGS (зарезервированный,
/// архитектурно всегда 1), чтобы обработчик в стиле `siglongjmp` видел те же
/// флаги. `0xFD7` — это `0xFD5` (девять арифметических/управляющих битов) с
/// этим фиксированным битом.
pub const USER_RFLAGS_MASK: u64 = 0x0000_0000_0000_0FD7;

/// Урезать `flags` до пользовательски устанавливаемых битов RFLAGS
/// ([`USER_RFLAGS_MASK`]).
pub fn sanitize_user_rflags(flags: u64) -> u64 {
    flags & USER_RFLAGS_MASK
}

// ─── Номера сигналов (x86_64 Linux) ───────────────────────────────────────────

pub const SIGHUP: u64 = 1;
pub const SIGINT: u64 = 2;
pub const SIGQUIT: u64 = 3;
pub const SIGILL: u64 = 4;
pub const SIGTRAP: u64 = 5;
pub const SIGABRT: u64 = 6;
pub const SIGBUS: u64 = 7;
pub const SIGFPE: u64 = 8;
pub const SIGKILL: u64 = 9;
/// `SIGUSR1` — первый пользовательский сигнал, естественный выбор для тестов и
/// программ, которым нужно чисто прикладное уведомление.
pub const SIGUSR1: u64 = 10;
pub const SIGSEGV: u64 = 11;
/// `SIGUSR2` — второй пользовательский сигнал.
pub const SIGUSR2: u64 = 12;
pub const SIGPIPE: u64 = 13;
pub const SIGALRM: u64 = 14;
/// `SIGTERM` — стандартный вежливый запрос завершения (`kill -TERM`); его
/// действие по умолчанию — `Term`.
pub const SIGTERM: u64 = 15;
pub const SIGCHLD: u64 = 17;
pub const SIGCONT: u64 = 18;
pub const SIGSTOP: u64 = 19;
pub const SIGTSTP: u64 = 20;
pub const SIGTTIN: u64 = 21;
pub const SIGTTOU: u64 = 22;
pub const SIGURG: u64 = 23;
pub const SIGWINCH: u64 = 28;

/// `sigaction(2)`: сбросить диспозицию в `SIG_DFL`.
pub const SIG_DFL: u64 = 0;
/// `sigaction(2)`: игнорировать сигнал.
pub const SIG_IGN: u64 = 1;

// ─── биты `sa_flags` ─────────────────────────────────────────────────────────

/// Выполнять обработчик на альтернативном стеке сигналов (`sigaltstack(2)`).
pub const SA_ONSTACK: u64 = 0x0800_0000;
/// `sa_restorer` валиден (glibc на x86_64 всегда его ставит).
pub const SA_RESTORER: u64 = 0x0400_0000;
/// Обработчик принимает `(signo, &siginfo, &ucontext)` — мы всё равно передаём
/// все три.
pub const SA_SIGINFO: u64 = 0x0000_0004;

// ─── значения `how` для `sigprocmask` ────────────────────────────────────────

pub const SIG_BLOCK: u64 = 0;
pub const SIG_UNBLOCK: u64 = 1;
pub const SIG_SETMASK: u64 = 2;

// ─── `sigaltstack` ───────────────────────────────────────────────────────────

/// Минимальный размер альтернативного стека, который принимаем
/// (`MINSIGSTKSZ` в Linux x86_64).
pub const MINSIGSTKSZ: u64 = 2048;
/// `stack_t.ss_flags`: альтернативный стек отключён.
pub const SS_DISABLE: u32 = 2;

// ─── алгебра sigset (битсет u64, бит N == сигнал N) ──────────────────────────

/// Битовая маска сигнала `sig` (1..=64); `sig == 0` даёт 0 (нет сигнала).
#[inline]
pub const fn sigbit(sig: u64) -> u64 {
    if sig == 0 || sig > SIGNAL_COUNT as u64 {
        0
    } else {
        1u64 << (sig - 1)
    }
}

/// Неблокируемые на уровне ядра сигналы: `SIGKILL` и `SIGSTOP`.
pub const UNBLOCKABLE_MASK: u64 = sigbit(SIGKILL) | sigbit(SIGSTOP);

/// Чистый пользовательский RFLAGS x86_64 для входа в обработчик: зарезервированный
/// бит 1 установлен, IF установлен, все системные флаги сброшены. Оба пути
/// доставки (возврат syscall через `sysretq`, тик через `iretq`) входят в
/// обработчик ровно с этими флагами; Linux санирует прерванные флаги так же при
/// доставке сигнала.
pub const USER_RFLAGS: u64 = 0x202;

/// Сигналы класса остановки (`SIG_KERNEL_STOP_MASK` в Linux): их действие по
/// умолчанию — Stop, а генерация `SIGCONT` удаляет их все из pending-наборов
/// (POSIX 2.4.1 / Linux `prepare_signal`).
pub const SIG_KERNEL_STOP_MASK: u64 =
    sigbit(SIGSTOP) | sigbit(SIGTSTP) | sigbit(SIGTTIN) | sigbit(SIGTTOU);

/// `sig` — сигнал класса остановки (действие по умолчанию Stop)?
pub fn is_stop_signal(sig: u64) -> bool {
    sig != 0 && sigbit(sig) & SIG_KERNEL_STOP_MASK != 0
}

/// Сбросить все ожидающие биты класса остановки (это делает генерация `SIGCONT`).
pub fn flush_stop_bits(pending: u64) -> u64 {
    pending & !SIG_KERNEL_STOP_MASK
}

/// Сбросить ожидающий `SIGCONT` (это делает генерация stop-сигнала — POSIX: «при
/// генерации любого stop-сигнала ... любой ожидающий SIGCONT отбрасывается»).
pub fn flush_cont_bits(pending: u64) -> u64 {
    pending & !sigbit(SIGCONT)
}

// ─── статусные слова `wait(2)` ──────────────────────────────────────────────

/// `wait(2)`-статус ребёнка, ОСТАНОВЛЕННОГО сигналом `sig`: `(sig << 8) | 0x7f`,
/// кодировка Linux, в которой `WIFSTOPPED(status)` — ровно `status & 0xff ==
/// 0x7f`. `WSTOPSIG(status)` — `status >> 8`.
#[inline]
pub const fn wait_status_stopped(sig: u64) -> u32 {
    (((sig as u32) & 0xff) << 8) | 0x7f
}

/// `wait(2)`-статус ребёнка, ВОЗОБНОВЛЁННОГО `SIGCONT`: `0xffff`
/// (`WIFCONTINUED(status)` — ровно `status == 0xffff`).
pub const WAIT_STATUS_CONTINUED: u32 = 0xffff;

/// `wait(2)`-статус нормально вышедшего ребёнка: код выхода в старшем байте
/// (соглашение pagh: `128 + sig` для завершённого сигналом ребёнка).
#[inline]
pub const fn wait_status_exited(code: u8) -> u32 {
    (code as u32) << 8
}

/// `WIFSTOPPED(status)`.
#[inline]
pub const fn wait_status_is_stopped(status: u32) -> bool {
    status & 0xff == 0x7f
}

/// `WIFCONTINUED(status)`.
#[inline]
pub const fn wait_status_is_continued(status: u32) -> bool {
    status == WAIT_STATUS_CONTINUED
}

/// Применить операцию `sigprocmask`. Биты `SIGKILL`/`SIGSTOP` никогда не могут
/// оказаться заблокированными; неизвестный `how` — `None` (вызывающий отображает
/// в `EINVAL`).
pub fn apply_mask_op(how: u64, old: u64, set: u64) -> Option<u64> {
    let mut new = match how {
        SIG_BLOCK => old | set,
        SIG_UNBLOCK => old & !set,
        SIG_SETMASK => set,
        _ => return None,
    };
    new &= !UNBLOCKABLE_MASK;
    Some(new)
}

// ─── действия по умолчанию (таблица `signal(7)`) ────────────────────────────

/// Что ядро делает с сигналом, чья диспозиция — `SIG_DFL`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DefaultAction {
    /// Завершить процесс (статус выхода `128 + sig`).
    Term,
    /// Игнорировать сигнал.
    Ignore,
    /// Остановить процесс (доставляется как парковка планировщиком: сохранённый
    /// фрейм задачи уходит в `STOPPED_TASKS` и остаётся там до `SIGCONT`).
    Stop,
    /// Продолжить процесс, если он остановлен, иначе игнорировать.
    Cont,
}

/// Действие по умолчанию для сигналов 1..=31 плюс обычные RT-номера; всё свыше
/// 34 по умолчанию завершает (в Linux real-time сигналы по умолчанию завершают).
pub fn default_action(sig: u64) -> DefaultAction {
    match sig {
        SIGCHLD | SIGURG | SIGWINCH => DefaultAction::Ignore,
        SIGCONT => DefaultAction::Cont,
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => DefaultAction::Stop,
        _ => DefaultAction::Term,
    }
}

/// Действие этого сигнала по умолчанию — завершение процесса? Используется
/// `tgkill`, чтобы сохранить поведение «фатальный сигнал самому себе без
/// обработчика завершает группу».
pub fn default_is_fatal(sig: u64) -> bool {
    default_action(sig) == DefaultAction::Term
}

/// Есть ли у `SIG_DFL`-диспозиции этого сигнала РАБОТА в обработчике?
///
/// `false` для `Ignore` (SIGCHLD/SIGURG/SIGWINCH) и `Cont` (SIGCONT):
/// возобновление по SIGCONT уже выполнено на времени генерации
/// (`signal::resume_group`), а «игнорировать» — не действие вообще. Такой сигнал
/// обязан быть потреблён без построения `rt_sigframe`: при `SIG_DFL` поле
/// handler равно нулю, и возврат «из обработчика» увёл бы процесс в RIP=0
/// (SIGSEGV). Ровно это убивало композитор после `bg`: pending SIGCONT
/// доставлялся тиком одному из возобновлённых процессов.
pub fn default_action_is_silent(sig: u64) -> bool {
    matches!(
        default_action(sig),
        DefaultAction::Ignore | DefaultAction::Cont
    )
}

// ─── записи диспозиций + альтернативного стека ──────────────────────────────

/// Ядерная копия одной диспозиции (`struct kernel_sigaction`, 32-байтный
/// пользовательский ABI при `sigsetsize == 8`: handler, flags, restorer, mask).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignalAction {
    /// `SIG_DFL` (0), `SIG_IGN` (1) или виртуальный адрес пользовательского
    /// обработчика.
    pub handler: u64,
    /// `sa_flags` (`SA_RESTORER`, `SA_ONSTACK`, ...).
    pub flags: u64,
    /// `sa_restorer` — трамплин sigreturn, который предоставляет glibc.
    pub restorer: u64,
    /// Маска, блокируемая на время обработчика (`sa_mask`, младшие 64 бита).
    pub mask: u64,
}

impl Default for SignalAction {
    fn default() -> Self {
        SignalAction {
            handler: SIG_DFL,
            flags: 0,
            restorer: 0,
            mask: 0,
        }
    }
}

/// Запускает ли эта диспозиция пользовательский код (ни `SIG_DFL`, ни `SIG_IGN`)?
pub fn is_user_handler(a: &SignalAction) -> bool {
    a.handler != SIG_DFL && a.handler != SIG_IGN
}

/// Ядерная копия `stack_t` (24-байтный пользовательский ABI: sp, flags(+pad),
/// size).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SigAltStack {
    /// База региона альтернативного стека.
    pub sp: u64,
    /// `SS_DISABLE` или 0 (включён).
    pub flags: u32,
    /// Размер региона в байтах.
    pub size: u64,
}

impl Default for SigAltStack {
    fn default() -> Self {
        SigAltStack {
            sp: 0,
            flags: SS_DISABLE,
            size: 0,
        }
    }
}

// ─── геометрия фрейма ───────────────────────────────────────────────────────

/// Смещение `ucontext` во фрейме (после `pretcode`).
pub const UC_OFFSET: u64 = 8;
/// Смещение `sigcontext_64` (mcontext) внутри `ucontext`
/// (`uc_flags` 8 + `uc_link` 8 + `uc_stack` 24).
pub const MCONTEXT_OFFSET: u64 = UC_OFFSET + 40;
/// Смещение `uc_sigmask` внутри `ucontext` (после 256-байтного mcontext).
pub const SIGMASK_OFFSET: u64 = UC_OFFSET + 40 + 256;
/// Смещение `siginfo` во фрейме (после 304-байтного ucontext).
pub const SIGINFO_OFFSET: u64 = UC_OFFSET + 304;
/// Полный размер фрейма: pretcode(8) + ucontext(304) + siginfo(128).
pub const RT_SIGFRAME_SIZE: u64 = 440;

/// Снимок GPR, хранимый в `sigcontext` фрейма и восстанавливаемый из него.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SigFrameRegs {
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub di: u64,
    pub si: u64,
    pub bp: u64,
    pub bx: u64,
    pub dx: u64,
    pub ax: u64,
    pub cx: u64,
    pub sp: u64,
    pub ip: u64,
    pub flags: u64,
}

impl SigFrameRegs {
    fn put(&self, buf: &mut [u8; 256]) {
        let fields = [
            self.r8, self.r9, self.r10, self.r11, self.r12, self.r13, self.r14, self.r15, self.di,
            self.si, self.bp, self.bx, self.dx, self.ax, self.cx, self.sp, self.ip, self.flags,
        ];
        for (i, v) in fields.iter().enumerate() {
            buf[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        // cs/gs/fs/ss (u16 по 144..152) и err/trapno/oldmask/cr2 остаются
        // нулевыми — ядерный читатель восстанавливает только GPR/IP/FLAGS/SP.
    }

    fn get(buf: &[u8; 256]) -> SigFrameRegs {
        let mut f = [0u64; 18];
        for (i, slot) in f.iter_mut().enumerate() {
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[i * 8..i * 8 + 8]);
            *slot = u64::from_le_bytes(b);
        }
        SigFrameRegs {
            r8: f[0],
            r9: f[1],
            r10: f[2],
            r11: f[3],
            r12: f[4],
            r13: f[5],
            r14: f[6],
            r15: f[7],
            di: f[8],
            si: f[9],
            bp: f[10],
            bx: f[11],
            dx: f[12],
            ax: f[13],
            cx: f[14],
            sp: f[15],
            ip: f[16],
            flags: f[17],
        }
    }
}

/// Куда ядро кладёт фрейм ниже прерванного RSP.
///
/// Адрес ≡ 8 (mod 16): при входе в обработчик `RSP = frame` выглядит ровно как
/// обычный адрес возврата, положенный `call`, с точки зрения выравнивания SysV,
/// поэтому прологи обработчиков с `movaps` сохраняют ABI-гарантию. `SA_ONSTACK`
/// с включённым и достаточно большим альтернативным стеком кладёт фрейм на
/// ВЕРШИНУ того стека (тоже ≡ 8 mod 16).
pub fn frame_location(user_rsp: u64, action: &SignalAction, alt: &SigAltStack) -> u64 {
    let base =
        if action.flags & SA_ONSTACK != 0 && alt.flags & SS_DISABLE == 0 && alt.size >= MINSIGSTKSZ
        {
            alt.sp + alt.size
        } else {
            user_rsp
        };
    ((base.saturating_sub(RT_SIGFRAME_SIZE)) & !0xF) - 8
}

/// Закодировать полный `rt_sigframe` в `out` (ровно `RT_SIGFRAME_SIZE` байт).
/// `restorer` становится `pretcode`; `newmask` — маска, которую восстановит
/// `rt_sigreturn` (маска до доставки, объединённая с `sa_mask` действия).
pub fn encode_rt_sigframe(
    out: &mut [u8; RT_SIGFRAME_SIZE as usize],
    restorer: u64,
    sig: u64,
    saved: &SigFrameRegs,
    newmask: u64,
    alt: &SigAltStack,
) {
    *out = [0u8; RT_SIGFRAME_SIZE as usize];
    // pretcode
    out[0..8].copy_from_slice(&restorer.to_le_bytes());
    // ucontext: uc_flags = 0, uc_link = NULL, uc_stack = ТЕКУЩЕЕ состояние
    // altstack (Linux сообщает активное; SS_DISABLE, когда не задан).
    // uc_stack = { ss_sp @ uc+16, ss_flags @ uc+24, ss_size @ uc+28 }.
    out[UC_OFFSET as usize + 24..UC_OFFSET as usize + 28].copy_from_slice(&alt.flags.to_le_bytes());
    out[UC_OFFSET as usize + 28..UC_OFFSET as usize + 36].copy_from_slice(&alt.size.to_le_bytes());
    // mcontext
    let mut mc = [0u8; 256];
    saved.put(&mut mc);
    out[MCONTEXT_OFFSET as usize..MCONTEXT_OFFSET as usize + 256].copy_from_slice(&mc);
    // uc_sigmask
    out[SIGMASK_OFFSET as usize..SIGMASK_OFFSET as usize + 8]
        .copy_from_slice(&newmask.to_le_bytes());
    // siginfo: si_signo, si_errno=0, si_code=SI_KERNEL (0x80); остальное нули.
    out[SIGINFO_OFFSET as usize..SIGINFO_OFFSET as usize + 4]
        .copy_from_slice(&(sig as u32).to_le_bytes());
    out[SIGINFO_OFFSET as usize + 8..SIGINFO_OFFSET as usize + 12]
        .copy_from_slice(&0x80u32.to_le_bytes());
}

/// Результат декодирования фрейма на `rt_sigreturn`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestoredFrame {
    /// Полный снимок GPR (восстанавливаются все, кроме `rcx`/`r11` — см. ниже).
    pub regs: SigFrameRegs,
    /// Маска сигналов для восстановления (маска до доставки).
    pub mask: u64,
}

/// Декодировать фрейм по `uc_addr` из 304 байт, начинающихся там (`ucontext`):
/// mcontext по `+40`, sigmask по `+296`.
///
/// `None`, когда mcontext RIP нулевой — защита от неверного использования
/// `rt_sigreturn` (аутентифицировать фрейм на стороне ядра нельзя; Linux тоже
/// доверяет).
pub fn decode_rt_sigframe(uc: &[u8; 304]) -> Option<RestoredFrame> {
    let mut mc = [0u8; 256];
    mc.copy_from_slice(
        &uc[MCONTEXT_OFFSET as usize - UC_OFFSET as usize
            ..MCONTEXT_OFFSET as usize - UC_OFFSET as usize + 256],
    );
    let mut regs = SigFrameRegs::get(&mc);
    // Фрейм может вернуть исполнение/стек только в пользовательскую половину.
    // Ядерный IP заставил бы `sysret` войти в ring 3 с CPL=3 по supervisor-
    // адресу (CPU упадёт, но фрейм бессмыслен); ядерный SP припарковал бы
    // пользовательский стек в ядерной половине и упал на первом push'е
    // обработчика. Отвергаем оба на этапе декодирования, как и нулевой IP.
    if regs.ip == 0 || regs.ip >= super::validate::USER_ADDR_MAX {
        return None;
    }
    if regs.sp >= super::validate::USER_ADDR_MAX {
        return None;
    }
    // `sys_rt_sigreturn` пишет это прямо в регистр, который `sysret` восстановит
    // как RFLAGS, а тот их не санирует. Маскируем привилегированные биты здесь,
    // чтобы каждый путь декодирования получал уже санированное значение.
    regs.flags = sanitize_user_rflags(regs.flags);
    let mut m = [0u8; 8];
    m.copy_from_slice(&uc[296..304]);
    Some(RestoredFrame {
        regs,
        mask: u64::from_le_bytes(m) & !UNBLOCKABLE_MASK,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_rflags_mask_drops_privileged_bits() {
        // IOPL=3 (атака), NT, VM, RF, AC должны исчезнуть.
        let attack = 0x3000u64 | 0x4000 | 0x20000 | 0x10000 | 0x40000;
        assert_eq!(sanitize_user_rflags(attack), 0);
        // Биты, законно принадлежащие пользовательской программе, выживают: CF,
        // PF, AF, ZF, SF, TF, IF, DF, OF и фиксированный зарезервированный бит 1.
        assert_eq!(sanitize_user_rflags(0xFD7), 0xFD7);
        // IF (0x200) — тот единственный, который нужен каждому обработчику,
        // чтобы прерывания оставались включены.
        assert_eq!(sanitize_user_rflags(0x202), 0x202);
        // Идемпотентно: маскирование уже замаскированного значения ничего не
        // меняет.
        let once = sanitize_user_rflags(0xFFFF_FFFF_FFFF_FFFF);
        assert_eq!(once, USER_RFLAGS_MASK);
        assert_eq!(sanitize_user_rflags(once), once);
    }

    #[test]
    fn sigbit_basics() {
        assert_eq!(sigbit(1), 1);
        assert_eq!(sigbit(SIGINT), 0b10); // бит N-1 для сигнала N
        assert_eq!(sigbit(0), 0);
        assert_eq!(sigbit(65), 0);
        assert_eq!(UNBLOCKABLE_MASK, sigbit(9) | sigbit(19));
    }

    #[test]
    fn mask_ops_always_leave_kill_stop_unblocked() {
        let m = apply_mask_op(SIG_SETMASK, 0, u64::MAX).unwrap();
        assert_eq!(m & UNBLOCKABLE_MASK, 0);
        let m = apply_mask_op(SIG_BLOCK, 0, u64::MAX).unwrap();
        assert_eq!(m & UNBLOCKABLE_MASK, 0);
        assert_eq!(apply_mask_op(99, 0, 1), None);
    }

    #[test]
    fn default_action_table() {
        assert_eq!(default_action(SIGINT), DefaultAction::Term);
        assert_eq!(default_action(SIGKILL), DefaultAction::Term);
        assert_eq!(default_action(SIGSEGV), DefaultAction::Term);
        assert_eq!(default_action(SIGCHLD), DefaultAction::Ignore);
        assert_eq!(default_action(SIGWINCH), DefaultAction::Ignore);
        assert_eq!(default_action(SIGSTOP), DefaultAction::Stop);
        assert_eq!(default_action(SIGCONT), DefaultAction::Cont);
        assert_eq!(default_action(42), DefaultAction::Term);
    }

    #[test]
    fn frame_location_keeps_sysv_alignment() {
        let a = SignalAction::default();
        let d = SigAltStack::default();
        for rsp in [0x7FFF_F000u64, 0x7FFF_F008, 0x7FFF_E010, 0x1000_0000] {
            let f = frame_location(rsp, &a, &d);
            assert_eq!(f % 16, 8, "rsp={rsp:#x} frame={f:#x}");
            assert!(f + RT_SIGFRAME_SIZE <= rsp);
        }
    }

    #[test]
    fn frame_location_uses_enabled_altstack() {
        let mut a = SignalAction::default();
        a.flags = SA_ONSTACK;
        let mut alt = SigAltStack::default();
        // Отключён → откат к обычному стеку.
        assert_eq!(
            frame_location(0x8000, &a, &alt),
            frame_location(0x8000, &SignalAction::default(), &alt)
        );
        alt.flags = 0;
        alt.sp = 0x10_0000;
        alt.size = MINSIGSTKSZ;
        let f = frame_location(0x8000, &a, &alt);
        assert!(f >= 0x10_0000 && f + RT_SIGFRAME_SIZE <= 0x10_0000 + MINSIGSTKSZ);
        assert_eq!(f % 16, 8);
        // Слишком маленький altstack игнорируется.
        alt.size = 64;
        assert_eq!(
            frame_location(0x8000, &a, &alt),
            frame_location(0x8000, &SignalAction::default(), &alt)
        );
    }

    #[test]
    fn roundtrip_encode_decode() {
        let saved = SigFrameRegs {
            r8: 0x88,
            r9: 0x99,
            r10: 0xAA,
            r11: 0x202,
            r12: 1,
            r13: 2,
            r14: 3,
            r15: 4,
            di: 0x1234,
            si: 0x5678,
            bp: 0x7FFF_F000,
            bx: 42,
            dx: 7,
            ax: 0xDEAD,
            cx: 0x401000,
            sp: 0x7FFF_F100,
            ip: 0x4020F0,
            flags: 0x246,
        };
        let alt = SigAltStack::default();
        let mut buf = [0u8; RT_SIGFRAME_SIZE as usize];
        encode_rt_sigframe(&mut buf, 0x401500, SIGINT, &saved, 0b1010, &alt);
        let mut uc = [0u8; 304];
        uc.copy_from_slice(&buf[UC_OFFSET as usize..UC_OFFSET as usize + 304]);
        let r = decode_rt_sigframe(&uc).expect("valid frame decodes");
        assert_eq!(r.regs, saved);
        assert_eq!(r.mask, 0b1010);
        // pretcode == sa_restorer
        let mut p = [0u8; 8];
        p.copy_from_slice(&buf[0..8]);
        assert_eq!(u64::from_le_bytes(p), 0x401500);
        // siginfo: si_signo = SIGINT, si_code = SI_KERNEL
        let mut s = [0u8; 4];
        s.copy_from_slice(&buf[SIGINFO_OFFSET as usize..SIGINFO_OFFSET as usize + 4]);
        assert_eq!(u32::from_le_bytes(s), SIGINT as u32);
        s.copy_from_slice(&buf[SIGINFO_OFFSET as usize + 8..SIGINFO_OFFSET as usize + 12]);
        assert_eq!(u32::from_le_bytes(s), 0x80);
    }

    #[test]
    fn decode_rejects_null_ip() {
        let mut uc = [0u8; 304];
        // Полностью нулевой mcontext → ip == 0 → отвергается.
        assert!(decode_rt_sigframe(&uc).is_none());
    }
}
