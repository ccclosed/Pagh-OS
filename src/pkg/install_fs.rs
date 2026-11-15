//! Эффектный ext2-инсталлятор пакетов (компонент дизайна 10, `Package_Installer`).
//!
//! Это *kernel-only* половина инсталлятора. Чистая логика выбора/нормализации живёт
//! в [`super::install`] (`install.rs`), который `#[path]`-включается `host-tests` и
//! потому ОБЯЗАН оставаться без зависимостей от ядра/VFS (R11.6). [`install_data_tar`]
//! — эффектная оболочка: переиспользует
//! [`normalize_entry_path`](super::install::normalize_entry_path) и пишет выбранные
//! regular files на реальную ext2 через трейт [`VfsNode`]. Вынесено в соседний
//! модуль — ровно как `net::http` (чистый) vs `net::http_fetch` (эффектный) — чтобы
//! `install.rs` оставался host-includable.
//!
//! Обязанности (R10.1–R10.4, R10.6, R10.7, R10.8, R12.4, R12.5):
//!   * Пропускать не-regular записи (R10.6) и `..`-escapes/пустые пути (R10.8).
//!   * Разрешать каждый безопасный путь относительно `root`, создавая недостающие
//!     родительские каталоги (R10.2) — но **никогда вне `root`** и никогда через
//!     симлинк, созданный тем же пакетом.
//!   * Создавать/перезаписывать целевой файл и писать ровно содержимое записи, чтобы
//!     сохранённый размер равнялся длине содержимого (R10.3, R10.7).
//!   * При нехватке места от ext2 удалить частичный файл и вернуть
//!     [`InstallError::NoSpace`] (R10.4).
//!   * Выдавать ровно одну структурированную диагностику на сбой с
//!     `component=Package_Installer`, стадией и путём файла (R12.4, R12.5).
//!
//! ## Confinement установочного корня
//!
//! Архив недоверенный, даже когда пришёл по аутентифицированному каналу: член `.deb`
//! может быть симлинком, и старый код резолвил родительские каталоги поздних членов
//! через **любой** симлинк дерева (`lookup_path_walk(_, true)`). Поскольку walker
//! интерпретирует цели ссылок в *гостевом* namespace, абсолютная цель вроде `/etc`
//! отображается в `/mnt/etc` — так что `./y -> /etc`, затем `./y/autorun` писали
//! `/mnt/etc/autorun`, который shell исполняет как root при загрузке (и
//! `./x -> /` + `./x/pwned_root` → `/mnt/pwned_root`). Ни один путь не содержит
//! `..`, так что нормализация путей архива это не ловила.
//!
//! Фикс — два независимых слоя:
//!
//!   * чистый [`plan_install`] собирает каждый создаваемый архивом симлинк и
//!     отвергает любого члена (или цель hard-link), чей компонент-предок — один из
//!     них; [`install_data_tar`] превращает это в отказ всего пакета
//!     ([`InstallError::UnsafePath`]) **до записи чего-либо**;
//!   * эффектный резолвер ниже идёт по родителям покомпонентно, начиная с узла
//!     установочного корня, и никогда его не покидает: компонент должен быть
//!     настоящим каталогом или отсутствующим именем (создаётся по запросу),
//!     симлинк разворачивается, только когда его цель **относительная** и
//!     склеенные компоненты остаются под корнем, а ссылка, созданная этой же
//!     установкой, не разворачивается вообще (резолвинг в стиле lstat).
//!     Абсолютные цели отвергаются сразу — их ремап в гостевой namespace и есть
//!     то, что делало их relative-to-install-root.
//!
//! ## Кросс-пакетная политика ссылок (F-r4-vfs-1)
//!
//! `own_links`/`own_link_inos` описывают только ссылки ТЕКУЩЕГО пакета, а
//! ссылка, установленная предыдущим пакетом, для следующего — обычная
//! pre-existing. Round-4 PoC: A ставит `x/evil -> ../etc`, B пишет
//! `x/evil/autorun` без своих ссылок → preflight B пуст, резолвер разворачивает
//! чужую ссылку и пишет `/mnt/etc/autorun`. Поэтому разворот pre-existing
//! ссылки разрешён ТОЛЬКО для точных документированных пар
//! [`PROVISIONED_INSTALL_LINKS`] (`lib64 -> usr/lib64`); любая другая —
//! [`InstallError::UnsafePath`] **до записи члена**, а
//! [`refuse_preexisting_redirectors`] проверяет весь пакет до первого inode,
//! чтобы отказ остался атомарным.
//!
//! Цели hard-link разрешаются тем же confined walker'ом (финальный компонент не
//! разворачивается, как требует `link(2)`), так что враждебный архив не может
//! захардлинковать узел снаружи корня.
//!
//! ## Hard-link-алиасы симлинков
//!
//! `link(2)` не разворачивает финальный компонент, поэтому hard link, чья цель —
//! симлинк, есть **второе имя inode ссылки**. Аудит round-2 использовал это, чтобы
//! обойти оба слоя выше (`./d/y -> ../etc`, `./d/h -> ./d/y`, затем
//! `./d/h/autorun -> ./d/pwn`): планировщик знал только `d/y`, `d/h` выглядел
//! обычным hard link, и член `d/h/autorun` попадал в `/mnt/etc/autorun`. Фикс — две
//! независимые половины, соответствующие слоям выше: чистый планировщик трактует
//! каждый hard-link-алиас как ссылку (замыкание эквивалентности, так что покрыты
//! алиасы алиасов и `..`-нормализованные цели) и отвергает весь пакет до любой
//! записи; эффектный слой никогда не создаёт второе имя для симлинка архива и
//! никогда не обходит симлинк, у которого больше одного имени — алиас, который не
//! помечен ни одной path-based записью `own_links`, например оставленный прошлой
//! установкой или регрессией планировщика.
//!
//! ## Обнаружение нехватки места через границу VFS
//!
//! ext2 сообщает исчерпанную bitmap блоков/inode как `FsError::OutOfSpace`, которую
//! адаптер ext2 `VfsNode` (`fs/ext2/mod.rs::fs_to_vfs`) сводит к
//! [`VfsError::IoError`]. Поверхность трейта VFS не выставляет отдельного варианта
//! out-of-space, поэтому write/create, падающий с [`VfsError::IoError`], — единственный
//! наблюдаемый сигнал нехватки места и здесь отображается в [`InstallError::NoSpace`]
//! (R10.4). Любая другая [`VfsError`] сообщается дословно через [`InstallError::Vfs`].
#![allow(dead_code)]

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::vfs::{self, VfsError, VfsNode};

