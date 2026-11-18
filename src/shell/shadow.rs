//! `/etc/shadow`: парольные хэши и их проверка (`$6$` SHA-512-crypt).
//!
//! Чистый `core`+`alloc`-модуль (host-свойство P60): разбор семипольного
//! shadow-файла, `crypt(3)`-совместимый SHA-512 (`$6$<salt>$<hash>`) и проверка
//! пароля. VFS здесь не читается — файл читает [`super::identity`], а этот файл
//! исполняется и ядром, и host-тестами.
//!
//! ## Почему настоящий `$6$`, а не собственный формат
//!
//! `/etc/shadow` — внешний контракт: его читают и пишут привычные утилиты, а
//! хэш, придуманный только для pagh, сделал бы файл несовместимым без выгоды.
//! SHA-512-crypt из `crypt(3)` (Drepper) — то, что производит `passwd`/
//! `openssl passwd -6`; реализация здесь своя (никакого GPL-кода) и проверена
//! против независимых векторов `openssl passwd -6` в host-свойстве P60.
//!
//! ## Что намеренно не моделируется
//!
//! `login.defs`, сроки действия пароля, `gshadow` и `crypt`-методы кроме `$6$`
//! (`$1$`/`$5$`/`$y$` не принимаются — `verify_password` на них отвечает
//! «не совпало», а не паникует). Поля shadow после хэша (`lastchg`, `min`,
//! `max`, `warn`, `inactive`, `expire`, `flag`) сохраняются как есть при смене
//! пароля, но не интерпретируются.
#![allow(dead_code)]

use alloc::string::String;
use alloc::vec::Vec;
use sha2::{Digest, Sha512};

/// Максимальная длина salt'а в `$6$`: `crypt(3)` читает не больше 16 символов,
/// остальное — уже часть хэша.
pub const SALT_MAX: usize = 16;

/// Верхняя граница принимаемого пароля. Ограничение осознанное: `passwd`
/// пришёл из командной строки ядра с ограниченной длиной, а хэширование
/// длинного пароля не должно давать работу, пропорциональную вводу злоумышленника.
pub const PASSWORD_MAX: usize = 256;

/// Алфавит crypt(3) для `$6$` (порядок важен: это не стандартный base64).
const B64: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Число раундов SHA-512-crypt по спецификации (`ROUNDS` в `crypt(3)`).
const SHA512_ROUNDS: usize = 5000;

/// Одна запись `/etc/shadow`: `name:hash:lastchg:min:max:warn:inactive:expire:flag`.
///
/// Держится только `name` и хэш: остальные поля сохраняются при записи
/// ([`set_password`]), но не разбираются.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowEntry {
    pub name: String,
    pub hash: String,
}

/// Разобрать `/etc/shadow`.
///
/// Комментарии (`#`) и пустые строки пропускаются; строка без `:` или с пустым
/// именем — тоже (boot не должен падать из-за кривой строки). Дубликаты имён не
/// разрешаются: как `getspnam(3)`, берётся первое совпадение.
pub fn parse_shadow(text: &str) -> Vec<ShadowEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with('+')
            || line.starts_with('-')
        {
            continue;
        }
        let mut fields = line.splitn(3, ':');
        let Some(name) = fields.next() else {
            continue;
        };
        let Some(hash) = fields.next() else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        out.push(ShadowEntry {
            name: String::from(name),
            hash: String::from(hash),
        });
    }
    out
}

/// Хэш пользователя `name` (первое совпадение, как `getspnam(3)`).
pub fn hash_for<'a>(entries: &'a [ShadowEntry], name: &str) -> Option<&'a str> {
    entries
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.hash.as_str())
}

/// Закрыт ли пароль без возможности входа.
///
/// Linux-контракт: пустое поле, `!`, `!!`, `*` и любой из них в начале строки —
/// «вход запрещён» (так выглядят системные учётки и учётка до первого
/// `passwd`). Проверка стоит **до** разбора `$6$`, чтобы `!$6$...` тоже был
/// закрыт.
pub fn is_locked(hash: &str) -> bool {
    let hash = hash.trim();
    hash.is_empty() || hash.starts_with('!') || hash.starts_with('*')
}

