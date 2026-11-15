// boot.rs — оркестратор загрузки ядра
// 64-битное ядро x86_64 под Limine на Rust (#![no_std])
//
// Этот модуль владеет упорядоченной последовательностью инициализации ядра,
// которая раньше жила прямо в `lib.rs::_start` (Requirement 1). Теперь `_start`
// просто делегирует в `boot::start()`.
//
// Структура (Requirement 1):
//  * 1.1 — инициализация идёт явной упорядоченной последовательностью шагов:
//          одна приватная `fn` на фазу, вызываемая по порядку из `start()`.
//  * 1.3 — каждый завершённый шаг печатает одну короткую строку `info!`;
//          подробности остаются на `debug!`.
//  * 1.4 — отсутствующий обязательный ответ Limine или упавший шаг проходят
//          через централизованный `fatal()`, который пишет одну строку `error!`
//          с именем шага и уходит в `arch::cpu::halt_loop()`.
//
// Поведение намеренно совпадает с прежним inline-`_start`. Inline-маппинг
// MMIO LAPIC/IOAPIC пока остаётся здесь (переедет к владельцам APIC/памяти).
// `KERNEL_SIZE` точно вычисляется из границ образа ядра (символы линкера)
// в `read_limine_responses`.

use core::sync::atomic::Ordering;

use crate::{arch, drivers, memory, net, shell, task, vfs};
use crate::{debug, error, info, warn};
use crate::{BASE_REVISION, HHDM_REQUEST, KERNEL_ADDR_REQUEST, MEMMAP_REQUEST, MODULES_REQUEST};
use crate::{HHDM_OFFSET, KERNEL_BASE, KERNEL_SIZE};

/// Централизованный обработчик фатальной ошибки загрузки (Requirement 1.4).
///
/// Пишет ровно одну строку `error!` с именем упавшего шага, затем навсегда
/// паркует CPU через безопасный halt-примитив `arch::cpu`. Не возвращается.
fn fatal(step: &str) -> ! {
    error!("boot: {} failed", step);
    crate::drivers::earlyfb::errorln(format_args!("BOOT FAILED at step: {}", step));
    arch::cpu::halt_loop();
}

/// Объявить, что шаг загрузки вот-вот начнётся, на аварийной консоли.
///
/// Печатается ДО шага: зависший шаг никогда не напечатает свой `ok`, поэтому
/// строка с его именем в состоянии «выполняется» — именно та, на которую надо
/// смотреть (так читается отчёт «встаёт после heap»: heap завершён, значит
/// висит шаг `> apic` под ним).
fn step_begin(visible: bool, name: &str) {
    if visible {
        crate::drivers::earlyfb::println(format_args!("  >  {}", name));
    }
}

/// Отметить завершённый шаг загрузки на аварийной консоли (если она есть).
fn step(visible: bool, name: &str) {
    if visible {
        crate::drivers::earlyfb::println(format_args!("  ok  {}", name));
    }
}

/// Выполнить упорядоченную инициализацию ядра и уйти в idle-цикл.
///
/// Каждый вызов ниже — один шаг инициализации (Requirement 1.1). Шаги, которым
/// нужен ответ Limine, читают его из статиков запросов в `lib.rs`; отсутствующий
/// ответ ведёт к [`fatal`] (Requirement 1.4).
pub fn start() -> ! {
    // Шаг 0: включить SSE прежде всего остального. Ядро и обработчики
    // `extern "x86-interrupt"` в Rust генерируют SSE-инструкции (`movaps`), но
    // Limine передаёт управление без OS-enable SSE, поэтому это должно
    // произойти до любого FP/SSE-использования и до включения прерываний.
    // См. `arch::cpu::enable_sse`.
    arch::cpu::enable_sse();

    // Аварийная консоль поднимается до всего, что может упасть, и не требует
    // heap: на реальном ПК нет serial-порта, поэтому без неё сбой любого
    // позднего шага оставил бы чёрный экран. С этого момента каждый шаг
    // печатает своё имя, так что остановившаяся машина показывает, где встала.
    // Строки лога захватываются в file-mirror ring с самого первого сообщения:
    // boot-лог нужен оператору после сбоя, а пишется он до появления файловой
    // системы (поток-flusher сливает ring после монтирования /mnt).
    crate::logfile::enable_buffering();

    let visible = crate::drivers::earlyfb::init();
    if visible {
        crate::drivers::earlyfb::clear();
        crate::drivers::earlyfb::println(format_args!(
            "pagh {} booting (emergency console)",
            env!("CARGO_PKG_VERSION")
        ));
    }

    step_begin(visible, "init_serial");
    init_serial();
    step(visible, "serial");
    step_begin(visible, "check_base_revision");
    check_base_revision();
    step(visible, "base revision");
    step_begin(visible, "read_limine_responses");
    read_limine_responses();
    step(visible, "limine responses");
    step_begin(visible, "init_descriptor_tables");
    init_descriptor_tables();
    step(visible, "gdt + idt");
    step_begin(visible, "init_syscalls");
    init_syscalls();
    step(visible, "syscalls");
    step_begin(visible, "init_pmm");
    init_pmm();
    step(visible, "pmm");
    step_begin(visible, "init_vmm");
    init_vmm();
    step(visible, "vmm");
    step_begin(visible, "init_heap");
    init_heap();
    step(visible, "heap");
    step_begin(visible, "init_apic");
    init_apic();
    step(visible, "apic");
    step_begin(visible, "init_drivers");
    init_drivers();
    step(visible, "drivers");
    step_begin(visible, "init_virtio");
    init_virtio();
    step(visible, "virtio");
    step_begin(visible, "init_scheduler");
    init_scheduler();
    step(visible, "scheduler");
    step_begin(visible, "init_usb");
    init_usb();
    step(visible, "usb");
    step_begin(visible, "init_vfs");
    init_vfs();
    step(visible, "vfs");
    step_begin(visible, "init_ramdisk");
    init_ramdisk();
    step(visible, "ramdisk");
    step_begin(visible, "init_fs");
    init_fs();
    step(visible, "fs");
    step_begin(visible, "init_net");
    init_net();
    step(visible, "net");

    // Загрузка окончена: стереть аварийный трассировочный вывод, чтобы shell
    // начался с чистого экрана. Консоль остаётся рабочей — поздняя паника
    // всё равно печатается здесь.
    if visible {
        crate::drivers::earlyfb::clear();
    }

    // Передача в run-фазу (спавнит shell, включает прерывания, уходит в idle).
    kernel_main();
}

