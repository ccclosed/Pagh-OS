// task/process.rs — создание пользовательских процессов.

use crate::arch::x86_64::gdt;
use crate::arch::x86_64::linux::cred::Access;
use crate::arch::x86_64::linux::mem::VmRegionSet;
use crate::memory::{pmm, vmm};
use crate::task::compat::{self, CompatState};
use crate::task::fd::FdTable;
use crate::task::scheduler;
use crate::task::scheduler::Tcb;
use crate::task::stack::{arg_gate, AuxInputs};
use crate::task::stack_map::{map_initial_stack, StackMapError};
use crate::vfs::elf::ElfLoader;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use x86_64::structures::paging::PageTableFlags;

use crate::memory::layout::{
    KERNEL_STACK_PAGES, PAGE_SIZE, USER_MMAP_BASE, USER_STACK_PAGES, USER_STACK_TOP,
};

/// Выделить и отобразить свежий стек ядра на PID в ядерной верхней половине и
/// запрограммировать TSS `RSP0` (и стек ядра для инструкции `syscall`) на его
/// вершину, вернув (исключающую) вершину стека.
///
/// Общий код для [`create_user_process`] и [`run_linux_binary`]. Записи PML4
/// ядерной верхней половины разделяются по ссылке в каждый пользовательский
/// PML4 (клонируются в `new_user_pml4`), поэтому отображение, добавленное здесь
/// под уже присутствующей записью верхнего уровня, видно при активном
/// пользовательском CR3 — ровно это нужно входу ring-3 → ring-0, который
/// попадает на RSP0.
fn setup_task_kernel_stack(pid: u64) -> Result<u64, &'static str> {
    let (_guard_base, kstack_base, kstack_top) = crate::memory::layout::kernel_stack_for_pid(pid);

    let kflags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    for page in 0..KERNEL_STACK_PAGES {
        let vaddr = kstack_base + page * PAGE_SIZE;
        let frame = pmm::alloc_frame().ok_or("process: PMM OOM for kernel stack")?;
        vmm::map(frame, vaddr, kflags).map_err(|_| "process: VMM map failed (kernel stack)")?;
    }

    // Запрограммировать RSP0, чтобы переходы ring-3 → ring-0 попадали на этот
    // стек ядра. (Ограничение одной задачи описано у `gdt::set_kernel_stack`.)
    gdt::set_kernel_stack(kstack_top);
    // Отразить ту же вершину стека для входа по инструкции `syscall`, который (в
    // отличие от `int 0x80`) не получает автоматического переключения стека от
    // CPU и читает это значение напрямую.
    crate::arch::x86_64::syscall::set_syscall_kernel_stack(kstack_top);

    Ok(kstack_top)
}

/// Построить начальный ring-3 фрейм стека ядра по `kstack_top` и вернуть
/// получившийся `kernel_rsp`, с которого возобновляется планировщик.
///
/// Фрейм ОБЯЗАН побайтно совпадать с порядком, в котором его снимает путь
/// восстановления планировщика (см. `irq32_stub` / `kernel_thread_spawn`):
///
/// ```text
///     mov rsp, kernel_rsp
///     popfq                     ; слово RFLAGS для popfq (младший адрес)
///     pop r15 … pop rax         ; 15 слотов GPR
///     iretq                     ; RIP, CS, RFLAGS, RSP, SS
/// ```
///
/// iretq-фрейм несёт ring-3 CS/SS (RPL 3), поэтому `iretq` меняет привилегии на
/// ring 3, загружая `RSP = user_rsp` и `RIP = entry` с `RFLAGS.IF`. Все 15
/// слотов GPR нулевые: свежая Linux/нативная программа начинается без контракта
/// регистров. Мы защитно ставим `3` в пользовательские селекторы, чтобы биты RPL
/// были выставлены независимо от того, как их строит крейт GDT.
///
/// # Safety
/// `kstack_top` обязан быть исключающей вершиной свежеотображённого
/// записываемого стека ядра, под которой есть хотя бы `18 * 8` байт; функция
/// пишет в `[kernel_rsp, kstack_top)`.
unsafe fn build_ring3_frame(kstack_top: u64, entry: u64, user_rsp: u64) -> u64 {
    let user_cs = (gdt::Selectors::user_code().0 | 3) as u64;
    let user_ss = (gdt::Selectors::user_data().0 | 3) as u64;

    let mut rsp = kstack_top;

    // iretq-фрейм (старшие адреса), снимается `iretq`.
    rsp -= 8;
    (rsp as *mut u64).write(user_ss); // SS  (RPL 3)
    rsp -= 8;
    (rsp as *mut u64).write(user_rsp); // RSP (указатель пользовательского стека)
    rsp -= 8;
    (rsp as *mut u64).write(0x202u64); // RFLAGS (IF установлен)
    rsp -= 8;
    (rsp as *mut u64).write(user_cs); // CS  (RPL 3)
    rsp -= 8;
    (rsp as *mut u64).write(entry); // RIP = пользовательская точка входа

    // 15 слотов GPR (rax старший .. r15 младший), все нулевые.
    for _ in 0..15 {
        rsp -= 8;
        (rsp as *mut u64).write(0);
    }

    // Слово RFLAGS, снимаемое `popfq` (младший адрес = итоговый kernel_rsp).
    // Инвариант IF=0: хвост восстановления идёт с ВЫКЛЮЧЕННЫМИ прерываниями до
    // `iretq`, который снова включит их из слота RFLAGS iret-фрейма выше.
    rsp -= 8;
    (rsp as *mut u64).write(0x002u64);

    rsp
}

