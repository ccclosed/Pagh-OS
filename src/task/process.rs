// task/process.rs — User process creation
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use alloc::vec::Vec;
use alloc::format;
use alloc::string::{String, ToString};
use crate::arch::x86_64::gdt;
use crate::memory::{pmm, vmm};
use crate::task::compat::{self, CompatState};
use crate::task::fd::FdTable;
use crate::task::scheduler;
use crate::task::scheduler::Tcb;
use crate::task::stack::{arg_gate, AuxInputs};
use crate::task::stack_map::{map_initial_stack, StackMapError};
use crate::arch::x86_64::linux::mem::VmRegionSet;
use crate::vfs::elf::ElfLoader;
use x86_64::structures::paging::PageTableFlags;

use crate::memory::layout::{
    PAGE_SIZE, USER_MMAP_BASE, USER_STACK_PAGES, USER_STACK_TOP, KERNEL_STACK_PAGES,
};

/// Allocate and map a fresh per-PID kernel stack in the kernel higher-half and
/// program the TSS `RSP0` (and the `syscall`-instruction kernel stack) to its
/// top, returning the (exclusive) stack top.
///
/// Shared by [`create_user_process`] and [`run_linux_binary`]. The kernel
/// higher-half PML4 entries are shared by reference into every user PML4 (cloned
/// in `new_user_pml4`), so a mapping added here under an already-present
/// top-level entry is visible while a user CR3 is active — exactly what the
/// ring-3 → ring-0 entry (which lands on RSP0) needs.
fn setup_task_kernel_stack(pid: u64) -> Result<u64, &'static str> {
    let (_guard_base, kstack_base, kstack_top) =
        crate::memory::layout::kernel_stack_for_pid(pid);

    let kflags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    for page in 0..KERNEL_STACK_PAGES {
        let vaddr = kstack_base + page * PAGE_SIZE;
        let frame = pmm::alloc_frame().ok_or("process: PMM OOM for kernel stack")?;
        vmm::map(frame, vaddr, kflags).map_err(|_| "process: VMM map failed (kernel stack)")?;
    }

    // Program RSP0 so ring-3 → ring-0 transitions land on this kernel stack.
    // (Single-task limitation documented on `gdt::set_kernel_stack`.)
    gdt::set_kernel_stack(kstack_top);
    // Mirror the same stack top for the `syscall`-instruction entry, which (unlike
    // `int 0x80`) gets no automatic CPU stack switch and reads this value directly.
    crate::arch::x86_64::syscall::set_syscall_kernel_stack(kstack_top);

    Ok(kstack_top)
}

/// Build the initial ring-3 kernel-stack frame at `kstack_top` and return the
/// resulting `kernel_rsp` the scheduler resumes from.
///
/// The frame MUST match — byte for byte — the order in which the scheduler
/// restore path consumes it (see `irq32_stub` / `kernel_thread_spawn`):
///
/// ```text
///     mov rsp, kernel_rsp
///     popfq                     ; RFLAGS-for-popfq word (lowest address)
///     pop r15 … pop rax         ; 15 GPR slots
///     iretq                     ; RIP, CS, RFLAGS, RSP, SS
/// ```
///
/// The `iretq` frame carries a ring-3 CS/SS (RPL 3), so `iretq` performs a
/// privilege change to ring 3, loading `RSP = user_rsp` and `RIP = entry` with
/// `RFLAGS.IF` set. All 15 GPR slots are zero: a fresh Linux/native program
/// starts with no register contract. We OR `3` into the user selectors
/// defensively so the RPL bits are set regardless of how the GDT crate builds
/// them.
///
/// # Safety
/// `kstack_top` must be the exclusive top of a freshly mapped, writable kernel
/// stack with at least `18 * 8` bytes available below it; this function writes
/// into `[kernel_rsp, kstack_top)`.
unsafe fn build_ring3_frame(kstack_top: u64, entry: u64, user_rsp: u64) -> u64 {
    let user_cs = (gdt::Selectors::user_code().0 | 3) as u64;
    let user_ss = (gdt::Selectors::user_data().0 | 3) as u64;

    let mut rsp = kstack_top;

    // iretq frame (highest addresses), consumed by `iretq`.
    rsp -= 8; (rsp as *mut u64).write(user_ss);    // SS  (RPL 3)
    rsp -= 8; (rsp as *mut u64).write(user_rsp);   // RSP (user stack pointer)
    rsp -= 8; (rsp as *mut u64).write(0x202u64);   // RFLAGS (IF set)
    rsp -= 8; (rsp as *mut u64).write(user_cs);    // CS  (RPL 3)
    rsp -= 8; (rsp as *mut u64).write(entry);      // RIP = user entry

    // 15 GPR slots (rax highest .. r15 lowest), all zero.
    for _ in 0..15 {
        rsp -= 8;
        (rsp as *mut u64).write(0);
    }

    // RFLAGS word consumed by `popfq` (lowest address = final kernel_rsp).
    // IF=0 invariant: the restore tail must run with interrupts OFF until
    // `iretq` re-enables them from the iret-frame RFLAGS slot above.
    rsp -= 8; (rsp as *mut u64).write(0x002u64);

    rsp
}

