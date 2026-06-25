// vfs/elf.rs — ELF64 binary loader
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use core::ptr;
use x86_64::structures::paging::PageTableFlags;
use crate::memory::vmm;

// Re-export the pure, host-testable ELF classifier and static-PIE bias selector
// (task 5.1). These live in the sibling `elf_classify` module so they carry no
// kernel/paging dependencies and can be `#[path]`-included by `host-tests`
// (R11.6). The effectful `ElfLoader::load` extension (task 13.1) consumes them.
#[allow(unused_imports)]
pub use crate::vfs::elf_classify::{choose_bias, classify_elf, ElfKind, ElfVerdict};

#[repr(C)]
pub struct Elf64Header {
    pub e_ident: [u8; 16],
    pub e_type: u16,
    pub e_machine: u16,
    pub e_version: u32,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_shoff: u64,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub e_phentsize: u16,
    pub e_phnum: u16,
    pub e_shentsize: u16,
    pub e_shnum: u16,
    pub e_shstrndx: u16,
}

#[repr(C)]
pub struct Elf64ProgramHeader {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

const PT_LOAD: u32   = 1;
const ET_EXEC: u16   = 2;
const EM_X86_64: u16 = 0x3E;

const PF_W: u32 = 2;
const PF_X: u32 = 1;

const EI_MAG0: usize     = 0;
const EI_CLASS: usize    = 4;
const EI_DATA: usize     = 5;
const ELFCLASS64: u8     = 2;
const ELFDATA2LSB: u8    = 1;

/// Exclusive upper bound of the lower-half canonical address space on x86_64
/// (48-bit VA): valid user addresses are `0 .. 0x0000_8000_0000_0000`. Anything
/// at or above this is either the non-canonical hole or the kernel higher-half;
/// mapping such an address would either alias kernel space or panic inside
/// `VirtAddr::new` (which rejects non-canonical addresses). We reject these in
/// validation so a malformed program header can never reach the mapping loop.
const USER_ADDR_MAX: u64 = 0x0000_8000_0000_0000;

/// Loader outputs for a successfully mapped Linux ELF image.
///
/// Beyond the entry point and the user PML4 that the existing pagh-native path
/// consumes, this carries the extra values the `Stack_Initializer` needs to
/// build a Linux-conformant auxiliary vector (`AT_PHDR`/`AT_PHENT`/`AT_PHNUM`/
/// `AT_ENTRY`) and to seed the per-process `VmRegionSet` program break
/// (design "Data Models" / component 4 Linux_ELF_Loader, R6.4).
pub struct ElfProcess {
    /// Program entry point, already adjusted by `load_bias` for `ET_DYN` (R5.2).
    pub entry: u64,
    /// Physical address of the freshly-created user PML4.
    pub pml4_phys: u64,
    /// Page-aligned load bias applied to every segment: `0` for `ET_EXEC`,
    /// a kernel-chosen value for static-PIE `ET_DYN` (R5.1, R5.2).
    pub load_bias: u64,
    /// `AT_PHDR`: the in-memory (biased) virtual address of the program-header
    /// table (R6.4).
    pub phdr_vaddr: u64,
    /// `AT_PHENT`: ELF program-header entry size (`e_phentsize`).
    pub phent: u16,
    /// `AT_PHNUM`: ELF program-header count (`e_phnum`).
    pub phnum: u16,
    /// Page-aligned top of the highest `PT_LOAD` (+bias); seeds the heap break.
    pub initial_brk: u64,
}

pub struct ElfLoader;

impl ElfLoader {
    pub fn load(data: &[u8]) -> Result<ElfProcess, &'static str> {
        if data.len() < core::mem::size_of::<Elf64Header>() {
            return Err("ELF: data too small for header");
        }

        // SAFETY: Slice length validated; pointer is byte-aligned.
        let header: &Elf64Header = unsafe { &*(data.as_ptr() as *const Elf64Header) };

