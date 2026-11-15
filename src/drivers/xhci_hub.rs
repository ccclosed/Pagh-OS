//! Политика подъёма и сброса корневого порта для драйвера xHCI по
//! `hub.c`/`xhci-hub.c`.
//!
//! Отдельный чистый модуль нужен потому, что решения о таймингах и порядке
//! записей не относятся к MMIO, а QEMU включает порты мгновенно, и boot-тест их
//! не увидит. Здесь host-тесты прогоняют ту же state machine, что ядро, против
//! симулированных портов PCH (линк, падающий в Polling; скачущее подключение;
//! reset, который контроллер не завершает; пять неудачных reset подряд).
//!
//! 1. Запись PORTSC выполняется только через neutral-значение: сохраняются
//!    read-only биты (CCS, speed) и read/write-биты, у которых 0 означает
//!    «не трогать» (PLS, PP), затем добавляется ровно один бит действия
//!    ([`port_state_to_neutral`]). В частности, PED — write-1-to-disable: запись
//!    прочитанного PED отключает порт.
//! 2. Порядок reset/change-битов: PR (USB2) или WPR (USB3) на neutral-значении,
//!    ожидание выхода из reset с подключением, проверка PED и только затем
//!    очистка C_PORT_RESET (PRC). PRC/CSC никогда не очищаются в той же записи,
//!    что выдаёт reset.
//! 3. Тайминги взяты из `hub.c`, а не придуманы: `HUB_DEBOUNCE_STEP` 25 мс,
//!    `HUB_DEBOUNCE_STABLE` 100 мс неизменного подключения до первого reset,
//!    `HUB_DEBOUNCE_TIMEOUT` 2000 мс всего, `PORT_RESET_TRIES` 5,
//!    `HUB_SHORT_RESET_TIME` 10 мс и `HUB_LONG_RESET_TIME` 200 мс на опрос,
//!    `HUB_RESET_TIMEOUT` 800 мс на reset и 100 мс power settle после PP.
//!    2 с — максимальный бюджет debounce, а не тишина: подключённый порт
//!    сбрасывается после 100 мс стабильного подключения.
//! 4. Power-cycle корневого порта при отказе включения не делается: порт здесь
//!    только включается, а порт, который не удалось включить, повторяется после
//!    backoff, а не остаётся обесточенным.
//! 5. После неудачного включения пауза ≥1 с и новый debounce, поэтому сломанный
//!    порт повторяется осознанно раз в несколько секунд, а не сбрасывается
//!    каждые 124 мс.
//!
//! Драйвер в `xhci.rs` — тонкий исполнитель: читает PORTSC, декодирует в
//! [`PortObservation`], вызывает [`PortPolicy::step`] и выполняет возвращённый
//! [`Step`]. Всё ожидание — сравнение дедлайна на следующих poll, поэтому отсчёт
//! boot-меню не блокируется на USB2 PHY.

// ─── Константы Link State, с которыми работает политика ─────────────────────
//
// Они повторяют кодировку полей PORTSC (xHCI 1.2, таблица 5-27). Здесь они нужны,
// чтобы host-тесты описывали порты теми же терминами, что драйвер; MMIO-маски
// драйвера отдельны и применяются до передачи наблюдения.

/// Link state U0: включён и активен.
pub const PLS_U0: u8 = 0;
/// Link state U3: suspended, устройство остаётся включённым.
pub const PLS_U3: u8 = 3;
/// Link state RxDetect: определение приёмника USB3.
#[allow(dead_code)]
pub const PLS_RXDETECT: u8 = 5;
/// Link state Polling: обнаружение/тренировка USB2; PED ещё не валиден.
#[allow(dead_code)]
pub const PLS_POLLING: u8 = 7;

// ─── Политика таймингов (микросекунды, если не указано иное) ─────────────────
//
// Каждая константа ниже взята из `hub.c`, с именем исходной константы: тайминги
// не придумываются.

/// `hub.c` HUB_DEBOUNCE_STEP: сэмплы debounce идут через 25 мс.
pub const HUB_DEBOUNCE_STEP_US: u64 = 25_000;
/// `hub.c` HUB_DEBOUNCE_STABLE: подключение должно быть неизменным столько
/// времени, прежде чем порт считается подключённым. USB 2.0 §7.1.7.1 требует
/// ≥100 мс.
pub const HUB_DEBOUNCE_STABLE_US: u64 = 100_000;
/// `hub.c` HUB_DEBOUNCE_TIMEOUT: если подключение не удерживается столько
/// времени, debounce сдаётся (порт скачет, устройства нет).
pub const HUB_DEBOUNCE_TIMEOUT_US: u64 = 2_000_000;
/// `hub.c` HUB_SHORT_RESET_TIME: первые два опроса статуса reset — через 10 мс.
pub const HUB_SHORT_RESET_TIME_US: u64 = 10_000;
/// `hub.c` HUB_LONG_RESET_TIME: после двух коротких опросов — каждые 200 мс.
pub const HUB_LONG_RESET_TIME_US: u64 = 200_000;
/// `hub.c` HUB_RESET_TIMEOUT: одна попытка reset прекращается через столько времени.
pub const HUB_RESET_TIMEOUT_US: u64 = 800_000;
/// `hub.c` PORT_RESET_TRIES: сколько reset пишется за один подъём до отказа.
pub const PORT_RESET_TRIES: u8 = 5;

/// Power settle после поднятия PP. USB 2.0 §7.1.7.1 требует 100 мс между
/// включением питания и первым reset; корневой hub дополнительно debounce-ит
/// подключение на `HUB_DEBOUNCE_STABLE`. Политика держит 100 мс power settle и
/// затем 100 мс stable-connect debounce — консервативная сумма обоих требований
/// (`bPwrOn2PwrGood` задаёт лишь минимум питания и debounce не заменяет).
pub const POWER_SETTLE_US: u64 = 100_000;

/// Непрерывный интервал PED+U0, необходимый перед перечислением. Один сэмпл PED
/// — не включение: PED+U0 может мелькнуть и потеряться. `hub.c` проверяет PED
/// один раз; этот драйвер дополнительно доказывает устойчивость включения, чтобы
/// мимолётный PED во время reset не привёл к перечислению.
pub const STABLE_US: u64 = 200_000;

/// Как часто перепроверяется пустой порт. Порт без устройства стоит одного
/// чтения PORTSC за интервал; без reset и записей питания.
pub const EMPTY_RETRY_US: u64 = 1_000_000;

/// Сколько политика ждёт программного resume до U0.
pub const RESUME_WAIT_US: u64 = 60_000;

