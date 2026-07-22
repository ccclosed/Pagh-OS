//! Pure Debian binary-repository `Packages` index parser and lookup index
//! (the read side of an `apt install <name>` feature).
//!
//! A Debian binary repository serves a `Packages` text file: one *stanza* per
//! package in an RFC822-ish format — lines are `Key: value`, a *continuation*
//! line begins with a space (or tab) and extends the previous field's value, and
//! a blank line separates stanzas. This module turns that byte buffer into a
//! queryable [`PackageIndex`].
//!
//! Everything here is `core` + `alloc` only — no hardware, no globals, no
//! networking — so the same source compiles into both the kernel
//! (`crate::pkg::apt_index`) and the `host-tests` crate via a `#[path]` include,
//! letting the unit/property tests in P30 exercise it on the host (R11.6).
//!
//! The parser is deliberately **robust and panic-free**: it tolerates CRLF or LF
//! line endings, continuation lines, missing optional fields (`Depends`/
//! `Provides` empty, `Size` 0), and unknown keys (ignored). Input is decoded
//! UTF-8-lossily so malformed bytes never panic. A stanza that carries no
//! `Package` key is skipped.
#![allow(dead_code)]

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::mem::size_of;

/// A range into a byte arena. Resolving it yields `&arena[off..off+len]`, which
/// is always valid UTF-8 because only valid UTF-8 is ever interned (the
/// `Packages` parser decodes its input UTF-8-lossily upstream, so every string
/// handed to [`Arena::intern`] is already well-formed).
///
/// # Invariants
///
/// - **`u32` offsets are sufficient.** The full Debian `main`/`amd64` arena is on
///   the order of tens of MiB — far below the `u32::MAX` (4 GiB) addressable
///   range, and well under the project's 128 MiB index-footprint ceiling. The
///   streaming pipeline caps the decompressed index long before an arena could
///   approach 4 GiB, so `off`/`len` never overflow in practice.
/// - **Only valid UTF-8 is ever interned**, so [`Arena::resolve`] is total: it
///   decodes defensively with `from_utf8(...).unwrap_or("")` and therefore never
///   panics, even on the impossible malformed case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrRef {
    /// Byte offset of the string's first byte within the arena.
    pub off: u32,
    /// Length of the string in bytes.
    pub len: u32,
}

impl StrRef {
    /// The canonical empty string: an absent/optional field. Independent of arena
    /// contents — `{ off: 0, len: 0 }` always resolves to `""` because its length
    /// is zero.
    pub const EMPTY: StrRef = StrRef { off: 0, len: 0 };
}

/// A growable byte arena: a single `Vec<u8>` into which every package string is
/// appended exactly once and referenced thereafter by a small [`StrRef`]
/// `{ off, len }` range. This replaces hundreds of thousands of per-field
/// `String` allocations with a handful of arena growths, so allocation work is
/// linear and the first-fit allocator's free-list walk does not degrade.
///
/// Tasks 2.1/3.2 embed this primitive into `PackageIndex` / `PackageIndexBuilder`;
/// it is introduced here standalone so it compiles and is unit-usable now. It is
/// pure `core` + `alloc` (no kernel deps), so it stays host-includable like the
/// rest of this module.
#[derive(Clone, Debug, Default)]
pub struct Arena {
    bytes: Vec<u8>,
}

impl Arena {
    /// Create an empty arena.
    pub fn new() -> Self {
        Arena { bytes: Vec::new() }
    }

    /// Number of bytes currently held by the arena.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// True if the arena holds no bytes.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Intern `s`: append its bytes to the arena and return the [`StrRef`] range
    /// that addresses them. The empty string is interned as [`StrRef::EMPTY`]
    /// without growing the arena (an empty range needs no backing bytes).
    ///
    /// # Panics
    ///
    /// Does not panic under the documented invariants (the arena stays far below
    /// 4 GiB), so the `off`/`len` `u32` casts never truncate.
    pub fn intern(&mut self, s: &str) -> StrRef {
        if s.is_empty() {
            return StrRef::EMPTY;
        }
        let off = self.bytes.len() as u32;
        self.bytes.extend_from_slice(s.as_bytes());
        let len = s.len() as u32;
        StrRef { off, len }
    }

    /// Resolve a [`StrRef`] back to its `&str`. Total and panic-free: an
    /// out-of-bounds range or (impossible) invalid UTF-8 resolves to `""` rather
    /// than panicking, so lookups over a built index can never fault.
    pub fn resolve(&self, r: StrRef) -> &str {
        let start = r.off as usize;
        let end = start.saturating_add(r.len as usize);
        match self.bytes.get(start..end) {
            Some(slice) => core::str::from_utf8(slice).unwrap_or(""),
            None => "",
        }
    }
}

/// One AND-group of a dependency expression. Within a group the alternatives are
/// OR-ed (the `|` operator in a `Depends` field); satisfying any one alternative
/// satisfies the whole group. Each alternative here is a *bare* package name —
/// the version constraint (`(...)`) and any arch qualifier (`:any`/`:native`)
/// have been stripped by [`parse_depends`].
#[derive(Clone, Debug)]
pub struct DepGroup {
    /// Bare package names, OR-ed together. Never empty for a group produced by
    /// [`parse_depends`] (empty groups are dropped).
    pub alts: Vec<String>,
}

/// A single parsed package stanza from a `Packages` file.
#[derive(Clone, Debug)]
pub struct PkgRecord {
    /// `Package:` — the package name (required; a stanza without it is skipped).
    pub package: String,
    /// `Version:` — the version string verbatim (empty if absent).
    pub version: String,
    /// `Architecture:` — e.g. `amd64`, `all` (empty if absent).
    pub arch: String,
    /// `Filename:` — pool-relative path to the `.deb` (empty if absent).
    pub filename: String,
    /// Merged `Pre-Depends:` then `Depends:` AND-groups, in that order. See the
    /// module docs: pre-depends are folded into `depends` since the resolver
    /// treats them identically (it ignores ordering *within* a package's deps).
    pub depends: Vec<DepGroup>,
    /// `Provides:` — virtual package names this package provides (bare names).
    pub provides: Vec<String>,
    /// `Size:` — download size in bytes (0 if absent or unparseable).
    pub size: u64,
}

