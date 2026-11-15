//! RFC 6125 hostname-матчинг для верификатора серверных TLS-сертификатов
//! (серия issue #14) — чистый `core` + `alloc`, самодостаточный, panic-free.
//!
//! Верификатор вызывает [`hostname_matches`] против каждой SAN-записи
//! `dNSName` серверного сертификата и [`ip_matches`] против записей `iPAddress`,
//! когда цель соединения была IP-литералом ([`is_ip_literal`]).
//!
//! Поддерживаемое подмножество (намеренно строгое — всё вне его fail closed,
//! то есть НЕТ совпадения, никогда не мягкое совпадение):
//!
//!   * регистронезависимое ASCII-сравнение по лейблам (`[u8]::eq_ignore_ascii_case`);
//!   * wildcard `*` ТОЛЬКО как целиком левый лейбл SAN-шаблона, совпадающий
//!     ровно с одним лейблом (`*.example.com` совпадает с `a.example.com`,
//!     НЕ с `b.a.example.com`, НЕ с `example.com`);
//!   * частичные wildcard'ы в лейбле (`w*.example.com`) и wildcard'ы не слева
//!     (`www.*.com`) не поддерживаются → никогда не совпадают;
//!   * `*` внутри стороны HOST — некорректный ввод → никогда не совпадает;
//!   * одна завершающая точка допускается с любой стороны (корень FQDN DNS);
//!     пустые лейблы (`a..b.com`, ведущие/завершающие точки сверх той) → нет
//!     совпадения;
//!   * не-ASCII байты с любой стороны → нет совпадения. IDN должен доходить до
//!     нас как A-лейблы (xn--…); CA, выдающий сырые UTF-8 записи dNSName, вне
//!     поддерживаемой поверхности.
//!
//! Известное ограничение (задокументировано, fail-open только в смысле
//! доступности): wildcard'ы вида `*.com`, которые public-suffix-aware клиент
//! отверг бы, здесь принимаются — PSL в ядро не входит. Настоящие CA не могут
//! выпустить такие сертификаты, так что путь доверия дефолтного зеркала это не
//! ослабляет.

use alloc::vec::Vec;

/// Разбить имя с точками на лейблы; `None`, когда есть пустой лейбл
/// (`a..b.com`, ведущая точка или завершающая сверх одной допустимой).
fn labels(name: &[u8]) -> Option<Vec<&[u8]>> {
    let ls: Vec<&[u8]> = name.split(|&b| b == b'.').collect();
    if ls.iter().any(|l| l.is_empty()) {
        return None;
    }
    Some(ls)
}

/// Одна допустимая завершающая точка (RFC 6125 §6.4.2 допускает форму FQDN-корня).
fn strip_trailing_dot(name: &[u8]) -> &[u8] {
    match name.strip_suffix(b".") {
        Some(s) => s,
        None => name,
    }
}

/// Гейт только printable-ASCII: не-ASCII (сырой UTF-8 IDN) или пробелы/control
/// fail closed.
fn is_ascii_name(name: &[u8]) -> bool {
    name.iter().all(|&b| (0x21..=0x7e).contains(&b))
}

/// Регистронезависимое равенство двух списков лейблов (к этому месту все ASCII).
fn labels_eq(a: &[&[u8]], b: &[&[u8]]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Сопоставить DNS `host` с SAN-записью `dNSName` сертификата
/// (RFC 6125 §6.4). Поддерживаемое подмножество — в docs модуля.
pub fn hostname_matches(host: &[u8], san: &[u8]) -> bool {
    if !is_ascii_name(host) || !is_ascii_name(san) {
        return false;
    }
    let host = strip_trailing_dot(host);
    let san = strip_trailing_dot(san);
    let (hl, sl) = match (labels(host), labels(san)) {
        (Some(h), Some(s)) => (h, s),
        _ => return false,
    };
    if hl.is_empty() || sl.is_empty() {
        return false;
    }

    // `*` на стороне HOST — некорректный ввод: host — имя, которое
    // ПРОВЕРЯЮТ, никогда не шаблон. Это должно выполняться до ветки wildcard —
    // иначе host `a.*.example.com` проскочил бы SAN-шаблон `*.example.com`
    // (там сравнивается только hl[1..], hl[0] не проверен).
    if hl.iter().any(|l| l.contains(&b'*')) {
        return false;
    }

    // Wildcard: ТОЛЬКО целиком левый SAN-лейбл, совпадающий ровно с одним
    // лейблом host. `*` в любом другом месте SAN тоже никогда не совпадает.
    if sl[0] == b"*" {
        return hl.len() == sl.len() && labels_eq(&hl[1..], &sl[1..]);
    }
    if sl.iter().any(|l| l.contains(&b'*')) {
        return false;
    }
    labels_eq(&hl, &sl)
}

/// Классифицировать цель соединения `host` как IPv4-литерал (четыре
/// разделённых точками десятичных октета, каждый 0..=255, не более трёх цифр).
pub fn is_ip_literal(host: &[u8]) -> bool {
    let parts: Vec<&[u8]> = host.split(|&b| b == b'.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.iter().all(u8::is_ascii_digit)
                && p.iter().fold(0u32, |v, &d| v * 10 + (d - b'0') as u32) <= 255
        })
}

/// Сопоставить IP-цель с SAN-записью `iPAddress`: обе — сырые октеты (4 для IPv4,
/// 16 для IPv6), сравниваемые побайтно.
pub fn ip_matches(host: &[u8], san: &[u8]) -> bool {
    (host.len() == 4 || host.len() == 16) && host == san
}