/// Пауза после неудачной попытки (потерян PED, таймаут reset, отказ
/// перечисления). Кооперативному опрашиваемому драйверу нужен явный период
/// тишины, чтобы мёртвый порт не превратился в цикл reset; ≥1 с требуется и для
/// восстановления после потерянного PED.
pub const RETRY_BACKOFF_US: u64 = 1_000_000;

/// Задержка повтора после неудачной попытки. Остаётся функцией, потому что
/// вызывающие логируют её, а host-тесты закрепляют нижнюю границу; она намеренно
/// плоская: каждая попытка и так стоит полного debounce и до пяти reset.
pub const fn backoff_us(_failures: u16) -> u64 {
    RETRY_BACKOFF_US
}

// ─── Запись PORTSC: neutral-значение ─────────────────────────────────────────

/// Биты PORTSC, которые являются read-only статусом или read/write-битами, где 0
/// означает «не трогать» (`XHCI_PORT_RO` / `XHCI_PORT_RWS` из `xhci-hub.c`):
///
/// * RO: CCS (0), бит 3, speed id (10-13), бит 30;
/// * RWS: PLS (5-8), PP (9), биты 14-15, биты 25-27.
///
/// Всё остальное — write-1-to-set / write-1-to-clear / reserved-zero и должно
/// писаться нулём. В частности, **PED (бит 1) — write-1-to-disable**: сохранение
/// прочитанного PED в записи PORTSC отключает порт. PR (бит 4), WPR (31), LWS (16)
/// и change-биты (17-23) эта маска тоже обнуляет.
pub fn port_state_to_neutral(raw: u32) -> u32 {
    const RO: u32 = (1 << 0) | (1 << 3) | (0xf << 10) | (1 << 30);
    const RWS: u32 = (0xf << 5) | (1 << 9) | (0x3 << 14) | (0x7 << 25);
    (raw & RO) | (raw & RWS)
}

/// Одно чтение PORTSC, декодированное драйвером.
///
/// `raw` хранится, потому что диагностика показывает полное значение регистра:
/// `pre`/`post`/`ped_sc` имеют смысл только со всеми битами (CCS, PED, PLS,
/// speed, PR, PRC, CSC).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortObservation {
    pub ccs: bool,
    pub ped: bool,
    pub pp: bool,
    pub speed: u8,
    pub pls: u8,
    /// Port Reset Change был установлен в этом чтении (sticky, write-1-to-clear).
    pub prc: bool,
    /// Connect Status Change был установлен в этом чтении (sticky, write-1-to-clear).
    pub csc: bool,
    /// Port Reset (бит 4): контроллер всё ещё выполняет reset.
    pub pr: bool,
    pub raw: u32,
}

impl PortObservation {
    pub const fn new(
        ccs: bool,
        ped: bool,
        pp: bool,
        speed: u8,
        pls: u8,
        prc: bool,
        csc: bool,
        pr: bool,
        raw: u32,
    ) -> Self {
        Self {
            ccs,
            ped,
            pp,
            speed,
            pls,
            prc,
            csc,
            pr,
            raw,
        }
    }

    /// Порт включён и активен: PED, U0, валидный speed id и не внутри reset.
    pub const fn enabled(&self) -> bool {
        self.ccs && self.ped && self.speed != 0 && self.pls == PLS_U0 && !self.pr
    }

    /// Suspended, но устройство включено: нужен resume, а не reset.
    pub const fn suspended(&self) -> bool {
        self.ccs && self.ped && self.speed != 0 && self.pls == PLS_U3 && !self.pr
    }

    /// Устройство всё ещё обнаруживается/debounce-ится/тренируется.
    #[allow(dead_code)] // public vocabulary for host tests/decoders
    pub const fn detecting(&self) -> bool {
        self.ccs && !self.ped && (self.pls == PLS_POLLING || self.pls == PLS_RXDETECT)
    }
}

/// Что драйвер должен сделать после одного [`PortPolicy::step`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    /// На этом poll делать нечего. Политика ждёт времени или следующего PORTSC.
    Idle,
    /// Поднять PP (питание порта) и прочитать обратно. Затем политика ждёт
    /// [`POWER_SETTLE_US`] перед debounce подключения.
    PowerOn,
    /// Записать PR (USB2) или WPR (USB3) на neutral-значении PORTSC. Затем политика
    /// ждёт завершения reset (PR clear + CCS) и PED, до [`HUB_RESET_TIMEOUT_US`] на
    /// запись и [`PORT_RESET_TRIES`] записей.
    Reset { warm: bool },
    /// Порт включён, но suspended: выполнить программный resume.
    Resume,
    /// Порт включён и стабилен: адресовать/перечислить его на `speed`.
    Enumerate { speed: u8 },
    /// Попытка не удалась; диагностика должна записать `reason` и повторить после
    /// backoff политики (до этого шаги возвращают [`Step::Idle`]).
    Failed(FailReason),
}

/// Почему закончилась попытка порта. [`FailReason::stage`] — строка, которую
/// печатают boot-меню и shell, и её же называет serial-лог.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailReason {
    /// Reset так и не включил порт за свои 800-мс окна.
    ResetTimeout,
    /// Reset завершился, но PED+U0 не удержался [`STABLE_US`].
    LostAfterReset,
    /// Устройство исчезло во время попытки.
    Disconnected,
    /// Подключение не удержалось [`HUB_DEBOUNCE_STABLE_US`].
    DebounceTimeout,
    /// Программный resume не дошёл до U0.
    ResumeLost,
    /// Порт включился, но перечисление (дескрипторы/конфигурация) не удалось.
    EnumFailed,
    /// PP не поднялся после записи включения питания.
    NoPower,
}

impl FailReason {
    pub const fn stage(self) -> &'static str {
        match self {
            FailReason::ResetTimeout => "reset-timeout",
            FailReason::LostAfterReset => "lost-after-reset",
            FailReason::Disconnected => "no-device",
            FailReason::DebounceTimeout => "debounce-timeout",
            FailReason::ResumeLost => "resume-lost",
            FailReason::EnumFailed => "enum-failed",
            FailReason::NoPower => "no-power",
        }
    }

    const fn needs_reset_before_retry(self) -> bool {
        // Отказ перечисления происходит на ВКЛЮЧЁННОМ порту: линк в порядке, но
        // состояние устройства/эндпоинта — нет, поэтому следующая попытка начинается с
        // reset. Потерянное включение — обратное: debounce и reset с нуля (hub workqueue
        // делает так же после неудачного reset).
        matches!(self, FailReason::EnumFailed)
    }
}