/// Strip a single dependency atom down to its bare package name by removing any
/// version constraint `(...)` and any `:qualifier` (e.g. `:any`, `:native`),
/// then trimming surrounding whitespace.
///
/// Examples: `libc6 (>= 2.34)` -> `libc6`; `libfoo:any` -> `libfoo`;
/// `zlib1g (>= 1:1.2)` -> `zlib1g`.
fn strip_atom(atom: &str) -> &str {
    // Cut everything from the first '(' (version constraint).
    let no_constraint = match atom.find('(') {
        Some(i) => &atom[..i],
        None => atom,
    };
    let trimmed = no_constraint.trim();
    // Cut a trailing arch qualifier after ':'.
    let no_qual = match trimmed.find(':') {
        Some(i) => &trimmed[..i],
        None => trimmed,
    };
    no_qual.trim()
}

/// Parse a `Depends:`/`Pre-Depends:` field value into AND-groups of OR-ed bare
/// names. Splits on `,` (AND), then `|` (OR), strips `(...)` constraints and
/// `:qualifier` arch tags, trims whitespace, and drops empty atoms and empty
/// groups. An empty/whitespace value yields an empty `Vec`.
pub fn parse_depends(value: &str) -> Vec<DepGroup> {
    let mut groups = Vec::new();
    for and_part in value.split(',') {
        let mut alts = Vec::new();
        for or_part in and_part.split('|') {
            let name = strip_atom(or_part);
            if !name.is_empty() {
                alts.push(name.to_string());
            }
        }
        if !alts.is_empty() {
            groups.push(DepGroup { alts });
        }
    }
    groups
}

/// Parse a `Provides:` field value into bare virtual package names. Commas
/// separate provided names; version constraints and qualifiers are stripped.
fn parse_provides(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in value.split(',') {
        let name = strip_atom(part);
        if !name.is_empty() {
            out.push(name.to_string());
        }
    }
    out
}

/// Which known field the parser's last `Key:` line selected. Continuation lines
/// are routed back to that field's slot. [`CurField::None`] marks either "no key
/// seen yet" or "last key was unknown" — in both cases continuation lines are
/// ignored (an unknown key's continuations were appended to a field nothing ever
/// reads, so dropping them is observably identical).
#[derive(Clone, Copy, PartialEq, Eq)]
enum CurField {
    None,
    Package,
    Version,
    Arch,
    Filename,
    Depends,
    PreDepends,
    Provides,
    Size,
}

/// Internal accumulator for a stanza being built line-by-line.
///
/// This replaces the old `BTreeMap<String, String>` + per-`Key:` `String`
/// allocations with one reusable `String` slot per field the parser actually
/// reads. The slots are *cleared* (not reallocated) between stanzas via
/// [`clear`](Self::clear), so the per-stanza heap churn that degraded the kernel
/// allocator (tens of thousands of small alloc/free per index) is eliminated: the
/// eight slot allocations are reused across every stanza in the file.
///
/// Parse semantics are byte-for-byte identical to the old map-backed builder:
/// only the eight keys below are read by `build_record`/`StanzaView`, a repeated
/// key overwrites (last-wins, matching `BTreeMap::insert`), an unknown key still
/// marks the stanza non-empty (matching the old `fields.is_empty()` flipping to
/// false on the first inserted key, known or not), and continuation lines join
/// with a single space.
#[derive(Default)]
struct StanzaBuilder {
    package: String,
    version: String,
    arch: String,
    filename: String,
    depends: String,
    pre_depends: String,
    provides: String,
    size: String,
    /// Which known field the last `Key:` line selected (for continuation lines);
    /// [`CurField::None`] for an unknown key (its continuations are ignored).
    cur: CurField,
    /// Whether any key line (known or unknown) set a field this stanza. Drives
    /// [`is_empty`](Self::is_empty), mirroring the old `fields.is_empty()`.
    any: bool,
}

impl Default for CurField {
    fn default() -> Self {
        CurField::None
    }
}

impl StanzaBuilder {
    fn new() -> Self {
        Self::default()
    }

    /// Clear every slot for reuse on the next stanza. `String::clear` retains the
    /// backing capacity — this capacity reuse is the entire point of the rework:
    /// no per-stanza allocation/free of the field strings.
    fn clear(&mut self) {
        self.package.clear();
        self.version.clear();
        self.arch.clear();
        self.filename.clear();
        self.depends.clear();
        self.pre_depends.clear();
        self.provides.clear();
        self.size.clear();
        self.cur = CurField::None;
        self.any = false;
    }

    fn is_empty(&self) -> bool {
        !self.any
    }

    /// True if this stanza carries a non-empty `Package` key. Mirrors the guard
    /// in [`build_record`](Self::build_record)/[`PkgRecord`] emission: a stanza
    /// without a `Package` is skipped, so the streaming view sink uses this to
    /// decide whether to emit a [`StanzaView`] at all.
    fn has_package(&self) -> bool {
        !self.package.trim().is_empty()
    }

    /// Map a known field selector to its slot, or `None` for an unknown key.
    fn slot_mut(&mut self, field: CurField) -> Option<&mut String> {
        match field {
            CurField::Package => Some(&mut self.package),
            CurField::Version => Some(&mut self.version),
            CurField::Arch => Some(&mut self.arch),
            CurField::Filename => Some(&mut self.filename),
            CurField::Depends => Some(&mut self.depends),
            CurField::PreDepends => Some(&mut self.pre_depends),
            CurField::Provides => Some(&mut self.provides),
            CurField::Size => Some(&mut self.size),
            CurField::None => None,
        }
    }

    /// Case-sensitive match of a trimmed key against the eight known field names.
    fn classify(key: &str) -> CurField {
        match key {
            "Package" => CurField::Package,
            "Version" => CurField::Version,
            "Architecture" => CurField::Arch,
            "Filename" => CurField::Filename,
            "Depends" => CurField::Depends,
            "Pre-Depends" => CurField::PreDepends,
            "Provides" => CurField::Provides,
            "Size" => CurField::Size,
            _ => CurField::None,
        }
    }

