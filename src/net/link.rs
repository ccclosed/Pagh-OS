//! Абстракция канального уровня: e1000 или USB RNDIS.
//!
//! Сетевой стек не должен знать, каким драйвером обслуживается Ethernet.
//! [`Link`] — это выбор одного из двух поллинг-драйверов; активный канал лежит в
//! отдельном локате, а не в [`super::Stack`], потому что его читают и `arp`, и
//! `ip` на путях передачи, где `Stack` уже заимствован изменяемо.
//!
//! Порядок выбора: e1000 (если карта найдена при загрузке), иначе USB RNDIS.
//! RNDIS может появиться позже e1000-проверки — телефон подключается после
//! запуска потока `usb`, — поэтому [`attach`] вызывается и из шага стека, пока
//! канал ещё не выбран.

use crate::drivers::e1000;
use crate::drivers::usb_rndis;
use crate::sync::spinlock::Spinlock;

/// Какой драйвер обслуживает Ethernet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// Intel 8254x (QEMU e1000 или совместимая карта).
    E1000,
    /// USB RNDIS: телефон в режиме USB-тетеринга.
    UsbRndis,
}

impl Link {
    pub const fn name(self) -> &'static str {
        match self {
            Link::E1000 => "e1000",
            Link::UsbRndis => "usb-rndis",
        }
    }

    /// Отправить один Ethernet-кадр; 0 — канал не принял кадр.
    pub fn send(self, frame: &[u8]) -> usize {
        match self {
            Link::E1000 => e1000::send(frame),
            Link::UsbRndis => usb_rndis::send(frame),
        }
    }

    /// Забрать один принятый кадр; `None` — очередь пуста.
    pub fn recv(self, dst: &mut [u8]) -> Option<usize> {
        match self {
            Link::E1000 => e1000::recv(dst),
            Link::UsbRndis => usb_rndis::recv(dst),
        }
    }
}

static ACTIVE: Spinlock<Option<Link>> = Spinlock::new(None);

/// Зафиксировать активный канал. Вызывается один раз при построении стека.
pub fn set(link: Link) {
    *ACTIVE.lock() = Some(link);
}

/// Активный канал, если стек уже выбрал его.
pub fn active() -> Option<Link> {
    *ACTIVE.lock()
}

/// Найти готовый канал: e1000 имеет приоритет, RNDIS — запасной.
///
/// Возвращает MAC, чтобы вызывающий мог построить [`super::Stack`] с правильным
/// адресом. e1000 уже мог быть подключён `net::init`; повторная проверка
/// безопасна и нужна позднему пути, когда карта/телефон появились после загрузки.
pub fn attach() -> Option<(Link, [u8; 6])> {
    if let Some(mac) = e1000::mac_address() {
        return Some((Link::E1000, mac));
    }
    if usb_rndis::is_ready() {
        if let Some(mac) = usb_rndis::mac() {
            return Some((Link::UsbRndis, mac));
        }
    }
    None
}

/// Отправить кадр через активный канал (для `arp`/`ip`, у которых нет `Stack`).
pub fn send(frame: &[u8]) -> usize {
    let link = *ACTIVE.lock();
    link.map_or(0, |l| l.send(frame))
}

/// Забрать кадр из активного канала.
pub fn recv(dst: &mut [u8]) -> Option<usize> {
    let link = *ACTIVE.lock();
    link.and_then(|l| l.recv(dst))
}