/// Видимая фаза — для диагностики и host-тестов. (Поля таймингов приватны,
/// поэтому единственный способ продвинуть машину — [`PortPolicy::step`].)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    /// Ещё не наблюдался (или появилось новое устройство).
    Idle,
    /// Ожидание [`POWER_SETTLE_US`] после поднятия PP.
    PowerSettle,
    /// Ничего не подключено; перечитывать раз в [`EMPTY_RETRY_US`].
    Empty,
    /// Сэмплировать CCS каждые [`HUB_DEBOUNCE_STEP_US`], пока подключение не станет
    /// неизменным [`HUB_DEBOUNCE_STABLE_US`] (`hub_port_debounce`).
    Debounce,
    /// PR/WPR выдан: ждать PR clear + CCS, затем PED, до [`HUB_RESET_TIMEOUT_US`] на
    /// запись (`hub_port_wait_reset`).
    ResetWait,
    /// PED+U0 виден: требовать [`STABLE_US`] непрерывного включения.
    Stabilize,
    /// Неудачная попытка: тишина до дедлайна backoff.
    Backoff,
    /// Идёт программный resume.
    ResumeWait,
    /// Порт включён, драйвер его перечисляет (драйвер сообщает обратно через
    /// [`PortPolicy::enumerated`] или [`PortPolicy::enumeration_failed`]).
    Enumerating,
}

/// Полная политика подъёма одного корневого порта. `Copy`, без аллокаций.
#[derive(Clone, Copy)]
pub struct PortPolicy {
    phase: Phase,
    /// Абсолютный дедлайн текущей фазы в монотонных микросекундах.
    deadline_us: u64,
    /// Начало текущего окна стабильности / наблюдения.
    since_us: u64,
    /// Начало текущей попытки (reset или adopt) — для `ms=`.
    attempt_started_us: u64,
    /// Команд PR/WPR с момента загрузки (`try=` в меню).
    resets: u16,
    /// Неудачных попыток с момента загрузки (управляет backoff/диагностикой).
    failures: u16,
    /// Включение, которое перечисляется, получено НАШИМ reset (стадия `reset-ok`) или
    /// порт был принят/resume-нут (`adopted`).
    attempt_had_reset: bool,
    /// Перед следующим перечислением нужен reset (отказ перечисления).
    need_reset: bool,
    /// Порт USB3 (Supported Protocol major revision 3): reset — WPR.
    usb3: bool,
    // ── состояние hub_port_debounce ─────────────────────────────────────────────
    /// Начало текущего окна debounce (база реального времени для таймаута).
    debounce_started_us: u64,
    /// Когда пора делать следующий 25-мс сэмпл debounce.
    debounce_next_us: u64,
    /// Прошедшее время debounce (`total_time`), для лога.
    debounce_total_us: u64,
    /// Время неизменного подключения, накопленное шагами по 25 мс (`stable_time`),
    /// для лога и решения.
    debounce_stable_us: u64,
    /// Установлен ли предыдущим сэмплом наблюдаемый статус подключения.
    debounce_have_conn: bool,
    /// Состояние подключения, которое считает таймер стабильности.
    debounce_conn: bool,
    /// CSC на предыдущем сэмпле, чтобы НОВЫЙ connect change сбрасывал таймер
    /// стабильности, хотя sticky-бит до reset не очищается.
    debounce_last_csc: bool,
    // ── состояние hub_port_wait_reset ───────────────────────────────────────────
    /// Reset-записи в текущей попытке (1..=[`PORT_RESET_TRIES`]).
    reset_tries: u8,
    /// Когда произошла текущая запись reset (база расписания опросов).
    reset_wait_started_us: u64,
    /// Когда пора делать следующий опрос статуса reset.
    reset_wait_next_us: u64,
    /// Текущий интервал опроса: 10 мс для первых двух, затем 200 мс.
    reset_wait_delay_us: u64,
    /// Опросов статуса reset для текущей записи (`delay_time`).
    reset_wait_samples: u8,
    // ── диагностика последней попытки (на порт, никогда не общая) ───────────────
    seen_prc: bool,
    seen_csc: bool,
    seen_ped: bool,
    /// PED виден с PLS=U0 и валидной скоростью (настоящее включение).
    seen_ped_u0: bool,
    /// PORTSC на последнем PED+U0; до первого такого — на последнем кратковременном
    /// PED. `seen_ped_u0` говорит, какое это значение.
    ped_sc: u32,
    pre: u32,
    post: u32,
}

impl PortPolicy {
    pub const fn new(usb3: bool) -> Self {
        Self {
            phase: Phase::Idle,
            deadline_us: 0,
            since_us: 0,
            attempt_started_us: 0,
            resets: 0,
            failures: 0,
            attempt_had_reset: false,
            need_reset: false,
            usb3,
            debounce_started_us: 0,
            debounce_next_us: 0,
            debounce_total_us: 0,
            debounce_stable_us: 0,
            debounce_have_conn: false,
            debounce_conn: false,
            debounce_last_csc: false,
            reset_tries: 0,
            reset_wait_started_us: 0,
            reset_wait_next_us: 0,
            reset_wait_delay_us: HUB_SHORT_RESET_TIME_US,
            reset_wait_samples: 0,
            seen_prc: false,
            seen_csc: false,
            seen_ped: false,
            seen_ped_u0: false,
            ped_sc: 0,
            pre: 0,
            post: 0,
        }
    }

    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// Reset-команд с момента загрузки (`try=` в меню).
    pub const fn resets(&self) -> u16 {
        self.resets
    }

    /// Reset-записей в ТЕКУЩЕЙ попытке (проход `PORT_RESET_TRIES`).
    pub const fn reset_tries(&self) -> u8 {
        self.reset_tries
    }

    /// Неудачных попыток с момента загрузки.
    pub const fn failures(&self) -> u16 {
        self.failures
    }

    pub const fn attempt_started_us(&self) -> u64 {
        self.attempt_started_us
    }

    pub const fn attempt_had_reset(&self) -> bool {
        self.attempt_had_reset
    }

    pub const fn seen_prc(&self) -> bool {
        self.seen_prc
    }

    pub const fn seen_csc(&self) -> bool {
        self.seen_csc
    }

    pub const fn seen_ped(&self) -> bool {
        self.seen_ped
    }

    pub const fn seen_ped_u0(&self) -> bool {
        self.seen_ped_u0
    }

    pub const fn ped_sc(&self) -> u32 {
        self.ped_sc
    }

    pub const fn pre(&self) -> u32 {
        self.pre
    }

    pub const fn post(&self) -> u32 {
        self.post
    }

    /// Прогресс debounce (`total_time` в строке лога), микросекунды.
    pub const fn debounce_total_us(&self) -> u64 {
        self.debounce_total_us
    }

    /// Прогресс debounce (`stable_time` в строке лога), микросекунды.
    pub const fn debounce_stable_us(&self) -> u64 {
        self.debounce_stable_us
    }

