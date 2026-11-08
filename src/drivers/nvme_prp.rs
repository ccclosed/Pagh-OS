//! Pure transfer planning for the NVMe driver.
//!
//! Nothing here touches MMIO, the PMM or the allocator. The module is included
//! verbatim by `host-tests` (`#[path]`, see `host-tests/src/properties/nvme_io.rs`),
//! so the arithmetic the kernel executes is also fuzzed on the host. Three pieces
//! are easy to get subtly wrong and are therefore isolated here:
//!
//! * **PRP construction** (NVMe Base Spec §3.3.3): PRP1 is the first data page
//!   (with an in-page byte offset in its low bits), PRP2 is the *second page
//!   address* for exactly two pages, and a *PRP-list page address* for three or
//!   more. In the list case the list's entry 0 is the **second** page (PRP1
//!   already covers the first), entry 1 the third, and so on. Off-by-one here
//!   makes the controller read or write unrelated physical memory.
//! * **Command chunking**: mapping a byte range of the caller's buffer to an
//!   LBA-aligned Read/Write command that fits one DMA slot. The command covers
//!   whole logical blocks, so the bytes before/after the caller's range inside
//!   the first/last LBA must be padded (reads) or preserved (writes).
//! * **Ring state**: the SQ tail and the polled CQ head/phase-tag. The phase tag
//!   inverts exactly on a wrap; a stale slot must never be consumed as a fresh
//!   completion.
//!
//! All address arithmetic is `core`-only and panic-free on hostile inputs: the
//! functions return `None`/`Err` rather than relying on callers to pre-validate.

/// The driver programs CC.MPS = 4 KiB and addresses every DMA buffer with 4 KiB
/// pages, so PRP entries are 4 KiB pages.
pub const PAGE_SIZE: usize = 4096;

/// One PRP entry is a 64-bit physical address.
pub const PRP_ENTRY_BYTES: usize = 8;

/// Number of PRP entries that fit in one 4 KiB PRP-list page.
pub const PRP_LIST_ENTRIES: usize = PAGE_SIZE / PRP_ENTRY_BYTES;

/// NLB is a 16-bit zero-based field: one command covers at most 2^16 logical
/// blocks. `u32` is used internally for the count.
pub const MAX_NLB: u32 = 0x1_0000;

// ─── PRP construction ───────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrpError {
    /// A Read/Write command must transfer at least one byte.
    EmptyTransfer,
    /// PRP1's in-page offset must be smaller than one page.
    OffsetOutOfRange,
    /// The scratch buffer must start on a page boundary.
    BaseMisaligned,
    /// A PRP-list page must be page-aligned (bits 11:0 carry no flags).
    ListMisaligned,
    /// The transfer spans more pages than the scratch buffer has.
    TransferTooLarge,
    /// The caller's PRP-list slice is shorter than the list the transfer needs.
    ListTooSmall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrpPlan {
    /// First data page physical address (plus the in-page byte offset).
    pub prp1: u64,
    /// Second data page address, a PRP-list page address, or 0 for one page.
    pub prp2: u64,
    /// Number of PRP-list entries written (0 unless `prp2` is a list page).
    pub list_len: usize,
}

/// Build the PRP1/PRP2 pair for a transfer inside one physically-contiguous
/// scratch buffer and fill the PRP list when the transfer needs one.
///
/// `base_phys`/`pages` describe the whole scratch buffer; `offset` is the byte
/// offset of the first transferred byte inside its first page and `len` the
/// number of bytes. `list_phys` is the page-aligned PRP-list page and `list`
/// its `u64` view.
///
/// Layout for `n` pages (n >= 3):
/// ```text
///   PRP1 = base + offset        (page 0, at `offset`)
///   PRP2 = list_phys
///   list[0] = base + 1 * PAGE_SIZE   (page 1)
///   list[1] = base + 2 * PAGE_SIZE   (page 2)
///   ...
///   list[n-2] = base + (n-1) * PAGE_SIZE
/// ```
pub fn build_prp(
    base_phys: u64,
    pages: usize,
    offset: usize,
    len: usize,
    list_phys: u64,
    list: &mut [u64],
) -> Result<PrpPlan, PrpError> {
    if len == 0 {
        return Err(PrpError::EmptyTransfer);
    }
    if offset >= PAGE_SIZE {
        return Err(PrpError::OffsetOutOfRange);
    }
    if base_phys & (PAGE_SIZE as u64 - 1) != 0 {
        return Err(PrpError::BaseMisaligned);
    }

    let end = offset.checked_add(len).ok_or(PrpError::TransferTooLarge)?;
    let needed = end
        .checked_add(PAGE_SIZE - 1)
        .ok_or(PrpError::TransferTooLarge)?
        / PAGE_SIZE;
    if needed > pages {
        return Err(PrpError::TransferTooLarge);
    }

    let prp1 = base_phys + offset as u64;
    if needed == 1 {
        return Ok(PrpPlan {
            prp1,
            prp2: 0,
            list_len: 0,
        });
    }
    if needed == 2 {
        // Exactly two pages: PRP2 is the second page itself, NOT a list.
        return Ok(PrpPlan {
            prp1,
            prp2: base_phys + PAGE_SIZE as u64,
            list_len: 0,
        });
    }

    if list_phys & (PAGE_SIZE as u64 - 1) != 0 {
        return Err(PrpError::ListMisaligned);
    }
    let list_len = needed - 1;
    if list.len() < list_len {
        return Err(PrpError::ListTooSmall);
    }
    // Entry 0 is the SECOND page: PRP1 already names the first.
    for (i, entry) in list[..list_len].iter_mut().enumerate() {
        *entry = base_phys + ((i + 1) * PAGE_SIZE) as u64;
    }
    Ok(PrpPlan {
        prp1,
        prp2: list_phys,
        list_len,
    })
}

