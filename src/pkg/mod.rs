//! Обработка Debian-пакетов (компоненты дизайна 8–10).
//!
//! Этот subsystem превращает скачанный байтовый буфер `.deb` в файлы,
//! установленные на ext2. Разделение идёт по границе чистых функций R11.6:
//!
//!   * [`deb`] — чистое перечисление `ar`-контейнера, поиск членов `.deb`,
//!     классификация сжатия по суффиксу (компонент 8, R9) и эффектная оболочка
//!     декомпрессии (весь буфер и streaming).
//!
//! Соседние модули [`tar`] (reader/writer ustar, компонент 9), [`install`]
//! (чистая нормализация путей/модель установки) и [`install_fs`] (эффектный
//! ext2-инсталлятор, компонент 10) уже на месте.
//!
//! Всё в [`deb`] — только `core` + `alloc`, без железа и глобалов, поэтому
//! `host-tests` `#[path]`-включает тот же исходник и property-тестирует его на
//! хосте (P23).

/// Эффектный фронтенд пакетного менеджера `apt`: по имени `update`/`install`/
/// `show`/`list`/`setmirror` поверх чистого индекса ([`apt_index`]), резолвера
/// ([`apt_resolve`]), парсера `.deb` ([`deb`]), tar reader'а ([`tar`]) и
/// ext2-инсталлятора ([`install_fs`]). Kernel-only (ведёт сеть + VFS), поэтому
/// живёт отдельно от чистых host-includable модулей.
pub mod apt;
/// Чистый парсер индекса `Packages` + lookup-индекс (сторона чтения `apt install`).
/// Только `core` + `alloc` — `#[path]`-включается `host-tests`, проверяется P30.
pub mod apt_index;
/// Чистый резолвер зависимостей поверх [`apt_index`] (сторона планирования
/// `apt install`). Только `core` + `alloc`; ссылается на [`apt_index`] через
/// `super::`, чтобы один исходник разрешался и в ядре, и в host-крейте. Тестируется P30.
pub mod apt_resolve;
pub mod deb;
pub mod install;
/// Эффектный ext2-инсталлятор (`Package_Installer`, компонент 10). Kernel-only:
/// ведёт трейт `VfsNode`, поэтому — как `net::http_fetch` рядом с чистым
/// `net::http` — живёт отдельно от чистого host-includable модуля `install`.
pub mod install_fs;
/// Чистый разбор host-аргумента `apt setmirror` (обработка префикса URL-схемы).
/// Только `core`, самодостаточный — `#[path]`-включается `host-tests`.
pub mod mirror;
pub mod tar;

/// Чистая политика OpenPGP-верификации метаданных репозитория (issue #32):
/// выбор доверенного ключа, subkey binding, обработка expiry/revocation/key-flag и
/// точки входа `Release.gpg`/`InRelease`. Только `core` + `alloc` —
/// `#[path]`-включается `host-tests` (P51–P53).
pub mod openpgp;
/// Чистая OpenPGP-криптография, используемая [`openpgp`]: правило дайджеста
/// v4-подписи, проверка RSA PKCS#1 v1.5 / EdDSA / ECDSA над этим дайджестом и
/// локальный SHA-1 только для v4-фингерпринтов (`sha1`-крейт не вендорится).
/// Только `core` + `alloc` — `#[path]`-включается `host-tests`.
pub mod openpgp_crypto;
/// Чистый слой OpenPGP-пакетов: ASCII armor (CRC24), framing пакетов, публичные
/// ключи, signature-пакеты, keyring-блоки и clearsign-framing `InRelease`.
/// Только `core` + `alloc`, panic-free и bounded — `#[path]`-включается
/// `host-tests`.
pub mod openpgp_packet;

/// СГЕНЕРИРОВАННЫЙ Debian-keyring (`tools/gen_debian_keyring.py`): доверенные
/// ключи как точные байтовые диапазоны keyring'а, каждый закреплён своим
/// v4-фингерпринтом, алгоритмом, размером, окном валидности и фингерпринтами
/// подключей. Закоммичен и просмотрен намеренно; ядро никогда не скачивает и не
/// обновляет ключи в runtime. Цепочка доверия apt потребляет эту таблицу.
pub mod openpgp_keys;

/// Чистый парсер тела Debian `Release`/`InRelease` (issue #32): поля, которые
/// policy-проверяет цепочка доверия apt (`Suite`/`Codename`/`Date`/`Valid-Until`),
/// и — важнейшее — секция `SHA256:`, чтобы тело `Packages` можно было связать с
/// подписанными метаданными до разбора. Только `core` + `alloc` —
/// `#[path]`-включается `host-tests` (P54).
pub mod release_file;

/// СГЕНЕРИРОВАННЫЙ E2E *test* trust anchor (`tools/gen_openpgp_testkey.py`):
/// детерминированный Ed25519 тестовый ключ плюс три закреплённых Debian-ключа,
/// используемые ТОЛЬКО harness-сборками, которые гоняют `apt update` против
/// локального тестового зеркала — `lx_selftest` (apt E2E-проверки) и `lx_bigindex`
/// (чей синтетический индекс подписан тем же тестовым ключом, чтобы реальный
/// `apt update` дошёл до стадии разбора). Компилируется вне всех прочих сборок,
/// так что обычное ядро — и live-mirror harness `lx_livetest`, который должен
/// доверять только Debian, — вообще не имеет тестового ключа в trust store.
#[cfg(any(
    feature = "lx_selftest",
    feature = "lx_bigindex",
    feature = "lx_bigindex_inram"
))]
pub mod openpgp_test_keys;
