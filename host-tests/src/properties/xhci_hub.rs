// Feature: политика подъёма корневого порта xHCI
//
// Эти свойства закрепляют чистую политику `src/drivers/xhci_hub.rs`, которую
// компилирует ядро (исходник включён в этот крейт дословно). Симулированные порты
// PCH покрывают то, чего QEMU с мгновенным включением портов не воспроизводит:
// debounce, reset без завершения, потерянный PED, скачущее подключение и серии
// повторов. Каждое утверждение ниже гоняет точный исходник ядра.

use crate::xhci_hub::{
    backoff_us, port_state_to_neutral, FailReason, Phase, PortObservation, PortPolicy, Step,
    HUB_DEBOUNCE_STEP_US, HUB_DEBOUNCE_TIMEOUT_US, HUB_LONG_RESET_TIME_US, HUB_RESET_TIMEOUT_US,
    HUB_SHORT_RESET_TIME_US, PLS_POLLING, PLS_RXDETECT, PLS_U0, PORT_RESET_TRIES,
    POWER_SETTLE_US, RETRY_BACKOFF_US, STABLE_US,
};
use proptest::prelude::*;

/// Собрать сырое слово PORTSC с той же раскладкой битов, что декодирует драйвер
/// (xHCI 1.2, таблица 5-27): CCS бит 0, PED бит 1, PR бит 4, PLS биты 5-8, PP бит 9,
/// speed биты 10-13, CSC бит 17, PRC бит 21, WPR бит 31.
fn raw_portsc(ccs: bool, ped: bool, pp: bool, speed: u8, pls: u8, prc: bool, csc: bool, pr: bool) -> u32 {
    let mut raw = 0u32;
    if ccs {
        raw |= 1 << 0;
    }
    if ped {
        raw |= 1 << 1;
    }
    if pr {
        raw |= 1 << 4;
    }
    raw |= (pls as u32 & 0xF) << 5;
    if pp {
        raw |= 1 << 9;
    }
    raw |= (speed as u32 & 0xF) << 10;
    if csc {
        raw |= 1 << 17;
    }
    if prc {
        raw |= 1 << 21;
    }
    raw
}

fn observation(
    ccs: bool,
    ped: bool,
    pp: bool,
    speed: u8,
    pls: u8,
    prc: bool,
    csc: bool,
    pr: bool,
) -> PortObservation {
    PortObservation::new(
        ccs,
        ped,
        pp,
        speed,
        pls,
        prc,
        csc,
        pr,
        raw_portsc(ccs, ped, pp, speed, pls, prc, csc, pr),
    )
}

/// Подключённый порт, ещё не включённый: CCS, без PED, PP включён.
fn detecting(pls: u8) -> PortObservation {
    observation(true, false, true, 0, pls, false, false, false)
}

/// Подключённый порт, включённый на U0 со скоростью `speed`.
fn enabled(speed: u8) -> PortObservation {
    observation(true, true, true, speed, PLS_U0, true, false, false)
}

/// Порт, каким его видит первый опрос, а затем `hub_port_wait_reset`.
fn in_reset() -> PortObservation {
    observation(true, false, true, 0, PLS_POLLING, true, false, true)
}

fn reset_warm(step: Step) -> Option<bool> {
    match step {
        Step::Reset { warm } => Some(warm),
        _ => None,
    }
}