    /// Feed one logical line (already stripped of its trailing `\r`/`\n`).
    fn feed(&mut self, line: &str) {
        // Continuation line: starts with a space or tab. Append to the current
        // field's value (with a single joining space), if any.
        if line.starts_with(' ') || line.starts_with('\t') {
            let cont = line.trim();
            if let Some(v) = self.slot_mut(self.cur) {
                if !cont.is_empty() {
                    if !v.is_empty() {
                        v.push(' ');
                    }
                    v.push_str(cont);
                }
            }
            // `cur == None` (no key yet, or last key unknown): ignore.
            return;
        }

        // A normal `Key: value` line.
        if let Some(colon) = line.find(':') {
            let key = line[..colon].trim();
            let value = line[colon + 1..].trim();
            if !key.is_empty() {
                // Any non-empty key (known or unknown) makes the stanza non-empty,
                // matching the old `BTreeMap` insert flipping `fields.is_empty()`.
                self.any = true;
                let field = Self::classify(key);
                self.cur = field;
                if let Some(slot) = self.slot_mut(field) {
                    // Repeated key overwrites (last-wins, matching map insert).
                    slot.clear();
                    slot.push_str(value);
                }
                // Unknown key: `cur` is now `None`, so its continuations are
                // ignored (the old code appended them to an unread field).
            }
        }
        // Lines without a colon and not a continuation are ignored.
    }

    /// Build a [`PkgRecord`] from the accumulated fields by borrowing `self`, or
    /// `None` if the stanza has no `Package`. Borrowing (rather than consuming)
    /// lets the same accumulated stanza back either the owned `PkgRecord`
    /// collector path or a borrowed [`StanzaView`] without being torn down first.
    fn build_record(&self) -> Option<PkgRecord> {
        let package = self.package.trim().to_string();
        if package.is_empty() {
            return None;
        }

        let version = self.version.trim().to_string();
        let arch = self.arch.trim().to_string();
        let filename = self.filename.trim().to_string();

        // Merge Pre-Depends (first) and Depends.
        let mut depends = Vec::new();
        if !self.pre_depends.is_empty() {
            depends.extend(parse_depends(&self.pre_depends));
        }
        if !self.depends.is_empty() {
            depends.extend(parse_depends(&self.depends));
        }

        let provides = if self.provides.is_empty() {
            Vec::new()
        } else {
            parse_provides(&self.provides)
        };

        let size = self.size.trim().parse::<u64>().unwrap_or(0);

        Some(PkgRecord {
            package,
            version,
            arch,
            filename,
            depends,
            provides,
            size,
        })
    }
}

/// One AND-group within a borrowed [`StanzaView`], yielding its OR-ed bare
/// package names without allocating. Backed by the raw `Depends`/`Pre-Depends`
/// field text held in the parser's transient stanza buffers; valid only for the
/// duration of the [`RecordSink::emit`] call that produced the view.
///
/// `and_part` is one comma-delimited slice of the original field value (one
/// AND-group). [`alts`](Self::alts) lazily splits it on `|` and strips each atom
/// down to its bare name, exactly as [`parse_depends`] does — but yielding `&str`
/// slices instead of building a `Vec<String>`.
pub struct DepGroupView<'a> {
    /// One AND-group's raw text (between two `,`), borrowed from the field value.
    and_part: &'a str,
}

impl<'a> DepGroupView<'a> {
    /// Lazily yield the bare OR-alternative names of this group. Empty atoms are
    /// dropped, matching [`parse_depends`].
    pub fn alts(&self) -> impl Iterator<Item = &'a str> {
        self.and_part
            .split('|')
            .map(strip_atom)
            .filter(|s| !s.is_empty())
    }

    /// True if the group has no non-empty alternative (so it should be dropped,
    /// mirroring [`parse_depends`] discarding empty groups).
    fn is_empty(&self) -> bool {
        self.alts().next().is_none()
    }
}

/// A borrowed, allocation-free view of one completed `Packages` stanza, handed to
/// a [`RecordSink`] as the [`StanzaParser`] finishes each stanza.
///
/// All accessors return `&str` slices (or lazy iterators of `&str`) that borrow
/// directly from the parser's transient per-stanza buffers, so no owned
/// `String`/`Vec` is materialized on this path — the [`PackageIndexBuilder`] sink
/// interns straight from these borrows into its arena. The view is valid **only**
/// for the duration of the [`RecordSink::emit`] call; the parser reuses/clears the
/// backing buffers immediately afterwards.
pub struct StanzaView<'a> {
    builder: &'a StanzaBuilder,
}

impl<'a> StanzaView<'a> {
    /// Resolve a field to its trimmed value, or `""` if absent. Mirrors the
    /// trimming [`StanzaBuilder::build_record`] applies, but borrows instead of
    /// allocating.
    fn field(&self, key: &str) -> &'a str {
        // `self.builder` is a `&'a StanzaBuilder`, so the borrow handed back lives
        // for `'a` (the stanza's transient lifetime), not merely for `&self`.
        let builder: &'a StanzaBuilder = self.builder;
        let slot = match StanzaBuilder::classify(key) {
            CurField::Package => &builder.package,
            CurField::Version => &builder.version,
            CurField::Arch => &builder.arch,
            CurField::Filename => &builder.filename,
            CurField::Depends => &builder.depends,
            CurField::PreDepends => &builder.pre_depends,
            CurField::Provides => &builder.provides,
            CurField::Size => &builder.size,
            // Unknown key: absent, like the old `fields.get(key) == None`.
            CurField::None => return "",
        };
        slot.trim()
    }

    /// `Package:` — the package name (non-empty for any emitted stanza).
    pub fn package(&self) -> &'a str {
        self.field("Package")
    }

    /// `Version:` verbatim (empty if absent).
    pub fn version(&self) -> &'a str {
        self.field("Version")
    }

    /// `Architecture:` (empty if absent).
    pub fn arch(&self) -> &'a str {
        self.field("Architecture")
    }

    /// `Filename:` pool-relative path (empty if absent).
    pub fn filename(&self) -> &'a str {
        self.field("Filename")
    }

    /// `Size:` in bytes (0 if absent or unparseable), matching [`build_record`].
    pub fn size(&self) -> u64 {
        self.field("Size").parse::<u64>().unwrap_or(0)
    }

    /// Yield the merged `Pre-Depends:` then `Depends:` AND-groups (in that order,
    /// matching [`build_record`]), each as a [`DepGroupView`] over borrowed text.
    /// Empty groups are dropped, exactly as [`parse_depends`] does.
    pub fn depends(&self) -> impl Iterator<Item = DepGroupView<'a>> {
        let pre = self.field("Pre-Depends");
        let dep = self.field("Depends");
        pre.split(',')
            .chain(dep.split(','))
            .map(|and_part| DepGroupView { and_part })
            .filter(|g| !g.is_empty())
    }

    /// Yield the bare `Provides:` virtual names, matching [`parse_provides`].
    pub fn provides(&self) -> impl Iterator<Item = &'a str> {
        self.field("Provides")
            .split(',')
            .map(strip_atom)
            .filter(|s| !s.is_empty())
    }
}