/// Create a ring-3 user process from an in-memory ELF image and add it to the
/// scheduler's ready queue (Requirements 13.1, 13.4, 4.4).
///
/// Steps:
///  1. `ElfLoader::load` parses/validates the ELF and maps its `PT_LOAD`
///     segments into a fresh user PML4 (the loader switches CR3 internally for
///     the segment maps and restores the kernel CR3 before returning).
///  2. Map the user stack at `memory::layout::USER_STACK_TOP` into the user
///     PML4 (CR3 switched around the maps, then restored).
///  3. Allocate this task's kernel stack in the kernel higher-half (shared by
///     reference into the user PML4) and program TSS RSP0 to its top so the
///     CPU has a valid stack for ring-3 → ring-0 transitions (timer / int 0x80).
///  4. Build the initial kernel-stack frame in EXACTLY the byte layout the
///     scheduler restore path consumes, whose `iretq` frame drops to ring 3 at
///     the ELF entry point on the user stack.
///
/// PRECONDITION: the caller must run this with interrupts disabled — it briefly
/// installs the user CR3 while still executing kernel code, and a timer tick in
/// that window would let the scheduler observe the foreign CR3. `kernel_main`
/// spawns the test process before enabling interrupts, satisfying this.
pub fn create_user_process(elf_data: &[u8]) -> Result<u64, &'static str> {
    // ── 1. Load the ELF into a fresh user PML4 ───────────────────────────────
    let elf_proc = ElfLoader::load(elf_data)?;
    let pml4_phys = elf_proc.pml4_phys;
    let entry = elf_proc.entry;

    // ── 2. Map the user stack into the user PML4 ─────────────────────────────
    let user_stack_bottom = USER_STACK_TOP - USER_STACK_PAGES * PAGE_SIZE;
    // Initial user RSP: 16-byte aligned, one slot below the (exclusive) top.
    let user_rsp = USER_STACK_TOP - 16;

    let uflags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE;

    let kernel_cr3 = vmm::current_pml4_phys();
    // SAFETY: `pml4_phys` is a valid PML4 with the kernel higher-half cloned in,
    // so kernel code/stack/heap remain mapped while it is installed.
    unsafe { vmm::load_cr3(pml4_phys); }
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
    // SAFETY: restore the kernel PML4 before doing anything else.
    unsafe { vmm::load_cr3(kernel_cr3); }
    if let Some(e) = stack_err {
        return Err(e);
    }

    // ── 3. Allocate the task's kernel stack + program TSS RSP0 ───────────────
    let pid = scheduler::next_pid();
    let kstack_top = setup_task_kernel_stack(pid)?;

    // ── 4. Build the initial kernel-stack frame ──────────────────────────────
    // SAFETY: writing into the freshly mapped kernel stack we just allocated.
    let kernel_rsp = unsafe { build_ring3_frame(kstack_top, entry, user_rsp) };

    let tcb = Tcb::new(pid, kernel_rsp, pml4_phys);
    let pid = scheduler::spawn(tcb);

    crate::info!("Created user process pid={} entry=0x{:x}", pid, entry);

    Ok(pid)
}

