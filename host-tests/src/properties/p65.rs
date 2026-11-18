// Feature: blocking AF_UNIX delivery — сон вместо busy-spin и `SO_RCVTIMEO`,
// property `p65`.
//
// Почему это существует: у планировщика этого ядра НЕТ очередей ожидания.
// Блокирующий `recvfrom`/`recvmsg`/`read` крутил `yield_current()`, то есть
// оставлял задачу ГОТОВОЙ и немедленно получал квант снова: на одном CPU это
// выедало процессор у остальных задач, а на каждом витке брался глобальный
// compat-лок. Живой PDP-клиент (`pagh-files`) в такой позе читал события, но не
// возвращался из `poll()` и не перерисовывал кадр — но настоящая причина
// вечного `recv` другая: Rust-std ставит `SO_RCVTIMEO` из
// `set_read_timeout`, а `setsockopt` эту опцию молча проглатывал, поэтому
// `recv` не отвечал `EAGAIN` никогда.
//
// Политику проверяет host-свойство: она — единственное место, где решается
// «спать / EAGAIN / EOF / EBADF», и вызывающий (`io_sys`, `unix_sock`) только
// исполняет вердикт.

use crate::sock_wait::{
    read_step, timeout_ms_from_timeval, write_step, Deadline, ReadStep, WaitVerdict, WriteStep,
};
use proptest::prelude::*;

proptest! {
    /// `SO_RCVTIMEO` не может «выключить» владение: осиротевшая очередь — это
    /// `EBADF` при любом таймауте и любом числе живых писателей.
    #[test]
    fn closed_reader_is_ebadf_before_any_timeout(
        writers in any::<usize>(),
        timed_out in any::<bool>(),
    ) {
        prop_assert_eq!(read_step(false, writers, timed_out), ReadStep::Ebadf);
    }

    /// Закрытие пира — `Ok(0)`, даже когда таймаут уже истёк: вызывающий не
    /// должен принять нормальный конец потока за истёкший таймаут.
    #[test]
    fn peer_close_beats_timeout(timed_out in any::<bool>()) {
        prop_assert_eq!(read_step(true, 0, timed_out), ReadStep::Eof);
    }

    /// Пока очередь жива и писатели есть, истёкший таймаут — это `EAGAIN`, а
    /// не продолжение сна. Это и есть регрессия, которую ловил watchdog.
    #[test]
    fn live_queue_with_expired_timeout_is_eagain(writers in 1usize..=usize::MAX) {
        prop_assert_eq!(read_step(true, writers, true), ReadStep::TimedOut);
    }

    /// Без таймаута ожидание — СОН (а не busy-spin, который нельзя выразить в
    /// политике). Вердикт стабилен: на каждом витке `Sleep`, пока данные не
    /// пришли, то есть `recv` просыпается на ПЕРВОМ же тике после записи.
    #[test]
    fn no_timeout_sleeps_until_data(writers in 1usize..=usize::MAX, ticks in 1u64..1000) {
        for _ in 0..ticks {
            prop_assert_eq!(read_step(true, writers, false), ReadStep::Sleep);
        }
    }

    /// Зеркало для записи: владение важнее таймаута (`SO_SNDTIMEO` не может
    /// превратить закрытый пишущий fd в ожидание), истёкший таймаут — `EAGAIN`.
    #[test]
    fn write_ownership_and_timeout(referenced in any::<bool>(), timed_out in any::<bool>()) {
        let step = write_step(referenced, timed_out);
        if !referenced {
            prop_assert_eq!(step, WriteStep::Ebadf);
        } else if timed_out {
            prop_assert_eq!(step, WriteStep::TimedOut);
        } else {
            prop_assert_eq!(step, WriteStep::Sleep);
        }
    }
}

/// `setsockopt(SO_RCVTIMEO)` — `struct timeval` в миллисекунды. Отрицательные
/// компоненты отвергаются (Linux: `EINVAL`), нули означают «без таймаута», а
/// дробные части округляются вверх, чтобы `tv_usec = 1` не исчез.
#[test]
fn timeval_to_timeout_rounds_up_and_rejects_negative() {
    assert_eq!(timeout_ms_from_timeval(0, 0), Some(0));
    assert_eq!(timeout_ms_from_timeval(0, 1), Some(1));
    assert_eq!(timeout_ms_from_timeval(0, 1_500), Some(2));
    assert_eq!(timeout_ms_from_timeval(0, 100_000), Some(100));
    assert_eq!(timeout_ms_from_timeval(2, 250_000), Some(2250));
    assert_eq!(timeout_ms_from_timeval(-1, 0), None);
    assert_eq!(timeout_ms_from_timeval(0, -1), None);
}

/// Дедлайн: нулевая длительность — «без таймаута»; иначе таймаут истекает ровно
/// на своём тике и переживает переполнение счётчика тиков.
#[test]
fn deadline_expires_on_its_tick_and_wraps() {
    assert!(Deadline::from_ticks(0, 12345).is_none());
    let d = Deadline::from_ticks(100, 5_000).expect("таймаут задан");
    assert!(!d.expired(5_000));
    assert!(!d.expired(5_099));
    assert!(d.expired(5_100));
    assert!(d.expired(5_101));
    // Астрономическое переполнение: `now` меньше начала.
    let wrap = Deadline::from_ticks(100, u64::MAX - 50).expect("таймаут задан");
    assert!(!wrap.expired(u64::MAX));
    assert!(wrap.expired(100));
}

/// Решение старого `wait_verdict` не должно разойтись с новым `read_step`:
/// `read_step` — это тот же вердикт плюс таймаут.
#[test]
fn read_step_agrees_with_wait_verdict() {
    for referenced in [false, true] {
        for writers in [0usize, 1, 7] {
            let verdict = crate::sock_wait::wait_verdict(referenced, writers);
            let step = read_step(referenced, writers, false);
            match verdict {
                WaitVerdict::Continue => assert_eq!(step, ReadStep::Sleep),
                WaitVerdict::Eof => assert_eq!(step, ReadStep::Eof),
                WaitVerdict::Ebadf => assert_eq!(step, ReadStep::Ebadf),
            }
        }
    }
}