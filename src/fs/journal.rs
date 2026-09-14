//! Write-ahead-log (WAL) journal over a `BlockDevice`.
//!
//! Provides atomic multi-block transactions and crash-consistent replay, as
//! specified in the `networking-and-storage` design ("Journal commit" /
//! "Journal replay" pseudocode). The journal region lives **after** the ext2
//! filesystem's last block (`>= s_blocks_count`), so the host-visible ext2
//! layout is never touched; transaction targets are ext2 block indices in
//! `[0, fs_blocks)`.
//!
//! On-disk a transaction is `[Descriptor][Data]*N[Commit]`. A transaction is
//! *committed* iff a valid `Commit` block (magic + matching `seq` + matching
//! CRC32 over the logged data) immediately follows its data blocks. `commit`
//! writes the log records and the commit record, **then** checkpoints the data
//! to its final ext2 locations; the commit point is the durability of the
//! commit record. `recover` scans the log from `tail` in `seq` order, replays
//! committed transactions idempotently, and stops (discarding the rest) at the
//! first uncommitted/corrupt transaction.

#![allow(dead_code)]

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::drivers::BlockDevice;
use crate::fs::ext2::structs::{
    self, JournalCommit, JournalDescriptor, JournalSuper, BS, JCMT_MAGIC, JDESC_MAX_TARGETS,
    JDES_MAGIC, JNL_MAGIC, SECTORS_PER_BLOCK,
};
use crate::fs::FsError;

/// Location of the journal region on the device (in FS-block units).
#[derive(Clone, Copy)]
pub struct JournalArea {
    /// FS block holding the journal superblock (== ext2 `s_blocks_count`).
    pub super_block: u64,
    /// Number of circular log blocks (excludes the journal superblock).
    pub log_blocks: u64,
    /// ext2 `s_blocks_count` — where the journal region starts / target bound.
    pub fs_blocks: u64,
}

/// An open transaction: a set of `(final ext2 block, new BS-byte contents)`.
pub struct Txn {
    records: Vec<(u64, Vec<u8>)>,
}

impl Txn {
    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// The WAL journal.
pub struct Journal {
    dev: Arc<dyn BlockDevice>,
    super_block: u64,
    log_start: u64, // first log block (super_block + 1)
    log_blocks: u64,
    fs_blocks: u64,
    head: u64, // next free log position (relative to log space)
    tail: u64, // oldest live log position
    next_seq: u64,
}

// ─── low-level block IO (4096-byte FS blocks over 512-byte sectors) ──────────

fn read_block(dev: &dyn BlockDevice, fs_block: u64) -> Result<Vec<u8>, FsError> {
    let mut buf = vec![0u8; BS];
    dev.read_block(fs_block * SECTORS_PER_BLOCK, &mut buf)
        .map(|_| ())
        .map_err(|_| FsError::IoError)?;
    Ok(buf)
}

fn write_block(dev: &dyn BlockDevice, fs_block: u64, data: &[u8]) -> Result<(), FsError> {
    debug_assert!(data.len() == BS);
    dev.write_block(fs_block * SECTORS_PER_BLOCK, data)
        .map(|_| ())
        .map_err(|_| FsError::IoError)
}

impl Journal {
    /// Absolute FS block for log position `pos` (wraps around the circular log).
    fn log_block_addr(&self, pos: u64) -> u64 {
        self.log_start + (pos % self.log_blocks)
    }

    fn read_log(&self, pos: u64) -> Result<Vec<u8>, FsError> {
        read_block(&*self.dev, self.log_block_addr(pos))
    }

    fn write_log(&self, pos: u64, data: &[u8]) -> Result<(), FsError> {
        write_block(&*self.dev, self.log_block_addr(pos), data)
    }

    /// Write an empty journal superblock into the reserved region (format).
    pub fn format(dev: &dyn BlockDevice, area: JournalArea) -> Result<(), FsError> {
        let js = JournalSuper {
            magic: JNL_MAGIC,
            head: 0,
            tail: 0,
            next_seq: 1,
            log_blocks: area.log_blocks,
            fs_blocks: area.fs_blocks,
            checksum: 0,
        };
        let mut buf = vec![0u8; BS];
        store_journal_super(&mut buf, &js);
        write_block(dev, area.super_block, &buf)
    }