/// Build, in memory, a minimal statically-linked `ET_EXEC` x86_64 ELF whose
/// program issues `SYS_WRITE(fd=1, msg, len)` then `SYS_EXIT(0)` via `int 0x80`.
///
/// The whole image is a single `PT_LOAD` RWX segment loaded at virtual base
/// `0x40_0000` (lower-half canonical, so `sys_write`'s pointer validation
/// accepts the message buffer). The message lives inside that mapped segment,
/// so it is `USER_ACCESSIBLE` in the user PML4 and reachable from ring 3.
///
/// The bytes are assembled here rather than committed as a binary so the kernel
/// needs no external assembler/linker to build.
fn build_test_elf() -> Vec<u8> {
    const VBASE: u64 = 0x40_0000;
    const EHSIZE: usize = 64; // sizeof(Elf64Header)
    const PHSIZE: usize = 56; // sizeof(Elf64ProgramHeader)
    let code_off = EHSIZE + PHSIZE; // file offset (and vaddr offset) of the code

    let msg: &[u8] = b"Hello from ring3 user process!\n";

    // ── Hand-assembled machine code (see byte comments) ──────────────────────
    // The message immediately follows the code, so compute its address first.
    const CODE_LEN: usize = 33;
    let msg_off = code_off + CODE_LEN;
    let msg_addr = VBASE + msg_off as u64;
    let len = msg.len() as u32;

    let mut code: Vec<u8> = Vec::with_capacity(CODE_LEN);
    code.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);          // mov eax, 1  (SYS_WRITE)
    code.extend_from_slice(&[0xBF, 0x01, 0x00, 0x00, 0x00]);          // mov edi, 1  (fd = stdout)
    code.push(0xBE); code.extend_from_slice(&(msg_addr as u32).to_le_bytes()); // mov esi, msg_addr
    code.push(0xBA); code.extend_from_slice(&len.to_le_bytes());      // mov edx, len
    code.extend_from_slice(&[0xCD, 0x80]);                            // int 0x80
    code.extend_from_slice(&[0xB8, 0x02, 0x00, 0x00, 0x00]);          // mov eax, 2  (SYS_EXIT)
    code.extend_from_slice(&[0x31, 0xFF]);                            // xor edi, edi (code = 0)
    code.extend_from_slice(&[0xCD, 0x80]);                            // int 0x80
    code.extend_from_slice(&[0xEB, 0xFE]);                            // 1: jmp 1b (fallback)
    debug_assert_eq!(code.len(), CODE_LEN);

    let entry = VBASE + code_off as u64;
    let total_len = (msg_off + msg.len()) as u64;

    let mut elf: Vec<u8> = Vec::new();

    // ── ELF64 header (64 bytes) ──────────────────────────────────────────────
    elf.extend_from_slice(&[0x7F, b'E', b'L', b'F']); // EI_MAG
    elf.push(2); // EI_CLASS = ELFCLASS64
    elf.push(1); // EI_DATA  = ELFDATA2LSB
    elf.push(1); // EI_VERSION
    elf.push(0); // EI_OSABI = System V
    // EI_ABIVERSION + 7 padding bytes
    elf.extend_from_slice(&[0u8; 8]);
    elf.extend_from_slice(&2u16.to_le_bytes());        // e_type    = ET_EXEC
    elf.extend_from_slice(&0x3Eu16.to_le_bytes());     // e_machine = EM_X86_64
    elf.extend_from_slice(&1u32.to_le_bytes());        // e_version
    elf.extend_from_slice(&entry.to_le_bytes());       // e_entry
    elf.extend_from_slice(&(EHSIZE as u64).to_le_bytes()); // e_phoff (phdr right after ehdr)
    elf.extend_from_slice(&0u64.to_le_bytes());        // e_shoff
    elf.extend_from_slice(&0u32.to_le_bytes());        // e_flags
    elf.extend_from_slice(&(EHSIZE as u16).to_le_bytes());  // e_ehsize
    elf.extend_from_slice(&(PHSIZE as u16).to_le_bytes());  // e_phentsize
    elf.extend_from_slice(&1u16.to_le_bytes());        // e_phnum
    elf.extend_from_slice(&0u16.to_le_bytes());        // e_shentsize
    elf.extend_from_slice(&0u16.to_le_bytes());        // e_shnum
    elf.extend_from_slice(&0u16.to_le_bytes());        // e_shstrndx
    debug_assert_eq!(elf.len(), EHSIZE);

    // ── Program header (56 bytes): one PT_LOAD covering the whole image ──────
    elf.extend_from_slice(&1u32.to_le_bytes());        // p_type  = PT_LOAD
    elf.extend_from_slice(&7u32.to_le_bytes());        // p_flags = PF_R|PF_W|PF_X
    elf.extend_from_slice(&0u64.to_le_bytes());        // p_offset (segment starts at file 0)
    elf.extend_from_slice(&VBASE.to_le_bytes());       // p_vaddr
    elf.extend_from_slice(&VBASE.to_le_bytes());       // p_paddr
    elf.extend_from_slice(&total_len.to_le_bytes());   // p_filesz
    elf.extend_from_slice(&total_len.to_le_bytes());   // p_memsz
    elf.extend_from_slice(&0x1000u64.to_le_bytes());   // p_align
    debug_assert_eq!(elf.len(), EHSIZE + PHSIZE);

    // ── Code + message ───────────────────────────────────────────────────────
    elf.extend_from_slice(&code);
    elf.extend_from_slice(msg);
    debug_assert_eq!(elf.len() as u64, total_len);

    elf
}