use super::install::{descends_through, plan_install_meta, MetaOp, MetaPlan, UnsafeKind};
use super::tar::{TarEntry, TarMeta, TarType};

/// Failure modes of the effectful ext2 install (design component 10).
#[derive(Debug)]
pub enum InstallError {
    /// The ext2 filesystem ran out of space while creating/writing `path`. Any
    /// partial file at that path has already been removed (R10.4).
    NoSpace { path: String },
    /// Any other VFS/ext2 error surfaced while installing (the kernel's real
    /// [`VfsError`]).
    Vfs(VfsError),
    /// The package tries to materialize `path` through a symbolic link instead of
    /// where its member path says (F-x86n-1). The whole package is refused before
    /// anything is written; `reason` names the refused construct.
    UnsafePath { path: String, reason: &'static str },
}

impl core::fmt::Display for InstallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InstallError::NoSpace { path } => {
                write!(f, "no space while writing '{}'", path)
            }
            InstallError::Vfs(e) => write!(f, "filesystem error {:?}", e),
            InstallError::UnsafePath { path, reason } => {
                write!(f, "refused unsafe install path '{}': {}", path, reason)
            }
        }
    }
}

/// Install every member of a decompressed `data.tar` onto ext2 under `root`,
/// returning the number of entries created (R10.1–R10.4, R10.6–R10.8, issue #18).
///
/// The selection and ordering rules live in the pure [`plan_install`]; this is the
/// effectful shell that executes the plan:
///
///   * regular files are written verbatim (replacing an existing entry so the
///     stored size matches the content length — R10.3, R10.7);
///   * **symlinks are created as symlinks**, with the archive's target stored
///     verbatim (`VfsNode::create_symlink`); a dangling target is normal;
///   * **hard links are created as hard links** (`VfsNode::link`), i.e. a second
///     name for the target inode — never a copy. The target may come from this
///     archive or from an earlier package, so the hard-link operations are retried
///     until no further progress, and whatever remains is reported and skipped;
///   * missing parent directories are created (R10.2) and resolved strictly inside
///     the install root: an existing `/lib64 -> usr/lib64` (relative, in-root) is
///     still followed so the member lands inside the target instead of creating a
///     parallel tree, but a link this package creates is never traversed and an
///     absolute target is refused (F-x86n-1, see the module docs);
///   * an existing entry at a member's path is removed first — through the
///     link-aware `unlink`, so removing one name of a hard-linked file cannot take
///     the data another name still uses. A directory in the way is skipped with one
///     diagnostic (never recursively deleted).
///
/// A package whose member paths descend through symlinks the archive itself
/// creates is refused **atomically** with [`InstallError::UnsafePath`] before any
/// write, so a hostile `.deb` cannot leave a partial tree behind (F-x86n-1).
///
/// On a no-space failure the partial file is removed and [`InstallError::NoSpace`]
/// is returned (R10.4); every failure emits one structured diagnostic (R12.4,
/// R12.5). Nothing is ever materialized as a copy.
pub fn install_data_tar(entries: &[TarEntry<'_>], root: &str) -> Result<usize, InstallError> {
    // W1 (apt-fast): the eager in-memory entry list is now a thin feeder over the
    // SAME metadata-plan + streaming executor the apt `.deb` path uses. This keeps
    // one implementation of the ordering/preflight/atomicity rules: the property
    // and in-QEMU tests that call this function exercise the streaming path.
    let metas: Vec<TarMeta> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let mut meta = TarMeta::from_entry(entry);
            meta.index = index;
            meta
        })
        .collect();
    let plan = plan_install_meta(&metas);
    let mut install = StreamingInstall::prepare(&plan, root)?;
    for (index, entry) in entries.iter().enumerate() {
        install.accept_entry(&metas[index])?;
        if !entry.content.is_empty() {
            install.accept_chunk(entry.content)?;
        }
    }
    install.finish()
}

/// The buffered-write size used by the streaming installer.
///
/// 256 KiB is ext2's 64-block metadata transaction size ([`Ext2File::write`]
/// commits per 64 data blocks), so one buffered flush is one transaction and one
/// inode round trip, matching the eager installer's one-shot `write` cost while
/// keeping the extra resident bytes to a quarter MiB. Smaller flushes would make
/// the streaming path pay a journal commit per decompressor chunk (8 KiB).
const STREAM_WRITE_BUFFER: usize = 256 * 1024;

/// A regular file whose content is still streaming in.
struct CurrentFile {
    /// Parent directory, kept so a failure can remove the partial file.
    dir: Arc<dyn VfsNode>,
    /// Final component within `dir`.
    filename: String,
    /// Absolute path for diagnostics.
    abs: String,
    /// The freshly created file node.
    node: Arc<dyn VfsNode>,
    /// Archive permission bits to apply on completion.
    mode: u32,
    /// Content size the plan recorded for this member.
    expected: u64,
    /// Bytes already written to the filesystem.
    written: u64,
    /// Buffered bytes not yet written.
    buf: Vec<u8>,
}

