//! Чистое построение HTTP/1.1-запроса и разбор головы ответа.
//!
//! Здесь реализовано только **чистое** ядро `Package_Fetcher` (компонент
//! дизайна 7): allocation-aware, но без сокетов — логика, строящая `GET`-запрос и
//! разбирающая голову HTTP-ответа. Только `core` + `alloc` — без типов сокетов,
//! глобалов и железа — так что собирается и в ядро (`crate::net::http`), и в
//! `host-tests` через `#[path]`-include, позволяя свойствам P20/P21 проверять тот
//! же исходник (R11.6).
//!
//! Эффектный `fetch_deb`, качающий реальный TCP-сокет и вызывающий
//! [`parse_http_head`], подключается отдельно.
//!
//! Весь разбор закалён против враждебного ввода: каждый индекс проверен на
//! границы, срезы используют полуоткрытые диапазоны из найденных позиций, и ни
//! один путь не паникует и не читает за `buf`.
#![allow(dead_code)]

use alloc::vec::Vec;

/// Результат разбора головы (строка статуса + заголовки) HTTP-ответа.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeadParse {
    /// Буфер ещё не содержит полной головы (нет терминатора `CRLFCRLF`).
    /// Вызывающему следует прочитать ещё байты и повторить.
    Need,
    /// Разобрана полная, корректная голова.
    Done {
        /// Числовой HTTP-код статуса из строки статуса.
        status: u16,
        /// Разобранный `Content-Length`, если был корректный заголовок.
        ///
        /// `Some(n)` только когда заголовок `Content-Length` есть и его значение
        /// парсится как неотрицательное целое; `None`, когда заголовок отсутствует
        /// или не парсится (R8.3 решает, что делать с `None`).
        content_length: Option<u64>,
        /// Байтовый индекс сразу за терминатором `CRLFCRLF` — где начинается тело
        /// внутри буфера.
        body_off: usize,
    },
    /// Терминатор головы был, но строка статуса некорректна.
    Malformed,
}

/// Последовательность CRLFCRLF, завершающая голову HTTP-сообщения.
const CRLFCRLF: &[u8] = b"\r\n\r\n";

/// Разобрать голову HTTP/1.x-ответа из `buf` (R8.2, R8.3, R8.5).
///
/// Ищет терминатор `CRLFCRLF`. Если его нет, голова неполна и возвращается
/// [`HeadParse::Need`]. Иначе разбирается строка статуса (`HTTP/1.x <3-значный
/// код> ...`); строка, не начинающаяся с `HTTP/`, или с кодом не ровно из трёх
/// ASCII-цифр даёт [`HeadParse::Malformed`]. Строки заголовков разбираются
/// регистронезависимо; `Content-Length`, чьё значение парсится как неотрицательное
/// целое, ставит `content_length = Some(n)`, иначе остаётся `None`. `body_off` —
/// индекс сразу за терминатором.
///
/// Функция никогда не индексирует за `buf` и не паникует ни на каком входе.
pub fn parse_http_head(buf: &[u8]) -> HeadParse {
    let term = match find_subslice(buf, CRLFCRLF) {
        Some(i) => i,
        None => return HeadParse::Need,
    };
    let body_off = term + CRLFCRLF.len();

    // Голова — всё до терминатора CRLFCRLF. Отдельные строки разделены CRLF;
    // сама голова не содержит завершающей пустой строки.
    let head = &buf[..term];
    let mut lines = SplitCrlf::new(head);

    let status_line = match lines.next() {
        Some(line) => line,
        None => return HeadParse::Malformed,
    };
    let status = match parse_status_code(status_line) {
        Some(code) => code,
        None => return HeadParse::Malformed,
    };

    let mut content_length: Option<u64> = None;
    for line in lines {
        if let Some((name, value)) = split_header(line) {
            if eq_ascii_ci(trim_ascii(name), b"content-length") {
                // Присутствующий, но непарсящийся оставляет content_length = None.
                content_length = parse_u64(trim_ascii(value));
            }
        }
    }

    HeadParse::Done {
        status,
        content_length,
        body_off,
    }
}