/// Совпадает ли `password` с сохранённым хэшем `stored`.
///
/// * закрытый (`[`is_locked`]`) или неподдерживаемый метод — `false`;
/// * `$6$<salt>$<hash>` — пересчитать SHA-512-crypt с тем же salt'ом и сравнить
///   за постоянное время (побайтовый XOR без early-return);
/// * всё остальное (в том числе `$1$`, `$5$`, `$y$`) — `false`: fail-closed, а
///   не «пропустить, раз не поняли».
pub fn verify_password(stored: &str, password: &str) -> bool {
    let stored = stored.trim();
    if is_locked(stored) || password.len() > PASSWORD_MAX {
        return false;
    }
    let Some(rest) = stored.strip_prefix("$6$") else {
        return false;
    };
    let Some((salt, expected)) = rest.split_once('$') else {
        return false;
    };
    if salt.is_empty() || expected.len() != 86 {
        return false;
    }
    let computed = sha512_crypt(password.as_bytes(), salt.as_bytes());
    constant_time_eq(computed.as_bytes(), expected.as_bytes())
}

/// Собрать полную строку хэша `$6$<salt>$<hash>` для [`set_password`].
pub fn hash_password(password: &str, salt: &str) -> String {
    let hash = sha512_crypt(password.as_bytes(), salt.as_bytes());
    let mut out = String::from("$6$");
    out.push_str(&truncate_salt(salt));
    out.push('$');
    out.push_str(&hash);
    out
}

/// Salt из случайных байт: каждый байт → символ алфавита `crypt(3)`.
///
/// 16 байт дают 16 символов — максимальную длину `$6$`-salt'а.
pub fn salt_from_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes.iter().take(SALT_MAX) {
        out.push(B64[(*byte as usize) & 0x3f] as char);
    }
    out
}

/// Записать `hash` в поле пароля пользователя `name`, сохранив остальной файл.
///
/// Все прочие строки (включая комментарии и записи других пользователей)
/// копируются побайтово; у записи `name` заменяется только второе поле, поэтому
/// `lastchg`/`max`/`flag` не теряются. Если записи нет — она добавляется в конец
/// в Linux-форме `name:hash:0:0:99999:7:::`, чтобы `shadow` и `passwd` не
/// разъезжались (без записи `passwd` вход невозможен вообще).
pub fn set_password(text: &str, name: &str, hash: &str) -> String {
    let mut out = String::new();
    let mut replaced = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(':').collect();
        if !replaced && fields.len() >= 2 && fields[0] == name {
            out.push_str(fields[0]);
            out.push(':');
            out.push_str(hash);
            for field in &fields[2..] {
                out.push(':');
                out.push_str(field);
            }
            out.push('\n');
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        out.push_str(name);
        out.push(':');
        out.push_str(hash);
        out.push_str(":0:0:99999:7:::\n");
    }
    out
}