/// The effectful executor of a [`MetaPlan`], fed by a **streaming** tar walk.
///
/// This is the W1 replacement for installing a fully materialized `data.tar`:
///
///   1. [`StreamingInstall::prepare`] runs the three atomic preflight checks
///      (F-x86n-1 planner verdict, F-r3-vfs-1 resolved-spelling split,
///      F-r4-vfs-1 pre-existing redirectors) before any inode exists;
///   2. [`StreamingInstall::accept_entry`] is called for each [`TarMeta`] in
///      archive order — a planned regular file starts a buffered writer, a planned
///      symlink is created, anything the planner dropped is skipped;
///   3. [`StreamingInstall::accept_chunk`] appends the current file's decompressed
///      bytes; it flushes in [`STREAM_WRITE_BUFFER`] pieces so peak memory stays
///      bounded by the metadata plan plus one buffer;
///   4. [`StreamingInstall::finish`] completes the last file and runs the bounded
///      hard-link fixpoint, exactly like the eager installer.
///
/// Security semantics are unchanged: the plan is validated in full before the
/// first inode, a member is written only if the plan accepted its archive
/// ordinal, and the confined resolver (`own_links`/`own_link_inos`) still guards
/// every write. As in the eager path, a no-space failure removes the partial file
/// and returns [`InstallError::NoSpace`].
pub struct StreamingInstall {
    root: String,
    /// Symlink spellings this install created (F-x86n-1/F-r3-vfs-1).
    own_links: BTreeSet<String>,
    /// Inodes of symlinks this install created (spelling-independent backstop).
    own_link_inos: BTreeSet<u64>,
    /// Planned file/symlink operations keyed by their source member ordinal.
    by_member: BTreeMap<usize, MetaOp>,
    /// Hard links to attempt after all files/symlinks (plan order preserved).
    deferred: Vec<(String, String)>,
    /// The regular file whose content is currently streaming, if any.
    current: Option<CurrentFile>,
    /// Operations materialized so far.
    installed: usize,
}

impl StreamingInstall {
    /// Validate `plan` against the tree as it exists now and return an executor.
    ///
    /// All three preflight refusals happen here — **before** the caller writes a
    /// single inode — preserving the installer's atomic-refusal property.
    pub fn prepare(plan: &MetaPlan, root: &str) -> Result<StreamingInstall, InstallError> {
        // F-x86n-1, layer 1 (pure plan): a member below a symlink this archive
        // creates would be written wherever that link points. Refuse the whole
        // package before creating a single inode.
        if let Some(m) = plan.unsafe_members.first() {
            let abs = join_abs(root, &m.path);
            let reason = match m.kind {
                UnsafeKind::MemberBelowSymlink => "member path descends through an archive symlink",
                UnsafeKind::HardlinkTargetBelowSymlink => {
                    "hard-link target descends through an archive symlink"
                }
                UnsafeKind::HardlinkAliasesSymlink => {
                    "hard link aliases an archive symlink (second name for the link inode)"
                }
            };
            return Err(unsafe_path_with_link(&abs, reason, &m.link));
        }

        if plan.skipped_unsafe > 0 || plan.skipped_empty_target > 0 {
            crate::warn!(
                "Package_Installer: skipped {} path-unsafe and {} empty-target members",
                plan.skipped_unsafe,
                plan.skipped_empty_target
            );
        }

        // F-r3-vfs-1: a symlink planned at an archive path can be *materialized*
        // at a different path when a pre-existing in-root redirector is followed
        // (the planner only knows archive spellings). If a later member is spelled
        // through the materialized path it would be written through the archive's
        // own link. Refuse before the first inode, so the refusal stays atomic.
        refuse_spelling_split(root, plan)?;
        // F-r4-vfs-1: the same split across packages. A pre-existing link created
        // by an earlier package cannot be told apart from a provisioned one, so
        // the only links the resolver may follow are the exact documented pairs.
        refuse_preexisting_redirectors(root, plan)?;

        let mut by_member: BTreeMap<usize, MetaOp> = BTreeMap::new();
        let mut deferred: Vec<(String, String)> = Vec::new();
        for op in plan.ops.iter() {
            match op {
                MetaOp::File { member, .. } | MetaOp::Symlink { member, .. } => {
                    by_member.insert(*member, op.clone());
                }
                MetaOp::Hardlink { path, target } => {
                    deferred.push((path.clone(), target.clone()));
                }
            }
        }
        if plan.unresolved.len() > 0 {
            crate::warn!(
                "Package_Installer: {} hard link(s) form an unresolvable cycle and will be skipped",
                plan.unresolved.len()
            );
        }

        Ok(StreamingInstall {
            root: String::from(root),
            own_links: BTreeSet::new(),
            own_link_inos: BTreeSet::new(),
            by_member,
            deferred,
            current: None,
            installed: 0,
        })
    }

    /// Handle one member header in archive order. A previous file's stream is
    /// complete by construction here (the tar walker serializes content), so this
    /// first finishes it, then starts/creates whatever the plan assigned to this
    /// member ordinal — or ignores the member when the planner dropped it.
    pub fn accept_entry(&mut self, meta: &TarMeta) -> Result<(), InstallError> {
        self.complete_current()?;
        let Some(op) = self.by_member.get(&meta.index).cloned() else {
            return Ok(());
        };
        match op {
            MetaOp::File {
                path, mode, size, ..
            } if meta.kind == TarType::Regular => self.begin_file(&path, mode, size),
            MetaOp::Symlink { path, target, .. } if meta.kind == TarType::Symlink => {
                install_symlink(
                    &self.root,
                    &path,
                    &target,
                    &mut self.own_links,
                    &mut self.own_link_inos,
                )?;
                self.installed += 1;
                Ok(())
            }
            // A kind mismatch cannot happen for a plan built from this same
            // stream; if it ever did, skipping is the safe outcome (never write
            // content the plan did not accept for that ordinal).
            _ => Ok(()),
        }
    }

    /// Append decompressed content for the current regular file (no-op when the
    /// current member was skipped, e.g. a file blocked by a directory).
    pub fn accept_chunk(&mut self, chunk: &[u8]) -> Result<(), InstallError> {
        let Some(current) = self.current.as_mut() else {
            return Ok(());
        };
        let mut rest = chunk;
        while !rest.is_empty() {
            let room = STREAM_WRITE_BUFFER - current.buf.len();
            let take = core::cmp::min(room, rest.len());
            current.buf.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if current.buf.len() == STREAM_WRITE_BUFFER {
                flush_current_file(current)?;
            }
        }
        Ok(())
    }