/// Создать ring-3 пользовательский процесс из ELF-образа в памяти и добавить его
/// в ready-очередь планировщика (Requirements 13.1, 13.4, 4.4).
///
/// Шаги:
///  1. `ElfLoader::load` разбирает/валидирует ELF и отображает его `PT_LOAD`
///     сегменты в свежий пользовательский PML4 (загрузчик внутри переключает CR3
///     на время отображения сегментов и восстанавливает ядерный CR3 перед
///     возвратом).
///  2. Отобразить пользовательский стек по
///     `memory::layout::USER_STACK_TOP` в пользовательский PML4 (CR3
///     переключается вокруг отображений, затем восстанавливается).
///  3. Выделить стек ядра этой задачи в ядерной верхней половине (разделён по
///     ссылке в пользовательский PML4) и запрограммировать TSS RSP0 на его
///     вершину, чтобы у CPU был валидный стек для переходов ring-3 → ring-0
///     (таймер / int 0x80).
///  4. Построить начальный фрейм стека ядра РОВНО в той байтовой раскладке,
///     которую снимает путь восстановления планировщика, чей `iretq`-фрейм
///     опускается в ring 3 на точку входа ELF на пользовательском стеке.
///
/// ПРЕДУСЛОВИЕ: вызывающий выполняет это с выключенными прерываниями — на время
/// исполнения кода ядра ставится пользовательский CR3, и тик таймера в этом окне
/// дал бы планировщику увидеть чужой CR3. `kernel_main` запускает тестовый
/// процесс до включения прерываний, так что условие выполнено.
pub fn create_user_process(elf_data: &[u8]) -> Result<u64, &'static str> {
    // ── 1. Загрузить ELF в свежий пользовательский PML4 ────────────────────
    let elf_proc = ElfLoader::load(elf_data)?;
    let pml4_phys = elf_proc.pml4_phys;
    let entry = elf_proc.entry;

    // ── 2. Отобразить пользовательский стек в пользовательский PML4 ─────────
    let user_stack_bottom = USER_STACK_TOP - USER_STACK_PAGES * PAGE_SIZE;
    // Начальный пользовательский RSP: выровнен на 16, на один слот ниже
    // (исключающей) вершины.
    let user_rsp = USER_STACK_TOP - 16;

    let uflags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE;

    let kernel_cr3 = vmm::current_pml4_phys();
    // SAFETY: `pml4_phys` — валидный PML4 с клонированной ядерной верхней
    // половиной, поэтому код/стек/куча ядра остаются отображёнными, пока он
    // установлен.
    unsafe {
        vmm::load_cr3(pml4_phys);
    }
    let mut stack_err: Option<&'static str> = None;
    for page in 0..USER_STACK_PAGES {
        let vaddr = user_stack_bottom + page * PAGE_SIZE;
        match pmm::alloc_frame() {
            Some(frame) => {
                if vmm::map(frame, vaddr, uflags).is_err() {
                    stack_err = Some("process: VMM map failed (user stack)");
                    break;
                }
            }
            None => {
                stack_err = Some("process: PMM OOM for user stack");
                break;
            }
        }
    }
    // SAFETY: сначала восстановить ядерный PML4.
    unsafe {
        vmm::load_cr3(kernel_cr3);
    }
    if let Some(e) = stack_err {
        return Err(e);
    }

    // ── 3. Выделить стек ядра задачи + запрограммировать TSS RSP0 ──────────
    let pid = scheduler::next_pid();
    let kstack_top = setup_task_kernel_stack(pid)?;

    // ── 4. Построить начальный фрейм стека ядра ─────────────────────────────
    // SAFETY: пишем в только что выделенный и отображённый стек ядра.
    let kernel_rsp = unsafe { build_ring3_frame(kstack_top, entry, user_rsp) };

    let tcb = Tcb::new(pid, kernel_rsp, pml4_phys);
    let pid = scheduler::spawn(tcb);

    crate::info!("Created user process pid={} entry=0x{:x}", pid, entry);

    Ok(pid)
}

