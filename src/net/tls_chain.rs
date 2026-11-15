//! Trust anchors + построение цепочки сертификатов для серверного TLS-верификатора
//! (серия issue #14) — чистый `core` + `alloc`, panic-free, самодостаточный.
//!
//! Модуль превращает примитивы предыдущих PR в полное решение path-validation
//! для одной серверной цепочки:
//!
//!   * [`TrustAnchor`] — самоподписанный корень, которому ядро доверяет, как сырой
//!     DER `Name` (сравнивается байтами, никогда не парсится) плюс его публичный ключ;
//!   * [`verify_chain`] — строит leaf → intermediates → anchor по **байтовому
//!     равенству** DER-имён `issuer`/`subject`, проверяет каждую подпись через
//!     [`super::tls_verify::verify_certificate_signature`], требует
//!     `basicConstraints cA=TRUE` у каждого эмитента (intermediate и root),
//!     сверяет окно валидности каждого сертификата с переданным вызывающим
//!     `now` и применяет **clock gate** ниже.
//!
//! Правила fail-closed (issue #14 именно про удаление fail-open путей):
//!
//!   * **Clock gate**: `now` ниже [`CLOCK_FLOOR`] (2025-01-01T00:00:00Z) — жёсткий
//!     отказ [`ChainError::Clock`]. При загрузке CMOS RTC может быть не выставлен
//!     (счёт с 1970), и «я не знаю, который час» должен отказывать в handshake,
//!     никогда его не пропускать: каждый реальный сертификат на текущем зеркале
//!     уже внутри 2025+, так что трактовка до-2025 часов как невыставленных
//!     безопасна и устраняет весь класс fail-open окон «RTC не выставлен ⇒ всё
//!     валидно».
//!   * Цепочка, не доходящая до anchor, — отказ ([`ChainError::NoAnchor`], когда
//!     имя эмитента вообще ни с чем не совпало); совпадение имени, чья подпись не
//!     проверяется, — [`ChainError::Verify`], никогда не pass.
//!   * Intermediate/root без `basicConstraints cA=TRUE` отвергаются
//!     ([`ChainError::NotCa`]) — RFC 5280 §4.2.1.9: DER опускает дефолтный
//!     `FALSE`, так что *отсутствующее* расширение здесь тоже отказ.
//!   * Лишние сертификаты в DER после найденного полного пути игнорируются
//!     (RFC 8446 §4.4.2 позволяет серверам слать ненужные клиенту записи); им
//!     никогда не доверяют, так что их игнор не убирает ни одной проверки.
//!
//! Без сервисов ядра, RNG и host-вызовов: `now` — параметр, так что kernel call
//! site (`net::tls`, через `tls_auth::authenticate_server`) передаёт
//! `rtc::now_unix() as i64`, а host property-тест P47 гоняет тот же код с
//! синтетическими значениями.

use alloc::vec;
use alloc::vec::Vec;

use super::tls_verify::verify_certificate_signature;
use super::x509::{
    find_extension, parse_basic_constraints, parse_certificate, CertificateRef, DerError, SpkiKey,
    OID_EXT_BASIC_CONSTRAINTS,
};

/// Нижняя граница clock gate: 2025-01-01T00:00:00Z в Unix-секундах.
///
/// Системные часы ниже этого значения считаются «реально не выставленными»
/// (свежие UEFI/CMOS загрузки часто стартуют с epoch сборки или 1970) и валят
/// handshake закрыто — см. docs модуля.
pub const CLOCK_FLOOR: i64 = 1_735_689_600;

/// Почему цепочка отвергнута. Каждый вариант — жёсткий отказ; `PartialEq`, чтобы
/// host property-тесты могли утверждать точные классы сбоя.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainError {
    /// Ни один trust anchor не совпал с финальным именем эмитента в цепочке.
    NoAnchor,
    /// Цепочка непригодна как есть: сертификат не распарсился или путь между
    /// leaf и anchor'ами порван (нет intermediate).
    IncompleteChain,
    /// Сертификат в пути за своим `notAfter`.
    Expired,
    /// Сертификат в пути до своего `notBefore`.
    NotYetValid,
    /// Системные часы ниже нижней границы — время суток неизвестно, поэтому
    /// валидность решить нельзя и handshake отвергается.
    Clock,
    /// Сертификат-эмитент (intermediate или root) не `cA=TRUE`.
    NotCa,
    /// Проверка подписи упала, с нижележащей причиной из
    /// [`super::tls_verify::SigVerifyError`].
    Verify(super::tls_verify::SigVerifyError),
}

impl From<SigVerifyError> for ChainError {
    fn from(e: SigVerifyError) -> Self {
        ChainError::Verify(e)
    }
}