    /// Open the journal, reading and validating its superblock.
    ///
    /// The persisted CRC32 (written by `store_journal_super`) is verified: a
    /// torn/garbage journal superblock used to pass as `head`/`tail` state and
    /// made replay start from arbitrary log positions.
    pub fn open(dev: Arc<dyn BlockDevice>, area: JournalArea) -> Result<Self, FsError> {
        let buf = read_block(&*dev, area.super_block)?;
        let js = load_journal_super(&buf)?;
        if js.magic != JNL_MAGIC {
            return Err(FsError::BadJournal);
        }
        if js.log_blocks == 0 {
            return Err(FsError::BadJournal);
        }
        Ok(Journal {
            dev,
            super_block: area.super_block,
            log_start: area.super_block + 1,
            log_blocks: js.log_blocks,
            fs_blocks: js.fs_blocks,
            head: js.head,
            tail: js.tail,
            next_seq: js.next_seq,
        })
    }

    fn persist_super(&self) -> Result<(), FsError> {
        let js = JournalSuper {
            magic: JNL_MAGIC,
            head: self.head,
            tail: self.tail,
            next_seq: self.next_seq,
            log_blocks: self.log_blocks,
            fs_blocks: self.fs_blocks,
            checksum: 0,
        };
        let mut buf = vec![0u8; BS];
        store_journal_super(&mut buf, &js);
        write_block(&*self.dev, self.super_block, &buf)
    }

    /// Begin a new (empty) transaction.
    pub fn begin(&self) -> Txn {
        Txn {
            records: Vec::new(),
        }
    }

    /// Append a block write to a transaction. `contents` is padded/truncated to
    /// exactly `BS` bytes; `final_block` is the destination ext2 block index.
    pub fn log_block(&self, txn: &mut Txn, final_block: u64, contents: &[u8]) {
        let mut block = vec![0u8; BS];
        let n = core::cmp::min(contents.len(), BS);
        block[..n].copy_from_slice(&contents[..n]);
        // If the same target is logged twice, the later write wins.
        if let Some(existing) = txn.records.iter_mut().find(|(t, _)| *t == final_block) {
            existing.1 = block;
        } else {
            txn.records.push((final_block, block));
        }
    }