/// A sink that receives each completed stanza as a borrowed [`StanzaView`]. The
/// streaming [`StanzaParser`] drives a sink via
/// [`push_view`](StanzaParser::push_view)/[`finish_view`](StanzaParser::finish_view),
/// interning directly from the view with no transient owned record.
/// [`PackageIndexBuilder`] is the production implementation (it interns into its
/// arena); the owned `Vec<PkgRecord>` path keeps using the original
/// [`push`](StanzaParser::push)/[`finish`](StanzaParser::finish) methods.
pub trait RecordSink {
    /// Consume one completed stanza. The `view` borrows the parser's transient
    /// buffers and is valid only for the duration of this call.
    fn emit(&mut self, view: &StanzaView<'_>);
}

/// What to do with a completed [`StanzaBuilder`] when the parser flushes a stanza.
/// This private seam lets the byte-exact chunk-splitting logic in
/// [`StanzaParser`] be written once and reused by both the owned `PkgRecord`
/// collector path and the borrowed [`StanzaView`]/[`RecordSink`] path.
trait StanzaConsumer {
    fn consume(&mut self, builder: &StanzaBuilder);
}

/// Adapts a `FnMut(PkgRecord)` emitter into a [`StanzaConsumer`] (the original
/// owned-record path; preserves [`parse_packages`] behavior verbatim).
struct OwnedSink<'f, F: FnMut(PkgRecord)> {
    emit: &'f mut F,
}

impl<F: FnMut(PkgRecord)> StanzaConsumer for OwnedSink<'_, F> {
    fn consume(&mut self, builder: &StanzaBuilder) {
        if let Some(rec) = builder.build_record() {
            (self.emit)(rec);
        }
    }
}

/// Adapts a [`RecordSink`] into a [`StanzaConsumer`] (the borrowed-view path).
/// Skips stanzas with no `Package`, matching the owned path's
/// [`StanzaBuilder::build_record`] `None` case.
struct ViewSink<'s, S: RecordSink> {
    sink: &'s mut S,
}

impl<S: RecordSink> StanzaConsumer for ViewSink<'_, S> {
    fn consume(&mut self, builder: &StanzaBuilder) {
        if builder.has_package() {
            let view = StanzaView { builder };
            self.sink.emit(&view);
        }
    }
}


/// An **incremental**, streaming `Packages` stanza parser.
///
/// This is the bounded-memory core of the apt-index pipeline (the fix for
/// `apt update` overrunning the heap): rather than decompressing the whole
/// `Packages` file into one giant `Vec` and parsing that buffer, the caller
/// decompresses the body in fixed-size chunks and feeds each chunk to
/// [`push`](Self::push). The parser keeps only a small **carry buffer** holding
/// the bytes of the current partial line, plus the [`StanzaBuilder`] for the
/// stanza in flight; it emits one [`PkgRecord`] per completed stanza through the
/// caller's `emit` closure and drops nothing else. Resident memory is therefore
/// `O(longest line)` + `O(one stanza)` regardless of the total index size — the
/// decompressed chunks are consumed and discarded as they arrive.
///
/// The byte-exact semantics match the whole-buffer [`parse_packages`]: lines are
/// split on `\n` (`0x0A`), a single trailing `\r` is stripped (CRLF tolerance),
/// each line is decoded UTF-8-lossily, a blank line flushes the current stanza,
/// and a stanza without a `Package` key is skipped. Splitting on the `0x0A` byte
/// *before* the lossy UTF-8 decode is equivalent to decoding the whole buffer and
/// splitting on the `'\n'` char, because `0x0A` can never be part of a multi-byte
/// UTF-8 sequence (continuation bytes are `0x80..=0xBF`), so a malformed sequence
/// never straddles a line boundary. This equivalence is what the P33 property
/// test pins down across arbitrary chunk splits.
pub struct StanzaParser {
    /// Bytes of the current line not yet terminated by a `\n`.
    carry: Vec<u8>,
    /// The stanza currently being accumulated line by line.
    builder: StanzaBuilder,
}

impl Default for StanzaParser {
    fn default() -> Self {
        Self::new()
    }
}

impl StanzaParser {
    /// Create an empty streaming parser.
    pub fn new() -> Self {
        StanzaParser {
            carry: Vec::new(),
            builder: StanzaBuilder::new(),
        }
    }

    /// Feed one decompressed chunk. Every line completed by a `\n` within
    /// `bytes` (joined with any carried prefix) is parsed immediately; trailing
    /// bytes after the last `\n` are carried for the next [`push`](Self::push) or
    /// [`finish`](Self::finish). Each completed stanza is delivered through `emit`.
    pub fn push(&mut self, bytes: &[u8], emit: &mut impl FnMut(PkgRecord)) {
        let mut consumer = OwnedSink { emit };
        self.push_with(bytes, &mut consumer);
    }

    /// Flush any trailing partial line and the final unterminated stanza. Mirrors
    /// the whole-buffer parser's handling of a file not ending in a blank line.
    pub fn finish(self, emit: &mut impl FnMut(PkgRecord)) {
        let mut consumer = OwnedSink { emit };
        self.finish_with(&mut consumer);
    }