/// Шаг 1: поднять COM1 (основной приёмник логов).
fn init_serial() {
    drivers::serial::init();
    info!("serial");
}

/// Шаг 2: проверить, что Limine дал поддерживаемую базовую ревизию.
fn check_base_revision() {
    if !BASE_REVISION.is_supported() {
        fatal("base revision");
    }
    info!("base revision");
}

/// Шаг 3: прочитать обязательные ответы Limine (смещение HHDM + адрес ядра)
/// и сохранить их в глобальные ячейки адресов в `lib.rs`.
fn read_limine_responses() {
    match HHDM_REQUEST.response() {
        Some(hhdm) => HHDM_OFFSET.store(hhdm.offset, Ordering::Relaxed),
        None => fatal("hhdm"),
    }

    match KERNEL_ADDR_REQUEST.response() {
        Some(kaddr) => {
            KERNEL_BASE.store(kaddr.virtual_base, Ordering::Relaxed);
            // KERNEL_SIZE — размер образа ядра, точно вычисленный из символов
            // линкера (`__kernel_end - __kernel_start`), а не угаданный.
            // KERNEL_BASE остаётся виртуальной базой Limine.
            KERNEL_SIZE.store(memory::layout::kernel_size(), Ordering::Relaxed);
        }
        None => fatal("kernel address"),
    }

    info!("limine responses");
}

/// Шаг 4: загрузить GDT (с TSS/IST) и установить IDT.
fn init_descriptor_tables() {
    arch::x86_64::gdt::init();
    arch::x86_64::idt::init();
    info!("gdt + idt");
}

/// Шаг 4.5: включить быстрый интерфейс системных вызовов SYSCALL/SYSRET.
///
/// Должен идти после `init_descriptor_tables`, потому что `syscall::init` читает
/// селекторы GDT (`Selectors::user_code()` / `kernel_code()`) для программирования
/// STAR.
fn init_syscalls() {
    arch::x86_64::syscall::init();
    info!("syscalls");
}

/// Шаг 5: инициализировать аллокатор физических кадров по memmap Limine.
fn init_pmm() {
    match MEMMAP_REQUEST.response() {
        Some(memmap) => memory::pmm::init(memmap),
        None => fatal("memmap"),
    }
    info!("pmm");
}

/// Шаг 6: инициализировать менеджер виртуальной памяти поверх HHDM.
fn init_vmm() {
    let hhdm_offset = HHDM_OFFSET.load(Ordering::Relaxed);
    memory::vmm::init(hhdm_offset);
    info!("vmm");
}

/// Шаг 7: инициализировать кучу ядра / глобальный аллокатор.
fn init_heap() {
    memory::heap::init();
    info!("heap");
}