/// Собрать в памяти минимальный статически слинкованный `ET_EXEC` x86_64 ELF,
/// чья программа выдаёт `SYS_WRITE(fd=1, msg, len)`, затем `SYS_EXIT(0)` через
/// `int 0x80`.
///
/// Всё изображение — один `PT_LOAD` RWX-сегмент, загруженный по виртуальной базе
/// `0x40_0000` (каноническая нижняя половина, поэтому валидация указателя в
/// `sys_write` принимает буфер сообщения). Сообщение живёт внутри отображённого
/// сегмента, то есть `USER_ACCESSIBLE` в пользовательском PML4 и достижимо из
/// ring 3.
///
/// Байты собираются здесь, а не коммитятся бинарём, чтобы ядру не нужен был
/// внешний ассемблер/линкер при сборке.
fn build_test_elf() -> Vec<u8> {
    const VBASE: u64 = 0x40_0000;
    const EHSIZE: usize = 64; // sizeof(Elf64Header)
    const PHSIZE: usize = 56; // sizeof(Elf64ProgramHeader)
    let code_off = EHSIZE + PHSIZE; // файловое смещение (и смещение vaddr) кода

    let msg: &[u8] = b"Hello from ring3 user process!\n";

    // ── Машинный код, собранный вручную (см. комментарии к байтам) ───────────
    // Сообщение идёт сразу за кодом, поэтому сначала вычисляем его адрес.
    const CODE_LEN: usize = 33;
    let msg_off = code_off + CODE_LEN;
    let msg_addr = VBASE + msg_off as u64;
    let len = msg.len() as u32;

    let mut code: Vec<u8> = Vec::with_capacity(CODE_LEN);
    code.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // mov eax, 1  (SYS_WRITE)
    code.extend_from_slice(&[0xBF, 0x01, 0x00, 0x00, 0x00]); // mov edi, 1  (fd = stdout)
    code.push(0xBE);
    code.extend_from_slice(&(msg_addr as u32).to_le_bytes()); // mov esi, msg_addr
    code.push(0xBA);
    code.extend_from_slice(&len.to_le_bytes()); // mov edx, len
    code.extend_from_slice(&[0xCD, 0x80]); // int 0x80
    code.extend_from_slice(&[0xB8, 0x02, 0x00, 0x00, 0x00]); // mov eax, 2  (SYS_EXIT)
    code.extend_from_slice(&[0x31, 0xFF]); // xor edi, edi (код = 0)
    code.extend_from_slice(&[0xCD, 0x80]); // int 0x80
    code.extend_from_slice(&[0xEB, 0xFE]); // 1: jmp 1b (запасной цикл)
    debug_assert_eq!(code.len(), CODE_LEN);

    let entry = VBASE + code_off as u64;
    let total_len = (msg_off + msg.len()) as u64;

    let mut elf: Vec<u8> = Vec::new();

    // ── Заголовок ELF64 (64 байта) ──────────────────────────────────────────
    elf.extend_from_slice(&[0x7F, b'E', b'L', b'F']); // EI_MAG
    elf.push(2); // EI_CLASS = ELFCLASS64
    elf.push(1); // EI_DATA  = ELFDATA2LSB
    elf.push(1); // EI_VERSION
    elf.push(0); // EI_OSABI = System V
                 // EI_ABIVERSION + 7 байт паддинга
    elf.extend_from_slice(&[0u8; 8]);
    elf.extend_from_slice(&2u16.to_le_bytes()); // e_type    = ET_EXEC
    elf.extend_from_slice(&0x3Eu16.to_le_bytes()); // e_machine = EM_X86_64
    elf.extend_from_slice(&1u32.to_le_bytes()); // e_version
    elf.extend_from_slice(&entry.to_le_bytes()); // e_entry
    elf.extend_from_slice(&(EHSIZE as u64).to_le_bytes()); // e_phoff (phdr right after ehdr)
    elf.extend_from_slice(&0u64.to_le_bytes()); // e_shoff
    elf.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    elf.extend_from_slice(&(EHSIZE as u16).to_le_bytes()); // e_ehsize
    elf.extend_from_slice(&(PHSIZE as u16).to_le_bytes()); // e_phentsize
    elf.extend_from_slice(&1u16.to_le_bytes()); // e_phnum
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shentsize
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shnum
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx
    debug_assert_eq!(elf.len(), EHSIZE);

    // ── Program header (56 байт): один PT_LOAD на весь образ ────────────────
    elf.extend_from_slice(&1u32.to_le_bytes()); // p_type  = PT_LOAD
    elf.extend_from_slice(&7u32.to_le_bytes()); // p_flags = PF_R|PF_W|PF_X
    elf.extend_from_slice(&0u64.to_le_bytes()); // p_offset (segment starts at file 0)
    elf.extend_from_slice(&VBASE.to_le_bytes()); // p_vaddr
    elf.extend_from_slice(&VBASE.to_le_bytes()); // p_paddr
    elf.extend_from_slice(&total_len.to_le_bytes()); // p_filesz
    elf.extend_from_slice(&total_len.to_le_bytes()); // p_memsz
    elf.extend_from_slice(&0x1000u64.to_le_bytes()); // p_align
    debug_assert_eq!(elf.len(), EHSIZE + PHSIZE);

    // ── Код + сообщение ──────────────────────────────────────────────────────
    elf.extend_from_slice(&code);
    elf.extend_from_slice(msg);
    debug_assert_eq!(elf.len() as u64, total_len);

    elf
}

/// Собрать встроенный тестовый ELF и запустить его как ring-3 пользовательский
/// процесс.
///
/// Используется путём загрузки (и вызывается из шелла), чтобы прогнать полный
/// цикл user-mode + syscall: процесс выполняет `SYS_WRITE` (наблюдаемо на
/// serial), затем `SYS_EXIT`.
pub fn spawn_test_user_process() -> Result<u64, &'static str> {
    let elf = build_test_elf();
    create_user_process(&elf)
}

/// Режимы отказа [`run_linux_binary`] (R7.3, R7.4, R7.5).
///
/// При любой ошибке функция возвращается ДО добавления задачи в ready-очередь
/// планировщика, поэтому неудачный запуск никогда не оставляет полупостроенный
/// `Compat_Process` работающим (R7.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunError {
    /// Список аргументов превысил гейт run-запроса (>256 аргументов или >4096
    /// суммарных байт); запрос отвергается до любой работы (R7.5).
    ArgsTooLarge,
    /// Запрошенный путь не существует или не читается с ext2 (R7.4).
    NotFound,
    /// ELF-загрузчик отверг бинарь; несёт описательную причину от загрузчика
    /// (R7.3).
    LoadFailed(&'static str),
    /// Начальный стек не удалось построить/отобразить (кодировщик `TooLarge` или
    /// отказ отображения); процесс не запускается (R7.3).
    StackFailed,
    /// Каталог на пути exec непросматриваем вызывающим (POSIX-право `x`).
    AccessDenied,
}

