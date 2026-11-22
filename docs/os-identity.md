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
Kernel: Linux 6.1.0-pagh
```

Строка `Kernel` остаётся Linux-совместимым UTS-идентификатором (см. §3); строку
`OS` задаёт `/etc/os-release`.

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
* UTS-константы остаются ровно `Linux`/`pagh`/`6.1.0-pagh`/`#1 pagh compat`.

Live-проверка в QEMU (первый boot с диском):

```sh
cat /mnt/etc/os-release
cat /proc/sys/kernel/ostype
lxrun /mnt/usr/bin/fastfetch   # строка OS: Pagh-OS <версия> x86_64
```

## 5. Честные ограничения

* `uname -s` по-прежнему `Linux` — сознательный compat-выбор, а не имя ОС.
* `fastfetch` не читает `/proc/sys/kernel/ostype`; этот файл нужен скриптам и
  procfs-контракту, а не строкам `fastfetch`.
* `OSTYPE`/`MACHTYPE` в shell — переменные окружения bash, а не идентичность
  ядра; они не являются источником истины.
* Внутренние исторические строки UI («Welcome to pagh OS Shell!», статус-бар
  `pagh OS`) — стилизованное имя оболочки/темы; каноническое имя ОС задаёт
  `/etc/os-release`. Переименование этих строк — отдельная задача, потому что
  на них стоят E2E-маркеры.