    /// Продвинуть машину одним наблюдением PORTSC в момент `now_us` (любые
    /// монотонные микросекунды; драйвер использует калиброванный TSC). Возвращает
    /// единственный побочный эффект, который драйвер обязан выполнить; всё ожидание —
    /// повторный вызов `step` позже, никогда не блокировка.
    pub fn step(&mut self, obs: PortObservation, now_us: u64) -> Step {
        self.observe(obs);
        match self.phase {
            Phase::Idle => self.begin(obs, now_us),
            Phase::PowerSettle => {
                if now_us < self.deadline_us {
                    Step::Idle
                } else {
                    self.begin(obs, now_us)
                }
            }
            Phase::Empty => {
                if !obs.pp {
                    // Никогда не оставлять порт обесточенным: PP здесь только поднимается.
                    self.begin(obs, now_us)
                } else if !obs.ccs {
                    if now_us >= self.deadline_us {
                        self.deadline_us = now_us.saturating_add(EMPTY_RETRY_US);
                    }
                    Step::Idle
                } else {
                    // Устройство появилось, пока никто не смотрел: наблюдать его как первое
                    // появление (никогда не сбрасывать сразу).
                    self.begin(obs, now_us)
                }
            }
            Phase::Debounce => self.debounce_step(obs, now_us),
            Phase::ResetWait => self.reset_wait_step(obs, now_us),
            Phase::Stabilize => {
                if obs.enabled() {
                    if now_us.saturating_sub(self.since_us) >= STABLE_US {
                        return self.enumerate(obs, now_us);
                    }
                    Step::Idle
                } else if obs.suspended() {
                    self.resume(now_us)
                } else {
                    // Включение не удержалось. НЕ сбрасывать здесь: отступить на ≥1 с, затем
                    // debounce и (только если всё ещё не включён) reset снова. Это путь потери PED.
                    self.fail(obs, now_us, FailReason::LostAfterReset)
                }
            }
            Phase::Backoff => {
                if now_us < self.deadline_us {
                    return Step::Idle;
                }
                // Никогда не делать power-cycle: перечитать и дать обычному пути поднять PP,
                // только если он действительно потерян.
                self.begin(obs, now_us)
            }
            Phase::ResumeWait => {
                if obs.enabled() {
                    // Resume уже подождал свои 20 мс; включение всё равно должно выдержать то же
                    // окно стабильности, что и любое другое.
                    self.stabilize(obs, now_us)
                } else if now_us >= self.deadline_us {
                    self.fail(obs, now_us, FailReason::ResumeLost)
                } else {
                    Step::Idle
                }
            }
            Phase::Enumerating => {
                // Драйвер сообщает `enumerated`/`enumeration_failed`; до этого делать дважды
                // одно и то же нечего.
                Step::Idle
            }
        }
    }

    /// Драйвер успешно перечислил порт.
    pub fn enumerated(&mut self) {
        self.phase = Phase::Enumerating;
        self.need_reset = false;
    }

    /// Перечисление ВКЛЮЧЁННОГО порта не удалось (путь дескрипторов/конфигурации).
    /// Следующая попытка начнётся с reset после backoff.
    pub fn enumeration_failed(&mut self, obs: PortObservation, now_us: u64) -> Step {
        self.fail(obs, now_us, FailReason::EnumFailed)
    }

    /// Драйвер не смог выполнить побочный эффект (PP не поднялся, запись resume
    /// не прошла, повторное чтение после reset потеряло порт). Отказ записывается и
    /// планируется backoff точно так же, как для внутренне обнаруженного.
    pub fn fail(&mut self, obs: PortObservation, now_us: u64, reason: FailReason) -> Step {
        self.post = obs.raw;
        self.phase = Phase::Backoff;
        self.failures = self.failures.saturating_add(1);
        self.need_reset = reason.needs_reset_before_retry();
        self.attempt_had_reset = false;
        self.reset_tries = 0;
        self.deadline_us = now_us.saturating_add(backoff_us(self.failures));
        Step::Failed(reason)
    }

    // ── внутреннее ──────────────────────────────────────────────────────────────

    /// Обновить снимок диагностики попытки из этого наблюдения.
    fn observe(&mut self, obs: PortObservation) {
        if obs.prc {
            self.seen_prc = true;
        }
        if obs.csc {
            self.seen_csc = true;
        }
        if obs.ped {
            self.seen_ped = true;
            if obs.enabled() {
                self.seen_ped_u0 = true;
                // Последний снимок настоящего включения. По построению на порт: каждая
                // PortPolicy — собственное состояние одного порта.
                self.ped_sc = obs.raw;
            } else if !self.seen_ped_u0 {
                // До первого U0 хранить ПОСЛЕДНИЙ кратковременный PED, а не первый: PED во
                // время reset — не включение, и последнее значение полезнее.
                self.ped_sc = obs.raw;
            }
        }
    }

    /// Первое решение после Idle/Empty/Backoff/PowerSettle: включить питание при
    /// необходимости, принять, resume или начать debounce детектящегося порта.
    /// НИКОГДА не сбрасывать порт до того, как его подключение стабильно
    /// [`HUB_DEBOUNCE_STABLE_US`].
    fn begin(&mut self, obs: PortObservation, now_us: u64) -> Step {
        // Сначала питание порта, даже если ничего не подключено: корневой hub включает
        // все порты при инициализации, а обесточенный порт не сообщает подключения
        // вообще, поэтому клавиатура на порту, оставленном HCRST без питания, иначе
        // осталась бы невидимой навсегда.
        if !obs.pp {
            self.phase = Phase::PowerSettle;
            self.deadline_us = now_us.saturating_add(POWER_SETTLE_US);
            self.attempt_had_reset = false;
            return Step::PowerOn;
        }
        if !obs.ccs {
            self.phase = Phase::Empty;
            self.deadline_us = now_us.saturating_add(EMPTY_RETRY_US);
            self.attempt_had_reset = false;
            return Step::Idle;
        }
        if obs.enabled() {
            if self.need_reset {
                return self.start_reset(obs, now_us);
            }
            // Уже включён (прошивка оставила, или восстановился за backoff): принять только
            // после того же непрерывного окна U0.
            return self.stabilize(obs, now_us);
        }
        if obs.suspended() {
            self.phase = Phase::ResumeWait;
            self.deadline_us = now_us.saturating_add(RESUME_WAIT_US);
            self.attempt_had_reset = false;
            return Step::Resume;
        }
        // Подключён, но не включён: сначала `hub_port_debounce` (шаг 25 мс, 100 мс
        // стабильности, бюджет 2 с), и только затем reset.
        self.start_debounce(obs, now_us);
        Step::Idle
    }