/// Шаг 8: инициализировать APIC и направить IRQ клавиатуры.
///
/// APIC сам владеет маппингом MMIO LAPIC/IOAPIC (Requirement 7.4): `apic::init()`
/// создаёт эти маппинги внутри себя через `vmm::map_mmio` до работы с регистрами,
/// так что inline-маппинга MMIO здесь нет.
fn init_apic() {
    arch::x86_64::apic::init();

    // Направить IRQ1 (клавиатура) -> вектор 33 и зарегистрировать обработчик.
    arch::x86_64::apic::route_irq(1, 33);
    arch::x86_64::apic::register_irq(33, drivers::ps2_kbd::irq_handler);

    // Направить IRQ12 (PS/2 мышь) -> вектор 44 и зарегистрировать обработчик.
    // Само устройство включается позже в `init_drivers` (когда framebuffer уже
    // поднят, чтобы знать границы экрана); ранняя настройка записи
    // перенаправления IOAPIC безвредна, потому что пакеты не идут, пока
    // устройство не начнёт сообщать данные.
    arch::x86_64::apic::route_irq(12, 44);
    arch::x86_64::apic::register_irq(44, drivers::ps2_mouse::irq_handler);

    info!("apic");
}

/// Шаг 9: инициализировать device manager (PS/2 клавиатура + framebuffer).
///
/// Framebuffer не нуждается в собственном маппинге MMIO: Limine отображает его
/// в верхнюю половину и отдаёт уже замапленный виртуальный адрес
/// (`fb.address()`), так что драйвер работает с этим маппингом напрямую.
fn init_drivers() {
    drivers::init();
    info!("drivers");
}

/// Шаг 9.5: найти и подключить virtio-устройства (перечисление PCI + драйверы).
///
/// Идёт после `init_drivers`, чтобы heap был поднят (PCI `enumerate()`
/// аллоцирует `Vec`). Подключает virtio-blk диск как `BlockDevice`
/// ("virtio-blk0"). Если virtio-устройства нужного типа нет, соответствующий
/// attach пишет предупреждение и ничего не делает, сохраняя загрузку (R17.4).
///
/// Здесь же `net::init` подключает сетевую карту e1000.
fn init_virtio() {
    let devs = drivers::pci::enumerate();
    drivers::virtio::blk::init_blk(&devs);
    info!("virtio");
}

/// Шаг 10: инициализировать вытесняющий планировщик.
///
/// Заодно фиксируется wall-clock время загрузки для строки `btime` в
/// `/proc/stat`: CMOS RTC читается до включения прерываний, и чтение здесь
/// (а не лениво в рендерере) делает `btime` моментом загрузки, как в Linux.
/// Мёртвый/до-эпоховый RTC даёт `0`; файл печатает его дословно, а контракт
/// документирует это как честный пробел (`docs/procfs.md` §4.8).
fn init_scheduler() {
    task::scheduler::init();
    task::stat::set_boot_unix(arch::x86_64::linux::rtc::now_unix());
    info!("scheduler");
}

/// Шаг 11: инициализировать виртуальную файловую систему.
fn init_vfs() {
    debug!("About to call vfs::init()...");
    vfs::init();
    debug!("vfs::init() returned");
    info!("vfs");
}

/// Шаг 11.35: поднять хост-контроллер USB, если он есть в машине.
///
/// Идёт после обхода PCI (его уже сделал virtio-шаг) и до файловой системы,
/// потому что USB-клавиатура — INPUT-устройство: чем раньше она жива, тем
/// раньше можно печатать на машине без PS/2. Сбой не фатален — нет контроллера
/// или он не стартует — остаётся путь PS/2 (и то, что прошивка в него
/// транслирует).
fn init_usb() {
    let devices = crate::drivers::pci::enumerate();
    if crate::drivers::xhci::init(&devices) {
        info!("xhci");
    } else {
        debug!("usb: no xHCI controller");
    }
}

/// Шаг 11.4: зарегистрировать загруженный с загрузчика образ диска как RAM-диск.
///
/// Зачем: на реальном железе ядро может смонтировать только устройство, для
/// которого есть драйвер (virtio-blk, NVMe), поэтому машина только с SATA — или
/// та, чей диск оператор не хочет отдавать, — оставалась без `/mnt`. Limine
/// умеет загружать файлы рядом с ядром, так что образ диска системы едет на
/// загрузочном носителе и монтируется из памяти. Устройство регистрируется под
/// именем самого модуля (`disk.img` -> `ram0`), чтобы `disk list` показывал,
/// откуда взялась файловая система.
///
/// Правило выбора: ПЕРВЫЙ модуль, имя которого оканчивается на `.img`.
/// Намеренно и узко — модуль `pagh.elf` или `notes.txt` не диск, а список
/// пишет только загрузочная конфигурация.
fn init_ramdisk() {
    let Some(response) = MODULES_REQUEST.response() else {
        return;
    };
    for module in response.modules() {
        let path = module.path();
        if !path.ends_with(".img") {
            continue;
        }
        let data = module.data();
        if data.is_empty() {
            warn!("ramdisk: module {} is empty; skipped", path);
            continue;
        }
        // SAFETY: a Limine module is a kernel-owned buffer in the HHDM that the
        // PMM keeps reserved for the kernel's lifetime, and no other block device
        // aliases it. `drivers::memdisk` only does bounds-checked byte copies.
        let sectors = unsafe {
            crate::drivers::memdisk::register("ram0", data.as_ptr() as usize, data.len())
        };
        if sectors == 0 {
            warn!(
                "ramdisk: module {} is smaller than one sector; skipped",
                path
            );
            continue;
        }
        info!(
            "ramdisk: {} ({} KiB, {} sectors) registered as ram0",
            path,
            data.len() / 1024,
            sectors
        );
        return;
    }
}