// Держать alias однозначным в местах использования ниже.
use super::tls_verify::SigVerifyError;

/// Самоподписанный корень, которому доверяет ядро: сырой DER `Name` (сравнивается
/// побайтно с полями `issuer` сертификатов) и публичный ключ, которым корень
/// подписывает. Парсинга сертификата на доверии нет — anchor есть настроенный
/// факт, а не разобранное утверждение.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustAnchor<'a> {
    /// Сырой DER subject `Name` anchor'а (та же кодировка, что несут сертификаты,
    /// так что сравнение с `CertificateRef::issuer` — простое побайтовое).
    /// byte-slice compare).
    pub subject: &'a [u8],
    /// Публичный ключ anchor'а (должен быть поддерживаемым верификатором).
    pub key: SpkiKey<'a>,
}

/// Провалидировать одну серверную цепочку сертификатов против заданных anchor'ов.
///
/// * `anchors` — настроенные trust anchors (корневые CA);
/// * `der` — DER leaf-сертификата, за которым идут любые intermediates, присланные
///   сервером (ровно раскладка TLS-сообщения `Certificate`);
/// * `now` — текущее Unix-время в секундах (`rtc::now_unix() as i64` в kernel call
///   site).
///
/// Возвращает `Ok(())`, только если существует полный путь leaf → … → anchor,
/// где каждая подпись проверяется ключом родителя, каждый эмитент в пути
/// (intermediates и root, когда он пришёл сертификатом) — `cA=TRUE`, каждый
/// сертификат пути в своём окне валидности, а `now` на нижней границе
/// [`CLOCK_FLOOR`] или выше.
///
/// Семантика fallback и выбора ошибки: одноимённый intermediate, который падает
/// (плохая подпись, non-CA, непарсящийся basicConstraints), НИКОГДА не затеняет
/// настроенные anchor'ы — после исчерпания списка кандидатов сообщения anchor'ы
/// всегда опрашиваются, и путь успешен, если проверяется любой anchor с
/// совпавшим именем. Когда не проверяется ничего, возвращается ПЕРВАЯ падающая
/// name-match запись в порядке сообщения (записи сообщения ранжируются раньше
/// набора anchor'ов; без перезаписи последним кандидатом); anchor, совпавший по
/// имени, но не прошедший проверку, сообщается только когда ни один intermediate-
/// кандидат не упал, а имя без подходящего сертификата и без подходящего anchor —
/// [`ChainError::NoAnchor`].
pub fn verify_chain(anchors: &[TrustAnchor<'_>], der: &[u8], now: i64) -> Result<(), ChainError> {
    // CLOCK GATE, прежде всего: невыставленный RTC не должен выглядеть как
    // валидное время для сертификата, чьё окно случайно его включает.
    if now < CLOCK_FLOOR {
        return Err(ChainError::Clock);
    }

    // Разобрать каждый сертификат, присланный peer'ом. Любая некорректная запись —
    // отказ (IncompleteChain) — peer не выбирает, какие части его мусора мы игнорируем.
    let mut rest = der;
    let mut certs: Vec<CertificateRef<'_>> = Vec::new();
    while !rest.is_empty() {
        let (cert, tail) = parse_certificate(rest).map_err(der_to_chain)?;
        certs.push(cert);
        rest = tail;
    }
    let leaf = match certs.first() {
        Some(c) => *c,
        None => return Err(ChainError::IncompleteChain),
    };

    // Валидность каждого сертификата сообщения проверяется сразу (даже для
    // записей, которые могут не попасть в путь — они присланы как часть цепочки,
    // а peer, шлющий просроченный материал, не тот peer, которому этот верификатор
    // доверит пройти).
    for cert in &certs {
        check_validity(cert, now)?;
    }

    // Идти по пути. `used` отслеживает, какие записи сообщения потреблены как
    // эмитенты, чтобы враждебный peer не мог переиспользовать одну запись дважды
    // (иначе self-issued цикл позволил бы цепочке «провериться» сама на себя).
    let mut used = vec![false; certs.len()];
    used[0] = true;
    let mut current = leaf;
    loop {
        // Есть ли неиспользованный intermediate в сообщении, называющий себя
        // эмитентом `current`? Пробовать каждого кандидата; упавший кандидат
        // (плохая подпись, non-CA, непарсящийся basicConstraints) никогда не
        // останавливает поиск: более поздние кандидаты и — см. ниже — настроенные
        // anchor'ы всё ещё получают шанс.
        let mut name_match_err: Option<ChainError> = None;
        let mut advanced = false;
        for (i, cand) in certs.iter().enumerate() {
            if used[i] || cand.subject != current.issuer {
                continue;
            }
            // Intermediate ОБЯЗАН быть CA (fail closed и на отсутствующем
            // basicConstraints — DER опускает дефолтный FALSE). Кандидат, чей
            // basicConstraints даже не парсится, пропускается как non-CA, с
            // запоминанием ошибки парсинга как результата кандидата (она
            // отображается в IncompleteChain, если больше ничего не проверится).
            let ca = match is_ca(cand) {
                Ok(ca) => ca,
                Err(e) => {
                    if name_match_err.is_none() {
                        name_match_err = Some(e);
                    }
                    continue;
                }
            };
            if !ca {
                if name_match_err.is_none() {
                    name_match_err = Some(ChainError::NotCa);
                }
                continue;
            }
            match verify_certificate_signature(
                current.sig_alg,
                current.tbs,
                current.signature,
                &cand.spki.key,
            ) {
                Ok(()) => {
                    used[i] = true;
                    current = *cand;
                    advanced = true;
                    break;
                }
                Err(e) => {
                    if name_match_err.is_none() {
                        name_match_err = Some(ChainError::Verify(e));
                    }
                }
            }
        }
        if advanced {
            continue;
        }

        // Ни один кандидат не проверился — БЕЗУСЛОВНО перейти к настроенным
        // anchor'ам. Одноимённый intermediate, который падает (ротированный ключ
        // корня, cross-sign, устаревшая копия), никогда не должен затенять trust
        // anchor: набор anchor'ов — настроенный факт, сообщение — лишь утверждение.
        // Путь успешен, если ЛЮБОЙ anchor с совпавшим именем проверяет подпись.
        let mut anchor_err: Option<ChainError> = None;
        for anchor in anchors {
            if anchor.subject != current.issuer {
                continue;
            }
            match verify_certificate_signature(
                current.sig_alg,
                current.tbs,
                current.signature,
                &anchor.key,
            ) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if anchor_err.is_none() {
                        anchor_err = Some(ChainError::Verify(e));
                    }
                }
            }
        }

        // Детерминированный выбор ошибки: побеждает ПЕРВАЯ падающая name-match
        // запись в порядке сообщения (записи сообщения идут до набора anchor'ов,
        // без перезаписи последним кандидатом). Anchor, совпавший по имени, но не
        // прошедший проверку, сообщается только когда ни один intermediate-
        // кандидат не упал; имя без подходящего сертификата и без подходящего
        // anchor — NoAnchor.
        if let Some(e) = name_match_err {
            return Err(e);
        }
        if let Some(e) = anchor_err {
            return Err(e);
        }
        // Ничто не называет эмитента `current`: путь порван (нет intermediate или
        // недоверенный корень).
        return Err(ChainError::NoAnchor);
    }
}