    /// Streaming/borrowed-view counterpart of [`push`](Self::push): every
    /// completed stanza is delivered to `sink` as a borrowed [`StanzaView`]
    /// (no owned `PkgRecord` is materialized). The byte-exact chunk semantics are
    /// identical — both paths share [`push_with`](Self::push_with).
    pub fn push_view<S: RecordSink>(&mut self, bytes: &[u8], sink: &mut S) {
        let mut consumer = ViewSink { sink };
        self.push_with(bytes, &mut consumer);
    }

    /// Streaming/borrowed-view counterpart of [`finish`](Self::finish): flushes
    /// the trailing partial line and final stanza to `sink` as a [`StanzaView`].
    pub fn finish_view<S: RecordSink>(self, sink: &mut S) {
        let mut consumer = ViewSink { sink };
        self.finish_with(&mut consumer);
    }

    /// The shared, byte-exact chunk-splitting core. Generic over the
    /// [`StanzaConsumer`] so the owned and borrowed-view paths cannot drift apart.
    fn push_with<C: StanzaConsumer>(&mut self, bytes: &[u8], consumer: &mut C) {
        let mut start = 0;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\n' {
                if self.carry.is_empty() {
                    self.process_line(&bytes[start..i], consumer);
                } else {
                    self.carry.extend_from_slice(&bytes[start..i]);
                    let line = core::mem::take(&mut self.carry);
                    self.process_line(&line, consumer);
                }
                start = i + 1;
            }
            i += 1;
        }
        // Whatever follows the last newline becomes (part of) the next line.
        if start < bytes.len() {
            self.carry.extend_from_slice(&bytes[start..]);
        }
    }

    /// Shared finish core; see [`finish`](Self::finish)/[`finish_view`](Self::finish_view).
    fn finish_with<C: StanzaConsumer>(mut self, consumer: &mut C) {
        if !self.carry.is_empty() {
            let line = core::mem::take(&mut self.carry);
            self.process_line(&line, consumer);
        }
        if !self.builder.is_empty() {
            consumer.consume(&self.builder);
        }
    }

    /// Parse one logical line (the bytes between two `\n`, exclusive). Strips a
    /// trailing `\r`, decodes UTF-8-lossily, then either flushes the stanza on a
    /// blank line or feeds the line into the [`StanzaBuilder`].
    fn process_line<C: StanzaConsumer>(&mut self, raw_line: &[u8], consumer: &mut C) {
        // Strip a single trailing '\r' to tolerate CRLF.
        let line_bytes = match raw_line.last() {
            Some(&b'\r') => &raw_line[..raw_line.len() - 1],
            _ => raw_line,
        };
        let decoded = String::from_utf8_lossy(line_bytes);
        let line: &str = &decoded;

        if line.is_empty() {
            // Blank line: stanza separator. Flush the current stanza (if any).
            if !self.builder.is_empty() {
                consumer.consume(&self.builder);
                self.builder.clear();
            }
            return;
        }

        self.builder.feed(line);
    }
}

/// Parse a whole `Packages` byte buffer into the list of records it contains, in
/// file order. Robust and panic-free: handles CRLF or LF, continuation lines,
/// missing optional fields, and unknown keys; decodes UTF-8 lossily. Stanzas
/// without a `Package` key are skipped.
///
/// This is now a thin convenience wrapper over the streaming [`StanzaParser`]:
/// it pushes the entire buffer in one chunk and collects the emitted records, so
/// the whole-buffer and the chunked/streaming paths share one parser
/// implementation (and cannot drift apart). The kernel's `apt update` uses
/// [`StanzaParser`] directly to avoid ever holding the full decompressed index
/// resident; callers that already have the bytes in RAM (host tests, small
/// fixtures) keep using this.
pub fn parse_packages(text: &[u8]) -> Vec<PkgRecord> {
    let mut records = Vec::new();
    let mut parser = StanzaParser::new();
    parser.push(text, &mut |rec| records.push(rec));
    parser.finish(&mut |rec| records.push(rec));
    records
}

/// A single package record stored in the compact arena representation. Every
/// field is fixed-size — strings are [`StrRef`] ranges into the index's byte
/// arena, dependency groups and provides names are ranges into the index's
/// `dep_groups`/`prov_refs` side tables — so a `Vec<PkgRecordC>` is one
/// contiguous heap block rather than 60k × several `String`/`Vec` allocations
/// (the whole reason for the arena rework; see the module/design docs).
#[derive(Clone, Copy, Debug)]
pub struct PkgRecordC {
    /// `Package:` — the package name (interned; required).
    pub package: StrRef,
    /// `Version:` — interned verbatim (empty `StrRef` if absent).
    pub version: StrRef,
    /// `Architecture:` — interned (empty `StrRef` if absent).
    pub arch: StrRef,
    /// `Filename:` — interned pool-relative path (empty `StrRef` if absent).
    pub filename: StrRef,
    /// `Size:` — download size in bytes (0 if absent or unparseable).
    pub size: u64,
    /// Start index of this record's dependency groups within `dep_groups`.
    pub dep_group_start: u32,
    /// Number of dependency groups belonging to this record.
    pub dep_group_len: u32,
    /// Start index of this record's provides names within `prov_refs`.
    pub prov_start: u32,
    /// Number of provides names belonging to this record.
    pub prov_len: u32,
}

/// One AND-group of a dependency expression in the compact representation: a
/// `[alt_start, alt_start + alt_len)` range into the index's `dep_alts` table,
/// whose entries are the OR-ed alternative names (interned [`StrRef`]s).
#[derive(Clone, Copy, Debug)]
pub struct DepGroupC {
    /// Start index of this group's alternatives within `dep_alts`.
    pub alt_start: u32,
    /// Number of OR-ed alternatives in this group.
    pub alt_len: u32,
}

/// A sorted `prov_sorted` entry: a provided (virtual) name and the record index
/// of the package that provides it. Only names **not** shadowed by a real
/// package are kept, and the first provider (lowest record index) wins.
#[derive(Clone, Copy, Debug)]
pub struct ProvEntry {
    /// The provided (virtual) name, interned into the arena.
    pub name: StrRef,
    /// Index into `records` of the providing package.
    pub record: u32,
}