/// Шаг 11.5: поднять ext2 на реальном диске virtio-blk/NVMe и смонтировать
/// его в `/mnt` (R5.6/R9.1/R9.2).
///
/// Порядок: идёт *после* `init_vfs` (а не внутри `init_virtio`), потому что
/// `vfs::mount_at` требует уже существующий корень VFS. Само блочное
/// устройство зарегистрировано раньше в `init_virtio`; heap/pmm/vmm подняты,
/// прерывания во время boot-init ещё выключены, поэтому форматирование и
/// монтирование, диск I/O и аллокации идут по одному непрерываемому пути.
///
/// Если нет ни virtio-blk, ни NVMe — предупреждение и загрузка продолжается
/// (R17.4). При первой загрузке устройство форматируется, но **только если оно
/// действительно пустое или несёт явный opt-in маркер**: решение живёт в
/// [`crate::fs::format_policy`], а любой другой расклад — валидный ext2
/// superblock, таблица разделов или любые ненулевые данные — отвергается по
/// имени (issue #33; на реальном железе SSD самой машины — как раз такое
/// устройство, а NVMe-путь берёт всё пространство имён, не раздел). После
/// успешного монтирования одноразовый self-demo пишет файл в `/mnt` и читает
/// его обратно, доказывая журналируемый путь записи+чтения на реальном диске
/// при headless-загрузке.
fn init_fs() {
    use crate::fs::ext2::Ext2Fs;
    use crate::fs::format_policy::{format_allowed, Refusal};

    // Подключить NVMe до обхода разделов: на реальной машине это тот диск,
    // что несёт таблицу разделов, и раздел с ext2 должен быть виден до выбора
    // fallback на RAM-диск.
    // Раньше этот путь выполнялся только при отсутствии RAM-диска, что под BIOS
    // означало, что он не выполнялся вообще — и оказалось, что там он сломан
    // (см. `nvme::bar0_mmio`: BAR, читающийся как все единицы, мапился как
    // устройство). Теперь он используется, поэтому и проверяется.
    {
        let devices = crate::drivers::pci::enumerate();
        if let Ok(true) = crate::drivers::nvme::attach(&devices) {
            crate::drivers::nvme::register_block_device();
            info!("fs: NVMe controller attached");
        }
    }

    // Разделы для каждого зарегистрированного целого устройства: на реальной
    // машине диск несёт таблицу разделов, и pagh должен уметь использовать ОДИН
    // раздел, не трогая остальные (см. `drivers::partition`).
    for (name, _sectors) in drivers::list_blocks() {
        if let Some(dev) = drivers::get_block(&name) {
            // Только целые устройства: сами разделы никогда не разбираются повторно.
            let is_partition = match name.rfind('p') {
                Some(pos) => {
                    !name[pos + 1..].is_empty()
                        && name[pos + 1..].chars().all(|c| c.is_ascii_digit())
                }
                None => false,
            };
            if !is_partition {
                drivers::partition::register_partitions(&dev);
            }
        }
    }

    // Оператор смотрит список до того, как ядро что-то решит. Это ЕДИНСТВЕННОЕ
    // место, где раздел может быть отформатирован или чужая файловая система
    // занята: ядро не отличит раздел, предназначенный pagh, от просто пустого,
    // а ошибка стоит данных. Меню истекает в неразрушающий вариант по
    // умолчанию, так что загрузка без оператора не меняется.
    // Классифицировать один раз и передать тот же список меню: то, что видит
    // оператор, должно быть ровно тем, с чем работает boot-путь. Пропущенное
    // меню затем переиспользует его, и ни один раздел не проверяется дважды за
    // загрузку.
    let partitions = drivers::partition::candidates();
    let menu = crate::boot_menu::run(&partitions);

    // SAFETY-CRITICAL, и поэтому ниже ничего не форматирует само по себе: без
    // выбора в меню boot-путь только МОНТИРУЕТ том, который уже несёт
    // собственный журнальный маркер pagh. Структурно валидная ext2/ext4 чужая
    // файловая система — это чья-то Linux-система: её монтирование перепишет
    // group descriptors и зарезервирует WAL-блоки внутри, а форматирование
    // будет ещё хуже. Такие разделы перечислены в меню и не трогаются здесь.
    for (dev, content) in &partitions {
        if matches!(content, drivers::partition::PartitionContent::ForeignExt2) {
            warn!(
                "fs: {} holds a foreign (non-pagh) filesystem; leaving it untouched",
                dev.name()
            );
        }
    }

    // Приоритет носителей: выбор оператора, иначе том, который pagh сам
    // отформатировал на подключённом диске, иначе образ диска с загрузочного
    // носителя (система, приехавшая на флешке), иначе virtio-blk (QEMU/dev
    // образы), иначе NVMe.
    let mut blk = match &menu {
        crate::boot_menu::Choice::Mount(dev) => {
            info!("fs: operator chose to mount {}", dev.name());
            Some(alloc::sync::Arc::clone(dev))
        }
        crate::boot_menu::Choice::Format(dev) => {
            warn!("fs: operator chose to format {}", dev.name());
            Some(alloc::sync::Arc::clone(dev))
        }
        crate::boot_menu::Choice::Skip => partitions
            .iter()
            .find(|(_, content)| {
                matches!(content, drivers::partition::PartitionContent::PaghVolume)
            })
            .map(|(dev, _)| {
                info!("fs: using partition {} (pagh volume)", dev.name());
                alloc::sync::Arc::clone(dev)
            })
            .or_else(|| {
                drivers::get_block("ram0").inspect(|_| {
                    info!("fs: using the RAM disk from the boot medium");
                })
            })
            .or_else(|| drivers::get_block("virtio-blk0"))
            .or_else(|| {
                let devices = crate::drivers::pci::enumerate();
                if let Ok(true) = crate::drivers::nvme::attach(&devices) {
                    crate::drivers::nvme::register_block_device();
                    info!("fs: using NVMe block device");
                }
                drivers::get_block("nvme0")
            }),
    };
    let blk = match blk.take() {
        Some(b) => b,
        None => {
            warn!("fs: no block device at all; /mnt not mounted");
            warn!("fs: attach a disk, or boot an image that carries one (`module_path: boot():/disk.img`)");
            info!("fs (no disk)");
            return;
        }
    };

    // Форматировать только то, что человек явно попросил отформатировать в
    // меню, набрав имя устройства. Всё остальное здесь монтируется или
    // отвергается.
    // Раньше это опиралось только на `fs::format_policy`, и пустой раздел,
    // выбранный boot-путём, форматировался без вопроса. Теперь он не
    // выбирается, если оператор не выбрал его (меню) или это RAM-диск
    // (черновой том, исчезающий при выключении) — см. ниже.
    let root = match menu {
        crate::boot_menu::Choice::Format(_) => {
            warn!(
                "fs: formatting {} on the operator's explicit instruction (menu)",
                blk.name()
            );
            if let Err(e) = Ext2Fs::format(blk.clone()) {
                error!("fs: format failed: {:?}; /mnt not mounted", e);
                return;
            }
            match Ext2Fs::mount(blk.clone()) {
                Ok(root) => root,
                Err(e) => {
                    error!("fs: post-format mount failed: {:?}", e);
                    return;
                }
            }
        }
        // Явное «смонтировать это» НИКОГДА молча не превращается в
        // «отформатировать это»: стирание тома не шаг восстановления, и
        // оператор, выбравший строку, не просил об этом. Сообщение намеренно
        // говорит, как этого добиться.
        crate::boot_menu::Choice::Mount(_) => match Ext2Fs::mount(blk.clone()) {
            Ok(root) => root,
            Err(e) => {
                error!(
                    "fs: mounting {} failed: {:?}; nothing was written - reboot and press F in the disk menu to format it",
                    blk.name(),
                    e
                );
                info!("fs (mount failed, nothing formatted)");
                return;
            }
        },
        crate::boot_menu::Choice::Skip => match Ext2Fs::mount(blk.clone()) {
            Ok(root) => root,
            Err(first_error) => {
                let has_superblock = Ext2Fs::has_valid_superblock(&*blk);
                // Проверка выполняется только здесь: неудачное монтирование —
                // единственная ситуация, когда стирание устройства вообще
                // рассматривается, так что счастливый путь за неё не платит.
                let probe = crate::fs::device::probe_boot_area(&*blk);
                // Единственный способ разрешить форматирование непустого
                // устройства: оператор намеренно записал
                // `format_policy::OPT_IN_MARKER` в сектор 0. Ничто другое — ни
                // флаг загрузчика, ни конфиг — не включает это, и маркер не
                // может появиться случайно.
                let allow_destructive = probe.is_marked();
                if allow_destructive {
                    warn!(
                        "fs: {} carries the PAGH-FORMAT opt-in marker; formatting over existing data",
                        blk.name()
                    );
                }
                match format_allowed(probe.layout(), has_superblock, allow_destructive) {
                    Ok(()) => {
                        info!(
                            "fs: formatting {} ({}), ext2 + WAL...",
                            blk.name(),
                            probe.layout().name()
                        );
                        if let Err(e) = Ext2Fs::format(blk.clone()) {
                            error!("fs: format failed: {:?}; /mnt not mounted", e);
                            return;
                        }
                        match Ext2Fs::mount(blk.clone()) {
                            Ok(root) => root,
                            Err(e) => {
                                error!("fs: post-format mount failed: {:?}", e);
                                return;
                            }
                        }
                    }
                    Err(refusal) => {
                        match refusal {
                            // Формулировка сохранена байт-в-байт: это историческая
                            // строка отказа, и документация её цитирует.
                            Refusal::ExistingFilesystem => error!(
                                "fs: existing ext2 mount failed: {:?}; refusing destructive reformat",
                                first_error
                            ),
                            other => error!(
                                "fs: refusing to format {} ({}): {:?}",
                                blk.name(),
                                other.reason(),
                                first_error
                            ),
                        }
                        // Пути вперёд внутри гостя нет (в shell нет `mkfs`),
                        // поэтому назовите варианты оператора вместо того,
                        // чтобы оставлять отказ последним словом в serial-строке.
                        error!(
                            "fs: {} left untouched; reboot and use the disk menu (F + the typed \
                             device name), or run `disk setup` at the shell to format it \
                             deliberately, or zero the start of the device (`wipefs -a` / `dd` \
                             from another system) / write the {} marker",
                            blk.name(),
                            crate::fs::format_policy::OPT_IN_MARKER_NAME
                        );
                        return;
                    }
                }
            }
        },
    };

    match vfs::mount_at_named("/mnt", root, "ext2") {
        Ok(()) => {
            info!("ext2 mounted at /mnt");
            fs_boot_demo();
            crate::provision::seed();
        }
        Err(e) => error!("fs: mount_at(/mnt) failed: {:?}", e),
    }
    info!("fs");
}