    /// Complete the install: finish the last file, then create deferred hard
    /// links (bounded fixpoint) and warn about any that never found a target.
    pub fn finish(mut self) -> Result<usize, InstallError> {
        self.complete_current()?;
        let mut deferred = core::mem::take(&mut self.deferred);
        loop {
            if deferred.is_empty() {
                break;
            }
            let mut progressed = false;
            let mut remaining: Vec<(String, String)> = Vec::new();
            for (path, target) in deferred {
                match install_hardlink(
                    &self.root,
                    &path,
                    &target,
                    &mut self.own_links,
                    &mut self.own_link_inos,
                ) {
                    Ok(()) => {
                        self.installed += 1;
                        progressed = true;
                    }
                    Err(HardlinkOutcome::Missing) => remaining.push((path, target)),
                    Err(HardlinkOutcome::Failed(e)) => return Err(e),
                }
            }
            deferred = remaining;
            if !progressed {
                break;
            }
        }
        // One diagnostic per hard link that never found its target. It is
        // skipped — turning it into a copy is exactly what issue #18 removed.
        for (path, target) in deferred {
            crate::warn!(
                "Package_Installer: hard link '{}' -> '{}' skipped (target missing after all passes)",
                path,
                target
            );
        }
        Ok(self.installed)
    }

    /// Create (or replace) the destination and start a buffered writer for it.
    fn begin_file(&mut self, rel: &str, mode: u32, expected: u64) -> Result<(), InstallError> {
        let abs = join_abs(&self.root, rel);
        let (dir, filename, _resolved_dir) =
            resolve_parent(&self.root, rel, &abs, &self.own_links, &self.own_link_inos)?;
        // R10.7: replace an existing entry so the stored size matches the new
        // content exactly (ext2 `write_file` only grows `i_size`).
        match dir.lookup(filename) {
            Ok(existing) => {
                if existing.is_directory() {
                    crate::warn!(
                        "Package_Installer: file '{}' skipped (a directory is in the way)",
                        abs
                    );
                    return Ok(());
                }
                if let Err(e) = dir.remove(filename) {
                    return Err(vfs_err("replace", &abs, e));
                }
            }
            Err(VfsError::NotFound) => {}
            Err(e) => return Err(vfs_err("create", &abs, e)),
        }
        let node: Arc<dyn VfsNode> = match dir.create_file(filename) {
            Ok(n) => n,
            Err(VfsError::IoError) => return Err(no_space("create", &abs)),
            Err(e) => return Err(vfs_err("create", &abs, e)),
        };
        self.current = Some(CurrentFile {
            dir,
            filename: String::from(filename),
            abs,
            node,
            mode,
            expected,
            written: 0,
            buf: Vec::with_capacity(STREAM_WRITE_BUFFER),
        });
        Ok(())
    }

    /// Flush the buffered tail of the current file, check the declared size,
    /// apply the archive mode and count the operation.
    fn complete_current(&mut self) -> Result<(), InstallError> {
        let Some(mut current) = self.current.take() else {
            return Ok(());
        };
        flush_current_file(&mut current)?;
        if current.written != current.expected {
            // The tar walker forwards exactly the declared content, so a mismatch
            // means the stream and the plan disagreed: never leave a truncated
            // file behind (R10.3/R10.4).
            let _ = current.dir.remove(&current.filename);
            return Err(vfs_err("write", &current.abs, VfsError::InvalidArgument));
        }
        // Preserve the archive's permission bits (see `install_one`).
        let perms = current.mode & 0o7777;
        if perms != 0 {
            if let Err(e) = current.node.set_mode(perms) {
                crate::warn!(
                    "Package_Installer: could not apply mode {:04o} to '{}': {:?}",
                    perms,
                    current.abs,
                    e
                );
            }
        }
        self.installed += 1;
        Ok(())
    }
}

/// Write the buffered bytes of `current` at its current offset, mapping a short
/// write / `IoError` to [`InstallError::NoSpace`] and removing the partial file.
fn flush_current_file(current: &mut CurrentFile) -> Result<(), InstallError> {
    if current.buf.is_empty() {
        return Ok(());
    }
    let len = current.buf.len() as u64;
    match current.node.write(current.written, &current.buf) {
        Ok(written) if written as u64 == len => {
            current.written += len;
            current.buf.clear();
            Ok(())
        }
        // A short write without an explicit error still means the full content did
        // not land (most plausibly an exhausted bitmap) → no-space + cleanup.
        Ok(_) => {
            let _ = current.dir.remove(&current.filename);
            Err(no_space("write", &current.abs))
        }
        Err(VfsError::IoError) => {
            let _ = current.dir.remove(&current.filename);
            Err(no_space("write", &current.abs))
        }
        Err(e) => {
            let _ = current.dir.remove(&current.filename);
            Err(vfs_err("write", &current.abs, e))
        }
    }
}