    /// Commit a transaction: write the descriptor + data + commit record to the
    /// log, checkpoint the data to its final ext2 locations, then advance the
    /// head and persist the journal superblock.
    ///
    /// Checkpointing is synchronous, so everything logged in `[tail, head)` is
    /// already on its final location when the head advances: when the ring
    /// wraps into the tail, the live records are dropped (`tail = head`) and
    /// the free-space check below only guards the current transaction.
    pub fn commit(&mut self, txn: Txn) -> Result<(), FsError> {
        let count = txn.records.len();
        if count == 0 {
            return Ok(());
        }
        if count > JDESC_MAX_TARGETS {
            // A single descriptor cannot describe more than this many blocks.
            return Err(FsError::OutOfSpace);
        }
        // Need count + 2 (descriptor + data + commit) log slots, and keep at
        // least one spare slot so head == tail always means "empty" (recover
        // relies on that disambiguation).
        if (count as u64) + 3 > self.log_blocks {
            return Err(FsError::OutOfSpace);
        }
        let used = if self.head >= self.tail {
            self.head - self.tail
        } else {
            self.head + self.log_blocks - self.tail
        };
        let free = self.log_blocks - used;
        if (count as u64) + 2 > free {
            // Everything between tail and head is already checkpointed
            // (synchronously, before the head advances); reclaim the whole
            // log for this transaction instead of wrapping straight over
            // live records.
            self.tail = self.head;
        }

        // Validate every target lies inside the ext2 region.
        for (target, contents) in &txn.records {
            if *target >= self.fs_blocks || contents.len() != BS {
                return Err(FsError::Corrupt);
            }
        }

        let seq = self.next_seq;
        let desc_pos = self.head;

        // 1. Descriptor block.
        let mut desc = JournalDescriptor {
            magic: JDES_MAGIC,
            seq,
            count: count as u32,
            targets: [0u64; JDESC_MAX_TARGETS],
        };
        for (i, (target, _)) in txn.records.iter().enumerate() {
            desc.targets[i] = *target;
        }
        let mut desc_buf = vec![0u8; BS];
        store_descriptor(&mut desc_buf, &desc);
        self.write_log(desc_pos, &desc_buf)?;

        // 2. Data records into the log (NOT yet to final locations).
        for (i, (_target, contents)) in txn.records.iter().enumerate() {
            self.write_log(desc_pos + 1 + i as u64, contents)?;
        }

        // 3. Commit record: CRC32 over all data blocks.
        let data_refs: Vec<&[u8]> = txn.records.iter().map(|(_, c)| c.as_slice()).collect();
        let checksum = structs::crc32_slices(&data_refs);
        let cmt = JournalCommit {
            magic: JCMT_MAGIC,
            seq,
            data_checksum: checksum,
            _pad: 0,
        };
        let mut cmt_buf = vec![0u8; BS];
        store_commit(&mut cmt_buf, &cmt);
        self.write_log(desc_pos + 1 + count as u64, &cmt_buf)?;

        // === Transaction is now atomic: the commit record makes it replayable.
        //
        // ...but only once it is *durable*. On a device with a volatile write
        // cache the log records above are acknowledged-but-not-stored, so flush
        // before checkpointing: a crash from here on can still replay the
        // committed transaction from the log (issue #15).
        self.dev.flush().map_err(|_| FsError::IoError)?;

        // 4. Checkpoint: copy logged blocks to their final ext2 locations.
        for (target, contents) in &txn.records {
            write_block(&*self.dev, *target, contents)?;
        }

        // The checkpointed images must also be durable *before* the head advance
        // below declares the log empty: otherwise a crash could leave a log that
        // recovery treats as replayed, and a superblock that says so, with the
        // data still only in the device's cache — a committed transaction lost.
        self.dev.flush().map_err(|_| FsError::IoError)?;

        // 5. Advance head and persist the journal superblock.
        self.head = (desc_pos + count as u64 + 2) % self.log_blocks;
        self.next_seq = seq + 1;
        self.persist_super()?;
        Ok(())
    }

