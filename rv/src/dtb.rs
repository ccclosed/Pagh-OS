//! Minimal device-tree discovery. OpenSBI passes a pointer to the flattened
//! device tree (DTB) in `a1`; we parse it (via the `fdt` crate) to learn the
//! physical RAM range for the PMM. Later milestones extend this to the UART,
//! PLIC, CLINT, and virtio-mmio register windows + IRQ numbers.

/// A physical memory range `[start, end)` in bytes.
#[derive(Clone, Copy, Debug)]
pub struct MemInfo {
    pub start: usize,
    pub end: usize,
}

/// Parse the main `/memory` region from the DTB at `dtb_ptr`. Returns `None` if
/// the blob is unparseable or carries no sized memory region (the caller then
/// falls back to a conservative default).
pub fn memory(dtb_ptr: usize) -> Option<MemInfo> {
    // SAFETY: OpenSBI guarantees a valid flattened DTB at this S-mode-readable
    // address; `fdt` validates the header/magic and bounds every access.
    let fdt = unsafe { fdt::Fdt::from_ptr(dtb_ptr as *const u8) }.ok()?;
    let region = fdt.memory().regions().next()?;
    let start = region.starting_address as usize;
    let size = region.size?;
    Some(MemInfo {
        start,
        end: start + size,
    })
}

/// Find the ns16550 UART's MMIO base address in the DTB.
pub fn uart(dtb_ptr: usize) -> Option<usize> {
    // SAFETY: as in `memory`.
    let fdt = unsafe { fdt::Fdt::from_ptr(dtb_ptr as *const u8) }.ok()?;
    let node = fdt.find_compatible(&["ns16550a", "ns16550", "snps,dw-apb-uart"])?;
    let region = node.reg()?.next()?;
    Some(region.starting_address as usize)
}