/// Build the embedded test ELF and launch it as a ring-3 user process.
///
/// Used by the boot path (and exercisable from the shell) to drive the
/// user-mode + syscall round trip end to end: the process performs a
/// `SYS_WRITE` (observable on serial) followed by `SYS_EXIT`.
pub fn spawn_test_user_process() -> Result<u64, &'static str> {
    let elf = build_test_elf();
    create_user_process(&elf)
}

/// Failure modes of [`run_linux_binary`] (R7.3, R7.4, R7.5).
///
/// In every error case the function returns *before* adding any task to the
/// scheduler ready queue, so a failed launch never leaves a half-built
/// `Compat_Process` running (R7.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunError {
    /// The argument list exceeded the run-request gate (>256 args or >4096
    /// combined bytes); the request is rejected before any work (R7.5).
    ArgsTooLarge,
    /// The requested path does not exist or could not be read from ext2 (R7.4).
    NotFound,
    /// The ELF loader rejected the binary; carries the loader's descriptive
    /// cause string (R7.3).
    LoadFailed(&'static str),
    /// The initial stack could not be built/mapped (encoder `TooLarge` or a
    /// mapping failure); the process is not started (R7.3).
    StackFailed,
}

/// Read the entire contents of an ext2 file reachable through the VFS at `path`.
///
/// Returns [`RunError::NotFound`] when the path does not resolve, names a
/// directory, or cannot be read (R7.4). Runs with interrupts enabled because the
/// ext2/VFS read path may block waiting on a device interrupt.
fn read_file_all(path: &str) -> Result<Vec<u8>, RunError> {
    let node = crate::vfs::lookup_path(path).map_err(|_| RunError::NotFound)?;
    if node.is_directory() {
        return Err(RunError::NotFound);
    }

    let size = node.size() as usize;
    let mut data = alloc::vec![0u8; size];
    let mut off: usize = 0;
    while off < size {
        match node.read(off as u64, &mut data[off..]) {
            Ok(0) => break, // short read / EOF
            Ok(n) => off += n,
            Err(_) => return Err(RunError::NotFound),
        }
    }
    data.truncate(off);
    Ok(data)
}

const PT_INTERP: u32 = 3;
const INTERP_BASE: u64 = 0x0000_7000_0000_0000;