        if header.e_ident[EI_MAG0] != 0x7F
            || &header.e_ident[1..4] != b"ELF"
        {
            return Err("ELF: invalid magic");
        }
        if header.e_ident[EI_CLASS] != ELFCLASS64 { return Err("ELF: not 64-bit"); }
        if header.e_ident[EI_DATA] != ELFDATA2LSB { return Err("ELF: not little-endian"); }
        if header.e_type != ET_EXEC { return Err("ELF: not ET_EXEC"); }
        if header.e_machine != EM_X86_64 { return Err("ELF: not x86_64"); }
        if header.e_version != 1 { return Err("ELF: unsupported version"); }

        // Validate the program-header table and every PT_LOAD segment with
        // overflow-safe arithmetic *before* allocating the user PML4 or mapping
        // anything. This guarantees every malformed-binary rejection returns an
        // Err without touching memory and — critically — that the later mapping
        // loop can never index out of bounds, copy past the input, or feed a
        // non-canonical address to `VirtAddr::new` (which would panic).
        Self::validate_program_headers(data, header)?;

        crate::debug!("Valid ELF64, entry=0x{:x}, {} phdrs",
            header.e_entry, header.e_phnum);

        let pml4_phys = vmm::new_user_pml4()
            .map_err(|_| "ELF: failed to create PML4")?;

        // Map the PT_LOAD segments INTO the freshly created user PML4. Because
        // `vmm::map`/`virt_to_phys` operate on the *active* CR3, we temporarily
        // install the user PML4 around the segment loop, then restore the
        // kernel CR3. The kernel higher-half (which holds this code, its stack,
        // the heap, the PMM bitmap and the HHDM used for the data copies) was
        // cloned into the user PML4 by `new_user_pml4`, so the kernel keeps
        // running normally while CR3 is switched.
        //
        // The caller (`create_user_process`) runs this with interrupts disabled,
        // so no timer tick can observe the temporarily-installed user CR3.
        let kernel_cr3 = vmm::current_pml4_phys();
        // SAFETY: `pml4_phys` is a valid PML4 containing the kernel higher-half.
        unsafe { vmm::load_cr3(pml4_phys); }
        let load_result = Self::map_segments(data, header);
        // SAFETY: restore the kernel PML4 regardless of success/failure so we
        // never return to the caller with a foreign address space installed.
        unsafe { vmm::load_cr3(kernel_cr3); }
        let brk = load_result?;

        crate::debug!("Loaded: entry=0x{:x} brk=0x{:x}", header.e_entry, brk);

        Ok(ElfProcess {
            entry: header.e_entry,
            pml4_phys,
            // The legacy pagh-native path only loads absolute ET_EXEC images, so
            // there is never a load bias and the entry/phdr addresses are absolute.
            load_bias: 0,
            phdr_vaddr: Self::compute_phdr_vaddr(data, header, 0),
            phent: header.e_phentsize,
            phnum: header.e_phnum,
            initial_brk: brk,
        })
    }

    /// Load a statically-linked Linux `ET_EXEC` or static-PIE `ET_DYN` image and
    /// expose the loader outputs the `Stack_Initializer` needs for the auxv
    /// (task 13.1, R5.1/R5.2/R5.3/R5.4/R5.7/R5.8/R7.6/R11.3/R12.1).
    ///
    /// This is an *additive* sibling of [`ElfLoader::load`]: the legacy path
    /// stays restricted to absolute `ET_EXEC` (and is what `create_user_process`
    /// / `spawn_test_user_process` still use), while this path additionally
    /// accepts static-PIE `ET_DYN` via the pure [`classify_elf`]/[`choose_bias`]
    /// classifier and returns the full [`ElfProcess`] with `load_bias`,
    /// `phdr_vaddr`, `phent`, `phnum`, and `initial_brk` populated.
    ///
    /// Steps:
    ///   1. [`classify_elf`] — on [`ElfVerdict::Reject`] emit exactly one
    ///      diagnostic naming the cause + a binary identifier and return `Err`
    ///      **without** allocating a PML4 or mapping anything (R5.3/R5.5/R5.6/
    ///      R5.9/R11.3/R12.1).
    ///   2. `ET_DYN` → [`choose_bias`] over `max(p_vaddr + p_memsz)`; `ET_EXEC`
    ///      → bias `0` (R5.1/R5.2).
    ///   3. Fresh user PML4; map every `PT_LOAD` at `p_vaddr + bias` with
    ///      `PF_W`→WRITABLE, `!PF_X`→NO_EXECUTE, always `USER_ACCESSIBLE`; copy
    ///      exactly `p_filesz` bytes and zero `[p_filesz, p_memsz)` (R5.4/R5.7/
    ///      R5.8/R7.6).
    ///   4. Compute `phdr_vaddr`, `phent`, `phnum`, `initial_brk`.
    pub fn load_linux(data: &[u8]) -> Result<ElfProcess, &'static str> {
        // 1. Pure classification — reject *before* touching any memory (R12.1).
        let kind = match classify_elf(data) {
            ElfVerdict::Reject(msg) => {
                // One diagnostic naming the rejection cause + a binary identifier
                // (we only have the byte buffer here, so the identifier is its
                // length). Severity is Error per R12.5.
                crate::error!(
                    "Linux ELF loader: rejecting binary (id=<{}-byte image>): {}",
                    data.len(),
                    msg
                );
                return Err(msg);
            }
            ElfVerdict::Load { kind, .. } => kind,
        };