/// F-r3-vfs-1 preflight — refuse a package whose planned symlink would be
/// materialized at a path other than its archive spelling while a later member
/// is spelled through that materialized path.
///
/// The planner (`plan_install`) only knows archive spellings, so it cannot see
/// this construct: the archive creates `usr/share/doc/libgcc-s1/evil`, a
/// **pre-existing** `libgcc-s1 -> gcc-14-base` redirector makes the effectful
/// resolver create that link at `usr/share/doc/gcc-14-base/evil`, and a later
/// member is spelled `usr/share/doc/gcc-14-base/evil/autorun` — descending
/// through the archive's own link. The round-3 audit used exactly that to plant
/// `/mnt/etc/autorun`.
///
/// This check runs before the first inode is written so the refusal stays a
/// whole-package `UnsafePath` (the string/inode registration in
/// [`install_symlink`] remains the effectful backstop). It resolves the planned
/// link's parent against the tree as it exists **before** the install and only
/// when that resolution succeeds without creating anything: a parent the archive
/// creates itself cannot be a pre-existing redirector, and members below an
/// archive-created link are already refused by the planner.
fn refuse_spelling_split(root: &str, plan: &MetaPlan) -> Result<(), InstallError> {
    // Every path this plan would materialize, hard links included.
    let mut members: Vec<&str> = Vec::new();
    for op in plan.ops.iter() {
        match op {
            MetaOp::File { path, .. }
            | MetaOp::Symlink { path, .. }
            | MetaOp::Hardlink { path, .. } => members.push(path.as_str()),
        }
    }
    let no_links: BTreeSet<String> = BTreeSet::new();
    let no_inos: BTreeSet<u64> = BTreeSet::new();

    for op in plan.ops.iter() {
        let MetaOp::Symlink { path, .. } = op else {
            continue;
        };
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        if comps.is_empty() {
            continue;
        }
        let (parents, filename) = comps.split_at(comps.len() - 1);
        let dir_rel = parents.join("/");
        let dir_abs = join_abs(root, &dir_rel);
        let (_dir, walked) =
            match resolve_confined(root, &dir_rel, &dir_abs, true, false, &no_links, &no_inos) {
                Ok(v) => v,
                // A pre-existing link outside the documented allowlist in the link's
                // own parent chain is a cross-package redirector (F-r4-vfs-1): refuse
                // the package now, before any inode is created, instead of letting
                // `install_symlink` hit the refusal after the earlier members landed.
                Err(ResolveFailure::Unsafe(reason)) => {
                    return Err(unsafe_path(&dir_abs, reason));
                }
                // The parent does not exist yet (the archive creates it) or is not a
                // traversable in-root directory: no pre-existing spelling split.
                Err(_) => continue,
            };
        let resolved_rel = join_rel(&walked, filename[0]);
        if resolved_rel == *path {
            continue;
        }
        for member in members.iter() {
            if *member != path.as_str() && descends_through(member, &resolved_rel) {
                return Err(unsafe_path_with_link(
                    &join_abs(root, member),
                    "member path descends through a symlink the archive creates \
                     (resolved spelling differs from the archive spelling)",
                    path,
                ));
            }
        }
    }
    Ok(())
}

/// Проверить весь план против дерева, каким оно существует ДО установки, и
/// отказать пакету, если хоть один член материализовался бы через pre-existing
/// ссылку вне [`super::install::PROVISIONED_INSTALL_LINKS`] (F-r4-vfs-1).
///
/// Проверяются родительские цепочки обычных членов и hard-link'ов (цели
/// hard-link'ов резолвятся lstat-стилем, без разворота финального компонента).
/// Отсутствующий компонент — нормально: его создаст этот же пакет, а ссылки под
/// ещё не существующим каталогом быть не может. `Unsafe` возвращается сразу,
/// поэтому отказ атомарный; прочие ошибки игнорируются — их сообщит настоящая
/// установка с точным путём.
fn refuse_preexisting_redirectors(root: &str, plan: &MetaPlan) -> Result<(), InstallError> {
    let no_links: BTreeSet<String> = BTreeSet::new();
    let no_inos: BTreeSet<u64> = BTreeSet::new();
    let mut paths: Vec<&str> = Vec::with_capacity(plan.ops.len());
    for op in plan.ops.iter() {
        match op {
            MetaOp::File { path, .. } | MetaOp::Symlink { path, .. } => paths.push(path),
            MetaOp::Hardlink { path, target } => {
                paths.push(path);
                paths.push(target);
            }
        }
    }
    for path in paths {
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        if comps.len() < 2 {
            continue;
        }
        let (parents, _) = comps.split_at(comps.len() - 1);
        let dir_rel = parents.join("/");
        let dir_abs = join_abs(root, &dir_rel);
        match resolve_confined(root, &dir_rel, &dir_abs, true, false, &no_links, &no_inos) {
            Ok(_) => {}
            Err(ResolveFailure::Unsafe(reason)) => {
                return Err(unsafe_path(&dir_abs, reason));
            }
            Err(_) => {}
        }
    }
    Ok(())
}

/// Outcome of one hard-link attempt.
enum HardlinkOutcome {
    /// The target does not exist (yet) — the caller may retry.
    Missing,
    /// A real failure (no space, I/O, a directory in the way, ...).
    Failed(InstallError),
}

/// Create `rel` as a symbolic link to `target` (stored verbatim).
fn install_symlink(
    root: &str,
    rel: &str,
    target: &str,
    own_links: &mut BTreeSet<String>,
    own_link_inos: &mut BTreeSet<u64>,
) -> Result<(), InstallError> {
    let abs = join_abs(root, rel);
    let (dir, filename, resolved_dir) = resolve_parent(root, rel, &abs, own_links, own_link_inos)?;

    // Replace any existing entry (link-aware removal), but never a directory.
    match dir.lookup(filename) {
        Ok(existing) => {
            if existing.is_directory() {
                crate::warn!(
                    "Package_Installer: link '{}' skipped (a directory is in the way)",
                    abs
                );
                return Ok(());
            }
            dir.remove(filename)
                .map_err(|e| vfs_err("replace", &abs, e))?;
        }
        Err(VfsError::NotFound) => {}
        Err(e) => return Err(vfs_err("stat", &abs, e)),
    }

    match dir.create_symlink(filename, target.as_bytes()) {
        Ok(node) => {
            // F-r3-vfs-1: the link may have been created at a *resolved* path
            // (a pre-existing redirector in the parent chain was followed while
            // `rel` is the archive spelling). Register both spellings and the
            // inode: the effectful resolver refuses to traverse either spelling
            // and, independently of any string, any symlink with this inode.
            own_links.insert(String::from(rel));
            let resolved_rel = join_rel(&resolved_dir, filename);
            if resolved_rel != rel {
                own_links.insert(resolved_rel);
            }
            let ino = node.fs_ino();
            if ino != 0 {
                own_link_inos.insert(ino);
            }
            crate::debug!("Package_Installer: link {} -> {}", abs, target);
            Ok(())
        }
        Err(VfsError::IoError) => Err(no_space("symlink", &abs)),
        Err(e) => Err(vfs_err("symlink", &abs, e)),
    }
}

