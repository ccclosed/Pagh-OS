// Feature: `/etc/shadow` + `$6$` SHA-512-crypt, Property 62.
//
// Authentication is a security boundary: a parser that panics on a hostile line
// or a verifier that accepts a wrong password is a login bypass. The exhaustive
// half lives here; the kernel compiles the same `src/shell/shadow.rs` through the
// `#[path]` include above, so these properties run against the production file.
//
//   * `hash_password`/`verify_password` round-trip for arbitrary passwords and
//     salts, including the empty password and the empty-password-salt corner;
//   * a wrong password never verifies (mutation: flip one character);
//   * locked / unknown crypt methods fail closed instead of "pass because we
//     did not understand it";
//   * `parse_shadow` never panics on arbitrary text and `set_password` changes
//     only the target user's password field, preserving every other line.

use proptest::prelude::*;

use crate::shadow::{
    hash_for, hash_password, is_locked, parse_shadow, salt_from_bytes, set_password,
    verify_password, SALT_MAX,
};

fn salt() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<u8>(), 1..=SALT_MAX).prop_map(|bytes| salt_from_bytes(&bytes))
}

/// Arbitrary UTF-8 passwords, bounded so the byte length stays under
/// `PASSWORD_MAX` (the verifier's documented cap).
fn password() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..48).prop_map(|chars| chars.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// A hash produced here verifies with the same password and salt shape.
    #[test]
    fn generated_hashes_verify_and_are_linux_shaped(pw in password(), s in salt()) {
        let hash = hash_password(&pw, &s);
        prop_assert!(hash.starts_with("$6$"));
        prop_assert_eq!(hash.len(), 3 + s.len() + 1 + 86);
        prop_assert!(verify_password(&hash, &pw));
        // Пересчёт с тем же salt'ом детерминирован.
        prop_assert_eq!(hash_password(&pw, &s), hash);
    }

    /// A one-character change in the password never verifies.
    #[test]
    fn wrong_password_never_verifies(pw in password(), s in salt()) {
        prop_assume!(!pw.is_empty());
        let hash = hash_password(&pw, &s);
        let mut wrong: Vec<char> = pw.chars().collect();
        let last = wrong.len() - 1;
        wrong[last] = if wrong[last] == 'x' { 'y' } else { 'x' };
        let wrong: String = wrong.into_iter().collect();
        prop_assert_ne!(&wrong, &pw);
        prop_assert!(!verify_password(&hash, &wrong));
    }

    /// Locked entries and non-`$6$` methods fail closed for every password.
    #[test]
    fn locked_and_foreign_hashes_fail_closed(pw in password()) {
        for stored in ["", "!", "!!", "*", "!$6$a$b", "$1$a$b", "$5$a$b", "$y$j9T$abc"] {
            prop_assert!(is_locked(stored) || !stored.starts_with("$6$"));
            prop_assert!(!verify_password(stored, &pw));
        }
    }

    /// Arbitrary text parses without panic; rewriting one user preserves the
    /// rest of the file byte-for-byte and is idempotent.
    #[test]
    fn rewrite_preserves_other_lines_and_is_idempotent(
        text in proptest::string::string_regex("[ -~\\n]{0,120}").unwrap(),
        name in "[a-z]{1,8}",
    ) {
        let entries = parse_shadow(&text);
        prop_assert_eq!(hash_for(&entries, &name).is_some(), entries.iter().any(|e| e.name == name));

        let rewritten = set_password(&text, &name, "$6$s$h");
        // Запись появилась и стала единственной с этим именем для пароля.
        let after = parse_shadow(&rewritten);
        prop_assert_eq!(hash_for(&after, &name), Some("$6$s$h"));
        // Повторная запись того же значения ничего не меняет.
        prop_assert_eq!(set_password(&rewritten, &name, "$6$s$h"), rewritten.clone());
        // Все прежние строки, кроме записи этого пользователя, на месте.
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() { continue; }
            let first = line.split(':').next().unwrap_or("");
            if first == name { continue; }
            prop_assert!(
                rewritten.lines().any(|l| l == line),
                "line missing after rewrite: {:?}",
                line
            );