/// Разобрать IPv4-литерал dotted-quad в его 4 сырых октета — форму, с которой
/// [`ip_matches`] сравнивает SAN-записи `iPAddress`. `None` для всего, что не
/// является ровно четырьмя разделёнными точками десятичными октетами в диапазоне
/// (та же грамматика, что классифицирует [`is_ip_literal`], так что они не
/// расходятся).
pub fn parse_ipv4_literal(host: &[u8]) -> Option<[u8; 4]> {
    if !is_ip_literal(host) {
        return None;
    }
    let mut out = [0u8; 4];
    for (slot, part) in out.iter_mut().zip(host.split(|&b| b == b'.')) {
        // is_ip_literal гарантировал ≤ 3 ASCII-цифры; fold не может переполниться.
        *slot = part.iter().fold(0u8, |v, &d| v * 10 + (d - b'0'));
    }
    Some(out)
}

// ───────────────────────────── self-tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn m(host: &str, san: &str) -> bool {
        hostname_matches(host.as_bytes(), san.as_bytes())
    }

    #[test]
    fn exact_and_case() {
        assert!(m("www.example.com", "www.example.com"));
        assert!(m("WWW.EXAMPLE.COM", "www.example.com"));
        assert!(m("www.example.com", "WWW.Example.Com"));
        assert!(!m("www.example.com", "www.example.org"));
        assert!(!m("www.example.com", "example.com"));
    }

    #[test]
    fn wildcard_leftmost_single_label() {
        assert!(m("a.example.com", "*.example.com"));
        assert!(m("A.EXAMPLE.COM", "*.example.com"));
        // Ровно ОДИН лейбл: не глубже и не мельче.
        assert!(!m("b.a.example.com", "*.example.com"));
        assert!(!m("example.com", "*.example.com"));
        // Сам wildcard регистронезависим.
        assert!(m("a.example.com", "*.EXAMPLE.com"));
    }

    #[test]
    fn unsupported_wildcards_never_match() {
        // Частичный wildcard в лейбле.
        assert!(!m("www.example.com", "w*.example.com"));
        // Wildcard не слева.
        assert!(!m("www.example.com", "www.*.com"));
        assert!(!m("www.example.com", "www.example.*"));
        // Некорректный wildcard на стороне host — В ТОМ ЧИСЛЕ против wildcard
        // SAN, чей хвост совпадает: проверяется host, никогда шаблон.
        assert!(!m("*.example.com", "*.example.com"));
        assert!(!m("a.*.example.com", "*.example.com"));
        assert!(!m("*.example.com", "a.example.com"));
        assert!(!m("a.*.com", "a.*.com"));
    }

    #[test]
    fn trailing_dot_and_empty_labels() {
        assert!(m("www.example.com.", "www.example.com"));
        assert!(m("www.example.com", "www.example.com."));
        assert!(m("WWW.EXAMPLE.COM.", "www.EXAMPLE.com."));
        // Двойные точки / ведущие точки дают пустые лейблы → нет совпадения.
        assert!(!m("www..example.com", "www.example.com"));
        assert!(!m(".www.example.com", "www.example.com"));
        assert!(!m("www.example.com", ".www.example.com"));
        // Пустые входы.
        assert!(!m("", ""));
        assert!(!m("a", ""));
    }

    #[test]
    fn non_ascii_fails_closed() {
        // Сырой UTF-8 IDN не должен совпадать ни с чем (поверхность — A-лейблы).
        assert!(!m("пример.рф", "пример.рф"));
        assert!(!hostname_matches(b"a\xffb.com", b"a\xffb.com"));
        // A-лейблы в порядке.
        assert!(m("xn--e1afmkfd.xn--p1ai", "*.xn--p1ai"));
    }

    #[test]
    fn ip_classification_and_match() {
        assert!(is_ip_literal(b"10.0.2.15"));
        assert!(is_ip_literal(b"127.0.0.1"));
        assert!(!is_ip_literal(b"999.1.1.1"));
        assert!(!is_ip_literal(b"1.2.3.4.5"));
        assert!(!is_ip_literal(b"1.2.3"));
        assert!(!is_ip_literal(b"example.com"));
        assert!(!is_ip_literal(b""));

        // Записи iPAddress сравниваются как сырые октеты, никогда как текст.
        assert!(ip_matches(&[10, 0, 2, 15], &[10, 0, 2, 15]));
        assert!(!ip_matches(&[10, 0, 2, 15], &[10, 0, 2, 16]));
        // IPv4-host никогда не совпадает с 16-байтовой (IPv6) SAN-записью.
        assert!(!ip_matches(&[10, 0, 2, 15], &[0u8; 16]));
        // Текстовые формы НЕ должны сравниваться через ip_matches.
        assert!(!ip_matches(b"10.0.2.15", b"10.0.2.15"));
    }

    #[test]
    fn ipv4_parse_agrees_with_classification() {
        assert_eq!(parse_ipv4_literal(b"10.0.2.15"), Some([10, 0, 2, 15]));
        assert_eq!(parse_ipv4_literal(b"0.0.0.0"), Some([0, 0, 0, 0]));
        assert_eq!(parse_ipv4_literal(b"255.255.255.255"), Some([255; 4]));
        // Каждый нелитерал, отвергнутый is_ip_literal, парсится в None тоже —
        // классификатор и парсер делят одну грамматику.
        for bad in [
            &b"999.1.1.1"[..],
            b"1.2.3.4.5",
            b"1.2.3",
            b"example.com",
            b"",
            b"1.2.3.256",
        ] {
            assert!(!is_ip_literal(bad));
            assert_eq!(parse_ipv4_literal(bad), None);
        }
    }
}