/// Join a resolved directory spelling (root-relative, possibly empty) and a
/// final component into a normalized root-relative path.
fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        String::from(name)
    } else {
        let mut s = String::from(dir);
        s.push('/');
        s.push_str(name);
        s
    }
}

/// Create `rel` as a hard link to the existing entry at `target` (a
/// root-relative path from the archive).
///
/// `own_links` is `&mut`: a hard link whose target is a symlink (one this archive
/// created, or a pre-existing one) makes `rel` a second name for that link inode,
/// so `rel` is registered there exactly like a symlink member. Later members whose
/// parent path runs through it are refused instead of being written wherever the
/// link points (F-r2-vfs-1). An alias of an *archive-created* symlink is refused
/// outright: creating it is the primitive the round-2 audit needed.
fn install_hardlink(
    root: &str,
    rel: &str,
    target: &str,
    own_links: &mut BTreeSet<String>,
    own_link_inos: &mut BTreeSet<u64>,
) -> Result<(), HardlinkOutcome> {
    let abs = join_abs(root, rel);
    let target_abs = join_abs(root, target);

    // The target is resolved **without** following a final symlink: `link(2)`
    // hard-links the entry itself, and a hard link to a symlink is legal. It goes
    // through the same confined walker as the parents (F-x86n-1), so a hostile
    // archive cannot hard-link a node from outside the install root.
    let target_node = match resolve_confined(
        root,
        target,
        &target_abs,
        false,
        false,
        own_links,
        own_link_inos,
    ) {
        Ok((n, _walked)) => n,
        // Not on the tree yet: a later member (or an earlier package) may still
        // provide it.
        Err(ResolveFailure::NotFound) => return Err(HardlinkOutcome::Missing),
        Err(failure) => {
            return Err(HardlinkOutcome::Failed(resolve_failure(
                "resolve_target",
                &target_abs,
                failure,
            )))
        }
    };
    if target_node.is_directory() {
        crate::warn!(
            "Package_Installer: hard link '{}' skipped (target '{}' is a directory)",
            abs,
            target_abs
        );
        return Ok(());
    }
    // F-r2-vfs-1 layer 2: an alias of a link this install created would be an
    // unregistered second name for it. The pure planner refuses this package
    // atomically; this check keeps the guarantee if that layer ever regresses.
    if target_node.is_symlink() && own_links.contains(target) {
        return Err(HardlinkOutcome::Failed(unsafe_path(
            &abs,
            "hard link to a symlink created by this archive",
        )));
    }

    let (dir, filename, resolved_dir) =
        match resolve_parent(root, rel, &abs, own_links, own_link_inos) {
            Ok(v) => v,
            Err(e) => return Err(HardlinkOutcome::Failed(e)),
        };
    match dir.lookup(filename) {
        Ok(existing) => {
            if existing.is_directory() {
                crate::warn!(
                    "Package_Installer: hard link '{}' skipped (a directory is in the way)",
                    abs
                );
                return Ok(());
            }
            if let Err(e) = dir.remove(filename) {
                return Err(HardlinkOutcome::Failed(vfs_err("replace", &abs, e)));
            }
        }
        Err(VfsError::NotFound) => {}
        Err(e) => return Err(HardlinkOutcome::Failed(vfs_err("stat", &abs, e))),
    }

    match dir.link(filename, &target_node) {
        Ok(_) => {
            // The new name is a second name for the link inode when the target is
            // a symlink (a pre-existing one here: archive aliases were refused
            // above/atomically). Register it so no later member is resolved
            // through it (F-r2-vfs-1).
            if target_node.is_symlink() {
                // F-r3-vfs-1: register both spellings and the inode, exactly as
                // for a symlink member — the alias can be reached by the
                // resolved spelling too.
                own_links.insert(String::from(rel));
                let resolved_rel = join_rel(&resolved_dir, filename);
                if resolved_rel != rel {
                    own_links.insert(resolved_rel);
                }
                let ino = target_node.fs_ino();
                if ino != 0 {
                    own_link_inos.insert(ino);
                }
            }
            Ok(())
        }
        Err(VfsError::IoError) => Err(HardlinkOutcome::Failed(no_space("link", &abs))),
        Err(e) => Err(HardlinkOutcome::Failed(vfs_err("link", &abs, e))),
    }
}

