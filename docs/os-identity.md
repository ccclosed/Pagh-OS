# Идентификация ОС: Pagh-OS, Pagh и Linux-совместимость

Pagh — **самостоятельная операционная система** с Linux-совместимым syscall ABI, а
не Linux-дистрибутив. Этот документ фиксирует, где именно ОС называет себя, какой
интерфейс что означает и почему `uname(2).sysname` остаётся `Linux`.

## 1. Канонические источники

| Интерфейс | Значение | Кто читает |
|---|---|---|
| `/etc/os-release` | `NAME="Pagh-OS"`, `ID=pagh`, `VERSION`/`VERSION_ID` = `<CARGO_PKG_VERSION>`, `PRETTY_NAME="Pagh-OS <версия>"`, `ARCH=x86_64` | `fastfetch` (строка `OS`), os-release-совместимые скрипты и библиотеки |
| `/proc/sys/kernel/ostype` | `Pagh\n` | procfs-контракт, диагностика, скрипты, которым нужно имя **ядра** |
| `/proc/sys/kernel/osrelease` | `<CARGO_PKG_VERSION>\n` | те же скрипты; версия работающего ядра (исторический `/etc/pagh-release` может отставать — см. ниже) |
| `/proc/version` | `pagh <версия> (x86_64) #1 SMP\n` | диагностика (`/proc/version`) |
| `/etc/pagh-release` | `NAME="Pagh-OS"`, `ID=pagh`, `VERSION`, `ARCH` | человек и маркер завершённого провижининга; ядром не парсится |
| `/etc/fastfetch/{config.jsonc,pagh-logo.txt}` | системное оформление `fastfetch`: ASCII-логотип `PAGH` (figlet «standard») и `kernel.format = "Pagh-OS {release}"` | `fastfetch` (логотип и строка `Kernel`) |
| `uname(2)` | `sysname=Linux`, `nodename=pagh`, `release=6.1.0-pagh`, `version=#1 pagh compat`, `machine=x86_64`, `domainname=(none)` | Linux-бинарники, `configure`, `config.guess`, `fastfetch` (строка `Kernel`) |

`/etc/os-release` пишет `provision.rs` (`seed_identity`) на каждом boot'е, но не
слепо:

* файла нет — создаёт (старые образы, где os-release никогда не писался);
* файл объявляет `NAME="Pagh-OS"` и `ID=pagh` — обновляет `VERSION`/`VERSION_ID`/
  `PRETTY_NAME` до версии **работающего** ядра. Иначе после обновления ядра на
  существующем диске файл навсегда застыл бы на версии первого boot'а (ровно это
  случилось с историческим `/etc/pagh-release`: на тестовом образе он до сих пор
  `2.3.0`);
* файл объявляет другую ОС (`NAME`/`ID` не наши) — считается пользовательской
  правкой и не перезаписывается.

Остальные файлы посева — по-прежнему `write_once` и не перезаписывают
пользовательские изменения.

Версия везде — `env!("CARGO_PKG_VERSION")`, единственный источник версии в
проекте (`Cargo.toml`); отдельной «release-строки» нет. `/etc/pagh-release`
остаётся маркером первого провижининга и на старом диске может нести версию
первого boot'а; источник истины о версии для userland — `/proc/version`,
`/proc/sys/kernel/osrelease` и `/etc/os-release`.

## 2. Почему `fastfetch` раньше показывал `linux-pagh`

Диагноз (fastfetch 2.40.4, `src/detection/os/os_linux.c` и
`src/modules/os/os.c`):

* строка `OS` берётся **только** из `/etc/os-release` (затем
  `/etc/lsb-release`, `/usr/lib/os-release`). `/proc/sys/kernel/ostype`
  `fastfetch` не читает вообще, а `uname(2).sysname` для строки `OS` не
  используется;
* до исправления `/etc/os-release` в образе не существовало: посев писал только
  нестандартный `/etc/pagh-release`. Модуль `OS` получал пустые поля и печатал
  ошибку «Could not detect OS» (в конфигурации образа она не показывалась);
* единственной Linux-подобной строкой оставалась строка `Kernel`, собранная из
  `uname`: `Linux 6.1.0-pagh`. Именно её и видели как «linux-pagh».

То есть имя ОС не «терялось» в `uname` — его просто не было в стандартном файле.
Исправление добавляет `/etc/os-release`, и `fastfetch` печатает:

```text
OS: Pagh-OS 2.24.0 x86_64
Kernel: Pagh-OS 6.1.0-pagh
Disk (/): 49.5 MiB / 255.8 MiB (19%) - rootfs
```

Строка `Kernel` остаётся Linux-совместимым UTS-идентификатором на уровне ABI
(см. §3), но в оформлении её имя — `Pagh-OS`, а `6.1.0-pagh` печатается после
него как UTS-релиз, без слова `Linux` (§2.1); строку `OS` задаёт
`/etc/os-release`, размер `Disk (/)` — `statvfs` корня ext2 (§2.2).

## 2.1 Оформление `fastfetch`: логотип и строка `Kernel`

ОС-имя из os-release решает строку `OS`, но два других дефекта видны только в
выводе `fastfetch` и поэтому оформляются его системной конфигурацией
(`src/os_identity.rs`, константы `FASTFETCH_CONFIG`/`FASTFETCH_LOGO`;
`provision.rs::seed_identity` пишет их как `/etc/fastfetch/...` через
`write_once`):

* **логотип.** `fastfetch` выбирает встроенный ASCII-арт по `ID=`/`NAME=`;
  `pagh` не совпадает ни с одним брендом, и оставался пингвин. Конфиг
  переключает `logo` на `type: "file"`, `source: "/etc/fastfetch/pagh-logo.txt"`.
  Арт — только ASCII, слово `PAGH` в стиле figlet «standard» (контур из
  `/ \ | _`): тот же файл печатается и в kernel-сессии терминального окна
  (шрифт 8x18 без Unicode-блоков), и на serial; цвет задаёт `logo.color`;
* **строка `Kernel`.** `uname(2).sysname` менять нельзя (§3), поэтому вместо
  «Linux 6.1.0-pagh» модуль печатает `Pagh-OS 6.1.0-pagh`: имя — Pagh-OS, а
  UTS-релиз следует за ним без слова `Linux`. Это оформление, а не UTS:
  `uname(2)` по-прежнему отвечает `Linux` (§3), но в выводе `fastfetch` этого
  слова нет;
* **`modules`.** В конфиге перечислены только модули, которые в этом госте
  действительно отвечают (`os`, `kernel`, `uptime`, `cpu`, `memory`, `disk`,
  `colors`), чтобы `fastfetch` не печатал ошибки автоопределения вместо
  идентичности.

Файл — пользовательская правка, как и остальной посев: `write_once` не
перезаписывает существующий `/etc/fastfetch/`, поэтому своё оформление
переживает boot, а свежий диск получает логотип Pagh без ручных действий.

## 2.2 `statfs` и размер `Disk (/)`: RAM — не диск

`fastfetch` собирает `Disk` из `/proc/mounts` и `statvfs(2)` точки
монтирования. Гостевой `/` раньше резолвился в синтетический корень VFS
(`rootfs`) с пустым деревом, а `statfs` для ФС без учёта ёмкости подставлял
счётчики кадров PMM — поэтому `Disk (/)` показывал объём **памяти** QEMU
(например `6.00 GiB`) вместо размера образа (256 МиБ), совпадая с `Memory`.

Исправление в двух местах (`src/arch/x86_64/linux/io_sys.rs`):

* `resolve_path("/")` отображает гостевой корень в `/mnt` — корень ext2, как и
  все остальные гостевые пути (`/etc/x` → `/mnt/etc/x`). Симлинк с целью `/`
  отображается так же (`vfs::link_walk::map_guest_target`), так что `stat`,
  `ls /` и `statvfs("/")` видят один и тот же каталог. `statvfs("/")` получает
  реальные счётчики ext2 (`fs_stat`);
* `build_statfs_for(None)` (псевдо-ФС: `/dev`, `/proc`, ramfs `/tmp`) больше не
  выдаёт PMM-кадры за блоки: `f_type = RAMFS_MAGIC`, ёмкость — нули, то есть
  честное «учёта ёмкости нет». Иначе `df /tmp` и `statvfs("/")` на образе без
  диска показывали бы RAM как диск.

## 3. Почему `uname -s` остаётся `Linux`

Linux-совместимость — это сервис ABI, а не самоназвание. `sysname` опрашивают
Linux-бинарники, `configure`/`config.guess`, пакетные скрипты и `dpkg`-подобные
инструменты; возврат `Pagh` включил бы у них неверные ветки и сломал ровно ту
совместимость, ради которой слой `arch/x86_64/linux/` существует. Поэтому:

* `uname(2)` — это **UTS-идентификатор совместимости** (`Linux`), а не имя ОС;
* имя ОС — `Pagh-OS` из `/etc/os-release`;
* имя ядра — `Pagh` из `/proc/sys/kernel/ostype`;
* версия ОС — `CARGO_PKG_VERSION` из `/etc/os-release`,
  `/proc/sys/kernel/osrelease` и `/proc/version`.

Константы UTS живут в `src/os_identity.rs`, а не в обработчике сисколла, и
зафиксированы host-свойством `os_identity`; in-QEMU `selftest_lx` проверяет
`uname` через реальный обработчик. Если когда-нибудь `sysname` решено будет
сменить на `Pagh`, это image/ABI-изменение: обновить `src/os_identity.rs`,
`selftest_lx`, этот документ и заново проверить Linux-userland, который
опрашивает `uname -s`. Молчаливая смена запрещена — свойство упадёт.

Практическое следствие: `uname -s` отвечает `Linux` и **не** является способом
узнать имя ОС. Для идентичности используйте:

```sh
cat /etc/os-release          # NAME="Pagh-OS", ID=pagh, VERSION=…
cat /proc/sys/kernel/ostype  # Pagh
```

## 4. Проверка

Host-свойство (`host-tests/src/properties/os_identity.rs`) фиксирует:

* `/etc/os-release` — ровно шесть полей, `NAME=Pagh-OS`, `ID=pagh`,
  `PRETTY_NAME="Pagh-OS <version>"`, версия интерполирована, строки `Linux` нет;
* `/etc/pagh-release` и `/etc/motd` называют ту же ОС и версию;
* UTS-константы остаются ровно `Linux`/`pagh`/`6.1.0-pagh`/`#1 pagh compat`;
* `FASTFETCH_CONFIG`/`FASTFETCH_LOGO`: логотип подключается как
  `type: "file"` из `FASTFETCH_LOGO_PATH`, строка `Kernel` начинается с
  `Pagh-OS` и несёт `{release}`, арт — ASCII 0x20..0x7E из пяти строк. Тест
  `fastfetch_is_branded_as_pagh` падает, если вернуть пингвина или
  `Kernel: Linux` или слово `Linux` в конфиге.
* `vfs::link_walk::map_guest_target("/")` = `/mnt` (свойство
  `guest_target_mapping`), чтобы ссылка на `/` и `statvfs("/")` не разъехались.

Live-проверка в QEMU (первый boot с диском):

```sh
cat /mnt/etc/os-release
cat /proc/sys/kernel/ostype
cat /etc/fastfetch/config.jsonc
lxrun /mnt/usr/bin/fastfetch   # Pagh-логотип, OS: Pagh-OS <версия> x86_64,
                               # Kernel: Pagh-OS <release>, Disk (/) — ext2
```

E2E `tools/e2e.py termwindow` дополнительно требует, чтобы строки `OS:`/
`Kernel:` называли `Pagh-OS` (и не было `Kernel: Linux`), а итог `Disk (/)` не
совпадал с итогом `Memory` — это регрессия на «RAM вместо диска».

## 5. Честные ограничения

* `uname -s` по-прежнему `Linux` — сознательный compat-выбор, а не имя ОС.
* `fastfetch` не читает `/proc/sys/kernel/ostype`; этот файл нужен скриптам и
  procfs-контракту, а не строкам `fastfetch`.
* Суффикс `Disk (/) … - rootfs` берётся из `/proc/mounts` (там корень —
  kernel-internal `rootfs`), тогда как **размер** приходит из `statvfs` корня
  ext2. Число честное, метка типа ФС — нет; чтобы совпала и она, `/proc/mounts`
  должен показывать гостевую (chroot-)таблицу монтирования, а не таблицу VFS
  ядра. Это отдельное изменение контракта procfs и здесь не делается.
* `OSTYPE`/`MACHTYPE` в shell — переменные окружения bash, а не идентичность
  ядра; они не являются источником истины.
* Внутренние исторические строки UI («Welcome to pagh OS Shell!», статус-бар
  `pagh OS`) — стилизованное имя оболочки/темы; каноническое имя ОС задаёт
  `/etc/os-release`. Переименование этих строк — отдельная задача, потому что
  на них стоят E2E-маркеры.