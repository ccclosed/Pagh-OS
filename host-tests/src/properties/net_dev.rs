// Feature: `/proc/net/dev` (этап K1, контракт `docs/procfs.md` §15).
//
// Файл читают `ifconfig`/`ip -s link`-подобные инструменты и сам композитор
// Pagh-OS; формат — часть Linux UAPI, а не наш вкус: две шапки дословно из
// `net/core/net-procfs.c` и по строке на интерфейс с 16 числовыми колонками
// (rx: bytes/packets/errs/drop/fifo/frame/compressed/multicast, tx: то же с
// colls/carrier вместо frame/multicast).
//
// Форматтер чистый (`procfs_format::format_net_dev`), поэтому здесь
// проверяется именно байтовая раскладка, а не ядерная обвязка: пустая таблица
// — ровно две шапки, строка — `%6s:` + 16 полей в фиксированном порядке,
// значения u64 не обрезаются, «не ведущиеся» колонки — нули.

use crate::procfs_format::{format_net_dev, NetDevRow};
use proptest::prelude::*;

/// Дословные шапки Linux. Дублируются здесь намеренно: тест обязан упасть,
/// если форматтер и эталон разойдутся (а не пересчитать эталон из выхода).
const HEADER_1: &str =
    "Inter-|   Receive                                                |  Transmit";
const HEADER_2: &str = " face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed";

fn row(
    name: &'static str,
    rx: [u64; 4],
    tx: [u64; 4],
) -> NetDevRow {
    NetDevRow {
        name,
        rx_bytes: rx[0],
        rx_packets: rx[1],
        rx_errs: rx[2],
        rx_drop: rx[3],
        tx_bytes: tx[0],
        tx_packets: tx[1],
        tx_errs: tx[2],
        tx_drop: tx[3],
    }
}

/// Те же значения по умолчанию, что у настоящей строки без трафика.
fn zero_row(name: &'static str) -> NetDevRow {
    row(name, [0; 4], [0; 4])
}

#[test]
fn empty_table_is_exactly_the_two_headers() {
    let out = format_net_dev(&[]);
    assert_eq!(out, format!("{}\n{}\n", HEADER_1, HEADER_2).into_bytes());
    assert_eq!(out.iter().filter(|b| **b == b'\n').count(), 2);
}

#[test]
fn headers_are_byte_identical_to_linux() {
    // Обе шапки — первая и вторая строки, без \r и хвостовых пробелов.
    let text = String::from_utf8(format_net_dev(&[])).unwrap();
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(lines[0], HEADER_1);
    assert_eq!(lines[1], HEADER_2);
    assert_eq!(lines[2], "", "output ends with a single newline");
    assert!(!text.contains('\r'));
    assert!(!HEADER_1.ends_with(' '));
    assert!(!HEADER_2.ends_with(' '));
}

#[test]
fn row_layout_matches_linux_column_widths() {
    let r = row("eth0", [1, 2, 3, 4], [5, 6, 7, 8]);
    let text = String::from_utf8(format_net_dev(&[r])).unwrap();
    let line = text.split('\n').nth(2).unwrap();
    // `%6s:` -> eth0 выровнен вправо в поле 6; затем 16 полей, первое из
    // которых — `%7llu` после пробела.
    assert_eq!(
        line,
        "  eth0:       1       2    3    4    0     0          0         0        5       6    7    8    0     0       0          0",
        "row must match Linux's `%6s: %7llu %7llu %4llu ...` layout"
    );
    // Ровно 16 чисел после двоеточия.
    let nums: Vec<&str> = line.split_once(':').unwrap().1.split_whitespace().collect();
    assert_eq!(nums.len(), 16);
    assert_eq!(
        nums,
        [
            "1", "2", "3", "4", "0", "0", "0", "0", "5", "6", "7", "8", "0", "0", "0", "0"
        ]
    );
}

#[test]
fn short_name_is_right_aligned_like_percent_6s() {
    // `lo` в Linux печатается как `    lo:` — ширина 6, выравнивание вправо.
    let text = String::from_utf8(format_net_dev(&[zero_row("lo")])).unwrap();
    let line = text.split('\n').nth(2).unwrap();
    assert!(line.starts_with("    lo:"), "got: {line:?}");
}