/// `$6$`-хэш без префикса: 86 символов crypt-base64 от 64 байт SHA-512.
///
/// Реализация по спецификации Drepper (`sha512-crypt.c`), включая
/// length-dependent битовый цикл и порядок перестановки при кодировании.
pub fn sha512_crypt(password: &[u8], salt: &[u8]) -> String {
    let salt = &salt[..salt.len().min(SALT_MAX)];
    let key = password;
    let key_len = key.len();

    // B = SHA512(key || salt || key).
    let b = sha512(&[key, salt, key]);

    // A: key || salt, затем B до длины key, затем length-dependent биты key.
    let mut ctx = Sha512::new();
    ctx.update(key);
    ctx.update(salt);
    let mut cnt = key_len;
    while cnt > 64 {
        ctx.update(b);
        cnt -= 64;
    }
    ctx.update(&b[..cnt]);
    let mut bits = key_len;
    while bits > 0 {
        if bits & 1 != 0 {
            ctx.update(b);
        } else {
            ctx.update(key);
        }
        bits >>= 1;
    }
    let mut alt: [u8; 64] = ctx.finalize().into();

    // P: key повторён key_len раз (усечённый SHA-512).
    let mut dp = Sha512::new();
    for _ in 0..key_len {
        dp.update(key);
    }
    let dp = dp.finalize();
    let p = repeat_to(&dp, key_len);

    // S: salt повторён 16 + A[0] раз (усечённый SHA-512).
    let mut ds = Sha512::new();
    for _ in 0..(16 + alt[0] as usize) {
        ds.update(salt);
    }
    let ds = ds.finalize();
    let s = repeat_to(&ds, salt.len());

    for round in 0..SHA512_ROUNDS {
        let mut round_ctx = Sha512::new();
        if round & 1 != 0 {
            round_ctx.update(&p);
        } else {
            round_ctx.update(alt);
        }
        if round % 3 != 0 {
            round_ctx.update(&s);
        }
        if round % 7 != 0 {
            round_ctx.update(&p);
        }
        if round & 1 != 0 {
            round_ctx.update(alt);
        } else {
            round_ctx.update(&p);
        }
        alt = round_ctx.finalize().into();
    }

    encode_crypt_base64(&alt)
}

/// Усечь salt до [`SALT_MAX`] (и вернуть как `&str` для печати).
fn truncate_salt(salt: &str) -> String {
    let end = salt
        .char_indices()
        .nth(SALT_MAX)
        .map(|(i, _)| i)
        .unwrap_or(salt.len());
    String::from(&salt[..end])
}

/// Повторить дайджест до `len` байт (P/S-последовательности crypt(3)).
fn repeat_to(digest: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let take = (len - out.len()).min(digest.len());
        out.extend_from_slice(&digest[..take]);
    }
    out
}

fn sha512(parts: &[&[u8]]) -> [u8; 64] {
    let mut ctx = Sha512::new();
    for part in parts {
        ctx.update(part);
    }
    ctx.finalize().into()
}

/// Сравнение без early-return: длина уже проверена вызывающим, но XOR-аккумулятор
/// не даёт времени ответа зависеть от позиции первого несовпадения.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Порядок перестановки байт дайджеста в crypt-base64 (Drepper).
///
/// Здесь только 21 полная группа по 4 символа; хвостовые 2 символа — особый
/// случай: `crypt(3)` кодирует ими **один** байт `digest[63]` с ведущими нулями
/// (`b64_from_24bit(0, 0, alt[63], 2)`), а не тройку индексов. Если записать
/// хвост как `(0, 0, 63)`, `digest[0]` попадёт в старшие биты и 86-й символ
/// разойдётся с openssl — именно так ловится ошибка в host-свойстве P60.
const PERM: [(usize, usize, usize, usize); 21] = [
    (0, 21, 42, 4),
    (22, 43, 1, 4),
    (44, 2, 23, 4),
    (3, 24, 45, 4),
    (25, 46, 4, 4),
    (47, 5, 26, 4),
    (6, 27, 48, 4),
    (28, 49, 7, 4),
    (50, 8, 29, 4),
    (9, 30, 51, 4),
    (31, 52, 10, 4),
    (53, 11, 32, 4),
    (12, 33, 54, 4),
    (34, 55, 13, 4),
    (56, 14, 35, 4),
    (15, 36, 57, 4),
    (37, 58, 16, 4),
    (59, 17, 38, 4),
    (18, 39, 60, 4),
    (40, 61, 19, 4),
    (62, 20, 41, 4),
];