/// Насколько глубоко [`probe_boot_area`] смотрит в устройство: 1 MiB.
///
/// Достаточно для таблицы разделов GPT/MBR, ext2 superblock и layouts
/// boot-сектора или superblock-по-смещению-0 любой ФС, которая может быть у
/// пользователя (NTFS, XFS, LUKS, ZFS, ext4-в-разделе). btrfs держит superblock
/// на 64 KiB — тоже внутри окна.
///
/// **Окно — реальное ограничение, а не формальность**: расклад, у которого всё
/// лежит за первым MiB (сигнатура `mkswap` в хвосте устройства, superblock
/// mdadm 1.0, внешние RAID-метаданные IMSM/DDF или устройство, начало которого
/// занулили прерванные `wipefs`/`dd`), проверяется как `Blank` и
/// форматируется. Ограничение проверки — то, что не даёт загрузке читать весь
/// SSD; цена — «пустой» означает «пустой в первом MiB».
const PROBE_LIMIT: u64 = 1024 * 1024;

/// Шаг 11.6: поднять сеть. Перечисляет PCI, подключает NIC e1000, строит
/// состояние собственного TCP/IP-стека и включает UDP echo-сервис на порту 7.
/// Получение адреса (DHCP, статический fallback) и обслуживание пакетов идут в
/// net-потоке, который спавнится из `kernel_main` после включения прерываний и
/// запуска тика LAPIC, двигающего сетевые часы.
///
/// Если virtio-net нет — предупреждение в лог и загрузка продолжается (R17.3).
/// Сеть полностью опциональна — загрузка всегда сохраняется.
fn init_net() {
    match net::init() {
        Ok(()) => {
            net::udp_echo_enable(7);
            net::tcp_echo_listen(7);
            info!("net");
        }
        Err(e) => {
            warn!("net: no interface available ({:?}); networking disabled", e);
            info!("net (no nic)");
        }
    }
}