    fn start_debounce(&mut self, obs: PortObservation, now_us: u64) {
        self.phase = Phase::Debounce;
        self.debounce_started_us = now_us;
        self.debounce_next_us = now_us.saturating_add(HUB_DEBOUNCE_STEP_US);
        self.debounce_total_us = 0;
        self.debounce_stable_us = 0;
        // Наблюдение, начавшее debounce, — первое чтение `usb_hub_port_status()`: оно
        // устанавливает `connection`, поэтому ровный порт завершает
        // HUB_DEBOUNCE_STABLE через 100 мс от этой точки, а не через 125.
        self.debounce_have_conn = true;
        self.debounce_conn = obs.ccs;
        self.debounce_last_csc = obs.csc;
        self.since_us = now_us;
    }

    /// `hub_port_debounce`: сэмпл каждые 25 мс; подключение должно быть неизменным
    /// 100 мс. Новый CSC или изменение CCS перезапускают таймер. Когда подключение
    /// стабильно, решить: принять включение, resume при suspend или (только теперь)
    /// сбросить всё ещё выключенный порт.
    fn debounce_step(&mut self, obs: PortObservation, now_us: u64) -> Step {
        if now_us < self.debounce_next_us {
            return Step::Idle;
        }
        self.debounce_next_us = now_us.saturating_add(HUB_DEBOUNCE_STEP_US);
        self.debounce_total_us = now_us.saturating_sub(self.debounce_started_us);

        let csc_edge = obs.csc && !self.debounce_last_csc;
        self.debounce_last_csc = obs.csc;
        let conn = obs.ccs;
        if !self.debounce_have_conn {
            self.debounce_have_conn = true;
            self.debounce_conn = conn;
            self.debounce_stable_us = 0;
        } else if !csc_edge && conn == self.debounce_conn {
            self.debounce_stable_us = self
                .debounce_stable_us
                .saturating_add(HUB_DEBOUNCE_STEP_US)
                .min(HUB_DEBOUNCE_STABLE_US);
        } else {
            self.debounce_stable_us = 0;
            self.debounce_conn = conn;
        }

        if self.debounce_stable_us >= HUB_DEBOUNCE_STABLE_US {
            if !conn {
                // Стабильное ОТКЛЮЧЕНИЕ (debounce работает в обе стороны).
                self.phase = Phase::Empty;
                self.deadline_us = now_us.saturating_add(EMPTY_RETRY_US);
                self.attempt_had_reset = false;
                return Step::Idle;
            }
            if obs.enabled() {
                // Сам включился во время detect/training: принять без PR, но всё равно
                // требовать окно стабильности (PED+U0 может мелькнуть и откатиться).
                return self.stabilize(obs, now_us);
            }
            if obs.suspended() {
                return self.resume(now_us);
            }
            return self.start_reset(obs, now_us);
        }

        if now_us.saturating_sub(self.debounce_started_us) >= HUB_DEBOUNCE_TIMEOUT_US {
            return self.fail(obs, now_us, FailReason::DebounceTimeout);
        }
        Step::Idle
    }

    fn start_reset(&mut self, obs: PortObservation, now_us: u64) -> Step {
        // Повтор внутри одной попытки (внешний проход `PORT_RESET_TRIES`) сохраняет
        // часы попытки и снимок pre от первой записи.
        if self.phase != Phase::ResetWait || self.reset_tries == 0 {
            self.attempt_started_us = now_us;
            self.reset_tries = 0;
            self.pre = obs.raw;
            self.seen_prc = false;
            self.seen_csc = false;
            self.seen_ped = false;
            self.seen_ped_u0 = false;
            self.ped_sc = 0;
        }
        self.reset_tries = self.reset_tries.saturating_add(1);
        self.phase = Phase::ResetWait;
        self.attempt_had_reset = true;
        self.need_reset = false;
        self.resets = self.resets.saturating_add(1);
        self.reset_wait_started_us = now_us;
        self.reset_wait_next_us = now_us.saturating_add(HUB_SHORT_RESET_TIME_US);
        self.reset_wait_delay_us = HUB_SHORT_RESET_TIME_US;
        self.reset_wait_samples = 0;
        Step::Reset { warm: self.usb3 }
    }

    /// `hub_port_wait_reset`: опрос через 10 мс дважды, затем каждые 200 мс, до
    /// 800 мс. Успех — PR clear И подключение; затем должен быть PED, иначе reset
    /// повторяется (до пяти записей на попытку).
    fn reset_wait_step(&mut self, obs: PortObservation, now_us: u64) -> Step {
        if now_us < self.reset_wait_next_us {
            return Step::Idle;
        }
        self.reset_wait_samples = self.reset_wait_samples.saturating_add(1);
        let elapsed = now_us.saturating_sub(self.reset_wait_started_us);

        if !obs.pr && obs.ccs {
            if obs.enabled() {
                // Reset завершён, порт включён на U0 и сообщает скорость.
                self.phase = Phase::Stabilize;
                self.since_us = now_us;
                self.deadline_us = now_us.saturating_add(STABLE_US);
                self.post = obs.raw;
                return Step::Idle;
            }
            // PR clear + подключение, но пригодного включения нет: это -EBUSY, и reset
            // пишется снова.
            return self.reset_retry_or_fail(obs, now_us, FailReason::ResetTimeout);
        }

        if elapsed >= HUB_RESET_TIMEOUT_US {
            let reason = if obs.ccs {
                FailReason::ResetTimeout
            } else {
                FailReason::Disconnected
            };
            return self.reset_retry_or_fail(obs, now_us, reason);
        }

        // Два коротких опроса, затем длинный интервал (`hub.c` HUB_LONG_RESET_TIME).
        if self.reset_wait_samples >= 2 {
            self.reset_wait_delay_us = HUB_LONG_RESET_TIME_US;
        }
        self.reset_wait_next_us = now_us.saturating_add(self.reset_wait_delay_us);
        Step::Idle
    }

    fn reset_retry_or_fail(
        &mut self,
        obs: PortObservation,
        now_us: u64,
        reason: FailReason,
    ) -> Step {
        if self.reset_tries < PORT_RESET_TRIES {
            return self.start_reset(obs, now_us);
        }
        self.fail(obs, now_us, reason)
    }

    /// Начать окно непрерывного PED+U0, которое должно предшествовать перечислению.
    fn stabilize(&mut self, obs: PortObservation, now_us: u64) -> Step {
        if self.attempt_started_us == 0 {
            self.attempt_started_us = now_us;
        }
        self.phase = Phase::Stabilize;
        self.since_us = now_us;
        self.deadline_us = now_us.saturating_add(STABLE_US);
        self.post = obs.raw;
        // `stabilize` — путь adopt/resume, поэтому `attempt_had_reset` здесь уже false;
        // путь reset входит в Stabilize напрямую и сохраняет признак reset-ok.
        Step::Idle
    }

