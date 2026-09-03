# `boot/` — конфиг загрузчика

## Файлы

| Файл | Роль |
|---|---|
| `limine.conf` | Канонический конфиг Limine; копируется в ISO на стадии `stage` |

Содержимое: `timeout: 5`, `verbose: yes`, `serial: yes`; запись `/pagh OS` с
`protocol: limine`, `kernel_path: boot():/pagh.elf`.

## Как используется

`tools/build.py stage` (и `build.sh --stage`, `run.cmd`) кладёт файл в два места
собираемого `iso_root/`: корень ISO и `EFI/BOOT/limine.conf`. Ядро обязано называться
ровно `pagh.elf` в корне ESP.

## Связанное

- `linker.ld` (корень репо): секция `.requests` (`KEEP(*(.requests .requests.*))`) обязательна —
  без неё Limine не видит валидных kernel-записей; higher-half база `0xffffffff80000000`.
- `limine-12.3.1/` — вендоренный загрузчик (`BOOTX64.EFI`).
- Фоновые `.cmd`-скрипты (`boot_qemu_bg.cmd` и др.) генерируют собственный inline-конфиг
  с `timeout: 0` и этот файл не используют.