/// Прочитать всё содержимое файла ext2, достижимого через VFS по `path`.
///
/// Возвращает [`RunError::NotFound`], когда путь не разрешается, называет
/// каталог или не читается (R7.4). Идёт с включёнными прерываниями, потому что
/// путь чтения ext2/VFS может блокироваться в ожидании прерывания устройства.
fn read_file_all(path: &str) -> Result<Vec<u8>, RunError> {
    // Разыменовывать символические ссылки: образ Debian обычно достижим через
    // одну из них (`/usr/bin/python3` → `python3.11`, `/lib64/ld-linux…`).
    //
    // Обход проверяет креденшелы: `execve(2)` не должен достигать образа через
    // каталог, который вызывающему нельзя просматривать (этот путь внутри ядра
    // для boot-selftest'ов и `lxrun`, работающих под root и потому ведущих себя
    // как раньше).
    let may_search = |node: &Arc<dyn crate::vfs::VfsNode>| {
        compat::current_creds().may(node.mode(), node.owner(), Access::Execute)
    };
    let node = crate::vfs::lookup_path_walk_checked(path, true, &may_search).map_err(|e| {
        if e == crate::vfs::link_walk::WalkError::AccessDenied {
            RunError::AccessDenied
        } else {
            RunError::NotFound
        }
    })?;
    if node.is_directory() {
        return Err(RunError::NotFound);
    }

    // `i_size` контролируется образом: атакующий может смонтировать файл,
    // заявляющий ~4 ГиБ без единого блока данных, поэтому `vec![0u8; size]` был
    // ещё одной точкой abort по аллокации. Ограничиваем размер образа и
    // резервируем с возможностью отказа; настоящий userland-ELF — несколько МиБ.
    const MAX_EXEC_IMAGE: usize = 256 * 1024 * 1024;
    let size = node.size() as usize;
    if size > MAX_EXEC_IMAGE {
        return Err(RunError::LoadFailed("ELF: image too large"));
    }
    let mut data = Vec::new();
    data.try_reserve_exact(size)
        .map_err(|_| RunError::LoadFailed("ELF: image too large"))?;
    data.resize(size, 0u8);
    let mut off: usize = 0;
    while off < size {
        match node.read(off as u64, &mut data[off..]) {
            Ok(0) => break, // короткое чтение / EOF
            Ok(n) => off += n,
            Err(_) => return Err(RunError::NotFound),
        }
    }
    data.truncate(off);
    Ok(data)
}

/// `st_ino` узла VFS, из которого запуск читает образ (или интерпретатор),
/// ровно как его сообщает `stat(2)`; `0`, когда у пути нет идентичности VFS.
///
/// `/proc/<pid>/maps` печатает его в строках file-backed сегментов
/// (`docs/procfs.md` §14) — альтернативой было бы заявлять `00:00 0` для файла,
/// у которого на самом деле есть реальный inode.
fn path_ino(path: &str) -> u64 {
    crate::vfs::lookup_path_walk(path, true)
        .ok()
        .map(|node| crate::arch::x86_64::linux::io_sys::node_ino(&node))
        .unwrap_or(0)
}

const PT_INTERP: u32 = 3;
const INTERP_BASE: u64 = 0x0000_7000_0000_0000;

fn elf_interpreter_path(data: &[u8]) -> Result<Option<String>, RunError> {
    if data.len() < 64 {
        return Err(RunError::LoadFailed("ELF: short header"));
    }
    let phoff = u64::from_le_bytes(data[32..40].try_into().unwrap()) as usize;
    let phentsize = u16::from_le_bytes(data[54..56].try_into().unwrap()) as usize;
    let phnum = u16::from_le_bytes(data[56..58].try_into().unwrap()) as usize;
    if phentsize < 56 {
        return Err(RunError::LoadFailed("ELF: invalid phentsize"));
    }
    for i in 0..phnum {
        let off = phoff
            .checked_add(
                i.checked_mul(phentsize)
                    .ok_or(RunError::LoadFailed("ELF: phdr overflow"))?,
            )
            .ok_or(RunError::LoadFailed("ELF: phdr overflow"))?;
        let ph = data
            .get(off..off + 56)
            .ok_or(RunError::LoadFailed("ELF: phdr outside file"))?;
        if u32::from_le_bytes(ph[0..4].try_into().unwrap()) != PT_INTERP {
            continue;
        }
        let start = u64::from_le_bytes(ph[8..16].try_into().unwrap()) as usize;
        let len = u64::from_le_bytes(ph[32..40].try_into().unwrap()) as usize;
        let end = start
            .checked_add(len)
            .ok_or(RunError::LoadFailed("ELF: interp overflow"))?;
        let raw = data
            .get(start..end)
            .ok_or(RunError::LoadFailed("ELF: interp outside file"))?;
        let nul = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
        let path = core::str::from_utf8(&raw[..nul])
            .map_err(|_| RunError::LoadFailed("ELF: invalid interp path"))?;
        return Ok(Some(path.to_string()));
    }
    Ok(None)
}

/// Разрешить ELF-интерпретатор: вернуть **гостевой** путь интерпретатора,
/// **ядерный** путь, который реально прочитали (для идентичности inode в
/// `/proc/<pid>/maps`), и байты. Путь — тот, что называет `PT_INTERP`, и именно
/// его `/proc/self/maps` сообщает для сегментов самого загрузчика.
fn read_interpreter(data: &[u8]) -> Result<Option<(String, String, Vec<u8>)>, RunError> {
    let Some(path) = elf_interpreter_path(data)? else {
        return Ok(None);
    };
    let disk_path = if path.starts_with('/') {
        format!("/mnt{}", path)
    } else {
        format!("/mnt/{}", path)
    };
    // PT_INTERP называет /lib64/ld-linux-x86-64.so.2, который в Debian-пакетах
    // существует только как симлинк; настоящий загрузчик лежит в
    // /usr/lib/x86_64-linux-gnu. Пробуем буквальный путь первым, затем известные
    // merged-usr места, чтобы отсутствующий симлинк не ломал каждый запуск.
    if let Ok(data) = read_file_all(&disk_path) {
        return Ok(Some((path, disk_path, data)));
    }
    let base = path.rsplit('/').next().unwrap_or(path.as_str());
    let usr_cand = if path.starts_with('/') {
        format!("/mnt/usr{}", path)
    } else {
        format!("/mnt/usr/{}", path)
    };
    let candidates = [
        format!("/mnt/usr/lib/x86_64-linux-gnu/{}", base),
        format!("/mnt/usr/lib64/{}", base),
        format!("/mnt/lib64/{}", base),
        usr_cand,
    ];
    for cand in candidates.iter() {
        if let Ok(data) = read_file_all(cand) {
            crate::warn!(
                "[linux] interpreter '{}' missing at '{}'; using fallback '{}'",
                path,
                disk_path,
                cand
            );
            return Ok(Some((path.clone(), cand.clone(), data)));
        }
    }
    // Ничего не совпало: вернуть исходную ошибку для буквального пути.
    read_file_all(&disk_path).map(|data| Some((path, disk_path, data)))
}