fn elf_interpreter_path(data: &[u8]) -> Result<Option<String>, RunError> {
    if data.len() < 64 { return Err(RunError::LoadFailed("ELF: short header")); }
    let phoff = u64::from_le_bytes(data[32..40].try_into().unwrap()) as usize;
    let phentsize = u16::from_le_bytes(data[54..56].try_into().unwrap()) as usize;
    let phnum = u16::from_le_bytes(data[56..58].try_into().unwrap()) as usize;
    if phentsize < 56 { return Err(RunError::LoadFailed("ELF: invalid phentsize")); }
    for i in 0..phnum {
        let off = phoff.checked_add(i.checked_mul(phentsize)
            .ok_or(RunError::LoadFailed("ELF: phdr overflow"))?)
            .ok_or(RunError::LoadFailed("ELF: phdr overflow"))?;
        let ph = data.get(off..off + 56)
            .ok_or(RunError::LoadFailed("ELF: phdr outside file"))?;
        if u32::from_le_bytes(ph[0..4].try_into().unwrap()) != PT_INTERP { continue; }
        let start = u64::from_le_bytes(ph[8..16].try_into().unwrap()) as usize;
        let len = u64::from_le_bytes(ph[32..40].try_into().unwrap()) as usize;
        let end = start.checked_add(len).ok_or(RunError::LoadFailed("ELF: interp overflow"))?;
        let raw = data.get(start..end).ok_or(RunError::LoadFailed("ELF: interp outside file"))?;
        let nul = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
        let path = core::str::from_utf8(&raw[..nul])
            .map_err(|_| RunError::LoadFailed("ELF: invalid interp path"))?;
        return Ok(Some(path.to_string()));
    }
    Ok(None)
}

fn read_interpreter(data: &[u8]) -> Result<Option<Vec<u8>>, RunError> {
    let Some(path) = elf_interpreter_path(data)? else { return Ok(None); };
    let disk_path = if path.starts_with('/') {
        format!("/mnt{}", path)
    } else {
        format!("/mnt/{}", path)
    };
    // STAGE-13.8 INTERP FALLBACK: PT_INTERP names /lib64/ld-linux-x86-64.so.2,
    // which in Debian debs exists only as a symlink; the real loader lives in
    // /usr/lib/x86_64-linux-gnu. Try the literal path first, then the known
    // merged-usr locations, so a missing symlink cannot break every launch.
    if let Ok(data) = read_file_all(&disk_path) {
        return Ok(Some(data));
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
                path, disk_path, cand
            );
            return Ok(Some(data));
        }
    }
    // Nothing matched: surface the original error for the literal path.
    read_file_all(&disk_path).map(Some)
}

struct LoadedLinux {
    elf: crate::vfs::elf::ElfProcess,
    start_entry: u64,
    interp_base: u64,
}

fn map_linux_images(data: &[u8], interpreter: Option<&[u8]>) -> Result<LoadedLinux, RunError> {
    let elf = ElfLoader::load_linux(data).map_err(RunError::LoadFailed)?;
    if let Some(interp) = interpreter {
        let start_entry = ElfLoader::map_interpreter(elf.pml4_phys, interp, INTERP_BASE)
            .map_err(RunError::LoadFailed)?;
        Ok(LoadedLinux { elf, start_entry, interp_base: INTERP_BASE })
    } else {
        let start_entry = elf.entry;
        Ok(LoadedLinux { elf, start_entry, interp_base: 0 })
    }
}