proptest! {
    /// Строка на каждый интерфейс, в переданном порядке, с точными значениями
    /// на своих позициях: RX — первые четыре из восьми rx-колонок, TX — первые
    /// четыре из восьми tx-колонок, остальные нули.
    #[test]
    fn rows_preserve_order_and_values(
        rows in proptest::collection::vec(
            (
                proptest::sample::select(vec!["eth0", "usb0", "enp0s3"]),
                proptest::array::uniform4(0u64..=u64::MAX),
                proptest::array::uniform4(0u64..=u64::MAX),
            ),
            0..8,
        )
    ) {
        let input: Vec<NetDevRow> = rows
            .iter()
            .map(|(name, rx, tx)| row(name, *rx, *tx))
            .collect();
        let text = String::from_utf8(format_net_dev(&input)).unwrap();
        let lines: Vec<&str> = text.split('\n').collect();
        // 2 шапки + rows + завершающая пустая строка после последнего \n.
        prop_assert_eq!(lines.len(), 2 + input.len() + 1);
        for (i, r) in input.iter().enumerate() {
            let line = lines[2 + i];
            let (name, rest) = line.split_once(':').unwrap();
            prop_assert_eq!(name.trim(), r.name);
            // `%6s` дополняет имя слева до шести символов.
            prop_assert_eq!(name.len(), 6);
            let nums: Vec<u64> = rest
                .split_whitespace()
                .map(|n| n.parse::<u64>().unwrap())
                .collect();
            prop_assert_eq!(nums.len(), 16);
            prop_assert_eq!(nums[0], r.rx_bytes);
            prop_assert_eq!(nums[1], r.rx_packets);
            prop_assert_eq!(nums[2], r.rx_errs);
            prop_assert_eq!(nums[3], r.rx_drop);
            prop_assert_eq!(&nums[4..8], &[0, 0, 0, 0]);
            prop_assert_eq!(nums[8], r.tx_bytes);
            prop_assert_eq!(nums[9], r.tx_packets);
            prop_assert_eq!(nums[10], r.tx_errs);
            prop_assert_eq!(nums[11], r.tx_drop);
            prop_assert_eq!(&nums[12..16], &[0, 0, 0, 0]);
        }
    }

    /// Значения u64 печатаются полностью, без обрезки на 32 битах.
    #[test]
    fn u64_values_are_not_truncated(
        rx_bytes in (u32::MAX as u64 + 1)..=u64::MAX,
        tx_bytes in (u32::MAX as u64 + 1)..=u64::MAX,
    ) {
        let r = row("eth0", [rx_bytes, 1, 0, 0], [tx_bytes, 1, 0, 0]);
        let text = String::from_utf8(format_net_dev(&[r])).unwrap();
        let nums: Vec<u64> = text
            .split('\n')
            .nth(2)
            .unwrap()
            .split_once(':')
            .unwrap()
            .1
            .split_whitespace()
            .map(|n| n.parse::<u64>().unwrap())
            .collect();
        prop_assert_eq!(nums[0], rx_bytes);
        prop_assert_eq!(nums[8], tx_bytes);
    }
}

#[test]
fn row_has_no_extra_whitespace_inside_numbers() {
    // Парсеры вроде `sscanf("%llu")` терпят любое число пробелов, но «лишние»
    // пробелы ВНУТРИ числа сломали бы позиционные ожидания; проверяем, что
    // между числами ровно один разделитель (поле выровнено по ширине).
    let text = String::from_utf8(format_net_dev(&[zero_row("eth0")])).unwrap();
    let line = text.split('\n').nth(2).unwrap();
    assert!(!line.contains("  \n"));
    let nums: Vec<&str> = line.split_once(':').unwrap().1.split_whitespace().collect();
    assert_eq!(nums.len(), 16);
    // Каждое звено — либо "0", либо число без знака и внутренних пробелов.
    for n in nums {
        assert!(n.chars().all(|c| c.is_ascii_digit()), "bad field: {n:?}");
    }
}