/// Why one step of a confined resolution failed.
///
/// Kept separate from [`InstallError`] so a hard-link attempt that simply has no
/// target yet (`NotFound`) can be retried without emitting a failure diagnostic;
/// the call site adds the stage and path when it really is an error.
enum ResolveFailure {
    /// A component does not exist and the caller did not ask to create it.
    NotFound,
    /// Following the path would leave the install root (F-x86n-1).
    Unsafe(&'static str),
    /// A real VFS/ext2 error.
    Vfs(VfsError),
    /// No space while creating a missing parent directory.
    NoSpace,
}

/// Linux `SYMLOOP_MAX`: the 41st symlink crossed in one resolution is refused.
const INSTALL_SYMLOOP_MAX: u32 = 40;
/// `PATH_MAX`-sized cap on the expanded path of one resolution.
const INSTALL_MAX_EXPANDED_BYTES: usize = 4096;

/// Resolve `rel` strictly inside the install root, following only relative
/// symbolic links that stay under it (F-x86n-1).
///
/// The walk starts at the install-root node and never consults the global VFS root
/// again: the root is the boundary, `..` may not pop it, and every traversed
/// component is a real directory. A symbolic link is followed only when its stored
/// target is **relative**; the spliced components are then resolved under the root
/// like any other component (so `..` inside a target cannot escape either).
/// Absolute targets are refused outright, because the guest namespace maps them to
/// install-root-relative paths — that mapping is exactly what the attack relied on.
/// A link this same install created (`own_links`) is never followed at all, and
/// neither is a symlink that has more than one hard link: it may be reached through
/// an unmarked hard-link alias (F-r2-vfs-1). A pre-existing link is followed only
/// when it is one of the exact [`super::install::PROVISIONED_INSTALL_LINKS`]
/// pairs (F-r4-vfs-1); anything else is refused before the member is created.
///
/// `follow_final` selects `true` for a parent directory (a member is placed
/// inside it) and `false` for a hard-link target (`link(2)` does not follow the
/// final component). `create_missing` creates a missing component as a directory
/// (R10.2) instead of returning [`ResolveFailure::NotFound`].
fn resolve_confined(
    root: &str,
    rel: &str,
    abs: &str,
    follow_final: bool,
    create_missing: bool,
    own_links: &BTreeSet<String>,
    own_link_inos: &BTreeSet<u64>,
) -> Result<(Arc<dyn VfsNode>, String), ResolveFailure> {
    let trimmed = root.trim_end_matches('/');
    let root_path = if trimmed.is_empty() { "/" } else { trimmed };
    let root_node =
        vfs::lookup_path_walk(root_path, true).map_err(|e| ResolveFailure::Vfs(walk_err(e)))?;
    if !root_node.is_directory() {
        return Err(ResolveFailure::Vfs(VfsError::InvalidArgument));
    }

    // Components still to resolve (LIFO) and the real directories walked so far.
    let mut pending: Vec<String> = rel
        .split('/')
        .filter(|c| !c.is_empty())
        .map(String::from)
        .collect();
    pending.reverse();
    let mut stack: Vec<Arc<dyn VfsNode>> = alloc::vec![root_node];
    let mut dirs: Vec<String> = Vec::new();
    let mut links: u32 = 0;
    let mut expanded = abs.len();

    while let Some(comp) = pending.pop() {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            // `..` applies to the directories actually walked; the install root has
            // no parent to escape to.
            if dirs.pop().is_none() {
                return Err(ResolveFailure::Unsafe("path escapes the install root"));
            }
            stack.pop();
            continue;
        }
        let parent = match stack.last() {
            Some(parent) => parent,
            None => return Err(ResolveFailure::Vfs(VfsError::NotFound)),
        };
        let node = match parent.lookup(&comp) {
            Ok(node) => node,
            Err(VfsError::NotFound) if create_missing => match parent.create_dir(&comp) {
                Ok(node) => node,
                // Never seen in a single-threaded install, but ext2 may report an
                // existing entry: fall back to a plain lookup instead of guessing.
                Err(VfsError::AlreadyExists) => match parent.lookup(&comp) {
                    Ok(node) => node,
                    Err(e) => return Err(ResolveFailure::Vfs(e)),
                },
                Err(VfsError::IoError) => return Err(ResolveFailure::NoSpace),
                Err(e) => return Err(ResolveFailure::Vfs(e)),
            },
            Err(VfsError::NotFound) => return Err(ResolveFailure::NotFound),
            Err(e) => return Err(ResolveFailure::Vfs(e)),
        };

        if node.is_symlink() {
            if pending.is_empty() && !follow_final {
                // lstat-style: a hard link to a symlink links the link itself.
                return Ok((node, join_walked(&dirs)));
            }
            // The root-relative path of the link itself, to compare against the
            // links this install created.
            let mut link_rel = String::new();
            for d in dirs.iter() {
                link_rel.push_str(d);
                link_rel.push('/');
            }
            link_rel.push_str(&comp);
            if own_links.contains(&link_rel) {
                return Err(ResolveFailure::Unsafe(
                    "descends through a symlink created by this archive",
                ));
            }
            // F-r3-vfs-1: the string sets know the archive spelling and the
            // resolved spelling of every link this install created; the inode
            // check is the spelling-independent backstop. A pre-existing
            // redirector in the parent chain can make the walked spelling differ
            // from both, but the node identity cannot be forged.
            let ino = node.fs_ino();
            if ino != 0 && own_link_inos.contains(&ino) {
                return Err(ResolveFailure::Unsafe(
                    "descends through a symlink created by this archive (inode identity)",
                ));
            }
            // F-r2-vfs-1: `link(2)` created a second name for this link inode
            // somewhere (`nlink > 1`). The path walked here may be that unmarked
            // alias — from an earlier install, or from a planner regression — so a
            // name-based `own_links` check alone cannot prove it is a link the
            // archive legitimately created. Refuse to traverse any multiply-linked
            // symlink; the archive's own aliases are refused before creation.
            if node.nlink() > 1 {
                return Err(ResolveFailure::Unsafe(
                    "descends through a symlink with a hard-link alias",
                ));
            }
            let target = match node.read_link() {
                Some(t) if !t.is_empty() => t,
                _ => return Err(ResolveFailure::Unsafe("unreadable or empty symlink target")),
            };
            if target.starts_with('/') {
                return Err(ResolveFailure::Unsafe(
                    "absolute symlink target (only relative in-root links are followed)",
                ));
            }
            // F-r4-vfs-1: a link that is not ours and is not one of the exact
            // documented provisioning pairs is a pre-existing redirector. Following
            // it is the cross-package split (package A plants `x/evil -> ../etc`,
            // package B writes `x/evil/autorun`). Refuse before splicing/creating
            // anything; the allowed pair is pinned to its exact target too.
            if !super::install::is_provisioned_in_root_link(&link_rel, &target) {
                return Err(ResolveFailure::Unsafe(
                    "pre-existing symlink outside the documented installer allowlist",
                ));
            }
            links = links.saturating_add(1);
            if links > INSTALL_SYMLOOP_MAX {
                return Err(ResolveFailure::Vfs(VfsError::InvalidArgument));
            }
            expanded = expanded.saturating_add(target.len());
            if expanded > INSTALL_MAX_EXPANDED_BYTES {
                return Err(ResolveFailure::Vfs(VfsError::InvalidArgument));
            }
            // Splice the relative target in front of the remaining components; it
            // resolves against the link's directory, i.e. the current stack.
            for part in target.split('/').rev() {
                pending.push(String::from(part));
            }
            continue;
        }
        if !node.is_directory() {
            if pending.is_empty() && !follow_final {
                return Ok((node, join_walked(&dirs)));
            }
            return Err(ResolveFailure::Vfs(VfsError::InvalidArgument));
        }
        dirs.push(comp);
        stack.push(node);
    }