/// Провести подключённый детектящийся порт через `hub_port_debounce` и первую
/// запись reset. Возвращает время записи (все времена в мкс).
fn drive_to_first_reset(p: &mut PortPolicy) -> u64 {
    let mut now = 0u64;
    assert_eq!(p.step(detecting(PLS_POLLING), now), Step::Idle);
    for _ in 0..4 {
        now += HUB_DEBOUNCE_STEP_US;
        let step = p.step(detecting(PLS_POLLING), now);
        if matches!(step, Step::Reset { .. }) {
            assert_eq!(step, Step::Reset { warm: false });
            return now;
        }
        assert_eq!(step, Step::Idle);
    }
    panic!("debounce did not produce a reset after 100 ms of stable connection");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Запись PORTSC не должна нести PED (write-1-to-disable), PR/WPR
    /// (write-1-to-reset), LWS или write-1-to-clear change-бит. Маска
    /// `xhci_hub_state_to_neutral` оставляет только RO и RWS биты; так же должна
    /// работать наша.
    #[test]
    fn neutral_portsc_write_drops_every_write_one_to_clear_bit(raw in any::<u32>()) {
        let neutral = port_state_to_neutral(raw);
        // Биты write-1-to-clear / write-1-to-set / reserved.
        let dangerous: u32 = (1 << 1)   // PED: write 1 disables the port
            | (1 << 2)                  // RsvdZ
            | (1 << 4)                  // PR: write 1 initiates a reset
            | (1 << 16)                 // LWS
            | (1 << 17)                 // CSC
            | (1 << 18)                 // PEC
            | (1 << 19)                 // WRC
            | (1 << 20)                 // OCC
            | (1 << 21)                 // PRC
            | (1 << 22)                 // PLC
            | (1 << 23)                 // CEC
            | (1 << 24)                 // RsvdZ
            | (1 << 28)
            | (1 << 29)
            | (1 << 31);                // WPR
        prop_assert_eq!(neutral & dangerous, 0, "neutral write carried {:08x}", neutral & dangerous);
        // Read-only (CCS, бит 3, speed, бит 30) и read/write-нейтральные (PLS, PP, биты
        // 14-15, 25-27) биты должны сохраняться.
        let ro: u32 = (1 << 0) | (1 << 3) | (0xF << 10) | (1 << 30);
        let rws: u32 = (0xF << 5) | (1 << 9) | (0x3 << 14) | (0x7 << 25);
        prop_assert_eq!(neutral & (ro | rws), raw & (ro | rws));
        // Reset-запись — neutral | PR: PED по-прежнему нет.
        let reset = neutral | (1 << 4);
        prop_assert_eq!(reset & (1 << 1), 0);
        // Нейтрализация уже нейтрального значения ничего не меняет.
        prop_assert_eq!(port_state_to_neutral(neutral), neutral);
    }

    /// `hub_port_debounce`: PR не пишется до 100 мс неизменного подключения, а затем
    /// ровно один reset (не один на poll).
    #[test]
    fn reset_waits_for_100ms_of_stable_connection(
        pls in prop_oneof![Just(PLS_POLLING), Just(PLS_RXDETECT)],
    ) {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        prop_assert_eq!(p.step(detecting(pls), now), Step::Idle);
        // 75 мс стабильного подключения недостаточно: окно — 100 мс.
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            prop_assert_eq!(p.step(detecting(pls), now), Step::Idle);
            prop_assert_eq!(p.resets(), 0);
        }
        // Четвёртый 25-мс сэмпл завершает окно: один reset, а не по одному на poll.
        now += HUB_DEBOUNCE_STEP_US;
        prop_assert_eq!(p.step(detecting(pls), now), Step::Reset { warm: false });
        prop_assert_eq!(p.resets(), 1);
        prop_assert_eq!(p.phase(), Phase::ResetWait);
    }

    /// Новый фронт CSC (переподключение) перезапускает 100-мс таймер стабильности,
    /// потому что sticky-бит CSC намеренно не очищается до reset.
    #[test]
    fn csc_edge_restarts_the_stable_timer(
        after_samples in 1usize..4,
        speed in 1u8..=4,
    ) {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let poll = detecting(PLS_POLLING);
        prop_assert_eq!(p.step(poll, now), Step::Idle);
        for _ in 0..after_samples {
            now += HUB_DEBOUNCE_STEP_US;
            prop_assert_eq!(p.step(poll, now), Step::Idle);
        }
        // Сэмпл connect change: таймер стабильности перезапускается здесь.
        now += HUB_DEBOUNCE_STEP_US;
        let with_csc = observation(true, false, true, 0, PLS_POLLING, false, true, false);
        prop_assert_eq!(p.step(with_csc, now), Step::Idle);
        prop_assert_eq!(p.resets(), 0);
        // Включиться он не мог: reset будет только после ещё 100 мс.
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            prop_assert_eq!(p.step(poll, now), Step::Idle);
            prop_assert_eq!(p.resets(), 0);
        }
        // Порт сам включается в перезапущенном окне: adopt, без PR.
        let en = enabled(speed);
        now += HUB_DEBOUNCE_STEP_US;
        prop_assert_eq!(p.step(en, now), Step::Idle);
        prop_assert_eq!(p.phase(), Phase::Stabilize);
        prop_assert_eq!(p.step(en, now + STABLE_US), Step::Enumerate { speed });
        prop_assert_eq!(p.resets(), 0);
    }

    /// Порт, который включается сам после стабильного подключения, принимается без
    /// PR и только после того, как включение удержится [`STABLE_US`].
    #[test]
    fn self_enabled_port_is_adopted_without_reset(speed in 1u8..=4) {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        prop_assert_eq!(p.step(detecting(PLS_POLLING), now), Step::Idle);
        // Включиться ровно к завершению debounce.
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            prop_assert_eq!(p.step(detecting(PLS_POLLING), now), Step::Idle);
        }
        now += HUB_DEBOUNCE_STEP_US;
        let en = enabled(speed);
        prop_assert_eq!(p.step(en, now), Step::Idle);
        prop_assert_eq!(p.phase(), Phase::Stabilize);
        prop_assert_eq!(p.step(en, now + STABLE_US - 1), Step::Idle);
        prop_assert_eq!(p.step(en, now + STABLE_US), Step::Enumerate { speed });
        prop_assert_eq!(p.resets(), 0);
    }

    /// `hub_port_wait_reset`: успех — PR clear + подключение, затем PED; второй reset
    /// не пишется, пока идёт ожидание.
    #[test]
    fn reset_wait_uses_hub_poll_spacing_and_checks_ped(
        speed in 1u8..=4,
        pr_polls in 0usize..3,
    ) {
        let mut p = PortPolicy::new(false);
        let reset_at = drive_to_first_reset(&mut p);
        let mut now = reset_at;
        // Контроллер держит PR несколько коротких опросов.
        for _ in 0..pr_polls {
            now += HUB_SHORT_RESET_TIME_US;
            prop_assert_eq!(p.step(in_reset(), now), Step::Idle);
            prop_assert_eq!(p.resets(), 1, "no second reset inside a wait");
        }
        // PR очищается, подключение на месте, порт включён на U0. После первых двух
        // коротких опросов ожидание опрашивается каждые 200 мс, поэтому успешный сэмпл
        // подаётся к следующему 200-мс опросу.
        let done = enabled(speed);
        let success_at = now + HUB_LONG_RESET_TIME_US;
        prop_assert_eq!(p.step(done, success_at), Step::Idle);
        prop_assert_eq!(p.phase(), Phase::Stabilize);
        prop_assert_eq!(p.resets(), 1);
        prop_assert_eq!(
            p.step(done, success_at + STABLE_US),
            Step::Enumerate { speed }
        );
    }

    /// PR clear + подключение, но без включения — это -EBUSY: reset пишется снова,
    /// не более `PORT_RESET_TRIES` раз на попытку.
    #[test]
    fn reset_without_enable_is_retried_at_most_five_times(
        speed in 1u8..=4,
    ) {
        let _ = speed; // the port never reports a speed without PED
        let mut p = PortPolicy::new(false);
        let mut now = drive_to_first_reset(&mut p);
        let no_enable = observation(true, false, true, 0, PLS_POLLING, true, false, false);
        for expected in 2..=PORT_RESET_TRIES {
            now += HUB_SHORT_RESET_TIME_US;
            prop_assert_eq!(p.step(no_enable, now), Step::Reset { warm: false });
            prop_assert_eq!(p.resets(), expected as u16);
            prop_assert_eq!(p.reset_tries(), expected);
        }
        now += HUB_SHORT_RESET_TIME_US;
        prop_assert_eq!(p.step(no_enable, now), Step::Failed(FailReason::ResetTimeout));
        prop_assert_eq!(p.phase(), Phase::Backoff);
    }

    /// Reset, который контроллер никогда не завершает, получает `HUB_RESET_TIMEOUT` на
    /// запись, и каждая запись повторяется только после этого окна (не на каждом poll).
    #[test]
    fn a_stuck_reset_is_abandoned_after_800ms(extra in 0u64..40) {
        let mut p = PortPolicy::new(false);
        let reset_at = drive_to_first_reset(&mut p);
        let mut now = reset_at;
        let mut next_reset = None;
        for _ in 0..(100 + extra) {
            now += 10_000;
            if let Step::Reset { warm: false } = p.step(in_reset(), now) {
                next_reset = Some(now);
                break;
            }
        }
        let retried = next_reset.expect("a stuck reset must be retried");
        prop_assert!(
            retried - reset_at >= HUB_RESET_TIMEOUT_US,
            "retried after only {} us",
            retried - reset_at
        );
        prop_assert_eq!(p.reset_tries(), 2);
    }

    /// После потерянного PED политика пауза ≥1 с, затем снова debounce до нового
    /// reset — никогда не цикл каждые 124 мс.
    #[test]
    fn lost_ped_backs_off_and_reobserves_before_resetting(
        speed in 1u8..=4,
        glimpse_us in 1_000u64..STABLE_US,
    ) {
        let mut p = PortPolicy::new(false);
        let reset_at = drive_to_first_reset(&mut p);
        let t = reset_at;
        let en = enabled(speed);
        prop_assert_eq!(p.step(en, t + 20_000), Step::Idle);
        let lost_at = t + 20_000 + glimpse_us;
        prop_assert_eq!(
            p.step(detecting(PLS_POLLING), lost_at),
            Step::Failed(FailReason::LostAfterReset)
        );
        let failed_at = lost_at;
        // Внутри 1-с backoff ничего не происходит.
        for delta in [0u64, 1, 100, 500, 999] {
            prop_assert_eq!(
                p.step(detecting(PLS_POLLING), failed_at + delta * 1_000),
                Step::Idle
            );
        }
        // На дедлайне порт наблюдается (debounce), а не сбрасывается.
        let resume_at = failed_at + RETRY_BACKOFF_US;
        prop_assert_eq!(p.step(detecting(PLS_POLLING), resume_at), Step::Idle);
        prop_assert_eq!(p.phase(), Phase::Debounce);
        let resets_before = p.resets();
        let mut now = resume_at;
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            prop_assert_eq!(p.step(detecting(PLS_POLLING), now), Step::Idle);
            prop_assert_eq!(p.resets(), resets_before);
        }
        now += HUB_DEBOUNCE_STEP_US;
        prop_assert_eq!(p.step(detecting(PLS_POLLING), now), Step::Reset { warm: false });
        prop_assert!(now - failed_at >= RETRY_BACKOFF_US);
    }

    /// PP только поднимается (никогда не циклируется), power settle — 100 мс, а порт
    /// без подключения остаётся наблюдаемым, а не сбрасываемым.
    #[test]
    fn power_is_raised_once_and_never_cycled(settle_fraction in 1u64..99) {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let unpowered = observation(true, false, false, 0, PLS_POLLING, false, false, false);
        prop_assert_eq!(p.step(unpowered, now), Step::PowerOn);
        prop_assert_eq!(p.phase(), Phase::PowerSettle);
        // Во время settle ничего не пишется.
        let inside = now + settle_fraction * POWER_SETTLE_US / 100;
        prop_assert_eq!(p.step(unpowered, inside), Step::Idle);
        // На дедлайне всё ещё обесточенный порт получает запись снова.
        prop_assert_eq!(p.step(unpowered, now + POWER_SETTLE_US), Step::PowerOn);
        // Питание есть, но пусто: наблюдается, никогда не сбрасывается, и PP больше не
        // трогается (шага PowerOff/PowerCycle больше нет).
        now += POWER_SETTLE_US;
        let empty = observation(false, false, true, 0, PLS_RXDETECT, false, false, false);
        prop_assert_eq!(p.step(empty, now + POWER_SETTLE_US), Step::Idle);
        prop_assert_eq!(p.phase(), Phase::Empty);
        for _ in 0..50 {
            now += 1_000_000;
            prop_assert_eq!(p.step(empty, now), Step::Idle);
        }
        prop_assert_eq!(p.resets(), 0);
        prop_assert_eq!(p.failures(), 0);
    }

    /// Скачущее подключение никогда не сбрасывается; debounce сдаётся после 2-с
    /// бюджета и записывает отказ.
    #[test]
    fn flapping_connection_times_out_without_a_reset(_start in any::<bool>()) {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let mut ccs = true;
        prop_assert_eq!(
            p.step(
                observation(ccs, false, true, 0, PLS_POLLING, false, false, false),
                now
            ),
            Step::Idle
        );
        // Переключать на каждом 25-мс сэмпле: stable_time никогда не достигнет 100 мс.
        let mut failed = false;
        for _ in 0..80 {
            ccs = !ccs;
            now += HUB_DEBOUNCE_STEP_US;
            match p.step(
                observation(ccs, false, true, 0, PLS_POLLING, false, false, false),
                now,
            ) {
                Step::Idle => {}
                Step::Failed(FailReason::DebounceTimeout) => {
                    failed = true;
                    break;
                }
                other => panic!("unexpected step {other:?}"),
            }
            prop_assert_eq!(p.resets(), 0);
        }
        prop_assert!(failed, "a flapping port must time out");
        prop_assert!(now >= HUB_DEBOUNCE_TIMEOUT_US);
        prop_assert_eq!(p.resets(), 0);
    }

    /// Когда порт включён и стабилен, отказ перечисления сбрасывает только после
    /// такого же backoff ≥1 с, никогда сразу на включённом порту.
    #[test]
    fn enumeration_failure_resets_after_the_backoff(speed in 1u8..=4) {
        let mut p = PortPolicy::new(false);
        let en = enabled(speed);
        prop_assert_eq!(p.step(en, 0), Step::Idle);
        prop_assert_eq!(p.step(en, STABLE_US), Step::Enumerate { speed });
        prop_assert_eq!(
            p.enumeration_failed(en, 5_000),
            Step::Failed(FailReason::EnumFailed)
        );
        prop_assert_eq!(p.step(en, 5_000 + RETRY_BACKOFF_US - 1), Step::Idle);
        prop_assert_eq!(
            p.step(en, 5_000 + RETRY_BACKOFF_US),
            Step::Reset { warm: false }
        );
    }

    /// USB3-порты (Supported Protocol major revision 3) используют WPR, USB2 — PR.
    #[test]
    fn usb3_ports_use_warm_reset(pls in prop_oneof![Just(PLS_POLLING), Just(PLS_RXDETECT)]) {
        let mut p = PortPolicy::new(true);
        let mut now = 0u64;
        prop_assert_eq!(p.step(detecting(pls), now), Step::Idle);
        let mut saw = None;
        for _ in 0..10 {
            now += HUB_DEBOUNCE_STEP_US;
            if let Some(warm) = reset_warm(p.step(detecting(pls), now)) {
                saw = Some(warm);
                break;
            }
        }
        prop_assert_eq!(saw, Some(true));
    }

    /// `ped_sc` — на порт и хранит последнее настоящее PED+U0; флаг `seen_ped_u0`
    /// отличает его от кратковременного чтения PED.
    #[test]
    fn ped_snapshot_is_the_last_u0_enable_and_per_port(
        speed_a in 1u8..=4,
        speed_b in 1u8..=4,
    ) {
        let mut a = PortPolicy::new(false);
        let mut b = PortPolicy::new(false);
        let transient = observation(true, true, true, 3, PLS_POLLING, true, false, false);
        prop_assert_eq!(a.step(transient, 0), Step::Idle);
        prop_assert_eq!(b.step(transient, 0), Step::Idle);
        prop_assert!(!a.seen_ped_u0(), "PED without U0 is not an enable");
        let en_a = enabled(speed_a);
        let en_b = enabled(speed_b);
        prop_assert_eq!(a.step(en_a, 10_000), Step::Idle);
        prop_assert_eq!(b.step(en_b, 10_000), Step::Idle);
        prop_assert!(a.seen_ped_u0() && b.seen_ped_u0());
        prop_assert_eq!((a.ped_sc() >> 10) & 0xF, speed_a as u32);
        prop_assert_eq!((b.ped_sc() >> 10) & 0xF, speed_b as u32);
        prop_assert_eq!(a.ped_sc() & 1, 1, "CCS is still in the per-port snapshot");
        if speed_a != speed_b {
            prop_assert_ne!(a.ped_sc(), b.ped_sc());
        }
    }

    /// Backoff — плоская нижняя граница ≥1 с; счётчик попыток не может переполниться в
    /// цикл занятых повторов.
    #[test]
    fn backoff_is_a_flat_one_second_floor(failures in 0u16..2_000) {
        prop_assert_eq!(backoff_us(failures), RETRY_BACKOFF_US);
        prop_assert!(RETRY_BACKOFF_US >= 1_000_000);
    }
}

