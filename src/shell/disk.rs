//! `disk` — выбрать блочное устройство, отформатировать его в ext2 и
//! смонтировать в `/mnt`.
//!
//! Зачем это нужно. `boot::init_fs` сам монтирует `/mnt`, когда диск машины уже
//! несёт файловую систему, и форматирует только когда он действительно пуст
//! (`fs::format_policy`). Всё остальное — SSD с разделами, диск с чужой ФС,
//! машина без поддерживаемого накопителя — заканчивали загрузку с `fs (no disk)`
//! и без пути вперёд: не было ни `mkfs`, ни `fdisk`, ни способа сказать «да,
//! используй ЭТО устройство». Этот модуль и есть путь вперёд, и он намеренно
//! интерактивный: единственная операция здесь разрушительна без восстановления.
//!
//! МАСТЕР ВЛАДЕЕТ СЛЕДУЮЩЕЙ СТРОКОЙ, А НЕ КЛАВИАТУРОЙ. `disk setup` печатает
//! таблицу устройств и взводит ожидающий вопрос; собственный REPL shell читает
//! ответ и передаёт сюда ([`feed_line`]). Это переиспользует существующие
//! редактор строки, echo, историю и каретку — второй цикл ввода внутри команды
//! должен был бы реализовать всё это заново и дрался бы с shell за клавиатуру.
//!
//! МОДЕЛЬ БЕЗОПАСНОСТИ. Классификация — `fs::device::probe_boot_area`, тот же
//! код, что у boot-пути, поэтому меню никогда не предложит стереть диск, который
//! boot-путь защищает. Непустое устройство форматируется только после того, как
//! оператор наберёт его имя в ответ (см. [`Step::ConfirmName`]); чистое решение,
//! разрешающее это, — `allow_destructive` из `format_policy::format_allowed`,
//! существующее ровно для «оператор так сказал». Набрать `nvme0` — заявление,
//! которое случайно не сделает никакой boot-флаг.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::drivers::BlockDevice;
use crate::fs::format_policy::{format_allowed, Layout, Refusal};
use crate::sync::spinlock::Spinlock;

/// Напечатать одну строку на консоли shell (собственный принтер обработчиков).
fn println_text(text: &str) {
    crate::shell::commands::shell_println(text);
}

/// Устройство в том виде, в каком его показывает меню.
struct Entry {
    name: String,
    sectors: u64,
    layout: Layout,
    has_ext2: bool,
}

impl Entry {
    fn size_text(&self) -> String {
        let bytes = self.sectors.saturating_mul(512);
        if bytes >= 1024 * 1024 * 1024 {
            format!("{} GiB", bytes / (1024 * 1024 * 1024))
        } else if bytes >= 1024 * 1024 {
            format!("{} MiB", bytes / (1024 * 1024))
        } else if bytes >= 1024 {
            format!("{} KiB", bytes / 1024)
        } else {
            format!("{} B", bytes)
        }
    }

    /// Что оператор собирается сделать с этим устройством, одним словом.
    fn state_text(&self) -> &'static str {
        if self.has_ext2 {
            "ext2 (mountable)"
        } else {
            match self.layout {
                Layout::Blank => "blank (safe to format)",
                Layout::PartitionTable => "partition table (has data)",
                Layout::Ext2 => "ext2 superblock, unusable (has data)",
                Layout::Foreign => "unrecognised data (has data)",
            }
        }
    }

    /// Можно ли форматировать это устройство без явного подтверждения?
    fn is_blank(&self) -> bool {
        self.layout == Layout::Blank && !self.has_ext2
    }
}

/// Какого ответа ждёт мастер.
enum Step {
    /// `pick <n>` — какое устройство форматировать.
    ChooseDevice(Vec<Entry>),
    /// `type <name>` — подтверждение для устройства, на котором есть данные.
    ConfirmName { entries: Vec<Entry>, index: usize },
}

static WIZARD: Spinlock<Option<Step>> = Spinlock::new(None);

/// Ждёт ли вопрос `disk` следующую строку, которую прочитает shell?
pub fn pending() -> bool {
    WIZARD.lock().is_some()
}

/// Бросить ожидающий вопрос (`disk cancel` или escape в стиле `^C`).
pub fn cancel() {
    *WIZARD.lock() = None;
}

/// Перечислить зарегистрированные блочные устройства с их on-disk
/// классификацией.
///
/// Читает первый MiB каждого устройства, так что это не hot path — выполняется,
/// когда оператор просит таблицу, то есть ровно когда ошибка чтения важна и
/// должна быть видна.
fn entries() -> Vec<Entry> {
    use crate::fs::ext2::Ext2Fs;

    crate::drivers::list_blocks()
        .into_iter()
        .map(|(name, sectors)| {
            let probe = crate::drivers::get_block(&name)
                .map(|blk| crate::fs::device::probe_boot_area(&*blk));
            let has_ext2 = crate::drivers::get_block(&name)
                .map(|blk| Ext2Fs::has_valid_superblock(&*blk))
                .unwrap_or(false);
            Entry {
                name,
                sectors,
                layout: probe.map(|p| p.layout()).unwrap_or(Layout::Foreign),
                has_ext2,
            }
        })
        .collect()
}