/// Read a static Linux binary from ext2, load it, build its System V initial
/// stack with `argv`/`envp`, register its per-process `Compat_Process` state, and
/// enqueue it as a ring-3 task (design component 6; R7.1–R7.6).
///
/// Pipeline:
///   1. **Argument gate (R7.5).** [`arg_gate`] enforces ≤256 args and ≤4096
///      combined bytes; over-limit → [`RunError::ArgsTooLarge`] before any work.
///   2. **Read the file (R7.4).** Resolve and read the ext2 path; missing or
///      unreadable → [`RunError::NotFound`].
///   3. **Load the ELF (R7.3).** [`ElfLoader::load_linux`] maps `PT_LOAD`
///      segments `USER_ACCESSIBLE` into a fresh user PML4 (R7.6); a rejection →
///      [`RunError::LoadFailed`] with **no** task enqueued.
///   4. **Build + map the stack (R7.6, R7.3).** Generate 16 `AT_RANDOM` bytes,
///      assemble [`AuxInputs`] from the loader outputs, and
///      [`map_initial_stack`] the SysV image into the user stack (also
///      `USER_ACCESSIBLE`); any failure → [`RunError::StackFailed`], no enqueue.
///   5. **Construct the `Compat_Process`.** Allocate a pid, set up its kernel
///      stack + TSS `RSP0`, seed [`CompatState`] (a [`FdTable`] with the standard
///      streams + a [`VmRegionSet`] from `initial_brk`/the lower-half mmap hint
///      base + `tid = pid`), and register it via
///      [`compat::install_compat`] **before** enqueue so the very first syscall
///      observes it.
///   6. **Enqueue.** Build the ring-3 `iretq` frame at `entry`/`initial_rsp` and
///      `scheduler::spawn` the task. `exit`/`exit_group` later terminate only
///      this task and drop its registry entry (R7.2).
///
/// Steps 3–6 run with interrupts disabled: [`ElfLoader::load_linux`] and
/// [`map_initial_stack`] each briefly install the user CR3 while executing kernel
/// code, exactly like [`create_user_process`], so a timer tick must not observe
/// the foreign address space. The ext2 read in step 2 runs *before* the guard so
/// it may block on disk I/O.
pub fn run_linux_binary(path: &str, argv: &[&[u8]], envp: &[&[u8]]) -> Result<u64, RunError> {
    // ── 1. Argument gate (R7.5) ──────────────────────────────────────────────
    if !arg_gate(argv) {
        return Err(RunError::ArgsTooLarge);
    }

    // ── 2. Read the ext2 file (R7.4) — interrupts enabled (may block on I/O) ─
    let data = read_file_all(path)?;
    let interpreter = read_interpreter(&data)?;

    // ── 3–6. CR3-sensitive launch, interrupts disabled (like create_user_process) ─
    crate::arch::cpu::without_interrupts(|| {
        // 3. Load the ELF into a fresh user PML4 (segments USER_ACCESSIBLE, R7.6).
        let loaded = map_linux_images(&data, interpreter.as_deref())?;
        let elf = loaded.elf;

        // 4. Build the SysV initial stack and map it (USER_ACCESSIBLE, R7.6).
        let random16 = crate::arch::x86_64::linux::misc::random_bytes_16();
        let aux = AuxInputs {
            phdr: elf.phdr_vaddr,
            phent: elf.phent as u64,
            phnum: elf.phnum as u64,
            entry: elf.entry,
            pagesz: PAGE_SIZE,
            base: loaded.interp_base,
            // The encoder owns the random block placement and ignores this field.
            random_ptr: 0,
        };
        let initial_rsp = map_initial_stack(elf.pml4_phys, argv, envp, &aux, random16)
            .map_err(|e: StackMapError| {
                // Any stack failure (encoder TooLarge or a mapping fault) aborts
                // the launch without enqueuing; the loader's PML4 is simply
                // abandoned (not handed to the scheduler), satisfying R7.3.
                crate::error!(
                    "[linux] run '{}': initial stack construction failed: {:?}",
                    path, e
                );
                RunError::StackFailed
            })?;

        // 5. Build the Compat_Process: pid, kernel stack + TSS RSP0, compat state.
        let pid = scheduler::next_pid();
        let kstack_top = setup_task_kernel_stack(pid).map_err(RunError::LoadFailed)?;

        let mut state = CompatState::new(
            FdTable::with_standard_streams(),
            VmRegionSet::new(elf.initial_brk, USER_MMAP_BASE),
            pid,
        );
        // STAGE-15: remember the image path for readlink("/proc/self/exe").
        state.exe_path = path.to_string();
        // Register BEFORE enqueue so the first syscall the process makes already
        // sees its CompatState (and so the dispatcher routes it as a Linux task).
        compat::install_compat(pid, state);

        // 6. Seed the ring-3 iretq frame at the ELF entry on the SysV stack and
        //    enqueue the task.
        // SAFETY: writing into the freshly mapped kernel stack from step 5.
        let kernel_rsp = unsafe { build_ring3_frame(kstack_top, loaded.start_entry, initial_rsp) };

        let tcb = Tcb::new(pid, kernel_rsp, elf.pml4_phys);
        let pid = scheduler::spawn(tcb);

        crate::info!(
            "[linux] Compat_Process pid={} started: entry=0x{:x} rsp=0x{:x} brk=0x{:x} from '{}'",
            pid, elf.entry, initial_rsp, elf.initial_brk, path
        );

        Ok(pid)
    })
}