struct LoadedLinux {
    elf: crate::vfs::elf::ElfProcess,
    start_entry: u64,
    interp_base: u64,
    /// Отображённые PT_LOAD-сегменты ELF-интерпретатора (пусто для статических
    /// образов), собраны для `/proc/self/maps`.
    interp_segments: Vec<crate::vfs::elf::LoadSegment>,
}

fn map_linux_images(data: &[u8], interpreter: Option<&[u8]>) -> Result<LoadedLinux, RunError> {
    let elf = ElfLoader::load_linux(data).map_err(RunError::LoadFailed)?;
    if let Some(interp) = interpreter {
        let mut interp_segments: Vec<crate::vfs::elf::LoadSegment> = Vec::new();
        let start_entry =
            ElfLoader::map_interpreter(elf.pml4_phys, interp, INTERP_BASE, &mut interp_segments)
                .map_err(RunError::LoadFailed)?;
        Ok(LoadedLinux {
            elf,
            start_entry,
            interp_base: INTERP_BASE,
            interp_segments,
        })
    } else {
        let start_entry = elf.entry;
        Ok(LoadedLinux {
            elf,
            start_entry,
            interp_base: 0,
            interp_segments: Vec::new(),
        })
    }
}

/// Прочитать статический Linux-бинарь с ext2, загрузить его, построить
/// начальный стек System V с `argv`/`envp`, зарегистрировать per-process
/// состояние `Compat_Process` и поставить его в очередь как ring-3 задачу
/// (design component 6; R7.1–R7.6).
///
/// Конвейер:
///   1. **Гейт аргументов (R7.5).** [`arg_gate`] требует ≤256 аргументов и ≤4096
///      суммарных байт; превышение → [`RunError::ArgsTooLarge`] до любой работы.
///   2. **Прочитать файл (R7.4).** Разрешить и прочитать путь ext2; отсутствует
///      или не читается → [`RunError::NotFound`].
///   3. **Загрузить ELF (R7.3).** [`ElfLoader::load_linux`] отображает `PT_LOAD`
///      сегменты `USER_ACCESSIBLE` в свежий пользовательский PML4 (R7.6); отказ →
///      [`RunError::LoadFailed`] с **не** поставленной в очередь задачей.
///   4. **Построить + отобразить стек (R7.6, R7.3).** Сгенерировать 16 байт
///      `AT_RANDOM`, собрать [`AuxInputs`] из выходов загрузчика и
///      [`map_initial_stack`] для SysV-образа в пользовательский стек (тоже
///      `USER_ACCESSIBLE`); любая ошибка → [`RunError::StackFailed`], без
///      постановки в очередь.
///   5. **Собрать `Compat_Process`.** Выделить pid, подготовить стек ядра + TSS
///      `RSP0`, засеять [`CompatState`] ([`FdTable`] со стандартными потоками +
///      [`VmRegionSet`] из `initial_brk`/mmap-подсказки нижней половины +
///      `tid = pid`) и зарегистрировать через [`compat::install_compat`] **до**
///      постановки в очередь, чтобы первый же syscall её увидел.
///   6. **В очередь.** Построить ring-3 `iretq`-фрейм на `entry`/`initial_rsp` и
///      `scheduler::spawn` задачу. `exit`/`exit_group` позже завершают только
///      эту задачу и удаляют её запись из реестра (R7.2).
///
/// Шаги 3–6 идут с выключенными прерываниями: [`ElfLoader::load_linux`] и
/// [`map_initial_stack`] ненадолго ставят пользовательский CR3, пока исполняется
/// код ядра, как и [`create_user_process`], поэтому тик таймера не должен видеть
/// чужое адресное пространство. Чтение ext2 на шаге 2 выполняется *до* заслона,
/// поэтому может блокироваться на дисковом I/O.
pub fn run_linux_binary(path: &str, argv: &[&[u8]], envp: &[&[u8]]) -> Result<u64, RunError> {
    // ── 1. Гейт аргументов (R7.5) ────────────────────────────────────────────
    if !arg_gate(argv) {
        return Err(RunError::ArgsTooLarge);
    }

    // ── 2. Прочитать файл ext2 (R7.4) — прерывания включены (может ждать I/O) ─
    let data = read_file_all(path)?;
    let interpreter = read_interpreter(&data)?;
    // Идентичность файла образа и его интерпретатора для `/proc/<pid>/maps`
    // (снята с тех же узлов VFS, что читает запуск; 0 для статических образов).
    let image_ino = path_ino(path);
    let interp_ino = interpreter
        .as_ref()
        .map(|(_, disk_path, _)| path_ino(disk_path))
        .unwrap_or(0);

    // ── 3–6. Запуск, чувствительный к CR3, с выключенными прерываниями ─────────
    crate::arch::cpu::without_interrupts(|| {
        // 3. Загрузить ELF в свежий пользовательский PML4 (сегменты USER_ACCESSIBLE, R7.6).
        let loaded = map_linux_images(&data, interpreter.as_ref().map(|(_, _, d)| d.as_slice()))?;
        let elf = loaded.elf;

        // 4. Построить начальный SysV-стек и отобразить его (USER_ACCESSIBLE, R7.6).
        let random16 = crate::arch::x86_64::linux::misc::random_bytes_16();
        let aux = AuxInputs {
            phdr: elf.phdr_vaddr,
            phent: elf.phent as u64,
            phnum: elf.phnum as u64,
            entry: elf.entry,
            pagesz: PAGE_SIZE,
            base: loaded.interp_base,
            // Кодировщик сам владеет размещением случайного блока и игнорирует это поле.
            random_ptr: 0,
        };
        let initial_rsp = map_initial_stack(elf.pml4_phys, argv, envp, &aux, random16).map_err(
            |e: StackMapError| {
                // Любая ошибка стека (кодировщик TooLarge или отказ отображения)
                // обрывает запуск без постановки в очередь; PML4 загрузчика просто
                // бросается (не передаётся планировщику), что выполняет R7.3.
                crate::error!(
                    "[linux] run '{}': initial stack construction failed: {:?}",
                    path,
                    e
                );
                RunError::StackFailed
            },
        )?;

        // 5. Собрать Compat_Process: pid, стек ядра + TSS RSP0, compat-состояние.
        let pid = scheduler::next_pid();
        let kstack_top = setup_task_kernel_stack(pid).map_err(RunError::LoadFailed)?;

        let mut state = CompatState::new(
            FdTable::with_standard_streams(),
            Arc::new(crate::sync::spinlock::Spinlock::new(VmRegionSet::new(
                elf.initial_brk,
                USER_MMAP_BASE,
            ))),
            pid,
        );
        // Запомнить путь образа для readlink("/proc/self/exe").
        state.exe_path = path.to_string();
        // Состояние procfs: argv, склеенные NUL для /proc/self/cmdline
        // (начальный стек хранит копию, но она живёт в памяти пользователя),
        // загруженные PT_LOAD сегменты для /proc/self/maps и путь интерпретатора.
        state.cmdline =
            crate::vfs::procfs_format::format_cmdline(argv, crate::vfs::procfs_format::CMDLINE_CAP);
        state.image_segments = elf.segments.clone();
        state.image_ino = image_ino;
        state.interp_segments = loaded.interp_segments.clone();
        state.interp_ino = interp_ino;
        state.interp_path = interpreter
            .as_ref()
            .map(|(p, _, _)| p.clone())
            .unwrap_or_default();
        // Регистрировать ДО постановки в очередь, чтобы первый же syscall
        // процесса уже видел его CompatState (и диспетчер маршрутизировал его как
        // Linux-задачу).
        compat::install_compat(pid, state);

        // 6. Засеять ring-3 iretq-фрейм на точке входа ELF на SysV-стеке и
        //    поставить задачу в очередь.
        // SAFETY: пишем в только что отображённый стек ядра из шага 5.
        let kernel_rsp = unsafe { build_ring3_frame(kstack_top, loaded.start_entry, initial_rsp) };

        let tcb = Tcb::new(pid, kernel_rsp, elf.pml4_phys);
        let pid = scheduler::spawn(tcb);

        crate::info!(
            "[linux] Compat_Process pid={} started: entry=0x{:x} rsp=0x{:x} brk=0x{:x} from '{}'",
            pid,
            elf.entry,
            initial_rsp,
            elf.initial_brk,
            path
        );

        Ok(pid)
    })
}