// ─── Command chunk planning ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkPlan {
    /// First logical block covered by the command.
    pub lba: u64,
    /// Offset of the caller's first byte inside that logical block.
    pub within: usize,
    /// Number of logical blocks the command transfers (>= 1).
    pub lbas: u32,
    /// Caller bytes carried by this chunk (>= 1, <= `lbas * lba_size`).
    pub data_len: usize,
}

/// Plan one Read/Write command for `remaining` bytes starting at `byte_off`.
///
/// The command always starts at an LBA boundary, so `within` bytes of the first
/// LBA precede the caller's data and the command may cover up to `lba_size - 1`
/// bytes past the caller's data. The command's device data starts at buffer
/// offset 0 in the driver's slot (PRP1 offset 0); the caller's bytes sit at
/// offset `within` **inside** that same buffer. `max_bytes` is the slot buffer
/// size; it must be a positive multiple of `lba_size` (the driver allocates
/// whole pages, and every NVMe LBA size is a power of two).
///
/// Returns `None` instead of producing a command that would not fit or would be
/// unrepresentable in the 16-bit NLB field.
pub fn plan_chunk(
    byte_off: u64,
    remaining: usize,
    lba_size: u64,
    max_bytes: usize,
) -> Option<ChunkPlan> {
    if remaining == 0 || lba_size == 0 || max_bytes == 0 {
        return None;
    }
    let lba_usize = usize::try_from(lba_size).ok()?;
    if lba_usize > max_bytes || max_bytes % lba_usize != 0 {
        return None;
    }

    let within = usize::try_from(byte_off % lba_size).ok()?;
    let avail = max_bytes - within;
    let data_len = remaining.min(avail);
    if data_len == 0 {
        return None;
    }

    let span = within.checked_add(data_len)?;
    let lbas_usize = span.checked_add(lba_usize - 1)? / lba_usize;
    if lbas_usize == 0 || lbas_usize > MAX_NLB as usize {
        return None;
    }
    // The command data starts at slot offset 0; `within` is where the caller's
    // data lives inside it. `max_bytes - within` bytes are therefore available
    // to the caller, and the whole LBA-aligned command must fit the slot.
    let command_bytes = lbas_usize.checked_mul(lba_usize)?;
    if command_bytes > max_bytes || within.checked_add(data_len)? > max_bytes {
        return None;
    }
    Some(ChunkPlan {
        lba: byte_off / lba_size,
        within,
        lbas: lbas_usize as u32,
        data_len,
    })
}

// ─── Queue rings ────────────────────────────────────────────────────────────

/// Largest power of two not exceeding both `desired` and `max_supported`.
///
/// CAP.MQES is zero-based, so the caller passes `MQES + 1`. NVMe does not
/// require a power-of-two queue size, but rings that are a power of two keep
/// the wrap arithmetic obvious; returning 0 lets the caller reject a controller
/// too small for the requested depth instead of creating an invalid queue.
pub fn queue_entries(desired: u32, max_supported: u32) -> u32 {
    let limit = desired.min(max_supported);
    if limit == 0 {
        return 0;
    }
    1u32 << (31 - limit.leading_zeros())
}

/// Submission-queue tail: the next slot to write and the queue size.
///
/// The controller and the host must agree on the size: `IOSQ.CDW10.QSIZE` is
/// zero-based, so a 64-entry ring is created with `63`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqRing {
    tail: u32,
    entries: u32,
}

impl SqRing {
    pub const fn new(entries: u32) -> Self {
        assert!(entries > 0);
        Self { tail: 0, entries }
    }

    pub const fn tail(&self) -> u32 {
        self.tail
    }

    /// Advance to the next submission slot, wrapping at the queue size.
    pub fn advance(&mut self) {
        self.tail += 1;
        if self.tail == self.entries {
            self.tail = 0;
        }
    }
}

/// Polled completion-queue head plus the expected phase tag.
///
/// The host zeroes the CQ memory, so every slot initially carries phase 0. The
/// first controller pass writes phase 1 (`CqRing` starts at 1) and the tag
/// inverts each time the head wraps. `ready` is the only test that may decide a
/// slot holds a fresh completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CqRing {
    head: u32,
    entries: u32,
    phase: u32,
}

impl CqRing {
    pub const fn new(entries: u32) -> Self {
        assert!(entries > 0);
        Self {
            head: 0,
            entries,
            phase: 1,
        }
    }

    pub const fn head(&self) -> u32 {
        self.head
    }

    pub const fn phase(&self) -> u32 {
        self.phase
    }

    /// Does this CQ entry belong to the current controller pass?
    #[inline]
    pub fn ready(&self, status: u16) -> bool {
        (status as u32 & 1) == self.phase
    }

    /// Consume the entry at the head and return its slot index.
    ///
    /// The caller must have observed `ready(status)` for that slot first; this
    /// only advances the ring and flips the phase on wrap.
    pub fn consume(&mut self) -> u32 {
        let slot = self.head;
        self.head += 1;
        if self.head == self.entries {
            self.head = 0;
            self.phase ^= 1;
        }
        slot
    }
}