/// Проверить `not_before <= now <= not_after` для одного сертификата.
fn check_validity(cert: &CertificateRef<'_>, now: i64) -> Result<(), ChainError> {
    if now > cert.validity.not_after {
        return Err(ChainError::Expired);
    }
    if now < cert.validity.not_before {
        return Err(ChainError::NotYetValid);
    }
    Ok(())
}

/// Несёт ли сертификат `basicConstraints cA=TRUE`? Отсутствующее расширение или
/// ошибка парсинга — НЕ CA (fail closed).
fn is_ca(cert: &CertificateRef<'_>) -> Result<bool, ChainError> {
    match find_extension(cert, OID_EXT_BASIC_CONSTRAINTS) {
        Ok(Some(octets)) => parse_basic_constraints(octets).map_err(der_to_chain),
        Ok(None) => Ok(false),
        Err(e) => Err(der_to_chain(e)),
    }
}

/// Отобразить DER-сбой материала цепочки в ошибку цепочки (некорректный ввод
/// посреди цепочки — неполная цепочка, а не отчёт о парсинге).
fn der_to_chain(e: DerError) -> ChainError {
    let _ = e;
    ChainError::IncompleteChain
}

// ───────────────────────────── self-tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// `TrustAnchor`, собранный из сырых байтов для проверок диспетчера.
    #[test]
    fn clock_gate_rejects_below_floor() {
        let anchor = TrustAnchor {
            subject: &[0x30, 0x00],
            key: SpkiKey::Unsupported,
        };
        // На секунду раньше нижней границы: «часы реально не выставлены».
        assert_eq!(
            verify_chain(&[anchor], &[], CLOCK_FLOOR - 1),
            Err(ChainError::Clock)
        );
        // На нижней границе и выше гейт пропускает (пустой DER падает позже, но
        // с IncompleteChain, никогда с Clock).
        assert_eq!(
            verify_chain(&[anchor], &[], CLOCK_FLOOR),
            Err(ChainError::IncompleteChain)
        );
    }
}