pub struct ExecImage { pub entry: u64, pub initial_rsp: u64, pub pml4_phys: u64 }

/// STAGE-15: resolve an execve path against the process cwd and the /mnt ext2
/// root. The forked nvim child execs the /proc/self/exe target or a $PATH hit
/// like "/mnt/usr/bin/nvim" directly; plain Linux paths ("/usr/bin/x") fall
/// through to the /mnt-prefixed form.
fn resolve_exec_path(path: &str) -> String {
    let abs = if path.starts_with('/') {
        path.to_string()
    } else {
        let cwd = compat::with_current_compat(|cs| cs.cwd.clone())
            .unwrap_or_else(|| String::from("/"));
        if cwd.ends_with('/') { format!("{}{}", cwd, path) } else { format!("{}/{}", cwd, path) }
    };
    if crate::vfs::lookup_path(&abs).is_ok() {
        return abs;
    }
    let mnt = format!("/mnt{}", abs);
    if crate::vfs::lookup_path(&mnt).is_ok() {
        return mnt;
    }
    abs
}

/// Prepare and install a fresh ELF address space for the current pid. Open file
/// descriptors, cwd, ppid and pid survive; VM/TLS/image state is reset.
pub fn exec_linux_image(path: &str, argv: &[&[u8]], envp: &[&[u8]]) -> Result<ExecImage, RunError> {
    if !arg_gate(argv) || !arg_gate(envp) { return Err(RunError::ArgsTooLarge); }
    // STAGE-15: resolve the path the way Linux userspace sees the tree.
    let resolved = resolve_exec_path(path);
    let path = resolved.as_str();
    let data = read_file_all(path)?;
    let interpreter = read_interpreter(&data)?;
    crate::arch::cpu::without_interrupts(|| {
        let loaded = map_linux_images(&data, interpreter.as_deref())?;
        let elf = loaded.elf;
        let aux = AuxInputs { phdr: elf.phdr_vaddr, phent: elf.phent as u64,
            phnum: elf.phnum as u64, entry: elf.entry, pagesz: PAGE_SIZE,
            base: loaded.interp_base, random_ptr: 0 };
        let rsp = map_initial_stack(elf.pml4_phys, argv, envp, &aux,
            crate::arch::x86_64::linux::misc::random_bytes_16()).map_err(|_| RunError::StackFailed)?;
        let pid = scheduler::current_pid();
        compat::with_current_compat(|st| {
            st.vm = VmRegionSet::new(elf.initial_brk, USER_MMAP_BASE);
            st.fs_base = 0; st.tid = pid; st.nosys_logged.clear(); st.exit_code = None;
            // STAGE-15: the close-on-exec sweep (libuv's fork error pipe
            // relies on it) + record the new image for /proc/self/exe.
            st.fds.close_cloexec();
            // STAGE-16.8 DIAG: what the fresh image actually sees on stdio —
            // answers whether the spawn wired the RPC socket onto fds 0/1.
            st.exec_stdio = [st.fds.describe_fd(0), st.fds.describe_fd(1), st.fds.describe_fd(2)];
            crate::warn!("[DIAG] execve pid={} fd0={} fd1={} fd2={}",
                pid, st.exec_stdio[0], st.exec_stdio[1], st.exec_stdio[2]);
            st.exe_path = resolved.clone();
        }).ok_or(RunError::LoadFailed("execve without compat state"))?;
        // SAFETY: loader built a valid PML4 with shared kernel higher-half mappings.
        unsafe { vmm::load_cr3(elf.pml4_phys); }
        crate::info!("[linux] execve pid={} '{}' new_pml4=0x{:x} entry=0x{:x}",
            pid, path, elf.pml4_phys, loaded.start_entry);
        Ok(ExecImage { entry: loaded.start_entry, initial_rsp: rsp, pml4_phys: elf.pml4_phys })
    })
}