pub struct ExecImage {
    pub entry: u64,
    pub initial_rsp: u64,
    pub pml4_phys: u64,
}

/// Разрешить путь execve относительно cwd процесса и корня ext2 /mnt.
/// Форк-ребёнок nvim делает exec цели /proc/self/exe или попадания $PATH вида
/// "/mnt/usr/bin/nvim" напрямую; обычные Linux-пути ("/usr/bin/x") откатываются
/// к форме с префиксом /mnt.
fn resolve_exec_path(path: &str) -> String {
    let abs = if path.starts_with('/') {
        path.to_string()
    } else {
        let cwd =
            compat::with_current_compat(|cs| cs.cwd.clone()).unwrap_or_else(|| String::from("/"));
        if cwd.ends_with('/') {
            format!("{}{}", cwd, path)
        } else {
            format!("{}/{}", cwd, path)
        }
    };
    if crate::vfs::lookup_path_walk(&abs, true).is_ok() {
        return abs;
    }
    let mnt = format!("/mnt{}", abs);
    if crate::vfs::lookup_path_walk(&mnt, true).is_ok() {
        return mnt;
    }
    abs
}

/// Разрешить `path` точно так же, как это сделал бы [`exec_linux_image`], и
/// вернуть узел VFS, который будет exec'нут (`None`, когда путь не разрешается —
/// тогда exec сам сообщит ошибку).
///
/// Слой syscall (`process_sys::sys_execve`) использует это для проверки права на
/// исполнение; ядерный запускатель (`run_linux_binary`, boot-selftest'ы и `lxrun`
/// шелла) намеренно не проходит эту проверку — это привилегированный ядерный
/// вызывающий, как и установщик пакетов.
pub fn exec_target_node(path: &str) -> Option<Arc<dyn crate::vfs::VfsNode>> {
    let resolved = resolve_exec_path(path);
    crate::vfs::lookup_path_walk(&resolved, true).ok()
}