/// `disk` — без аргумента напечатать таблицу.
pub fn cmd_list() {
    let entries = entries();
    if entries.is_empty() {
        println_text("disk: no block devices");
        println_text(
            "disk: the kernel has drivers for virtio-blk (QEMU) and NVMe. A boot image can also \
             carry one: see `module_path: boot():/disk.img` in limine.conf.",
        );
        return;
    }
    println_text("  #  DEVICE      SIZE      STATE");
    for (i, e) in entries.iter().enumerate() {
        println_text(&format!(
            "{:>3}  {:<10}  {:<8}  {}",
            i + 1,
            e.name,
            e.size_text(),
            e.state_text()
        ));
    }
    let mounted = crate::vfs::mount_snapshot()
        .iter()
        .any(|m| m.point == "/mnt");
    println_text(&format!(
        "{}",
        if mounted {
            "/mnt is mounted; `disk mount <n>` re-mounts another device (reboot to unmount)"
        } else {
            "nothing is mounted at /mnt; run `disk setup`, or `disk mount <n>` for an ext2 device"
        }
    ));
}

/// `disk setup` — интерактивный путь: выбрать, подтвердить, отформатировать, смонтировать.
pub fn cmd_setup() {
    let entries = entries();
    if entries.is_empty() {
        println_text("disk setup: no block devices to choose from");
        return;
    }
    println_text("Format which device as ext2 and mount it at /mnt?");
    for (i, e) in entries.iter().enumerate() {
        println_text(&format!(
            "  [{}] {:<10} {:<8} {}",
            i + 1,
            e.name,
            e.size_text(),
            e.state_text()
        ));
    }
    println_text("Enter a number, or press Enter to cancel.");
    *WIZARD.lock() = Some(Step::ChooseDevice(entries));
}

/// `disk mount <n>` — смонтировать существующую ext2; никогда не форматирует.
pub fn cmd_mount(index: usize) {
    let Some(entry) = entries().into_iter().nth(index) else {
        println_text(&format!("disk mount: no device number {}", index + 1));
        return;
    };
    if !entry.has_ext2 {
        println_text(&format!(
            "disk mount: {} has no usable ext2 filesystem ({})",
            entry.name,
            entry.state_text()
        ));
        return;
    }
    mount(&entry.name);
}

/// `disk format <n> [--force]` — неинтерактивное форматирование для скриптов и E2E.
///
/// Непустому устройству нужен `--force` — то же подтверждение, которое мастер
/// просит по имени; скрипт, передающий его, говорит то же, что оператор,
/// набирающий имя устройства.
pub fn cmd_format(index: usize, force: bool, mount_after: bool) {
    let Some(entry) = entries().into_iter().nth(index) else {
        println_text(&format!("disk format: no device number {}", index + 1));
        return;
    };
    if !entry.is_blank() && !force {
        println_text(&format!(
            "disk format: {} is not blank ({}); re-run with --force if you mean it",
            entry.name,
            entry.state_text()
        ));
        return;
    }
    format_and_mount_with(&entry.name, force, mount_after);
}

/// Передать одну строку ввода shell ожидающему вопросу мастера.
///
/// Вызывается REPL'ом ВМЕСТО диспетчера команд, пока [`pending`] истинно, чтобы
/// ответ мастера никогда не был принят за команду.
pub fn feed_line(line: &str) {
    let line = line.trim();
    let step = WIZARD.lock().take();
    match step {
        None => {}
        Some(Step::ChooseDevice(entries)) => {
            if line.is_empty() {
                println_text("disk setup: cancelled");
                return;
            }
            let Ok(n) = line.parse::<usize>() else {
                println_text(&format!(
                    "disk setup: `{}` is not a number; cancelled",
                    line
                ));
                return;
            };
            let Some(index) = n.checked_sub(1).filter(|i| *i < entries.len()) else {
                println_text(&format!("disk setup: no device number {}", n));
                return;
            };
            let name = entries[index].name.clone();
            let blank = entries[index].is_blank();
            let state = entries[index].state_text();
            if blank {
                format_and_mount(&name, false);
                return;
            }
            // Устройство с данными: оператор должен назвать его обратно. Это
            // подтверждение, для которого и существует `allow_destructive` у `format_policy`.
            println_text(&format!(
                "{} holds {} — formatting destroys it.",
                name, state
            ));
            println_text(&format!(
                "Type the device name (`{}`) to confirm, or press Enter to cancel.",
                name
            ));
            *WIZARD.lock() = Some(Step::ConfirmName { entries, index });
        }
        Some(Step::ConfirmName { entries, index }) => {
            let Some(entry) = entries.get(index) else {
                return;
            };
            if line != entry.name {
                println_text(&format!(
                    "disk setup: cancelled (expected `{}`)",
                    entry.name
                ));
                return;
            }
            format_and_mount(&entry.name, true);
        }
    }
}

