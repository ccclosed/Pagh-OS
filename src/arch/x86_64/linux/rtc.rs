//! Читатель CMOS real-time clock.
//!
//! Ядерная эффектная половина поддержки стенных часов: выполняет port I/O к CMOS
//! RTC (порт индекса `0x70`, порт данных `0x71`) и отдаёт всю арифметику —
//! декодирование BCD и преобразование гражданской даты в Unix-секунды — чистому,
//! host-тестируемому [`super::timeconv`]. Такое разделение позволяет проверять
//! математику на хосте, а неизбежно эффектные чтения устройства держать здесь.
//!
//! ## Протокол чтения
//!
//! RTC обновляет регистры примерно раз в секунду; чтение посреди обновления может
//! вернуть несогласованные поля. Поэтому:
//!   1. ждём сброса флага «идёт обновление» (Status Register A, бит 7),
//!   2. читаем все регистры времени/даты,
//!   3. читаем их второй раз и принимаем значения, только когда два
//!      последовательных чтения совпали (стандартная защита «прочитать дважды»
//!      от обновления, проскочившего между чтениями регистров).
//!
//! Status Register B (`0x0B`) сообщает формат данных: установленный бит 2 (`DM`)
//! означает, что регистры уже двоичные; сброшенный (обычное умолчание) — BCD.
//! Установленный бит 1 (`24/12`) — 24-часовой режим; сброшенный — 12-часовой, где
//! старший бит регистра часов помечает PM.
#![allow(dead_code)]

use x86_64::instructions::port::Port;

use super::timeconv::{bcd_to_bin, civil_to_unix};

/// Адресный/индексный порт CMOS.
const CMOS_ADDR: u16 = 0x70;
/// Порт данных CMOS.
const CMOS_DATA: u16 = 0x71;

/// Индексы регистров RTC.
const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;

/// Бит 7 Status Register A: идёт обновление RTC.
const STATUS_A_UPDATE_IN_PROGRESS: u8 = 0x80;
/// Бит 1 Status Register B: часы в 24-часовом формате.
const STATUS_B_24_HOUR: u8 = 0x02;
/// Бит 2 Status Register B: регистры уже двоичные (не BCD).
const STATUS_B_BINARY: u8 = 0x04;
/// Старший бит регистра часов, помечающий PM в 12-часовом режиме.
const HOUR_PM_FLAG: u8 = 0x80;

/// Сырые значения регистров, прочитанные одним согласованным снимком.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RtcRaw {
    sec: u8,
    min: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
}

/// Прочитать один регистр CMOS по индексу. Отключать NMI здесь не нужно.
fn read_cmos(reg: u8) -> u8 {
    // SAFETY: порты CMOS index/data — фиксированные legacy-I/O порты; выбор
    // регистра и чтение порта данных не влияют на память.
    unsafe {
        let mut addr = Port::<u8>::new(CMOS_ADDR);
        let mut data = Port::<u8>::new(CMOS_DATA);
        addr.write(reg);
        data.read()
    }
}

/// Идёт ли сейчас обновление RTC?
fn update_in_progress() -> bool {
    read_cmos(REG_STATUS_A) & STATUS_A_UPDATE_IN_PROGRESS != 0
}

/// Взять один сырой снимок регистров (без защиты от идущего обновления).
fn read_raw() -> RtcRaw {
    RtcRaw {
        sec: read_cmos(REG_SECONDS),
        min: read_cmos(REG_MINUTES),
        hour: read_cmos(REG_HOURS),
        day: read_cmos(REG_DAY),
        month: read_cmos(REG_MONTH),
        year: read_cmos(REG_YEAR),
    }
}

/// Декодированные дата/время RTC в UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcDate {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Прочитать согласованные дату/время из CMOS RTC, декодируя BCD/двоичный и
/// 12/24-часовой формат, и вернуть [`RtcDate`] в UTC. Использует защиту
/// «прочитать дважды», поэтому обновление регистра между чтениями отвергается и
/// повторяется (с ограничением, чтобы патологический RTC не повесил загрузку).
pub fn read_date() -> RtcDate {
    // Переждать идущее обновление с щедрым пределом.
    let mut guard = 0u32;
    while update_in_progress() && guard < 1_000_000 {
        guard += 1;
    }

    // Читать дважды, пока два последовательных снимка не совпадут.
    let mut prev = read_raw();
    let mut attempts = 0u32;
    loop {
        while update_in_progress() && attempts < 1_000_000 {
            attempts += 1;
        }
        let cur = read_raw();
        if cur == prev || attempts >= 1_000_000 {
            prev = cur;
            break;
        }
        prev = cur;
        attempts += 1;
    }

    let status_b = read_cmos(REG_STATUS_B);
    let is_binary = status_b & STATUS_B_BINARY != 0;
    let is_24h = status_b & STATUS_B_24_HOUR != 0;

    let decode = |v: u8| if is_binary { v } else { bcd_to_bin(v) };

    let sec = decode(prev.sec) as u32;
    let min = decode(prev.min) as u32;

    // С часами особая осторожность: в 12-часовом BCD-режиме флаг PM едет в
    // старшем бите сырого регистра, поэтому его нужно снять до декодирования BCD
    // и учесть после.
    let raw_hour = prev.hour;
    let pm = !is_24h && (raw_hour & HOUR_PM_FLAG != 0);
    let hour_val = decode(raw_hour & !HOUR_PM_FLAG) as u32;
    let hour = if is_24h {
        hour_val
    } else {
        // 12 часов: 12AM -> 0, 12PM -> 12, иначе прибавить 12 для PM.
        match (hour_val % 12, pm) {
            (h, false) => h,     // 1..=11 AM, и 12AM -> 0
            (h, true) => h + 12, // 1..=11 PM -> 13..=23, и 12PM -> 12
        }
    };

    let day = decode(prev.day) as u32;
    let month = decode(prev.month) as u32;
    // Двузначный год: предполагаем 2000-е (ядро нацелено на текущую эпоху).
    let year = 2000 + decode(prev.year) as i64;

    RtcDate {
        year,
        month,
        day,
        hour,
        minute: min,
        second: sec,
    }
}

/// Прочитать текущее стенное время из CMOS RTC как Unix-секунды (секунды с
/// 1970-01-01T00:00:00Z) в предположении, что RTC держит UTC.
pub fn now_unix() -> u64 {
    let d = read_date();
    let secs = civil_to_unix(d.year, d.month, d.day, d.hour, d.minute, d.second);
    if secs < 0 {
        0
    } else {
        secs as u64
    }
}