unsafe fn build_clone_frame(top:u64, regs:&crate::arch::x86_64::linux::regs::SavedRegs, rip:u64, user_rsp:u64)->u64{
    let ucs=(gdt::Selectors::user_code().0|3) as u64; let uss=(gdt::Selectors::user_data().0|3) as u64;
    let mut sp=top; sp-=8;(sp as *mut u64).write(uss); sp-=8;(sp as *mut u64).write(user_rsp);
    sp-=8;(sp as *mut u64).write(0x202); sp-=8;(sp as *mut u64).write(ucs); sp-=8;(sp as *mut u64).write(rip);
    let vals=[0,regs.rbx,regs.rcx,regs.rdx,regs.rsi,regs.rdi,regs.rbp,regs.r8,regs.r9,regs.r10,regs.r11,regs.r12,regs.r13,regs.r14,regs.r15];
    for value in vals { sp-=8;(sp as *mut u64).write(value); } sp-=8;(sp as *mut u64).write(0x002); sp // popfq slot: IF=0 invariant (restore tail runs with IRQs off)
}
pub fn spawn_linux_thread(regs:&crate::arch::x86_64::linux::regs::SavedRegs,user_rsp:u64)->Result<u64,&'static str>{
    let parent=scheduler::current_pid(); let child=scheduler::next_pid();
    let child_top=setup_task_kernel_stack(child)?;
    let parent_top=crate::memory::layout::kernel_stack_for_pid(parent).2;
    gdt::set_kernel_stack(parent_top); crate::arch::x86_64::syscall::set_syscall_kernel_stack(parent_top);
    let krsp=unsafe{build_clone_frame(child_top,regs,regs.rcx,user_rsp)};
    scheduler::spawn(Tcb::new(child,krsp,vmm::current_pml4_phys())); Ok(child)
}

/// STAGE-15 fork: duplicate the current Compat_Process into a fresh pid with a
/// deep-copied address space (eager page copy — no COW). The child resumes at
/// the same user RIP (`regs.rcx` on the syscall path) with `rax = 0` (the zero
/// rax slot in `build_clone_frame`) on its private copy of the parent's user
/// stack; the parent receives the child pid from `sys_clone`'s return value.
///
/// Runs with interrupts disabled so (a) the page-copy walk sees a stable
/// parent address space and (b) the child cannot be scheduled before its
/// CompatState is registered.
pub fn fork_linux_process(regs:&crate::arch::x86_64::linux::regs::SavedRegs, clear_child_tid:u64)->Result<(u64,u64),&'static str>{
    crate::arch::cpu::without_interrupts(|| {
        let parent=scheduler::current_pid(); let child=scheduler::next_pid();
        let parent_pml4=vmm::current_pml4_phys();
        let child_pml4=vmm::new_user_pml4().map_err(|_|"fork: no frame for child PML4")?;
        vmm::clone_user_space(parent_pml4,child_pml4).map_err(|_|"fork: user-space copy failed")?;
        let child_top=setup_task_kernel_stack(child)?;
        // setup_task_kernel_stack points TSS RSP0 / the syscall stack at the
        // CHILD; the PARENT keeps running after this syscall, so point them back
        // (same pattern as spawn_linux_thread above).
        let parent_top=crate::memory::layout::kernel_stack_for_pid(parent).2;
        gdt::set_kernel_stack(parent_top); crate::arch::x86_64::syscall::set_syscall_kernel_stack(parent_top);
        // Child resumes at the parent's return RIP with the parent's user RSP
        // from the per-task slot at +120 above the SavedRegs frame.
        let user_rsp=unsafe{ ((regs as *const crate::arch::x86_64::linux::regs::SavedRegs as *const u64).add(15)).read() };
        let krsp=unsafe{build_clone_frame(child_top,regs,regs.rcx,user_rsp)};
        // Register the child's CompatState BEFORE enqueue so its very first
        // syscall is already routed with full Linux semantics.
        if !compat::fork_current_compat(child, clear_child_tid) {
            return Err("fork: parent has no compat state");
        }
        scheduler::spawn(Tcb::new(child,krsp,child_pml4));
        crate::info!("[linux] fork: parent pid={} pml4=0x{:x} -> child pid={} pml4=0x{:x}",
            parent, parent_pml4, child, child_pml4);
        Ok((child,child_pml4))
    })
}