    /// Replay committed transactions on mount.
    ///
    /// Scans the log from `tail`: a transaction is replayed iff it is committed
    /// (valid descriptor + commit record, matching seq + CRC32) **and** its
    /// sequence number is one the persisted journal superblock has not yet
    /// acknowledged. Replay applies each data block to its final location
    /// (idempotent block overwrites); scanning stops at the first uncommitted,
    /// corrupt or out-of-order transaction and leaves the rest alone. The log is
    /// then reset to empty.
    ///
    /// Liveness is decided by `seq` against the persisted `next_seq`, **not** by
    /// `head == tail` (issue #35). `commit` persists the head advance *after* the
    /// checkpoint, so a crash between the commit record and that write leaves a
    /// committed transaction in the log while the on-disk head/tail still
    /// describe the previous state — `head == tail` there means "the superblock
    /// has not moved", not "the log is empty", and returning early silently lost
    /// every block of that transaction that had not been checkpointed yet.
    ///
    /// The sequence rule keeps the protection that guard was added for: records
    /// with `seq < next_seq` were acknowledged by an earlier superblock, so their
    /// blocks are already checkpointed and they are **skipped**, never replayed
    /// (in a wrapped ring the stale records after `head` carry older sequence
    /// numbers, so old images can never be re-applied over newer data). Only
    /// records with `seq == next_seq` chaining upwards are replayed.
    pub fn recover(&mut self) -> Result<u32, FsError> {
        // open() already validated JNL_MAGIC.
        let mut pos = self.tail;
        let mut replayed: u32 = 0;
        let mut expected = self.next_seq;
        let mut last_seq: Option<u64> = None;
        // Inspect at most one lap of the ring, so corrupt data cannot make the
        // scan spin.
        let mut budget = self.log_blocks;

        while budget > 0 {
            let desc_buf = self.read_log(pos)?;
            let desc = load_descriptor(&desc_buf);
            if desc.magic != JDES_MAGIC {
                break; // nothing valid here
            }
            let count = desc.count as u64;
            if count == 0 || count as usize > JDESC_MAX_TARGETS || count + 2 > self.log_blocks {
                break; // structurally impossible descriptor
            }

            if last_seq.is_none() && desc.seq < expected {
                // Acknowledged by an earlier superblock: already checkpointed.
                // Step over it — replaying it is what would re-apply an old
                // image over newer data.
                let step = count + 2;
                pos = (pos + step) % self.log_blocks;
                budget = budget.saturating_sub(step);
                continue;
            }
            if desc.seq != expected {
                break; // gap / duplicate / out-of-order: keep the rest untouched
            }

            // Read back the data blocks and verify the commit record.
            let mut data_blocks: Vec<Vec<u8>> = Vec::with_capacity(count as usize);
            for i in 0..count {
                data_blocks.push(self.read_log(pos + 1 + i)?);
            }
            let commit_pos = pos + 1 + count;
            let cmt_buf = self.read_log(commit_pos)?;
            let cmt = load_commit(&cmt_buf);

            let data_refs: Vec<&[u8]> = data_blocks.iter().map(|b| b.as_slice()).collect();
            let checksum = structs::crc32_slices(&data_refs);

            let committed =
                cmt.magic == JCMT_MAGIC && cmt.seq == desc.seq && cmt.data_checksum == checksum;
            if !committed {
                break; // stop at first incomplete/corrupt txn; discard rest
            }

            // Re-apply (idempotent) to final locations.
            for i in 0..count as usize {
                let target = desc.targets[i];
                if target >= self.fs_blocks {
                    return Err(FsError::Corrupt);
                }
                write_block(&*self.dev, target, &data_blocks[i])?;
            }

            replayed += 1;
            last_seq = Some(desc.seq);
            expected = desc.seq + 1;
            pos = (commit_pos + 1) % self.log_blocks;
            budget = budget.saturating_sub(count + 2);
        }

        // After replay everything live is checkpointed; empty the log. The
        // replayed block images must reach stable storage before the emptied log
        // is persisted, or a second crash could lose a transaction this recovery
        // just declared replayed (issue #15).
        self.tail = pos;
        self.head = pos;
        if let Some(seq) = last_seq {
            self.next_seq = core::cmp::max(self.next_seq, seq + 1);
            self.dev.flush().map_err(|_| FsError::IoError)?;
        }
        self.persist_super()?;
        Ok(replayed)
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }
}

// ─── journal struct (de)serialization ────────────────────────────────────────

fn store_journal_super(buf: &mut [u8], js: &JournalSuper) {
    // Serialize via the repr(C) layout, then patch the trailing CRC32 to cover
    // every preceding byte of the struct.
    unsafe { structs::write_struct(buf, js) };
    let size = core::mem::size_of::<JournalSuper>();
    let csum_off = size - 4;
    let checksum = structs::crc32(&buf[..csum_off]);
    structs::write_u32(buf, csum_off, checksum);
}

/// Read a journal superblock and verify its CRC32 (the trailing `checksum`
/// field covers every preceding byte of the struct).
fn load_journal_super(buf: &[u8]) -> Result<JournalSuper, FsError> {
    let js: JournalSuper = unsafe { structs::read_struct(buf) };
    let size = core::mem::size_of::<JournalSuper>();
    let stored = structs::read_u32(buf, size - 4);
    if structs::crc32(&buf[..size - 4]) != stored {
        return Err(FsError::BadJournal);
    }
    Ok(js)
}

fn store_descriptor(buf: &mut [u8], desc: &JournalDescriptor) {
    unsafe { structs::write_struct(buf, desc) };
}

fn load_descriptor(buf: &[u8]) -> JournalDescriptor {
    unsafe { structs::read_struct(buf) }
}

fn store_commit(buf: &mut [u8], cmt: &JournalCommit) {
    unsafe { structs::write_struct(buf, cmt) };
}

fn load_commit(buf: &[u8]) -> JournalCommit {
    unsafe { structs::read_struct(buf) }
}