/// Подготовить и установить свежее ELF-адресное пространство для текущего pid.
/// Открытые дескрипторы, cwd, ppid и pid сохраняются; состояние VM/TLS/образа
/// сбрасывается.
pub fn exec_linux_image(path: &str, argv: &[&[u8]], envp: &[&[u8]]) -> Result<ExecImage, RunError> {
    if !arg_gate(argv) || !arg_gate(envp) {
        return Err(RunError::ArgsTooLarge);
    }
    // Разрешить путь так, как его видит Linux-userspace.
    let resolved = resolve_exec_path(path);
    let path = resolved.as_str();
    let data = read_file_all(path)?;
    let interpreter = read_interpreter(&data)?;
    // Идентичность файла нового образа и его интерпретатора для
    // `/proc/<pid>/maps` (сегменты старого образа заменяются ниже).
    let image_ino = path_ino(path);
    let interp_ino = interpreter
        .as_ref()
        .map(|(_, disk_path, _)| path_ino(disk_path))
        .unwrap_or(0);
    crate::arch::cpu::without_interrupts(|| {
        let loaded = map_linux_images(&data, interpreter.as_ref().map(|(_, _, d)| d.as_slice()))?;
        let elf = loaded.elf;
        let aux = AuxInputs {
            phdr: elf.phdr_vaddr,
            phent: elf.phent as u64,
            phnum: elf.phnum as u64,
            entry: elf.entry,
            pagesz: PAGE_SIZE,
            base: loaded.interp_base,
            random_ptr: 0,
        };
        let rsp = map_initial_stack(
            elf.pml4_phys,
            argv,
            envp,
            &aux,
            crate::arch::x86_64::linux::misc::random_bytes_16(),
        )
        .map_err(|_| RunError::StackFailed)?;
        let pid = scheduler::current_pid();
        compat::with_current_compat(|st| {
            st.vm = Arc::new(crate::sync::spinlock::Spinlock::new(VmRegionSet::new(
                elf.initial_brk,
                USER_MMAP_BASE,
            )));
            st.fs_base = 0;
            st.tid = pid;
            st.nosys_logged.clear();
            st.exit_code = None;
            // Вычистка close-on-exec (на ней держится error-pipe fork у libuv) +
            // запись нового образа для /proc/self/exe.
            st.fds.close_cloexec();
            // Что свежий образ реально видит на stdio — отвечает, подключил ли
            // spawn RPC-сокет к fd 0/1.
            st.exec_stdio = [
                st.fds.describe_fd(0),
                st.fds.describe_fd(1),
                st.fds.describe_fd(2),
            ];

            st.exe_path = resolved.clone();
            st.cmdline = crate::vfs::procfs_format::format_cmdline(
                argv,
                crate::vfs::procfs_format::CMDLINE_CAP,
            );
            st.image_segments = elf.segments.clone();
            st.image_ino = image_ino;
            st.interp_segments = loaded.interp_segments.clone();
            st.interp_ino = interp_ino;
            st.interp_path = interpreter
                .as_ref()
                .map(|(p, _, _)| p.clone())
                .unwrap_or_default();
        })
        .ok_or(RunError::LoadFailed("execve without compat state"))?;
        // SAFETY: загрузчик построил валидный PML4 с разделяемыми ядерными отображениями верхней половины.
        let old_cr3 = vmm::current_pml4_phys();
        unsafe {
            vmm::load_cr3(elf.pml4_phys);
        }
        // Заменённое адресное пространство мертво: вернуть его кадры (COW-листья
        // делают unref, поэтому exec'нувшийся ребёнок fork освобождает
        // разделяемые кадры родителя). Оно остаётся живым, пока на нём ещё
        // выполняется другая живая задача — поток-сосед (CLONE_VM), который
        // готов, припаркован SIGSTOP или ждёт reaping. Linux убил бы группу
        // потоков здесь; это ядро оставляет поток живым в старом образе, что
        // безопасно по памяти (таблицы страниц остаются во владении до выхода
        // последнего пользователя) и является исправлением page-table UAF.
        let freed = scheduler::drop_exclusive_user_space(old_cr3);
        crate::info!(
            "[linux] execve pid={} '{}' new_pml4=0x{:x} entry=0x{:x} old_cr3=0x{:x} freed={}",
            pid,
            path,
            elf.pml4_phys,
            loaded.start_entry,
            old_cr3,
            freed
        );
        Ok(ExecImage {
            entry: loaded.start_entry,
            initial_rsp: rsp,
            pml4_phys: elf.pml4_phys,
        })
    })
}

unsafe fn build_clone_frame(
    top: u64,
    regs: &crate::arch::x86_64::linux::regs::SavedRegs,
    rip: u64,
    user_rsp: u64,
) -> u64 {
    let ucs = (gdt::Selectors::user_code().0 | 3) as u64;
    let uss = (gdt::Selectors::user_data().0 | 3) as u64;
    let mut sp = top;
    sp -= 8;
    (sp as *mut u64).write(uss);
    sp -= 8;
    (sp as *mut u64).write(user_rsp);
    sp -= 8;
    (sp as *mut u64).write(0x202);
    sp -= 8;
    (sp as *mut u64).write(ucs);
    sp -= 8;
    (sp as *mut u64).write(rip);
    let vals = [
        0, regs.rbx, regs.rcx, regs.rdx, regs.rsi, regs.rdi, regs.rbp, regs.r8, regs.r9, regs.r10,
        regs.r11, regs.r12, regs.r13, regs.r14, regs.r15,
    ];
    for value in vals {
        sp -= 8;
        (sp as *mut u64).write(value);
    }
    sp -= 8;
    (sp as *mut u64).write(0x002);
    sp // слот popfq: инвариант IF=0 (хвост восстановления идёт с выключенными IRQ)
}
pub fn spawn_linux_thread(
    regs: &crate::arch::x86_64::linux::regs::SavedRegs,
    user_rsp: u64,
) -> Result<u64, &'static str> {
    let parent = scheduler::current_pid();
    let child = scheduler::next_pid();
    let child_top = setup_task_kernel_stack(child)?;
    let parent_top = crate::memory::layout::kernel_stack_for_pid(parent).2;
    gdt::set_kernel_stack(parent_top);
    crate::arch::x86_64::syscall::set_syscall_kernel_stack(parent_top);
    let krsp = unsafe { build_clone_frame(child_top, regs, regs.rcx, user_rsp) };
    scheduler::spawn(Tcb::new(child, krsp, vmm::current_pml4_phys()));
    Ok(child)
}