    let node = stack.pop().ok_or(ResolveFailure::Vfs(VfsError::NotFound))?;
    Ok((node, join_walked(&dirs)))
}

/// Join the directories actually walked by [`resolve_confined`] into a
/// root-relative spelling (empty for the install root).
fn join_walked(dirs: &[String]) -> String {
    let mut s = String::new();
    for (i, d) in dirs.iter().enumerate() {
        if i > 0 {
            s.push('/');
        }
        s.push_str(d);
    }
    s
}

/// Resolve the parent directory of a normalized root-relative path, creating any
/// missing component — strictly inside the install root (F-x86n-1).
///
/// A pre-existing relative link that stays inside the root is still followed, so a
/// member under `/lib64 -> usr/lib64` lands inside the target instead of creating a
/// parallel tree; a link the same package creates is not followed, and an absolute
/// target is refused (see [`resolve_confined`]).
fn resolve_parent<'a>(
    root: &str,
    rel: &'a str,
    abs: &str,
    own_links: &BTreeSet<String>,
    own_link_inos: &BTreeSet<u64>,
) -> Result<(Arc<dyn VfsNode>, &'a str, String), InstallError> {
    let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
    if comps.is_empty() {
        return Err(vfs_err("resolve", abs, VfsError::InvalidArgument));
    }
    let (dirs, last) = comps.split_at(comps.len() - 1);
    let filename = last[0];

    // Join the parent components into the root-relative path the walker resolves.
    // A top-level member has none: the walker returns the install-root node.
    let mut dir_rel = String::new();
    for (i, comp) in dirs.iter().enumerate() {
        if i > 0 {
            dir_rel.push('/');
        }
        dir_rel.push_str(comp);
    }
    let (dir, walked) = resolve_confined(root, &dir_rel, abs, true, true, own_links, own_link_inos)
        .map_err(|failure| resolve_failure("resolve", abs, failure))?;
    if !dir.is_directory() {
        return Err(vfs_err("resolve", abs, VfsError::InvalidArgument));
    }
    // The walked spelling is the resolved root-relative parent directory; a
    // member materialized under a pre-existing redirector differs from `dir_rel`
    // (F-r3-vfs-1 records both spellings in `own_links`).
    Ok((dir, filename, walked))
}

/// Map a confined-resolution failure onto the public [`InstallError`] (with one
/// structured diagnostic naming the stage and path — R12.4, R12.5).
fn resolve_failure(stage: &str, path: &str, failure: ResolveFailure) -> InstallError {
    match failure {
        ResolveFailure::NotFound => vfs_err(stage, path, VfsError::NotFound),
        ResolveFailure::Unsafe(reason) => unsafe_path(path, reason),
        ResolveFailure::Vfs(e) => vfs_err(stage, path, e),
        ResolveFailure::NoSpace => no_space("mkdir", path),
    }
}

/// Map a walk failure onto a VFS error for diagnostics.
fn walk_err(e: crate::vfs::link_walk::WalkError) -> VfsError {
    use crate::vfs::link_walk::WalkError;
    match e {
        WalkError::NotFound => VfsError::NotFound,
        // `AccessDenied` is unreachable from this caller (the installer uses
        // the unchecked walker: it is a privileged kernel action, like boot
        // provisioning), but the match is exhaustive so a future credential-
        // aware installer cannot silently treat it as "not found".
        WalkError::AccessDenied => VfsError::InvalidArgument,
        WalkError::NotDir | WalkError::TooManyLinks | WalkError::TooLong => {
            VfsError::InvalidArgument
        }
    }
}

/// Join the installation root and a normalized relative path into an absolute path
/// for diagnostics and the [`InstallError::NoSpace`] payload.
fn join_abs(root: &str, rel: &str) -> String {
    let mut s = String::from(root.trim_end_matches('/'));
    s.push('/');
    s.push_str(rel);
    s
}

/// Build a [`InstallError::NoSpace`] and emit the single structured diagnostic for it
/// (R10.4, R12.4, R12.5).
fn no_space(stage: &str, path: &str) -> InstallError {
    crate::error!(
        "Package_Installer: stage={} path={} cause=NoSpace",
        stage,
        path
    );
    InstallError::NoSpace {
        path: String::from(path),
    }
}

/// Build a [`InstallError::Vfs`] and emit the single structured diagnostic for it
/// (R12.4, R12.5).
fn vfs_err(stage: &str, path: &str, e: VfsError) -> InstallError {
    crate::error!(
        "Package_Installer: stage={} path={} cause=Vfs({:?})",
        stage,
        path,
        e
    );
    InstallError::Vfs(e)
}

/// Build an [`InstallError::UnsafePath`] and emit the single structured diagnostic
/// for it (F-x86n-1, R12.4, R12.5).
fn unsafe_path(path: &str, reason: &'static str) -> InstallError {
    crate::error!(
        "Package_Installer: stage=confine path={} cause=UnsafePath reason={}",
        path,
        reason
    );
    InstallError::UnsafePath {
        path: String::from(path),
        reason,
    }
}

/// [`unsafe_path`] for a refusal decided by the pure planner, naming the archive
/// symlink the member descends through.
fn unsafe_path_with_link(path: &str, reason: &'static str, link: &str) -> InstallError {
    crate::error!(
        "Package_Installer: stage=plan path={} cause=UnsafePath link={} reason={}",
        path,
        link,
        reason
    );
    InstallError::UnsafePath {
        path: String::from(path),
        reason,
    }
}