/// Одноразовый boot self-demo: прогнать журналируемую запись и чтение с
/// реального диска, чтобы headless-загрузка доказала, что путь работает
/// end-to-end. Пишет известную строку в `/mnt/bootdemo.txt`, затем перечитывает
/// её через свежий VFS-lookup и логирует PASS/FAIL. Не фатально: любая ошибка
/// логируется, загрузка продолжается.
fn fs_boot_demo() {
    use crate::vfs::{self, VfsError};

    const CONTENT: &[u8] = b"pagh-ext2-journaled-write-OK";

    let dir = match vfs::lookup_path("/mnt") {
        Ok(d) => d,
        Err(e) => {
            warn!("fs demo: /mnt lookup failed: {:?}", e);
            return;
        }
    };

    // Создать или открыть demo-файл (терпеть его существование с прошлой
    // загрузки, так как образ диска переживает запуски).
    let file = match dir.create_file("bootdemo.txt") {
        Ok(f) => f,
        Err(VfsError::AlreadyExists) => match dir.lookup("bootdemo.txt") {
            Ok(f) => {
                // Диск переживает загрузки; более короткая demo-строка не должна
                // оставлять хвост прошлого запуска (write(2) не обрезает файл,
                // совсем как POSIX).
                let _ = f.truncate(0);
                f
            }
            Err(e) => {
                warn!("fs demo: open existing failed: {:?}", e);
                return;
            }
        },
        Err(e) => {
            warn!("fs demo: create_file failed: {:?}", e);
            return;
        }
    };

    match file.write(0, CONTENT) {
        Ok(n) if n == CONTENT.len() => {}
        Ok(n) => {
            warn!("fs demo: short write ({} of {} bytes)", n, CONTENT.len());
            return;
        }
        Err(e) => {
            warn!("fs demo: write failed: {:?}", e);
            return;
        }
    }
    dir.sync();

    // Перечитать через свежий lookup, чтобы не зависеть от кэшированного состояния узла.
    let rfile = match vfs::lookup_path("/mnt/bootdemo.txt") {
        Ok(f) => f,
        Err(e) => {
            warn!("fs demo: read-back lookup failed: {:?}", e);
            return;
        }
    };
    let mut buf = [0u8; 64];
    match rfile.read(0, &mut buf) {
        Ok(n) if n == CONTENT.len() && &buf[..n] == CONTENT => {
            info!(
                "fs demo: /mnt/bootdemo.txt write+read round-trip PASS ({} bytes)",
                n
            );
        }
        Ok(n) => {
            error!("fs demo: read-back MISMATCH ({} bytes): FAIL", n);
        }
        Err(e) => warn!("fs demo: read-back failed: {:?}", e),
    }
}