/// Отформатировать `name` и смонтировать результат в `/mnt`.
fn format_and_mount(name: &str, allow_destructive: bool) {
    format_and_mount_with(name, allow_destructive, true)
}

/// Отформатировать `name`; смонтировать тоже, если `mount_after` не false.
///
/// `--no-mount` существует, чтобы диск можно было ПОДГОТОВИТЬ, пока `/mnt`
/// занят другой ФС: установка ФС на раздел — шаг, который нужен пользователю
/// перед переключением монтирования, а umount в pagh нет — альтернативой было
/// бы «нельзя форматировать ничего, пока /mnt занят».
fn format_and_mount_with(name: &str, allow_destructive: bool, mount_after: bool) {
    let Some(blk) = crate::drivers::get_block(name) else {
        println_text(&format!("disk: {} disappeared", name));
        return;
    };
    if mount_after
        && crate::vfs::mount_snapshot()
            .iter()
            .any(|m| m.point == "/mnt")
    {
        println_text(
            "disk: /mnt is already mounted; use `--no-mount` to just format, or reboot to change the mounted device",
        );
        return;
    }

    // Та же классификация и то же решение, что у boot-пути; устройство,
    // отвергнутое здесь, было бы отвергнуто и там.
    let probe = crate::fs::device::probe_boot_area(&*blk);
    let has_superblock = crate::fs::ext2::Ext2Fs::has_valid_superblock(&*blk);
    if let Err(refusal) = format_allowed(
        probe.layout(),
        has_superblock,
        probe.is_marked() || allow_destructive,
    ) {
        let reason = match refusal {
            Refusal::ExistingFilesystem => "a valid ext2 filesystem is on it",
            other => other.reason(),
        };
        println_text(&format!("disk: refusing to format {}: {}", name, reason));
        return;
    }

    println_text(&format!(
        "disk: formatting {} ({}) as ext2 + WAL...",
        name,
        probe.layout().name()
    ));
    if let Err(e) = crate::fs::ext2::Ext2Fs::format(Arc::clone(&blk) as Arc<dyn BlockDevice>) {
        println_text(&format!("disk: format failed: {:?}", e));
        return;
    }
    if !mount_after {
        println_text(&format!(
            "disk: {} formatted as ext2 + WAL (not mounted; a reboot will use it)",
            name
        ));
        return;
    }
    match crate::fs::ext2::Ext2Fs::mount(Arc::clone(&blk) as Arc<dyn BlockDevice>) {
        Ok(root) => {
            if let Err(e) = crate::vfs::mount_at_named("/mnt", root, "ext2") {
                println_text(&format!("disk: mount at /mnt failed: {:?}", e));
                return;
            }
            println_text(&format!("disk: {} formatted and mounted at /mnt", name));
            // Тот же follow-up, что у boot-пути: дерево userland, которое ждут
            // shell и `lxrun`, появляется только после seeding `/mnt` провижинингом.
            crate::provision::seed();
            println_text("disk: /mnt seeded; try `ls /mnt`");
        }
        Err(e) => println_text(&format!("disk: post-format mount failed: {:?}", e)),
    }
}

/// Смонтировать уже отформатированное устройство в `/mnt`.
fn mount(name: &str) {
    let Some(blk) = crate::drivers::get_block(name) else {
        println_text("disk: no device named that");
        return;
    };
    if crate::vfs::mount_snapshot()
        .iter()
        .any(|m| m.point == "/mnt")
    {
        println_text("disk: /mnt is already mounted; reboot to change the device");
        return;
    }
    match crate::fs::ext2::Ext2Fs::mount(Arc::clone(&blk) as Arc<dyn BlockDevice>) {
        Ok(root) => match crate::vfs::mount_at_named("/mnt", root, "ext2") {
            Ok(()) => {
                println_text(&format!("disk: {} mounted at /mnt", name));
                crate::provision::seed();
            }
            Err(e) => println_text(&format!("disk: mount at /mnt failed: {:?}", e)),
        },
        Err(e) => println_text(&format!("disk: {} cannot be mounted: {:?}", name, e)),
    }
}

/// Команда shell: `disk [list|setup|mount N|format N [--force]|cancel]`.
pub fn cmd(args: &[&str]) {
    match args.first().copied() {
        None | Some("list") => cmd_list(),
        Some("setup") | Some("wizard") => cmd_setup(),
        Some("cancel") => {
            cancel();
            println_text("disk: cancelled");
        }
        Some("mount") => match args.get(1).and_then(|a| a.parse::<usize>().ok()) {
            Some(n) if n >= 1 => cmd_mount(n - 1),
            _ => println_text("usage: disk mount <number>"),
        },
        Some("format") => match args.get(1).and_then(|a| a.parse::<usize>().ok()) {
            Some(n) if n >= 1 => cmd_format(
                n - 1,
                args.iter().any(|a| *a == "--force"),
                !args.iter().any(|a| *a == "--no-mount"),
            ),
            _ => println_text("usage: disk format <number> [--force] [--no-mount]"),
        },
        Some(other) => println_text(&format!("disk: unknown subcommand `{}`", other)),
    }
}