/// Построить HTTP/1.1 `GET`-запрос для `path` на `host` (R8.1, Property 21).
///
/// Даёт ровно:
///
/// ```text
/// GET <path> HTTP/1.1\r\n
/// Host: <host>\r\n
/// Connection: close\r\n
/// ```
///
/// Результат всегда начинается с `GET <path> HTTP/1.1\r\n`, содержит
/// `Host: <host>\r\n` и кончается пустой строкой.
pub fn build_get_request(host: &str, path: &str) -> Vec<u8> {
    let mut req = Vec::new();
    req.extend_from_slice(b"GET ");
    req.extend_from_slice(path.as_bytes());
    req.extend_from_slice(b" HTTP/1.1\r\n");
    req.extend_from_slice(b"Host: ");
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(b"\r\n");
    req.extend_from_slice(b"Connection: close\r\n");
    req.extend_from_slice(b"\r\n");
    req
}

// ---------------------------------------------------------------------------
// Чистые хелперы разбора (bounds-checked, panic-free)
// ---------------------------------------------------------------------------

/// Вернуть индекс первого вхождения `needle` в `haystack` или `None`. Пустой
/// `needle` считается ненайденным.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let n = needle.len();
    if n == 0 || haystack.len() < n {
        return None;
    }
    let mut i = 0;
    while i + n <= haystack.len() {
        if &haystack[i..i + n] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Итератор по CRLF-разделённым строкам среза головы. Голый завершающий CRLF
/// (без следующих байтов) не даёт лишней пустой строки; промежуточные пустые
/// строки сохраняются, но в корректной голове до терминатора не встречаются.
struct SplitCrlf<'a> {
    rest: &'a [u8],
    done: bool,
}

impl<'a> SplitCrlf<'a> {
    fn new(s: &'a [u8]) -> Self {
        SplitCrlf {
            rest: s,
            done: s.is_empty(),
        }
    }
}

impl<'a> Iterator for SplitCrlf<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.done {
            return None;
        }
        match find_subslice(self.rest, b"\r\n") {
            Some(i) => {
                let line = &self.rest[..i];
                self.rest = &self.rest[i + 2..];
                if self.rest.is_empty() {
                    self.done = true;
                }
                Some(line)
            }
            None => {
                self.done = true;
                Some(self.rest)
            }
        }
    }
}

/// Разобрать числовой код статуса из строки вида `HTTP/1.x <code> [reason]`.
/// Возвращает `None`, если строка не начинается с `HTTP/`, а код — не ровно три
/// ASCII-цифры.
fn parse_status_code(line: &[u8]) -> Option<u16> {
    if !line.starts_with(b"HTTP/") {
        return None;
    }
    // Код статуса идёт после первого пробела.
    let sp = line.iter().position(|&b| b == b' ')?;
    let rest = &line[sp + 1..];
    // Токен кода кончается на следующем пробеле или конце строки.
    let code_end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
    let code = &rest[..code_end];
    if code.len() != 3 || !code.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value =
        (code[0] - b'0') as u16 * 100 + (code[1] - b'0') as u16 * 10 + (code[2] - b'0') as u16;
    Some(value)
}

/// Разбить строку заголовка на `(name, value)` по первому двоеточию или `None`,
/// если двоеточия нет.
fn split_header(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = line.iter().position(|&b| b == b':')?;
    let name = &line[..colon];
    let value = &line[colon + 1..];
    Some((name, value))
}

/// Обрезать ведущие и завершающие ASCII-пробелы и табы из байтового среза.
fn trim_ascii(mut s: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = s {
        if *first == b' ' || *first == b'\t' {
            s = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = s {
        if *last == b' ' || *last == b'\t' {
            s = rest;
        } else {
            break;
        }
    }
    s
}

/// Регистронезависимое ASCII-равенство байтовых срезов.
fn eq_ascii_ci(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Разобрать неотрицательное десятичное целое. `None` для пустого среза, любого
/// не-цифрового байта (включая знаки) или переполнения `u64`.
fn parse_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut acc: u64 = 0;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add((b - b'0') as u64)?;
    }
    Some(acc)
}
