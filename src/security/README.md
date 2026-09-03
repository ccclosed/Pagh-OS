# `src/security/` — security-sensitive примитивы

Узкие API вокруг чувствительных операций. Сейчас — только аппаратная энтропия.

## Файлы

| Файл | Роль |
|---|---|
| `mod.rs` | Корень: `pub mod entropy;` |
| `entropy.rs` | Аппаратная энтропия x86_64: RDSEED (предпочтительно), RDRAND (фоллбэк) |

## Ключевые символы

- `EntropyError::Unavailable`
- `is_available() -> bool` — CPUID: leaf 1 ECX bit 30 (RDRAND), leaf 7 EBX bit 18 (RDSEED)
- `secure_u64() -> Result<u64, EntropyError>`
- `fill(dest: &mut [u8]) -> Result<(), EntropyError>` (LE-заполнение u64, хвост по частичному слову)

## Как работает

CPUID-проба возможностей; `rdseed_word`/`rdrand_word` — `#[target_feature]`-обёртки над
`_rdseed64_step`/`_rdrand64_step` с ретраями до 128 раз + `spin_loop()` между попытками.

**Fail-closed по дизайну**: никогда не подставляет таймстампы или PRNG;
`EntropyError::Unavailable` обязан прокидываться вызывающим.

## Зависимости

- **От:** только `core::arch::x86_64`.
- **На неё:** `net::tls::rs` (`KernelRng` — адаптер к `rand_core::RngCore + CryptoRng`
  для embedded-tls: ключи TLS-сессий отсюда).

## Замечания

- RDSEED/RDRAND-бэкенд не компенсирует отсутствия аутентификации пира в TLS (VARIANT A).
- Транзиентный сбой инструкции после успешной проверки доступности трактуется как
  нарушение инварианта, а не повод подставить предсказуемые байты.