    fn enumerate(&mut self, obs: PortObservation, now_us: u64) -> Step {
        if self.attempt_started_us == 0 {
            self.attempt_started_us = now_us;
        }
        self.phase = Phase::Enumerating;
        self.post = obs.raw;
        Step::Enumerate { speed: obs.speed }
    }

    fn resume(&mut self, now_us: u64) -> Step {
        self.phase = Phase::ResumeWait;
        self.deadline_us = now_us.saturating_add(RESUME_WAIT_US);
        Step::Resume
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(
        ccs: bool,
        ped: bool,
        pp: bool,
        speed: u8,
        pls: u8,
        prc: bool,
        csc: bool,
        pr: bool,
    ) -> PortObservation {
        let mut raw = 0u32;
        if ccs {
            raw |= 1 << 0;
        }
        if ped {
            raw |= 1 << 1;
        }
        if pp {
            raw |= 1 << 9;
        }
        raw |= (speed as u32 & 0xF) << 10;
        raw |= (pls as u32 & 0xF) << 5;
        if prc {
            raw |= 1 << 21;
        }
        if csc {
            raw |= 1 << 17;
        }
        if pr {
            raw |= 1 << 4;
        }
        PortObservation::new(ccs, ped, pp, speed, pls, prc, csc, pr, raw)
    }

    fn normal(pls: u8) -> PortObservation {
        obs(true, false, true, 0, pls, false, false, false)
    }

    fn enabled(speed: u8) -> PortObservation {
        obs(true, true, true, speed, PLS_U0, false, false, false)
    }

    /// Провести детектящийся порт через полный стабильный debounce (четыре
    /// посчитанных сэмпла по 25 мс после первого установочного).
    fn stable_debounce(p: &mut PortPolicy, now: &mut u64, pls: u8) {
        assert_eq!(p.step(normal(pls), *now), Step::Idle);
        for _ in 0..5 {
            *now += HUB_DEBOUNCE_STEP_US;
            let step = p.step(normal(pls), *now);
            if !matches!(step, Step::Idle) {
                return;
            }
        }
    }

    #[test]
    fn neutral_write_never_carries_ped_pr_or_change_bits() {
        // Сырой PORTSC: CCS+PED+PP+speed=3+U0+PRC+CSC+PR+WPR.
        let mut raw = 0u32;
        raw |= 1 << 0; // CCS
        raw |= 1 << 1; // PED (write 1 disables!)
        raw |= 1 << 3;
        raw |= 0xF << 5; // PLS
        raw |= 1 << 9; // PP
        raw |= 3 << 10; // speed
        raw |= 1 << 16; // LWS
        raw |= 1 << 17; // CSC
        raw |= 1 << 21; // PRC
        raw |= 1 << 31; // WPR
        raw |= 1 << 4; // PR
        let neutral = port_state_to_neutral(raw);
        assert_eq!(neutral & (1 << 1), 0, "PED must never be written back");
        assert_eq!(neutral & (1 << 4), 0, "PR must not be re-issued");
        assert_eq!(neutral & (1 << 31), 0, "WPR must not be re-issued");
        assert_eq!(neutral & (1 << 16), 0, "LWS must not be set implicitly");
        assert_eq!(neutral & (1 << 17), 0, "CSC is write-1-to-clear");
        assert_eq!(neutral & (1 << 21), 0, "PRC is write-1-to-clear");
        // Read-only статус / нейтральные при записи биты сохраняются.
        assert_eq!(neutral & 1, 1, "CCS is read-only");
        assert_eq!(neutral & (0xF << 10), 3 << 10, "speed is read-only");
        assert_eq!(neutral & (0xF << 5), 0xF << 5, "PLS is neutral with LWS=0");
        assert_eq!(neutral & (1 << 9), 1 << 9, "PP survives");
        // Reset-запись — neutral | PR: reset не должен отключать порт.
        let reset = neutral | (1 << 4);
        assert_eq!(reset & (1 << 1), 0);
    }

    #[test]
    fn debounce_needs_100ms_of_unchanged_connection() {
        let mut p = PortPolicy::new(false);
        let poll = normal(PLS_POLLING);
        let mut now = 0u64;
        assert_eq!(p.step(poll, now), Step::Idle);
        assert_eq!(p.phase(), Phase::Debounce);
        // Сэмплы на 25/50/75 мс: всё ещё внутри 100-мс окна стабильности.
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            assert_eq!(p.step(poll, now), Step::Idle);
            assert_eq!(p.resets(), 0);
        }
        // Четвёртый посчитанный сэмпл завершает 100 мс стабильного подключения, и
        // только тогда пишется PR (`hub_port_debounce` → `hub_port_reset`).
        now += HUB_DEBOUNCE_STEP_US;
        assert_eq!(p.step(poll, now), Step::Reset { warm: false });
        assert_eq!(p.resets(), 1);
        assert_eq!(p.reset_tries(), 1);
    }