/// Run-фаза: запустить поток shell, включить прерывания и стать idle-циклом.
///
/// Старый отладочный блок busy-spin на ~5M циклов (наблюдение IRQ таймера при
/// bringup) удалён согласно Requirement 1.5.
fn kernel_main() -> ! {
    // Проба энтропии (issue #16). `AT_RANDOM` не может fail closed — glibc
    // читает его безусловно при старте процесса, — поэтому без RDSEED/RDRAND
    // значение выводится из смешанного boot-seed, а не из старого
    // предсказуемого xorshift; эта строка сообщает оператору об использовании
    // fallback ДО старта первого процесса. С аппаратной энтропией — no-op (без
    // строки лога).
    crate::security::entropy::report_capabilities();

    debug!("Spawning shell thread...");
    task::scheduler::kernel_thread_spawn_named(shell_thread, "shell");
    debug!("Shell thread spawned (PID should be 1)");

    // Спавнить поток опроса сети (R13.4). Он должен стартовать, когда
    // планировщик уже работает, а прерывания включены (ниже), потому что часы
    // стека двигаются от тика LAPIC и обслуживание DHCP/echo опирается на
    // периодический poll. Если NIC не подключён, `net::poll` — дешёвый no-op,
    // так что безусловный spawn безвреден.
    task::scheduler::kernel_thread_spawn_named(net::net_thread, "net");

    // Поток опроса USB: перечисляет новые устройства и собирает HID-отчёты.
    // Спавнится безусловно — без контроллера `xhci::poll` это no-op, а условие
    // потребовало бы второго источника истины об оборудовании, которое уже
    // предоставляет сам контроллер.
    task::scheduler::kernel_thread_spawn_named(crate::drivers::xhci::usb_thread, "usb");

    // Зеркало логов: сливает in-memory ring в /mnt/pagh-log.txt. Отдельный поток
    // ядра, а не inline-запись, потому что logger также работает в контексте
    // прерываний и паники, где обращение к файловой системе приведёт к дедлоку.
    task::scheduler::kernel_thread_spawn_named(crate::logfile::flush_thread, "logd");
    debug!("Net thread spawned");

    // Спавнить встроенный ring-3 тестовый пользовательский процесс
    // (Requirements 13.1 / 13.4).
    // Это ДОЛЖНО происходить, пока прерывания ещё выключены:
    // `create_user_process` ненадолго ставит пользовательский CR3, чтобы
    // заполнить адресное пространство, и тик таймера в этом окне дал бы
    // планировщику увидеть чужой CR3. Здесь же программируется TSS RSP0 для
    // перехода ring-3 → ring-0.
    // Процесс делает `SYS_WRITE` (видно на serial), затем `SYS_EXIT`, убирая
    // себя из ротации планировщика; поток shell продолжает работать, и промпт
    // остаётся интерактивным.
    // Пропускается при включённой фиче `lx_selftest`: этот стенд запускает
    // собственный ring-3 `Compat_Process`, а TSS `RSP0` ядра — одно-задачный
    // (см. `gdt::set_kernel_stack`), поэтому два ring-3 процесса делили бы
    // один стек ядра. Под фичей compat-процесс — единственная ring-3 задача,
    // и его `write`/`exit_group` чисто наблюдаются.
    #[cfg(not(any(
        feature = "lx_selftest",
        feature = "lx_livetest",
        feature = "lx_bigindex",
        feature = "lx_tlsbig"
    )))]
    match task::process::spawn_test_user_process() {
        Ok(pid) => info!("user test process spawned (pid {})", pid),
        Err(e) => error!("user test process failed: {}", e),
    }

    // Boot-time self-test Linux-совместимости (cargo feature `lx_selftest`).
    // Идёт здесь — после монтирования ext2 и init сети, пока прерывания ещё
    // выключены, — чтобы поставить в очередь Compat_Process, как нативный
    // тестовый процесс выше, и чтобы проверка `fetch_no_network` увидела
    // состояние «нет адреса интерфейса» до DHCP. Полностью компилируется вне
    // сборки при выключенной фиче (по умолчанию).
    #[cfg(feature = "lx_selftest")]
    crate::selftest_lx::run();

    // Пост-сетевые проверки в одном потоке, чтобы идти *последовательно* и не
    // конкурировать за сетевой насос. Локальный end-to-end `apt`-тест
    // запускается ПЕРВЫМ (быстро, против gateway хоста QEMU), затем внешний
    // HTTPS smoke (может быть медленным/недоступным и иначе монополизировал бы
    // насос). Компилируется вне сборки при выключенной фиче.
    #[cfg(feature = "lx_selftest")]
    task::scheduler::kernel_thread_spawn_named(
        crate::selftest_lx::run_post_net_checks,
        "lx-selftest",
    );

    // Живая проверка полного обновления (cargo feature `lx_livetest`).
    // Спавнится отдельным потоком, чтобы идти после поднятия прерываний/DHCP.
    // Отделён от `lx_selftest`: он ходит на живое зеркало `deb.debian.org`
    // (конфиг HTTPS по умолчанию, НЕ локальный mini-repo). На практике
    // взаимоисключающи: если включены обе фичи, побеждает локальный selftest,
    // а live-проверка пропускается, чтобы два ring-3 процесса не делили
    // одно-задачный стек ядра. Компилируется вне сборки при выключенной фиче.
    #[cfg(all(feature = "lx_livetest", not(feature = "lx_selftest")))]
    task::scheduler::kernel_thread_spawn_named(
        crate::selftest_lx::run_live_update_check,
        "lx-live",
    );

    // ДИАГНОСТИКА (Part B): воспроизведение краша на стадии разбора apt-update
    // на ЛОКАЛЬНОМ большом индексе (cargo feature `lx_bigindex`). Спавнится
    // отдельным потоком после включения прерываний/DHCP. Направляет apt на
    // `http://10.0.2.2:8000` (`tools/mini_repo.py bigindex`) и гоняет
    // `apt::update()` (или, с `lx_bigindex_inram`, in-RAM вариант Test-A).
    // Компилируется вне сборки при выключенной фиче.
    #[cfg(feature = "lx_bigindex")]
    task::scheduler::kernel_thread_spawn_named(
        crate::selftest_lx::run_bigindex_check,
        "lx-bigindex",
    );

    // Регрессия большого HTTPS-потока (cargo feature `lx_tlsbig`, issue #19).
    // Один многомегабайтный аутентифицированный `https_get` к полному
    // `Packages.gz` живого зеркала, изолирующий TLS-транспорт от конвейера
    // индексов. Пропускается, когда включён более широкий стенд, чтобы за
    // загрузку шла ровно одна живая внешняя загрузка (и никакие два ring-3
    // процесса не делили одно-задачный стек ядра).
    #[cfg(all(
        feature = "lx_tlsbig",
        not(any(feature = "lx_selftest", feature = "lx_livetest"))
    ))]
    task::scheduler::kernel_thread_spawn_named(crate::selftest_lx::run_tls_big_check, "lx-tlsbig");

    // Первая загрузка базового userland (glibc + python3) теперь opt-in.
    // shell_thread спрашивает Y/n в консоли и запускает
    // crate::provision::ensure_base_packages_thread только с согласия.

    arch::cpu::enable_interrupts();
    info!("interrupts enabled");

    // Главный поток становится idle-циклом.
    arch::cpu::halt_loop()
}

/// Поток ядра shell: дать планировщику стабилизироваться, затем запустить shell.
fn shell_thread() {
    // Дать планировщику время стабилизироваться.
    for _ in 0..1_000_000 {
        core::hint::spin_loop();
    }

    // Первая загрузка больше не останавливается на вопросе о загрузке python3:
    // свежий диск получает файловый скелет из `provision::seed()`, а userland
    // glibc/python ставится по требованию из shell (`apt update` +
    // `apt install python3`). Вопрос на загрузке блокировал shell в консольном
    // промпте без таймаута — загрузка так делать не должна.
    shell::shell_main();
}