/// A queryable index over a set of packages, stored in the **compact arena
/// representation**: one growable byte [`arena`](PackageIndex::arena) holds every
/// package string, and records / lookup tables hold only [`StrRef`] ranges and
/// small integers. This replaces the old `Vec<PkgRecord>` + two
/// `BTreeMap<String, usize>` design (hundreds of thousands of small allocations
/// at full-index scale) with a handful of contiguous `Vec`s.
///
/// The lookup tables are integer/range-keyed and binary-searched by comparing the
/// query `&str` against arena-resolved names, so no per-key `String` is
/// allocated:
///
/// - `name_sorted` — record indices sorted by their arena-resolved `Package`
///   name; on a duplicate-name run the largest index sorts last, so [`get`] can
///   return the last record for a name (matching the old `BTreeMap`
///   insert-overwrite "last-wins" semantics).
/// - `prov_sorted` — [`ProvEntry`]s sorted by arena-resolved provided name,
///   holding only names **not** shadowed by a real package, first-provider-wins.
///
/// Real names take precedence in [`get_provider`]: a virtual name is only
/// resolved if no real package owns it.
pub struct PackageIndex {
    /// All interned strings, concatenated. [`StrRef`]s address slices of this.
    arena: Vec<u8>,
    /// One entry per package, in file order.
    records: Vec<PkgRecordC>,
    /// Flattened dependency groups; each record owns a contiguous range.
    dep_groups: Vec<DepGroupC>,
    /// Flattened OR-alternative names across all dependency groups.
    dep_alts: Vec<StrRef>,
    /// Flattened `Provides:` names across all records.
    prov_refs: Vec<StrRef>,
    /// Record indices sorted by arena-resolved `Package` name; ties → larger
    /// index last (last-wins on duplicate names).
    name_sorted: Vec<u32>,
    /// Provided-name → providing record, sorted by arena-resolved name; only
    /// names not shadowed by a real package; first provider wins.
    prov_sorted: Vec<ProvEntry>,
}

impl PackageIndex {
    /// Build an index from parsed owned records (back-compat for the
    /// `Vec<PkgRecord>` collector path and small-fixture callers). Interns the
    /// owned records field-by-field into a [`PackageIndexBuilder`] and finishes
    /// it, producing a query-equivalent compact index. On duplicate `Package`
    /// names the last record wins; provides do not shadow a real package and the
    /// first provider wins among virtuals — identical to the old `BTreeMap`
    /// behavior.
    pub fn from_records(records: Vec<PkgRecord>) -> Self {
        let mut builder = PackageIndexBuilder::new();
        for rec in &records {
            let package = builder.intern(&rec.package);
            let version = builder.intern(&rec.version);
            let arch = builder.intern(&rec.arch);
            let filename = builder.intern(&rec.filename);
            let size = rec.size;

            let dep_group_start = builder.dep_groups.len() as u32;
            let mut dep_group_len: u32 = 0;
            for group in &rec.depends {
                let alt_start = builder.dep_alts.len() as u32;
                let mut alt_len: u32 = 0;
                for alt in &group.alts {
                    let r = builder.intern(alt);
                    builder.dep_alts.push(r);
                    alt_len += 1;
                }
                builder.dep_groups.push(DepGroupC { alt_start, alt_len });
                dep_group_len += 1;
            }

            let prov_start = builder.prov_refs.len() as u32;
            let mut prov_len: u32 = 0;
            for prov in &rec.provides {
                let r = builder.intern(prov);
                builder.prov_refs.push(r);
                prov_len += 1;
            }

            builder.records.push(PkgRecordC {
                package,
                version,
                arch,
                filename,
                size,
                dep_group_start,
                dep_group_len,
                prov_start,
                prov_len,
            });
        }
        builder.finish()
    }

    /// Build an index from a streaming [`PackageIndexBuilder`] (the kernel
    /// `apt update` path). Equivalent to [`PackageIndexBuilder::finish`].
    pub fn from_builder(b: PackageIndexBuilder) -> Self {
        b.finish()
    }

    /// Deterministic resident footprint: the sum of every component buffer's
    /// byte-length. This is the Resident_Index_Footprint accounting identity the
    /// design pins against the 128 MiB ceiling — it depends only on the buffer
    /// lengths, not allocator internals.
    pub fn footprint(&self) -> usize {
        self.arena.len()
            + self.records.len() * size_of::<PkgRecordC>()
            + self.dep_groups.len() * size_of::<DepGroupC>()
            + self.dep_alts.len() * size_of::<StrRef>()
            + self.prov_refs.len() * size_of::<StrRef>()
            + self.name_sorted.len() * size_of::<u32>()
            + self.prov_sorted.len() * size_of::<ProvEntry>()
    }

    /// Resolve a [`StrRef`] against this index's arena. Total and panic-free: an
    /// out-of-bounds range or (impossible) invalid UTF-8 resolves to `""` rather
    /// than panicking, mirroring [`Arena::resolve`].
    fn resolve(&self, r: StrRef) -> &str {
        let start = r.off as usize;
        let end = start.saturating_add(r.len as usize);
        match self.arena.get(start..end) {
            Some(slice) => core::str::from_utf8(slice).unwrap_or(""),
            None => "",
        }
    }

    /// Arena-resolved `Package` name of the record at index `ri`.
    fn record_name(&self, ri: u32) -> &str {
        self.resolve(self.records[ri as usize].package)
    }