        // `classify_elf` guarantees the buffer is at least an ELF64 header, has
        // valid magic / ELFCLASS64 / ELFDATA2LSB / EM_X86_64, and that every
        // PT_LOAD is in bounds with a (zero-bias) page-rounded range below
        // USER_ADDR_MAX. Reading the header struct is therefore in-bounds.
        // SAFETY: length validated by `classify_elf`; byte pointer.
        let header: &Elf64Header = unsafe { &*(data.as_ptr() as *const Elf64Header) };

        // 2. Choose the load bias.
        let bias = match kind {
            ElfKind::Exec => 0u64,
            ElfKind::Dyn => {
                let max_end = Self::max_load_vaddr_end(data, header)?;
                choose_bias(max_end)
                    .ok_or("ELF: no load bias fits in user address space")?
            }
        };

        // Entry point adjusted by the bias (0 for ET_EXEC). Overflow-safe.
        let entry = header
            .e_entry
            .checked_add(bias)
            .ok_or("ELF: entry point overflow under bias")?;

        crate::debug!(
            "Linux ELF: kind={:?} bias=0x{:x} entry=0x{:x} {} phdrs",
            kind, bias, entry, header.e_phnum
        );

        // 3. Fresh user PML4, then map the biased PT_LOAD segments into it. As in
        // the legacy path we install the user CR3 around the mapping loop (so
        // `vmm::map`/`virt_to_phys` operate on it) and restore the kernel CR3
        // afterwards regardless of outcome. The caller runs with interrupts off.
        let pml4_phys = vmm::new_user_pml4().map_err(|_| "ELF: failed to create PML4")?;

        let kernel_cr3 = vmm::current_pml4_phys();
        // SAFETY: `pml4_phys` is a valid PML4 containing the kernel higher-half.
        unsafe { vmm::load_cr3(pml4_phys); }
        let map_result = Self::map_segments_biased(data, header, bias);
        // SAFETY: always restore the kernel PML4 before returning to the caller.
        unsafe { vmm::load_cr3(kernel_cr3); }
        let initial_brk = map_result?;

        // 4. Loader outputs for the auxv.
        let phdr_vaddr = Self::compute_phdr_vaddr(data, header, bias);

        crate::debug!(
            "Linux ELF loaded: entry=0x{:x} bias=0x{:x} phdr=0x{:x} phent={} phnum={} brk=0x{:x}",
            entry, bias, phdr_vaddr, header.e_phentsize, header.e_phnum, initial_brk
        );