/// Кодирование 64 байт дайджеста в 86 символов crypt-base64.
fn encode_crypt_base64(digest: &[u8; 64]) -> String {
    let mut out = String::with_capacity(86);
    for (b2, b1, b0, n) in PERM {
        let mut w = ((digest[b2] as u32) << 16) | ((digest[b1] as u32) << 8) | digest[b0] as u32;
        for _ in 0..n {
            out.push(B64[(w & 0x3f) as usize] as char);
            w >>= 6;
        }
    }
    // Хвост: literal 0,0 в старших байтах, младший — digest[63].
    let mut w = digest[63] as u32;
    for _ in 0..2 {
        out.push(B64[(w & 0x3f) as usize] as char);
        w >>= 6;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Независимые векторы `openssl passwd -6 -salt <salt> <password>`.
    const V_PASSWORD: &str =
        "$6$saltsalt$qFmFH.bQmmtXzyBY0s9v7Oicd2z4XSIecDzlB5KiA2/jctKu9YterLp8wwnSq.qc.eoxqOmSuNp2xS0ktL3nh/";
    const V_LONG: &str = "$6$xyz$J6PlYGjCRPOvyFnn9z/SZ163aVFQ9NqqgAqsrt8jx77I9O741W.Ao90vybNtAZUaTi2waoIjxgBv3nRWAw9vG.";

    #[test]
    fn known_crypt_vectors_verify_and_reject_wrong_passwords() {
        assert!(verify_password(V_PASSWORD, "password"));
        assert!(!verify_password(V_PASSWORD, "Password"));
        assert!(!verify_password(V_PASSWORD, ""));
        // 70 символов 'a' — длинный пароль (вектор openssl).
        assert!(verify_password(V_LONG, &"a".repeat(70)));
        assert!(!verify_password(V_LONG, &"a".repeat(69)));
    }

    #[test]
    fn generated_hashes_carry_the_linux_prefix_and_verify() {
        let hash = hash_password("hunter2", "abcdefgh");
        assert!(hash.starts_with("$6$abcdefgh$"));
        assert_eq!(hash.len(), "$6$abcdefgh$".len() + 86);
        assert!(verify_password(&hash, "hunter2"));
        assert!(!verify_password(&hash, "hunter3"));
    }

    #[test]
    fn locked_and_unknown_methods_fail_closed() {
        for stored in ["", "!", "!!", "*", "!$6$abc$def"] {
            assert!(is_locked(stored));
            assert!(!verify_password(stored, "password"));
        }
        assert!(!verify_password("$1$abc$whatever", "password"));
        assert!(!verify_password("$5$abc$whatever", "password"));
        assert!(!verify_password("plaintext", "plaintext"));
    }

    #[test]
    fn shadow_parses_and_rewrites_preserving_other_lines() {
        let text = "# managed\nroot:*:19000:0:99999:7:::\noleg:!:19000:0:99999:7:::\n";
        let entries = parse_shadow(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "root");
        assert_eq!(hash_for(&entries, "oleg"), Some("!"));
        assert_eq!(hash_for(&entries, "nobody"), None);

        let next = set_password(text, "oleg", "$6$s$h");
        assert!(next.contains("# managed\n"));
        assert!(next.contains("root:*:19000:0:99999:7:::\n"));
        // Остальные поля записи oleg сохранены.
        assert!(next.contains("oleg:$6$s$h:19000:0:99999:7:::\n"));

        // Отсутствующего пользователя добавляем, а не теряем молча.
        let added = set_password(text, "alice", "$6$a$b");
        assert!(added.contains("alice:$6$a$b:0:0:99999:7:::\n"));
        assert_eq!(parse_shadow(&added).len(), 3);
    }

    #[test]
    fn salt_from_bytes_stays_in_the_crypt_alphabet() {
        let salt = salt_from_bytes(&[0u8, 1, 2, 63, 64, 200, 255]);
        assert_eq!(salt.len(), 7);
        assert!(salt.bytes().all(|b| B64.contains(&b)));
        assert_eq!(salt_from_bytes(&[0u8; 32]).len(), SALT_MAX);
    }
}