    /// Build a borrowed [`PkgRef`] for the record at index `ri`.
    fn pkg_ref(&self, ri: u32) -> PkgRef<'_> {
        PkgRef {
            index: self,
            rec: &self.records[ri as usize],
        }
    }

    /// Number of package records held.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// True if the index holds no records.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Look up a record by its real `Package` name. On a duplicate-name run the
    /// last (largest record index) is returned, matching the old `BTreeMap`
    /// "last record wins" behavior.
    pub fn get(&self, name: &str) -> Option<PkgRef<'_>> {
        // `name_sorted` is sorted by (resolved name, index) ascending, so the
        // element just before the first name strictly greater than `name` is the
        // largest-index record whose name is <= `name`.
        let ub = self
            .name_sorted
            .partition_point(|&ri| self.record_name(ri) <= name);
        if ub == 0 {
            return None;
        }
        let cand = self.name_sorted[ub - 1];
        if self.record_name(cand) == name {
            Some(self.pkg_ref(cand))
        } else {
            None
        }
    }

    /// Iterate the package names held by the index, sorted and de-duplicated.
    /// Equal names are contiguous in `name_sorted`, so runs are collapsed. Used
    /// by `apt list` to enumerate available packages without exposing storage.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        let mut prev: Option<&str> = None;
        self.name_sorted.iter().filter_map(move |&ri| {
            let n = self.record_name(ri);
            if prev == Some(n) {
                None
            } else {
                prev = Some(n);
                Some(n)
            }
        })
    }

    /// Resolve `name` to a providing record: a real package of that name if one
    /// exists, otherwise any package that `Provides:` that (virtual) name. Real
    /// names take precedence (a virtual name is only consulted if no real package
    /// owns it).
    pub fn get_provider(&self, name: &str) -> Option<PkgRef<'_>> {
        if let Some(r) = self.get(name) {
            return Some(r);
        }
        let ub = self
            .prov_sorted
            .partition_point(|e| self.resolve(e.name) <= name);
        if ub == 0 {
            return None;
        }
        let e = self.prov_sorted[ub - 1];
        if self.resolve(e.name) == name {
            Some(self.pkg_ref(e.record))
        } else {
            None
        }
    }

    /// True if `name` is satisfiable by the index — a real package or a provided
    /// virtual name.
    pub fn contains(&self, name: &str) -> bool {
        self.get_provider(name).is_some()
    }
}

/// A borrowed, allocation-free view of one [`PkgRecordC`] in a [`PackageIndex`],
/// resolving the record's [`StrRef`] fields against the index's arena on demand.
/// This is the query-surface return type that replaces `&PkgRecord`.
#[derive(Clone, Copy)]
pub struct PkgRef<'a> {
    index: &'a PackageIndex,
    rec: &'a PkgRecordC,
}

impl<'a> PkgRef<'a> {
    /// `Package:` — the package name.
    pub fn package(&self) -> &'a str {
        let idx: &'a PackageIndex = self.index;
        idx.resolve(self.rec.package)
    }

    /// `Version:` (empty if absent).
    pub fn version(&self) -> &'a str {
        let idx: &'a PackageIndex = self.index;
        idx.resolve(self.rec.version)
    }

    /// `Architecture:` (empty if absent).
    pub fn arch(&self) -> &'a str {
        let idx: &'a PackageIndex = self.index;
        idx.resolve(self.rec.arch)
    }

    /// `Filename:` pool-relative path (empty if absent).
    pub fn filename(&self) -> &'a str {
        let idx: &'a PackageIndex = self.index;
        idx.resolve(self.rec.filename)
    }

    /// `Size:` in bytes (0 if absent or unparseable).
    pub fn size(&self) -> u64 {
        self.rec.size
    }

    /// Iterate the record's merged `Pre-Depends:`-then-`Depends:` AND-groups,
    /// each as a borrowed [`DepGroupRef`].
    pub fn depends(&self) -> impl Iterator<Item = DepGroupRef<'a>> {
        let idx: &'a PackageIndex = self.index;
        let start = self.rec.dep_group_start as usize;
        let len = self.rec.dep_group_len as usize;
        idx.dep_groups[start..start + len]
            .iter()
            .map(move |group| DepGroupRef { index: idx, group })
    }
}

/// A borrowed AND-group within a [`PkgRef`]: yields its OR-ed alternative names
/// (interned [`StrRef`]s resolved against the index arena) without allocating.
#[derive(Clone, Copy)]
pub struct DepGroupRef<'a> {
    index: &'a PackageIndex,
    group: &'a DepGroupC,
}

impl<'a> DepGroupRef<'a> {
    /// Yield the bare OR-alternative names of this group.
    pub fn alts(&self) -> impl Iterator<Item = &'a str> {
        let idx: &'a PackageIndex = self.index;
        let start = self.group.alt_start as usize;
        let len = self.group.alt_len as usize;
        idx.dep_alts[start..start + len]
            .iter()
            .map(move |&r| idx.resolve(r))
    }
}

/// A streaming builder that accumulates the compact arena representation as
/// stanzas arrive, then sorts the lookup tables once in [`finish`](Self::finish).
/// It is the production [`RecordSink`]-side target for the kernel `apt update`
/// path: each [`StanzaView`] is interned straight into the arena with no
/// transient owned [`PkgRecord`].
#[derive(Clone, Debug, Default)]
pub struct PackageIndexBuilder {
    arena: Vec<u8>,
    records: Vec<PkgRecordC>,
    dep_groups: Vec<DepGroupC>,
    dep_alts: Vec<StrRef>,
    prov_refs: Vec<StrRef>,
}

