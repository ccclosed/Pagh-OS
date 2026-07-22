// task/stack_map.rs — Effectful initial-stack mapper (kernel-only)
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// This module is the **effectful half** of the `Stack_Initializer` component
// (design §5, task 13.2). It wraps the pure `task::stack::build_initial_stack`
// encoder: it maps the ring-3 user-stack pages, asks the pure encoder for the
// exact SysV byte image, copies that image into the mapped stack, and returns
// the entry `rsp` for the process builder.
//
// It lives in its OWN file (not `task/stack.rs`) on purpose: `task/stack.rs` is
// `#[path]`-included verbatim by the `host-tests` crate so its pure encoder can
// be property-tested on the host (R11.6). This wrapper uses kernel-only
// `memory::{pmm, vmm}` paging APIs that do not exist on the host, so keeping it
// separate leaves `task/stack.rs` purely host-testable while this file is
// compiled only as part of the kernel.
//
// Requirements covered: R6.6 (AT_RANDOM bytes live in the mapped stack), R6.8
// (insufficient space aborts without starting the process), R7.6 (stack pages
// mapped USER_ACCESSIBLE so ring-3 can reach them).
#![allow(dead_code)]

use core::cmp::min;
use core::ptr;

use x86_64::structures::paging::PageTableFlags;

use crate::memory::layout::{PAGE_SIZE, USER_STACK_PAGES, USER_STACK_TOP};
use crate::memory::{pmm, vmm};
use crate::task::stack::{build_initial_stack, AuxInputs, StackError, StackImage};

/// Failure modes of [`map_initial_stack`].
///
/// [`TooLarge`](StackMapError::TooLarge) forwards the pure encoder's
/// [`StackError::TooLarge`] (R6.8). The other two cover the effectful mapping
/// step (physical-frame exhaustion / page-table failure); in every error case
/// the wrapper unwinds any pages it mapped and the process is NOT started
/// (R6.8, R7.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StackMapError {
    /// The encoded SysV image does not fit in the user-stack region (R6.8).
    TooLarge,
    /// A physical frame for a stack page could not be allocated.
    OutOfMemory,
    /// Mapping a stack page into the user PML4 failed.
    MapFailed,
}

impl From<StackError> for StackMapError {
    fn from(e: StackError) -> Self {
        match e {
            StackError::TooLarge => StackMapError::TooLarge,
        }
    }
}