        Ok(ElfProcess {
            entry,
            pml4_phys,
            load_bias: bias,
            phdr_vaddr,
            phent: header.e_phentsize,
            phnum: header.e_phnum,
            initial_brk,
        })
    }

    /// Maximum `p_vaddr + p_memsz` across all `PT_LOAD` segments (the highest
    /// un-biased virtual address the image touches), used to feed [`choose_bias`]
    /// for static-PIE `ET_DYN`. Overflow-safe; returns `0` when there are no
    /// loadable segments.
    ///
    /// PRECONDITION: `classify_elf(data)` has already succeeded, so the phdr
    /// table is fully in bounds and each program header struct read below lies
    /// within `data`.
    fn max_load_vaddr_end(data: &[u8], header: &Elf64Header) -> Result<u64, &'static str> {
        let phoff = header.e_phoff as usize;
        let phentsize = header.e_phentsize as usize;
        let phnum = header.e_phnum as usize;
        let mut max_end: u64 = 0;

        for i in 0..phnum {
            // In bounds: classify_elf validated phoff + phnum*phentsize <= len.
            let ph_offset = phoff + i * phentsize;
            // SAFETY: `ph_offset + phentsize <= table_end <= data.len()`.
            let ph: &Elf64ProgramHeader = unsafe {
                &*(data.as_ptr().add(ph_offset) as *const Elf64ProgramHeader)
            };
            if ph.p_type != PT_LOAD { continue; }
            let end = ph
                .p_vaddr
                .checked_add(ph.p_memsz)
                .ok_or("ELF: vaddr range overflow")?;
            if end > max_end { max_end = end; }
        }
        Ok(max_end)
    }

    /// Compute `AT_PHDR`: the in-memory virtual address of the program-header
    /// table once the image is loaded at `bias`.
    ///
    /// Standard SysV convention: the headers are mapped as part of whichever
    /// `PT_LOAD` segment's file range `[p_offset, p_offset + p_filesz)` covers
    /// `e_phoff`; for that segment the table sits at
    /// `p_vaddr + bias + (e_phoff - p_offset)`. When no segment explicitly covers
    /// `e_phoff` (rare), fall back to "headers live at the start of the first
    /// `PT_LOAD`", i.e. `first_load.p_vaddr + bias + e_phoff`, and finally to
    /// `bias + e_phoff` if there are no loadable segments at all.
    ///
    /// PRECONDITION: `classify_elf(data)` has already succeeded.
    fn compute_phdr_vaddr(data: &[u8], header: &Elf64Header, bias: u64) -> u64 {
        let phoff = header.e_phoff;
        let phentsize = header.e_phentsize as usize;
        let phnum = header.e_phnum as usize;
        let table_off = header.e_phoff as usize;
        let mut first_load_vaddr: Option<u64> = None;

        for i in 0..phnum {
            // In bounds: classify_elf validated the phdr table fits in `data`.
            let ph_offset = table_off + i * phentsize;
            // SAFETY: bounds guaranteed by classify_elf.
            let ph: &Elf64ProgramHeader = unsafe {
                &*(data.as_ptr().add(ph_offset) as *const Elf64ProgramHeader)
            };
            if ph.p_type != PT_LOAD { continue; }
            if first_load_vaddr.is_none() {
                first_load_vaddr = Some(ph.p_vaddr);
            }
            // Does this segment's file range cover the phdr table offset?
            let seg_file_end = ph.p_offset.saturating_add(ph.p_filesz);
            if ph.p_offset <= phoff && phoff < seg_file_end {
                return ph
                    .p_vaddr
                    .wrapping_add(bias)
                    .wrapping_add(phoff - ph.p_offset);
            }
        }

        if let Some(v) = first_load_vaddr {
            return v.wrapping_add(bias).wrapping_add(phoff);
        }
        bias.wrapping_add(phoff)
    }

    /// Map all `PT_LOAD` segments at `p_vaddr + bias` into the **currently
    /// active** PML4 (the caller installs the user PML4 first) and return the
    /// page-aligned program break (the page-rounded top of the highest segment).
    ///
    /// Identical in structure to [`ElfLoader::map_segments`] but applies a load
    /// bias (for static-PIE) to every segment's virtual address. Per-segment
    /// flags: `PF_W`→WRITABLE, `!PF_X`→NO_EXECUTE, always `USER_ACCESSIBLE`
    /// (R5.4/R7.6). Copies exactly `p_filesz` bytes from `p_offset` and zeroes
    /// `[p_filesz, p_memsz)` (R5.7/R5.8). All arithmetic is overflow-safe and the
    /// biased range is re-checked against `USER_ADDR_MAX` defensively.
    ///
    /// PRECONDITION: `classify_elf(data)` has already succeeded for this image.
    fn map_segments_biased(
        data: &[u8],
        header: &Elf64Header,
        bias: u64,
    ) -> Result<u64, &'static str> {
        let mut brk: u64 = 0;
        let phoff = header.e_phoff as usize;
        let phentsize = header.e_phentsize as usize;
        let phnum = header.e_phnum as usize;

        if phnum == 0 {
            return Ok(0);
        }
        if phentsize < core::mem::size_of::<Elf64ProgramHeader>() {
            return Err("ELF: invalid program header size");
        }

        for i in 0..phnum {
            let ph_offset = phoff
                .checked_add(i.checked_mul(phentsize).ok_or("ELF: phdr index overflow")?)
                .ok_or("ELF: phdr offset overflow")?;
            let ph_end = ph_offset
                .checked_add(phentsize)
                .ok_or("ELF: phdr end overflow")?;
            if ph_end > data.len() {
                return Err("ELF: program header beyond data");
            }

            // SAFETY: Bounds checked above.
            let ph: &Elf64ProgramHeader = unsafe {
                &*(data.as_ptr().add(ph_offset) as *const Elf64ProgramHeader)
            };

            if ph.p_type != PT_LOAD { continue; }

            let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
            if ph.p_flags & PF_W != 0 { flags |= PageTableFlags::WRITABLE; }
            if ph.p_flags & PF_X == 0 { flags |= PageTableFlags::NO_EXECUTE; }

            // Apply the load bias to the segment's virtual address (R5.2).
            let vaddr_start = ph
                .p_vaddr
                .checked_add(bias)
                .ok_or("ELF: vaddr+bias overflow")?;
            let vaddr_end = vaddr_start
                .checked_add(ph.p_memsz)
                .ok_or("ELF: vaddr range overflow")?;
            if vaddr_start >= USER_ADDR_MAX || vaddr_end > USER_ADDR_MAX {
                return Err("ELF: segment outside user address space");
            }
            let page_start = (vaddr_start / 4096) * 4096;
            let page_end = vaddr_end
                .checked_add(4095)
                .ok_or("ELF: vaddr page-round overflow")?
                / 4096 * 4096;

            crate::debug!("PT_LOAD@bias: 0x{:x}..0x{:x} fsz={} msz={} fl=0x{:x}",
                vaddr_start, vaddr_end, ph.p_filesz, ph.p_memsz, ph.p_flags);

            let mut addr = page_start;
            while addr < page_end {
                let frame = crate::memory::pmm::alloc_frame()
                    .ok_or("ELF: PMM OOM for LOAD")?;
                vmm::map(frame, addr, flags)
                    .map_err(|_| "ELF: VMM map failed")?;
                addr += 4096;
            }

            if ph.p_filesz > 0 {
                let file_off = ph.p_offset as usize;
                let file_sz = ph.p_filesz as usize;
                let file_end = file_off
                    .checked_add(file_sz)
                    .ok_or("ELF: segment file range overflow")?;
                if file_end > data.len() {
                    return Err("ELF: file data beyond input");
                }
                // SAFETY: Offset validated above.
                let src = unsafe { data.as_ptr().add(file_off) };
                let dst_phys = vmm::virt_to_phys(vaddr_start)
                    .ok_or("ELF: failed to translate vaddr")?;
                let dst = vmm::phys_to_virt(dst_phys) as *mut u8;

                // SAFETY: src within data, dst is the mapped page.
                unsafe {
                    ptr::copy_nonoverlapping(src, dst, file_sz);
                }
            }

            if ph.p_memsz > ph.p_filesz {
                let bss_start = vaddr_start + ph.p_filesz;
                let bss_end = vaddr_end;
                let bss_phys = vmm::virt_to_phys(bss_start)
                    .ok_or("ELF: failed to translate bss")?;
                let bss_virt = vmm::phys_to_virt(bss_phys) as *mut u8;
                // SAFETY: bss_virt is valid mapped memory.
                unsafe {
                    ptr::write_bytes(bss_virt, 0, (bss_end - bss_start) as usize);
                }
            }

            if vaddr_end > brk { brk = vaddr_end; }
        }

        brk = (brk + 4095) & !4095;
        Ok(brk)
    }

    /// Validate the program-header table and all `PT_LOAD` segments using only
    /// overflow-safe arithmetic. Returns `Ok(())` if every segment is in bounds
    /// and addressable, otherwise an `Err` describing the first problem found.
    ///
    /// This performs NO allocation and NO mapping — it exists so the loader can
    /// reject malformed binaries before creating the user PML4, and so the
    /// mapping loop in `map_segments` is guaranteed never to panic or run past
    /// the input buffer.
    ///
    /// A zero-`e_phnum` image is accepted (there is simply nothing to map); the
    /// caller still gets a valid entry and handles any run failure itself.
    fn validate_program_headers(data: &[u8], header: &Elf64Header) -> Result<(), &'static str> {
        let phnum = header.e_phnum as usize;
        if phnum == 0 {
            // No program headers: nothing to load. Accept gracefully.
            return Ok(());
        }

        let phentsize = header.e_phentsize as usize;
        if phentsize < core::mem::size_of::<Elf64ProgramHeader>() {
            return Err("ELF: invalid program header size");
        }

        let phoff = header.e_phoff as usize;

        // Bound the whole phdr table: phoff + phnum*phentsize must fit in `data`.
        let table_bytes = phnum
            .checked_mul(phentsize)
            .ok_or("ELF: program header table size overflow")?;
        let table_end = phoff
            .checked_add(table_bytes)
            .ok_or("ELF: program header table offset overflow")?;
        if table_end > data.len() {
            return Err("ELF: program header table beyond data");
        }

        for i in 0..phnum {
            // Safe: i < phnum and phnum*phentsize did not overflow, so this is
            // <= table_bytes which is within usize range.
            let ph_offset = phoff + i * phentsize;

            // SAFETY: `ph_offset + phentsize <= table_end <= data.len()` and
            // `phentsize >= size_of::<Elf64ProgramHeader>()`, so the full
            // program-header struct lies within `data`.
            let ph: &Elf64ProgramHeader = unsafe {
                &*(data.as_ptr().add(ph_offset) as *const Elf64ProgramHeader)
            };

            if ph.p_type != PT_LOAD { continue; }

            // File data range must lie within the input buffer.
            if ph.p_filesz > 0 {
                let file_off = ph.p_offset as usize;
                let file_sz = ph.p_filesz as usize;
                let file_end = file_off
                    .checked_add(file_sz)
                    .ok_or("ELF: segment file range overflow")?;
                if file_end > data.len() {
                    return Err("ELF: file data beyond input");
                }
            }

            // filesz must not exceed memsz (the bss tail would be negative).
            if ph.p_filesz > ph.p_memsz {
                return Err("ELF: p_filesz exceeds p_memsz");
            }

            // Virtual range must be canonical lower-half (user) and not overflow.
            let vaddr_end = ph.p_vaddr
                .checked_add(ph.p_memsz)
                .ok_or("ELF: vaddr range overflow")?;
            if ph.p_vaddr >= USER_ADDR_MAX || vaddr_end > USER_ADDR_MAX {
                return Err("ELF: segment outside user address space");
            }

            // Page-rounded end must also not overflow (used by the map loop).
            vaddr_end
                .checked_add(4095)
                .ok_or("ELF: vaddr page-round overflow")?;
        }

        Ok(())
    }

    /// Map all `PT_LOAD` segments described by `header` from `data` into the
    /// **currently active** PML4 (the caller installs the user PML4 first), and
    /// return the computed program break (`brk`).
    ///
    /// PRECONDITION: `validate_program_headers` has already succeeded for this
    /// `data`/`header`, so all offsets/sizes are in bounds and all virtual
    /// addresses are canonical lower-half. Bounds checks are nonetheless
    /// repeated defensively with checked arithmetic so this function can never
    /// panic or index out of bounds even if called independently.
    fn map_segments(data: &[u8], header: &Elf64Header) -> Result<u64, &'static str> {
        let mut brk: u64 = 0;
        let phoff = header.e_phoff as usize;
        let phentsize = header.e_phentsize as usize;
        let phnum = header.e_phnum as usize;

        if phnum == 0 {
            return Ok(0);
        }

        if phentsize < core::mem::size_of::<Elf64ProgramHeader>() {
            return Err("ELF: invalid program header size");
        }

        for i in 0..phnum {
            let ph_offset = phoff
                .checked_add(i.checked_mul(phentsize).ok_or("ELF: phdr index overflow")?)
                .ok_or("ELF: phdr offset overflow")?;
            let ph_end = ph_offset
                .checked_add(phentsize)
                .ok_or("ELF: phdr end overflow")?;
            if ph_end > data.len() {
                return Err("ELF: program header beyond data");
            }

            // SAFETY: Bounds checked above.
            let ph: &Elf64ProgramHeader = unsafe {
                &*(data.as_ptr().add(ph_offset) as *const Elf64ProgramHeader)
            };

            if ph.p_type != PT_LOAD { continue; }

            let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
            if ph.p_flags & PF_W != 0 { flags |= PageTableFlags::WRITABLE; }
            if ph.p_flags & PF_X == 0 { flags |= PageTableFlags::NO_EXECUTE; }

            let vaddr_start = ph.p_vaddr;
            let vaddr_end = vaddr_start
                .checked_add(ph.p_memsz)
                .ok_or("ELF: vaddr range overflow")?;
            if vaddr_start >= USER_ADDR_MAX || vaddr_end > USER_ADDR_MAX {
                return Err("ELF: segment outside user address space");
            }
            let page_start = (vaddr_start / 4096) * 4096;
            let page_end = vaddr_end
                .checked_add(4095)
                .ok_or("ELF: vaddr page-round overflow")?
                / 4096 * 4096;

            crate::debug!("PT_LOAD: 0x{:x}..0x{:x} fsz={} msz={} fl=0x{:x}",
                vaddr_start, vaddr_end, ph.p_filesz, ph.p_memsz, ph.p_flags);

            let mut addr = page_start;
            while addr < page_end {
                let frame = crate::memory::pmm::alloc_frame()
                    .ok_or("ELF: PMM OOM for LOAD")?;
                vmm::map(frame, addr, flags)
                    .map_err(|_| "ELF: VMM map failed")?;
                addr += 4096;
            }

            if ph.p_filesz > 0 {
                let file_off = ph.p_offset as usize;
                let file_sz = ph.p_filesz as usize;
                let file_end = file_off
                    .checked_add(file_sz)
                    .ok_or("ELF: segment file range overflow")?;
                if file_end > data.len() {
                    return Err("ELF: file data beyond input");
                }
                // SAFETY: Offset validated above.
                let src = unsafe { data.as_ptr().add(file_off) };
                let dst_phys = vmm::virt_to_phys(vaddr_start)
                    .ok_or("ELF: failed to translate vaddr")?;
                let dst = vmm::phys_to_virt(dst_phys) as *mut u8;

                // SAFETY: src within data, dst is mapped page.
                unsafe {
                    ptr::copy_nonoverlapping(src, dst, file_sz);
                }
            }

            if ph.p_memsz > ph.p_filesz {
                let bss_start = vaddr_start + ph.p_filesz;
                let bss_end = vaddr_end;
                let bss_phys = vmm::virt_to_phys(bss_start)
                    .ok_or("ELF: failed to translate bss")?;
                let bss_virt = vmm::phys_to_virt(bss_phys) as *mut u8;
                // SAFETY: bss_virt is valid mapped memory.
                unsafe {
                    ptr::write_bytes(bss_virt, 0, (bss_end - bss_start) as usize);
                }
            }

            if vaddr_end > brk { brk = vaddr_end; }
        }

        brk = (brk + 4095) & !4095;
        Ok(brk)
    }
}