impl PackageIndexBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of package records accumulated so far. Used by the streaming
    /// `apt update` path for progress logging by package count.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// True if no package records have been accumulated yet.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Intern `s`: append its bytes to the arena and return the addressing
    /// [`StrRef`]. The empty string is [`StrRef::EMPTY`] and does not grow the
    /// arena (same invariant as [`Arena::intern`]).
    fn intern(&mut self, s: &str) -> StrRef {
        if s.is_empty() {
            return StrRef::EMPTY;
        }
        let off = self.arena.len() as u32;
        self.arena.extend_from_slice(s.as_bytes());
        let len = s.len() as u32;
        StrRef { off, len }
    }

    /// Intern one completed stanza (interns the fields, the dependency groups'
    /// OR-alternatives, and the provides names), then push the assembled
    /// [`PkgRecordC`]. Field semantics mirror [`StanzaBuilder::build_record`]
    /// exactly — `view.depends()` already merges Pre-Depends-then-Depends.
    pub fn push_stanza(&mut self, view: &StanzaView<'_>) {
        let package = self.intern(view.package());
        let version = self.intern(view.version());
        let arch = self.intern(view.arch());
        let filename = self.intern(view.filename());
        let size = view.size();

        let dep_group_start = self.dep_groups.len() as u32;
        let mut dep_group_len: u32 = 0;
        for group in view.depends() {
            let alt_start = self.dep_alts.len() as u32;
            let mut alt_len: u32 = 0;
            for alt in group.alts() {
                let r = self.intern(alt);
                self.dep_alts.push(r);
                alt_len += 1;
            }
            self.dep_groups.push(DepGroupC { alt_start, alt_len });
            dep_group_len += 1;
        }

        let prov_start = self.prov_refs.len() as u32;
        let mut prov_len: u32 = 0;
        for prov in view.provides() {
            let r = self.intern(prov);
            self.prov_refs.push(r);
            prov_len += 1;
        }

        self.records.push(PkgRecordC {
            package,
            version,
            arch,
            filename,
            size,
            dep_group_start,
            dep_group_len,
            prov_start,
            prov_len,
        });
    }

    /// Finish the build: sort the lookup tables and move every buffer into a
    /// [`PackageIndex`].
    ///
    /// `name_sorted` is `0..records.len()` sorted by (arena-resolved `Package`
    /// name, record index) ascending, so the largest index for a duplicated name
    /// sorts last → [`PackageIndex::get`] returns it (last-wins, matching the old
    /// `BTreeMap` insert-overwrite). `prov_sorted` keeps only provided names that
    /// are **not** a real package name (virtual must not shadow real), with the
    /// first provider (lowest record index) winning among duplicates, sorted by
    /// arena-resolved name.
    pub fn finish(self) -> PackageIndex {
        let PackageIndexBuilder {
            arena,
            records,
            dep_groups,
            dep_alts,
            prov_refs,
        } = self;

        // Local arena resolve, identical to `PackageIndex::resolve`.
        let resolve = |r: StrRef| -> &str {
            let start = r.off as usize;
            let end = start.saturating_add(r.len as usize);
            match arena.get(start..end) {
                Some(slice) => core::str::from_utf8(slice).unwrap_or(""),
                None => "",
            }
        };

        // name_sorted: by (resolved package name, index) ascending.
        let mut name_sorted: Vec<u32> = (0..records.len() as u32).collect();
        name_sorted.sort_by(|&a, &b| {
            let na = resolve(records[a as usize].package);
            let nb = resolve(records[b as usize].package);
            na.cmp(nb).then(a.cmp(&b))
        });

        // True if `name` is a real package name (binary search over name_sorted).
        let is_real = |name: &str| -> bool {
            name_sorted
                .binary_search_by(|&ri| resolve(records[ri as usize].package).cmp(name))
                .is_ok()
        };

        // prov_sorted: collect provides not shadowed by a real package, then sort
        // by (name, record) and keep the first provider (lowest index) per name.
        let mut prov_sorted: Vec<ProvEntry> = Vec::new();
        for (i, rec) in records.iter().enumerate() {
            let start = rec.prov_start as usize;
            let end = start + rec.prov_len as usize;
            for &pr in &prov_refs[start..end] {
                let pname = resolve(pr);
                if pname.is_empty() || is_real(pname) {
                    continue;
                }
                prov_sorted.push(ProvEntry {
                    name: pr,
                    record: i as u32,
                });
            }
        }
        prov_sorted.sort_by(|a, b| resolve(a.name).cmp(resolve(b.name)).then(a.record.cmp(&b.record)));
        prov_sorted.dedup_by(|a, b| resolve(a.name) == resolve(b.name));

        PackageIndex {
            arena,
            records,
            dep_groups,
            dep_alts,
            prov_refs,
            name_sorted,
            prov_sorted,
        }
    }
}

/// Drive the streaming [`StanzaParser`]'s borrowed-view path straight into the
/// compact builder: each completed stanza is interned into the arena with no
/// transient owned [`PkgRecord`]. This is the kernel `apt update` sink.
impl RecordSink for PackageIndexBuilder {
    fn emit(&mut self, view: &StanzaView<'_>) {
        self.push_stanza(view);
    }
}

#[cfg(test)]
mod arena_tests {
    use super::*;

    #[test]
    fn intern_resolve_round_trip() {
        let mut arena = Arena::new();
        let a = arena.intern("libc6");
        let b = arena.intern("busybox-static");
        let c = arena.intern("amd64");

        // Each StrRef resolves back to exactly the interned string.
        assert_eq!(arena.resolve(a), "libc6");
        assert_eq!(arena.resolve(b), "busybox-static");
        assert_eq!(arena.resolve(c), "amd64");

        // Distinct strings get distinct, non-overlapping ranges appended in order.
        assert_eq!(a.off, 0);
        assert_eq!(a.len, 5);
        assert_eq!(b.off, 5);
        assert_eq!(b.len, "busybox-static".len() as u32);
        assert_eq!(c.off, b.off + b.len);
        assert_eq!(arena.len(), "libc6busybox-staticamd64".len());
    }

    #[test]
    fn empty_string_interns_to_empty_without_growing() {
        let mut arena = Arena::new();
        assert!(arena.is_empty());

        let e = arena.intern("");
        assert_eq!(e, StrRef::EMPTY);
        assert_eq!(e.off, 0);
        assert_eq!(e.len, 0);
        // Interning the empty string does not grow the arena.
        assert!(arena.is_empty());
        assert_eq!(arena.resolve(e), "");

        // A real intern after an empty one still starts at offset 0.
        let r = arena.intern("zlib1g");
        assert_eq!(r.off, 0);
        assert_eq!(arena.resolve(r), "zlib1g");
    }

    #[test]
    fn empty_strref_resolves_to_empty_on_any_arena() {
        // EMPTY resolves to "" regardless of arena contents (len is zero).
        let mut arena = Arena::new();
        arena.intern("filler-bytes");
        assert_eq!(arena.resolve(StrRef::EMPTY), "");
    }

    #[test]
    fn resolve_is_total_for_out_of_bounds_ranges() {
        // A defensive resolve never panics on a range past the arena end.
        let mut arena = Arena::new();
        arena.intern("abc");
        let bogus = StrRef { off: 100, len: 10 };
        assert_eq!(arena.resolve(bogus), "");
        let partial_oob = StrRef { off: 1, len: 100 };
        assert_eq!(arena.resolve(partial_oob), "");
    }

    #[test]
    fn unicode_round_trips() {
        let mut arena = Arena::new();
        let s = "naïve-π-pkg";
        let r = arena.intern(s);
        assert_eq!(arena.resolve(r), s);
        assert_eq!(r.len as usize, s.len());
    }
}