/// Map the initial user stack and populate it with the System V image (R6.6,
/// R6.8, R7.6).
///
/// Mirrors the user-mapping approach in `task::process::create_user_process`:
/// the user address space is `pml4_phys` (the fresh PML4 the loader produced as
/// `ElfProcess::pml4_phys`), so this wrapper temporarily installs that CR3 while
/// it maps and writes the stack, then restores the kernel CR3 on every return
/// path. The kernel higher-half (this code, its stack, the heap that backs the
/// pure encoder's `Vec`, and the HHDM used for the copy) stays mapped under the
/// user PML4, so it is safe to run while that CR3 is active.
///
/// PRECONDITION: the caller must run this with interrupts disabled — it briefly
/// installs the user CR3 while executing kernel code, exactly like
/// `create_user_process`/`ElfLoader::load`.
///
/// Steps:
///  1. Derive the stack region `[stack_low, USER_STACK_TOP)` from the layout
///     constants (`USER_STACK_TOP`, `USER_STACK_PAGES`) — the same region
///     `create_user_process` uses.
///  2. Map every stack page `WRITABLE | NO_EXECUTE | USER_ACCESSIBLE` (R7.6),
///     backing each with a PMM frame, tracking how many we mapped.
///  3. Call the pure [`build_initial_stack`] encoder for that region.
///  4. On `Ok`, copy `StackImage::bytes` into the mapped stack at `argc_addr`
///     (page-by-page via the HHDM, so we never assume the freshly allocated
///     frames are physically contiguous) and return `initial_rsp`.
///  5. On any error (encoder `TooLarge`, PMM OOM, or VMM map failure), unwind
///     every page mapped in step 2 and propagate the error so the process is
///     not started (R6.8, R7.3).
///
/// Returns the initial `rsp` (== the 16-byte-aligned `argc` address) on success.
pub fn map_initial_stack(
    pml4_phys: u64,
    argv: &[&[u8]],
    envp: &[&[u8]],
    aux: &AuxInputs,
    random16: [u8; 16],
) -> Result<u64, StackMapError> {
    let stack_top = USER_STACK_TOP;
    let stack_low = USER_STACK_TOP - USER_STACK_PAGES * PAGE_SIZE;

    // Ring-3 stack pages: writable data, never executable, reachable from ring 3
    // (R7.6). Identical flag set to `create_user_process`'s user stack.
    let uflags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE;

    let kernel_cr3 = vmm::current_pml4_phys();
    // SAFETY: `pml4_phys` is a valid PML4 with the kernel higher-half cloned in,
    // so kernel code/stack/heap/HHDM remain mapped while it is installed. We
    // restore `kernel_cr3` on every return path below before handing back.
    unsafe {
        vmm::load_cr3(pml4_phys);
    }

    // ── Map the stack region, remembering how many pages we committed ────────
    let mut mapped_pages: u64 = 0;
    let mut map_err: Option<StackMapError> = None;
    for page in 0..USER_STACK_PAGES {
        let vaddr = stack_low + page * PAGE_SIZE;
        match pmm::alloc_frame() {
            Some(frame) => {
                if vmm::map(frame, vaddr, uflags).is_err() {
                    // Mapping failed: the frame is not referenced by any PTE, so
                    // free it directly (the unwind loop only sees mapped pages).
                    pmm::free_frame(frame);
                    map_err = Some(StackMapError::MapFailed);
                    break;
                }
                mapped_pages += 1;
            }
            None => {
                map_err = Some(StackMapError::OutOfMemory);
                break;
            }
        }
    }

    // ── Build the pure image and copy it in (only if mapping fully succeeded) ─
    let result: Result<u64, StackMapError> = match map_err {
        None => match build_initial_stack(stack_top, stack_low, argv, envp, aux, random16) {
            Ok(image) => {
                copy_image_to_user_stack(&image);
                Ok(image.initial_rsp)
            }
            Err(e) => Err(StackMapError::from(e)),
        },
        Some(e) => Err(e),
    };

    // ── On any error, unwind the pages we mapped (R6.8) ──────────────────────
    if result.is_err() {
        for page in 0..mapped_pages {
            let vaddr = stack_low + page * PAGE_SIZE;
            // Translate before unmapping so we can return the backing frame.
            if let Some(phys) = vmm::virt_to_phys(vaddr) {
                let _ = vmm::unmap(vaddr);
                pmm::free_frame(phys);
            }
        }
    }

    // SAFETY: restore the kernel PML4 before returning to the caller so we never
    // hand control back with a foreign address space installed.
    unsafe {
        vmm::load_cr3(kernel_cr3);
    }

    result
}

/// Copy a built [`StackImage`] into the already-mapped user stack.
///
/// The image occupies `[argc_addr, stack_top)`; `bytes[0]` lives at `argc_addr`.
/// The copy walks one page at a time and translates each page through the HHDM
/// (`virt_to_phys` → `phys_to_virt`) — the same technique `ElfLoader` uses — so
/// it is correct even though the freshly allocated stack frames are not
/// guaranteed to be physically contiguous.
///
/// PRECONDITION: the user PML4 backing `argc_addr` is the active CR3 and every
/// page the image spans is mapped writable (guaranteed by the caller).
fn copy_image_to_user_stack(image: &StackImage) {
    let bytes = &image.bytes;
    let dst_base = image.argc_addr;
    let mut written: usize = 0;
    while written < bytes.len() {
        let dst_vaddr = dst_base + written as u64;
        let page_base = dst_vaddr & !(PAGE_SIZE - 1);
        let page_off = (dst_vaddr - page_base) as usize;
        let chunk = min(PAGE_SIZE as usize - page_off, bytes.len() - written);

        if let Some(phys) = vmm::virt_to_phys(page_base) {
            let dst = (vmm::phys_to_virt(phys) + page_off as u64) as *mut u8;
            // SAFETY: `dst` is the HHDM alias of a mapped, writable user stack
            // page; `chunk` stays within that page and within `bytes`.
            unsafe {
                ptr::copy_nonoverlapping(bytes.as_ptr().add(written), dst, chunk);
            }
        }
        written += chunk;
    }
}