/// Продублировать текущий Compat_Process в свежий pid с глубоко скопированным
/// адресным пространством (жадное копирование страниц — без COW). Ребёнок
/// возобновляется с тем же user RIP (`regs.rcx` на пути syscall) с `rax = 0`
/// (нулевой слот rax в `build_clone_frame`) на своей приватной копии
/// пользовательского стека родителя; родитель получает pid ребёнка как возврат
/// `sys_clone`.
///
/// Идёт с выключенными прерываниями, чтобы (а) обход копирования страниц видел
/// стабильное адресное пространство родителя и (б) ребёнка нельзя было
/// запланировать до регистрации его CompatState.
///
/// `parent_tid` / `child_tid` — выходные указатели `CLONE_PARENT_SETTID` /
/// `CLONE_CHILD_SETTID` (0 = флаг не запрошен). И проверка, и записи происходят
/// внутри этого окна с выключенными прерываниями: поток-сосед вызывающего может
/// делить его адресное пространство (`CLONE_VM`), поэтому проверка до fork и
/// запись после него дали бы соседу munmap страницы между ними — и запись из
/// ring 0 упала бы в режиме ядра, остановив машину. Маскирование прерываний
/// делает проверенное отображение неизменным до завершения обеих записей;
/// ребёнок поставлен в очередь, но тоже не может выполняться. Запись на стороне
/// ребёнка попадает в уже скопированное адресное пространство ребёнка через окно
/// HHDM (собственная страница родителя сохраняет значение); без этого окна
/// ребёнок мог бы выполниться, munmap свою страницу `child_tid` и оставить
/// translate-then-write пишущим в освобождённый кадр.
pub fn fork_linux_process(
    regs: &crate::arch::x86_64::linux::regs::SavedRegs,
    clear_child_tid: u64,
    parent_tid: u64,
    child_tid: u64,
) -> Result<(u64, u64), crate::arch::x86_64::linux::errno::Errno> {
    use crate::arch::x86_64::linux::errno::Errno;
    crate::arch::cpu::without_interrupts(|| {
        // Проверить выходы TID ДО появления ребёнка, чтобы плохой указатель дал
        // EFAULT без полупостроенного состояния для отката.
        if parent_tid != 0 && crate::arch::x86_64::linux::check_user_ptr(parent_tid, 4).is_err() {
            return Err(Errno::EFAULT);
        }
        if child_tid != 0 && crate::arch::x86_64::linux::check_user_ptr(child_tid, 4).is_err() {
            return Err(Errno::EFAULT);
        }
        let parent = scheduler::current_pid();
        let child = scheduler::next_pid();
        let parent_pml4 = vmm::current_pml4_phys();
        let child_pml4 = vmm::new_user_pml4().map_err(|_| Errno::ENOMEM)?;
        // Разделение copy-on-write: ребёнок отражает листовые кадры родителя
        // (с подсчётом ссылок), записываемые записи с обеих сторон флипаются в
        // RO, а следующая запись любой стороны копирует страницу приватно.
        // Откат к жадному копированию, если пространство использует огромные
        // страницы (сегодня таких нет).
        if vmm::fork_user_space_cow(parent_pml4, child_pml4).is_err() {
            vmm::clone_user_space(parent_pml4, child_pml4).map_err(|_| Errno::ENOMEM)?;
        }
        let child_top = setup_task_kernel_stack(child).map_err(|_| Errno::ENOMEM)?;
        // setup_task_kernel_stack направляет TSS RSP0 / стек syscall на РЕБЁНКА;
        // РОДИТЕЛЬ продолжает выполняться после этого syscall, поэтому
        // возвращаем их назад (та же схема, что в spawn_linux_thread выше).
        let parent_top = crate::memory::layout::kernel_stack_for_pid(parent).2;
        gdt::set_kernel_stack(parent_top);
        crate::arch::x86_64::syscall::set_syscall_kernel_stack(parent_top);
        // Ребёнок возобновляется на return RIP родителя с user RSP родителя из
        // per-task слота по +120 над фреймом SavedRegs.
        let user_rsp = unsafe {
            ((regs as *const crate::arch::x86_64::linux::regs::SavedRegs as *const u64).add(15))
                .read()
        };
        let krsp = unsafe { build_clone_frame(child_top, regs, regs.rcx, user_rsp) };
        // Зарегистрировать CompatState ребёнка ДО постановки в очередь, чтобы его
        // первый же syscall уже шёл с полной семантикой Linux.
        if !compat::fork_current_compat(child, clear_child_tid) {
            return Err(Errno::EINVAL);
        }
        scheduler::spawn(Tcb::new(child, krsp, child_pml4));
        // SAFETY: оба указателя проверены выше, всё ещё внутри окна с
        // выключенными прерываниями — ни одна задача не могла изменить эти
        // отображения, а CR3 родителя активен, поэтому запись в пространство
        // родителя прямая. Запись на стороне ребёнка идёт через физический кадр
        // PML4 ребёнка (HHDM), тоже в том же окне.
        if parent_tid != 0 {
            unsafe { (parent_tid as *mut u32).write_unaligned(child as u32) }
        }
        if child_tid != 0 {
            if let Some(phys) = vmm::virt_to_phys_in(child_pml4, child_tid) {
                unsafe { (vmm::phys_to_virt(phys) as *mut u32).write_unaligned(child as u32) }
            }
        }
        crate::info!(
            "[linux] fork: parent pid={} pml4=0x{:x} -> child pid={} pml4=0x{:x}",
            parent,
            parent_pml4,
            child,
            child_pml4
        );
        Ok((child, child_pml4))
    })
}