/// Сценарий порта, который подключён, не включается во время debounce и лишь на
/// миг даёт PED+U0 после каждого reset, а затем откатывается в Polling. Политика не
/// должна его забивать: не больше пяти reset на попытку, ≥1 с между попытками и
/// свежий 100-мс стабильный debounce перед каждой серией.
#[test]
fn lost_after_reset_retries_in_bursts() {
    let mut p = PortPolicy::new(false);
    let mut now = 0u64;
    let mut reset_times: Vec<u64> = Vec::new();
    let mut glimpse_until = 0u64;
    let mut last_fail = None::<u64>;
    let mut reset_burst = 0u16;
    while now < 30_000_000 {
        let o = if now < glimpse_until {
            enabled(1) // ped_sc=0x00200603: FS speed id 1 at U0
        } else {
            detecting(PLS_POLLING)
        };
        match p.step(o, now) {
            Step::Reset { .. } => {
                if let Some(at) = last_fail.take() {
                    assert!(now - at >= RETRY_BACKOFF_US);
                }
                reset_burst += 1;
                assert!(reset_burst <= PORT_RESET_TRIES as u16);
                reset_times.push(now);
                glimpse_until = now + 30_000;
            }
            Step::Failed(_) => {
                last_fail = Some(now);
                reset_burst = 0;
            }
            _ => {}
        }
        now += 1_000;
    }
    assert!(reset_times.len() >= PORT_RESET_TRIES as usize, "the port keeps retrying");
    // Каждый reset — либо внутри серии из 5 записей (≈10-20 мс), либо новая попытка
    // (≥1 с + debounce после последнего отказа). Каденция 124 мс невозможна: никакой
    // reset не идёт через 100..999 мс после другого.
    for pair in reset_times.windows(2) {
        let spacing = pair[1] - pair[0];
        assert!(
            spacing <= 200_000 || spacing >= 1_000_000,
            "resets {pair:?} only {spacing} us apart"
        );
    }
}