    #[test]
    fn a_new_csc_edge_restarts_the_debounce_timer() {
        let mut p = PortPolicy::new(false);
        let poll = normal(PLS_POLLING);
        let mut now = 0u64;
        assert_eq!(p.step(poll, now), Step::Idle);
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            assert_eq!(p.step(poll, now), Step::Idle);
        }
        // Connect change на 100 мс: таймер стабильности перезапускается, поэтому reset,
        // который был уже должен, не происходит.
        now += HUB_DEBOUNCE_STEP_US;
        let with_csc = obs(true, false, true, 0, PLS_POLLING, false, true, false);
        assert_eq!(p.step(with_csc, now), Step::Idle);
        assert_eq!(p.resets(), 0);
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            assert_eq!(p.step(poll, now), Step::Idle);
            assert_eq!(p.resets(), 0);
        }
        now += HUB_DEBOUNCE_STEP_US;
        assert_eq!(p.step(poll, now), Step::Reset { warm: false });
    }

    #[test]
    fn connection_that_never_settles_times_out_after_2s() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        // CCS переключается на каждом сэмпле: stable_time никогда не достигнет 100 мс.
        let mut ccs = true;
        // 80 сэмплов покрывают ровно 2-с бюджет debounce.
        for _ in 0..80 {
            assert_eq!(
                p.step(
                    obs(ccs, false, true, 0, PLS_POLLING, false, false, false),
                    now
                ),
                Step::Idle,
                "a bouncing port is never reset"
            );
            ccs = !ccs;
            now += HUB_DEBOUNCE_STEP_US;
        }
        assert_eq!(now, HUB_DEBOUNCE_TIMEOUT_US);
        let step = p.step(
            obs(ccs, false, true, 0, PLS_POLLING, false, false, false),
            now,
        );
        assert_eq!(step, Step::Failed(FailReason::DebounceTimeout));
        assert_eq!(p.resets(), 0);
    }

    #[test]
    fn self_enabled_port_is_adopted_without_any_reset() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        assert_eq!(p.step(normal(PLS_POLLING), now), Step::Idle);
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            assert_eq!(p.step(normal(PLS_POLLING), now), Step::Idle);
        }
        // PHY завершает тренировку сам до окончания debounce.
        now += HUB_DEBOUNCE_STEP_US;
        let en = enabled(3);
        assert_eq!(p.step(en, now), Step::Idle); // starts the 200 ms stability window
        assert_eq!(p.phase(), Phase::Stabilize);
        assert_eq!(p.step(en, now + STABLE_US - 1), Step::Idle);
        assert_eq!(p.step(en, now + STABLE_US), Step::Enumerate { speed: 3 });
        assert_eq!(p.resets(), 0, "a self-enabled port must not be reset");
    }

    #[test]
    fn reset_wait_follows_hub_polling_and_never_re_resets_inside_one_wait() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        stable_debounce(&mut p, &mut now, PLS_POLLING);
        // `stable_debounce` остановился, как только появился шаг Reset.
        assert_eq!(p.resets(), 1);
        // Первый опрос статуса через 10 мс всё ещё видит PR.
        let in_reset = obs(true, false, true, 0, PLS_POLLING, true, false, true);
        assert_eq!(p.step(in_reset, now + 5_000), Step::Idle);
        assert_eq!(p.step(in_reset, now + HUB_SHORT_RESET_TIME_US), Step::Idle);
        // Второй опрос через 20 мс после записи: PR clear, подключение, PED+U0 на FS.
        let done = obs(true, true, true, 1, PLS_U0, true, false, false);
        assert_eq!(p.step(done, now + 2 * HUB_SHORT_RESET_TIME_US), Step::Idle);
        assert_eq!(p.phase(), Phase::Stabilize);
        assert_eq!(p.resets(), 1, "no second reset inside the wait");
        // Включение должно продержаться все 200 мс до перечисления.
        let t = now + 2 * HUB_SHORT_RESET_TIME_US;
        assert_eq!(p.step(done, t + STABLE_US - 1), Step::Idle);
        assert_eq!(p.step(done, t + STABLE_US), Step::Enumerate { speed: 1 });
        assert_eq!(p.resets(), 1);
    }

    #[test]
    fn reset_without_enable_is_written_again_up_to_five_times() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        stable_debounce(&mut p, &mut now, PLS_POLLING);
        assert_eq!(p.resets(), 1);
        // PR очищается, CCS остаётся, но PED не появляется: это -EBUSY и повторная
        // запись reset, пока не кончится PORT_RESET_TRIES.
        let no_enable = obs(true, false, true, 0, PLS_POLLING, true, false, false);
        for expected in 2..=PORT_RESET_TRIES {
            now += HUB_SHORT_RESET_TIME_US;
            assert_eq!(
                p.step(no_enable, now),
                Step::Reset { warm: false },
                "reset write #{expected}"
            );
            assert_eq!(p.resets(), expected as u16);
            assert_eq!(p.reset_tries(), expected);
        }
        // Пятое ожидание тоже без включения: попытка неудачна.
        now += HUB_SHORT_RESET_TIME_US;
        assert_eq!(
            p.step(no_enable, now),
            Step::Failed(FailReason::ResetTimeout)
        );
        assert_eq!(p.phase(), Phase::Backoff);
        assert_eq!(p.failures(), 1);
    }

    #[test]
    fn a_reset_that_never_completes_waits_800ms_per_try() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        stable_debounce(&mut p, &mut now, PLS_POLLING);
        let in_reset = obs(true, false, true, 0, PLS_POLLING, true, false, true);
        let reset_at = now;
        // 10 мс, 20 мс, затем интервал 200 мс; на 820 мс ожидание истекает, и reset
        // пишется снова.
        let mut retries = 0;
        for _ in 0..120 {
            now += 10_000;
            match p.step(in_reset, now) {
                Step::Reset { .. } => {
                    retries += 1;
                    break;
                }
                Step::Idle => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(retries, 1);
        assert!(
            now - reset_at >= HUB_RESET_TIMEOUT_US,
            "first wait lasted only {} us",
            now - reset_at
        );
    }

    #[test]
    fn lost_ped_backs_off_one_second_then_debounces_before_reset() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        stable_debounce(&mut p, &mut now, PLS_POLLING);
        let t = now;
        let en = enabled(1);
        assert_eq!(p.step(en, t + 20_000), Step::Idle);
        // PED+U0 меньше 200 мс, затем Polling.
        assert_eq!(
            p.step(normal(PLS_POLLING), t + 20_000 + 100_000),
            Step::Failed(FailReason::LostAfterReset)
        );
        assert_eq!(p.phase(), Phase::Backoff);
        // Целую секунду никакого reset.
        for delta in [1u64, 250, 500, 999] {
            assert_eq!(
                p.step(normal(PLS_POLLING), t + 20_000 + 100_000 + delta * 1_000),
                Step::Idle
            );
        }
        let after = t + 20_000 + 100_000 + RETRY_BACKOFF_US;
        // На дедлайне backoff порт снова наблюдается (debounce), а не сбрасывается сразу.
        assert_eq!(p.step(normal(PLS_POLLING), after), Step::Idle);
        assert_eq!(p.phase(), Phase::Debounce);
        let resets_before = p.resets();
        let mut t2 = after;
        for _ in 0..3 {
            t2 += HUB_DEBOUNCE_STEP_US;
            assert_eq!(p.step(normal(PLS_POLLING), t2), Step::Idle);
            assert_eq!(p.resets(), resets_before);
        }
        t2 += HUB_DEBOUNCE_STEP_US;
        assert_eq!(p.step(normal(PLS_POLLING), t2), Step::Reset { warm: false });
        assert_eq!(p.resets(), resets_before + 1);
    }

    #[test]
    fn power_on_is_retried_and_never_powered_off() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let unpowered = obs(true, false, false, 0, PLS_POLLING, false, false, false);
        assert_eq!(p.step(unpowered, now), Step::PowerOn);
        assert_eq!(p.phase(), Phase::PowerSettle);
        // Во время 100-мс power settle ничего не происходит.
        for delta in [1u64, 50, 99] {
            assert_eq!(p.step(unpowered, now + delta * 1_000), Step::Idle);
        }
        // Всё ещё обесточен на дедлайне: попросить снова (драйвер прочитал обратно).
        assert_eq!(p.step(unpowered, now + POWER_SETTLE_US), Step::PowerOn);
        // Питание есть, но пусто: пустой порт только наблюдается, никогда не сбрасывается.
        now += POWER_SETTLE_US;
        let powered_empty = obs(false, false, true, 0, PLS_POLLING, false, false, false);
        assert_eq!(p.step(powered_empty, now + POWER_SETTLE_US), Step::Idle);
        assert_eq!(p.phase(), Phase::Empty);
        assert_eq!(p.resets(), 0);
        assert_eq!(p.failures(), 0);
        // Включение питания идемпотентно: PP=1 больше не даёт PowerOn.
        for _ in 0..10 {
            now += 1_000_000;
            assert_eq!(p.step(powered_empty, now), Step::Idle);
        }
    }

    #[test]
    fn no_power_cycle_ever_after_many_failures() {
        // Провести постоянно падающий подключённый порт через множество попыток и
        // проверить, что единственный шаг с питанием — PowerOn (никогда не выключение), и
        // что ни одна попытка не делается без PP. (Шага PowerOff в enum больше нет.)
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let poll = normal(PLS_POLLING);
        let mut power_ons = 0;
        let mut resets_in_attempt = 0u16;
        let mut attempts = 0u16;
        let mut last_fail: Option<u64> = None;
        for _ in 0..400_000 {
            match p.step(poll, now) {
                Step::PowerOn => power_ons += 1,
                Step::Reset { .. } => {
                    if let Some(failed_at) = last_fail.take() {
                        assert!(
                            now - failed_at >= RETRY_BACKOFF_US,
                            "attempt restarted only {} us after failure",
                            now - failed_at
                        );
                    }
                    resets_in_attempt += 1;
                    assert!(
                        resets_in_attempt <= PORT_RESET_TRIES as u16,
                        "more than PORT_RESET_TRIES resets in one attempt"
                    );
                }
                Step::Failed(_) => {
                    attempts += 1;
                    resets_in_attempt = 0;
                    last_fail = Some(now);
                }
                Step::Idle => {}
                other => panic!("unexpected {other:?}"),
            }
            now += 1_000;
        }
        assert!(attempts >= 5, "the port keeps retrying (slowly)");
        assert_eq!(power_ons, 0, "PP=1 must never be toggled off/on");
    }

    #[test]
    fn empty_port_is_never_reset_or_power_cycled() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let empty = obs(false, false, true, 0, PLS_RXDETECT, false, false, false);
        for _ in 0..100 {
            assert_eq!(p.step(empty, now), Step::Idle);
            now += 100_000;
        }
        assert_eq!(p.resets(), 0);
        assert_eq!(p.failures(), 0);
        assert_eq!(p.phase(), Phase::Empty);
    }

    #[test]
    fn usb3_ports_use_warm_reset() {
        let mut p = PortPolicy::new(true);
        let mut now = 0u64;
        stable_debounce(&mut p, &mut now, PLS_POLLING);
        // `stable_debounce` возвращается на шаге Reset; перезапустить чисто.
        let mut q = PortPolicy::new(true);
        let mut t = 0u64;
        assert_eq!(q.step(normal(PLS_POLLING), t), Step::Idle);
        let mut last = Step::Idle;
        for _ in 0..5 {
            t += HUB_DEBOUNCE_STEP_US;
            last = q.step(normal(PLS_POLLING), t);
            if matches!(last, Step::Reset { .. }) {
                break;
            }
        }
        assert_eq!(last, Step::Reset { warm: true });
    }

    #[test]
    fn resume_is_requested_for_suspended_enabled_ports() {
        let mut p = PortPolicy::new(false);
        let suspended = obs(true, true, true, 3, PLS_U3, false, false, false);
        assert_eq!(p.step(suspended, 0), Step::Resume);
        assert_eq!(p.phase(), Phase::ResumeWait);
        let en = enabled(3);
        assert_eq!(p.step(en, 20_000), Step::Idle);
        assert_eq!(p.step(en, 20_000 + STABLE_US), Step::Enumerate { speed: 3 });
        assert_eq!(p.resets(), 0);
    }

    #[test]
    fn enumeration_failure_resets_after_backoff_not_immediately() {
        let mut p = PortPolicy::new(false);
        let en = enabled(3);
        assert_eq!(p.step(en, 0), Step::Idle);
        assert_eq!(p.step(en, STABLE_US), Step::Enumerate { speed: 3 });
        assert_eq!(
            p.enumeration_failed(en, 5_000),
            Step::Failed(FailReason::EnumFailed)
        );
        // Во время 1-с backoff всё ещё включённый порт трогать нельзя.
        assert_eq!(p.step(en, 100_000), Step::Idle);
        // На дедлайне политика сбрасывает перед новым перечислением.
        assert_eq!(
            p.step(en, 5_000 + RETRY_BACKOFF_US),
            Step::Reset { warm: false }
        );
    }

    #[test]
    fn ped_snapshot_is_per_policy_and_records_the_last_real_enable() {
        let mut hs = PortPolicy::new(false);
        let mut fs = PortPolicy::new(false);
        let transient = obs(true, true, true, 3, PLS_POLLING, true, false, false);
        let hs_enabled = enabled(3);
        let fs_enabled = enabled(1);
        hs.observe(transient);
        fs.observe(transient);
        hs.observe(hs_enabled);
        fs.observe(fs_enabled);
        assert_ne!(hs.ped_sc(), fs.ped_sc(), "per-port speed must be visible");
        assert!(hs.seen_ped_u0() && fs.seen_ped_u0());
        assert_eq!(hs.ped_sc() & 0xF << 10, 3 << 10);
        assert_eq!(fs.ped_sc() & 0xF << 10, 1 << 10);
    }

    #[test]
    fn transient_only_ped_is_flagged_as_not_u0() {
        let mut p = PortPolicy::new(false);
        p.observe(obs(true, true, true, 3, PLS_POLLING, false, false, false));
        assert!(p.seen_ped());
        assert!(!p.seen_ped_u0(), "PED without U0 is not an enable");
        p.observe(obs(true, true, true, 1, PLS_POLLING, false, false, false));
        assert_eq!(p.ped_sc() & 0xF << 10, 1 << 10);
    }

    #[test]
    fn backoff_is_flat_one_second() {
        assert_eq!(backoff_us(0), RETRY_BACKOFF_US);
        assert_eq!(backoff_us(1), RETRY_BACKOFF_US);
        assert_eq!(backoff_us(u16::MAX), RETRY_BACKOFF_US);
    }
}
