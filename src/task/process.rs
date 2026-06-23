// task/process.rs — User process creation
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use alloc::vec::Vec;
use crate::arch::x86_64::gdt;
use crate::memory::{pmm, vmm};
use crate::task::scheduler;
use crate::task::scheduler::Tcb;
use crate::vfs::elf::ElfLoader;
use x86_64::structures::paging::PageTableFlags;

use crate::memory::layout::{PAGE_SIZE, USER_STACK_PAGES, USER_STACK_TOP, KERNEL_STACK_PAGES};

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
    //
    // The kernel stack lives in the per-PID kernel-stack region (higher half).
    // The kernel higher-half PML4 entries are shared by reference into the user
    // PML4 (cloned in `new_user_pml4`), so a mapping added here under an
    // already-present top-level entry is visible while the user CR3 is active —
    // exactly what the ring-3 → ring-0 entry (which lands on RSP0) needs.
    let pid = scheduler::next_pid();
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

    // ── 4. Build the initial kernel-stack frame ──────────────────────────────
    //
    // This MUST match — byte for byte — the order in which the scheduler restore
    // path consumes it (see `irq32_stub` / `kernel_thread_spawn`):
    //     mov rsp, kernel_rsp
    //     popfq                     ; RFLAGS-for-popfq word (lowest address)
    //     pop r15 … pop rax         ; 15 GPR slots
    //     iretq                     ; RIP, CS, RFLAGS, RSP, SS
    //
    // The `iretq` frame carries a ring-3 CS/SS (RPL 3), so `iretq` performs a
    // privilege change to ring 3, loading RSP = user stack top and RIP = entry.
    // All 15 GPR slots are zero: a fresh user program starts with no register
    // contract (it does not expect rdi = entry like a kernel thread does).
    //
    // `Descriptor::user_code_segment()`/`user_data_segment()` should already
    // yield DPL-3 selectors with RPL 3; we OR in 3 defensively so the RPL bits
    // are guaranteed set regardless of how the GDT crate constructs them.
    let user_cs = (gdt::Selectors::user_code().0 | 3) as u64;
    let user_ss = (gdt::Selectors::user_data().0 | 3) as u64;

    // SAFETY: writing into the freshly mapped kernel stack we just allocated.
    let kernel_rsp = unsafe {
        let mut rsp = kstack_top;

        // iretq frame (highest addresses), consumed by `iretq`.
        rsp -= 8; (rsp as *mut u64).write(user_ss);    // SS  (RPL 3)
        rsp -= 8; (rsp as *mut u64).write(user_rsp);   // RSP (user stack top)
        rsp -= 8; (rsp as *mut u64).write(0x202u64);   // RFLAGS (IF set)
        rsp -= 8; (rsp as *mut u64).write(user_cs);    // CS  (RPL 3)
        rsp -= 8; (rsp as *mut u64).write(entry);      // RIP = user entry

        // 15 GPR slots (rax highest .. r15 lowest), all zero.
        for _ in 0..15 {
            rsp -= 8;
            (rsp as *mut u64).write(0);
        }

        // RFLAGS word consumed by `popfq` (lowest address = final kernel_rsp).
        rsp -= 8; (rsp as *mut u64).write(0x202u64);

        rsp
    };

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