/// Стабильный подключённый порт, который включает себя только через 700 мс (медленный
/// PCH PHY), сбрасывается после 100-мс debounce, а позднее самостоятельное включение
/// всё равно принимается, когда PED+U0 держится 200 мс — позднее включение не
/// теряется в цикле reset.
#[test]
fn slow_self_enabling_port_is_adopted_after_the_reset() {
    let mut p = PortPolicy::new(false);
    let mut now = 0u64;
    let mut resets = 0;
    let mut enumerated = false;
    while now < 5_000_000 {
        let o = if now >= 700_000 {
            enabled(3)
        } else {
            detecting(PLS_POLLING)
        };
        match p.step(o, now) {
            Step::Reset { .. } => resets += 1,
            Step::Enumerate { speed } => {
                assert_eq!(speed, 3);
                enumerated = true;
                break;
            }
            _ => {}
        }
        now += 10_000;
    }
    assert!(enumerated, "the port never enumerated");
    assert!(resets >= 1, "hub.c resets a connected, non-enabled port after 100 ms");
    assert!(
        resets <= 2 * PORT_RESET_TRIES,
        "the port must not be reset over and over (resets={resets})"
    );
}

/// Пустой порт перепроверяется раз в секунду и никогда не сбрасывается.
#[test]
fn empty_port_retry_interval_is_a_second() {
    let mut p = PortPolicy::new(false);
    let empty = observation(false, false, true, 0, PLS_RXDETECT, false, false, false);
    assert_eq!(p.step(empty, 0), Step::Idle);
    assert_eq!(p.phase(), Phase::Empty);
    assert_eq!(p.step(empty, 1_000_000 - 1), Step::Idle);
    assert_eq!(p.step(empty, 1_000_000), Step::Idle);
    assert_eq!(p.resets(), 0);
}
