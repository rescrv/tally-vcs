//! §2 The loose format: the logical content, laid out on a filesystem.
//!
//! The interchange form, the emergency form, and the definition of truth.
//! `blobs/`, `forks/*/log.jsonl`, and `anchors/` are append-only or
//! immutable (I3); `index/` is a cache and deleting it is always safe.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::blobs::{BlobStore, fsync_dir};
use crate::fork::{ForkFile, validate_fork_name};
use crate::ident::{ElementRecord, Sum};
use crate::ignore::Ignore;
use crate::log::{Annotation, LogLine, Provenance, ViewSpan, last_state_position,
                 parse_log_lenient};
use crate::manifest::Manifest;
use crate::patch::{Intent, Realization, apply_intent, apply_realized_to_manifest,
                   apply_realized_to_sum};
use crate::{Error, Result, ioerr};

/// The contents of `.abelian/version`.
pub const VERSION: &str = "abelian v0\n";

/// A loose repository: a working tree with a `.abelian/` beside it.
pub struct Repository {
    root: PathBuf,
    dot: PathBuf,
}

/// Exclusive possession of a fork, backed by an OS lock on `forks/<f>/lock`.
/// One writer per fork log (I8); a fork is a session, so this is the model,
/// not a concession.
pub struct ForkLock {
    file: fs::File,
}

impl Drop for ForkLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl Repository {
    /// The working tree root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Initialize a repository at `root` with an empty `main` fork.
    pub fn init(root: impl Into<PathBuf>) -> Result<Self> {
        let repo = Repository::init_bare(root)?;
        repo.create_fork_raw("main", &ForkFile::empty())?;
        Ok(repo)
    }

    /// Initialize the layout with no forks (unpack restores its own).
    pub fn init_bare(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let dot = root.join(".abelian");
        if dot.exists() {
            return Err(Error::Invalid(format!(
                "already a repository: {}",
                dot.display()
            )));
        }
        fs::create_dir_all(&dot).map_err(ioerr("creating .abelian"))?;
        fs::write(dot.join("version"), VERSION).map_err(ioerr("writing version"))?;
        for sub in ["forks", "anchors", "index"] {
            fs::create_dir_all(dot.join(sub)).map_err(ioerr("creating layout"))?;
        }
        BlobStore::init(dot.join("blobs"))?;
        let repo = Repository { root, dot };
        repo.write_anchor_manifest(&Manifest::new())?;
        Ok(repo)
    }

    /// Restore a fork from its parts, byte-preserved (I4): the exact
    /// `log.jsonl` bytes an unpack produced.  The anchor manifest must
    /// already exist.
    pub fn restore_fork(&self, name: &str, fork_file: &ForkFile, log_bytes: &[u8]) -> Result<()> {
        self.create_fork_raw(name, fork_file)?;
        let path = self.log_path(name);
        fs::write(&path, log_bytes).map_err(ioerr("restoring fork log"))?;
        fsync_dir(self.fork_dir(name).as_path())?;
        Ok(())
    }

    /// The exact bytes of a fork's log (for byte-preserved packing).
    pub fn log_bytes(&self, fork: &str) -> Result<Vec<u8>> {
        validate_fork_name(fork)?;
        fs::read(self.log_path(fork)).map_err(ioerr(format!("reading log bytes of {fork}")))
    }

    /// Open a repository whose working tree is `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let dot = root.join(".abelian");
        let version = fs::read_to_string(dot.join("version"))
            .map_err(ioerr(format!("reading {}/version", dot.display())))?;
        if version != VERSION {
            return Err(Error::Corrupt(format!("unsupported version: {version:?}")));
        }
        Ok(Repository { root, dot })
    }

    /// Walk up from `start` to find a repository.
    pub fn discover(start: impl Into<PathBuf>) -> Result<Self> {
        let mut dir = start.into();
        loop {
            if dir.join(".abelian").join("version").exists() {
                return Repository::open(dir);
            }
            if !dir.pop() {
                return Err(Error::Invalid(
                    "not inside an abelian repository (no .abelian found)".to_string(),
                ));
            }
        }
    }

    /// The blob pool.
    pub fn blobs(&self) -> BlobStore {
        BlobStore::open(self.dot.join("blobs"))
    }

    //////////////////////////////////////// anchors //////////////////////////////////////////

    /// Path of the anchor manifest for `sum`.
    fn anchor_path(&self, sum_hex: &str) -> PathBuf {
        self.dot.join("anchors").join(format!("{sum_hex}.manifest"))
    }

    /// Write (put-if-absent) an anchor manifest; returns its sum.
    pub fn write_anchor_manifest(&self, manifest: &Manifest) -> Result<String> {
        let sum_hex = manifest.sum().hexdigest();
        let path = self.anchor_path(&sum_hex);
        if !path.exists() {
            fs::write(&path, manifest.to_bytes()).map_err(ioerr("writing anchor manifest"))?;
            fsync_dir(self.dot.join("anchors").as_path())?;
        }
        Ok(sum_hex)
    }

    /// Read and verify the anchor manifest for `sum`.
    pub fn read_anchor_manifest(&self, sum_hex: &str) -> Result<Manifest> {
        let bytes = fs::read(self.anchor_path(sum_hex))
            .map_err(ioerr(format!("reading anchor manifest {sum_hex}")))?;
        let manifest = Manifest::parse(&bytes)?;
        if manifest.sum().hexdigest() != sum_hex {
            return Err(Error::Corrupt(format!(
                "anchor manifest {sum_hex} contains a different state"
            )));
        }
        Ok(manifest)
    }

    ///////////////////////////////////////// forks ///////////////////////////////////////////

    fn fork_dir(&self, name: &str) -> PathBuf {
        self.dot.join("forks").join(name)
    }

    /// List fork names.
    pub fn fork_names(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let entries =
            fs::read_dir(self.dot.join("forks")).map_err(ioerr("listing forks"))?;
        for entry in entries {
            let entry = entry.map_err(ioerr("listing forks"))?;
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
        names.sort();
        Ok(names)
    }

    pub(crate) fn create_fork_raw(&self, name: &str, fork: &ForkFile) -> Result<()> {
        validate_fork_name(name)?;
        let dir = self.fork_dir(name);
        if dir.exists() {
            return Err(Error::Invalid(format!("fork already exists: {name}")));
        }
        fs::create_dir_all(&dir).map_err(ioerr("creating fork directory"))?;
        fs::write(dir.join("fork"), fork.to_bytes()).map_err(ioerr("writing fork file"))?;
        fs::write(dir.join("log.jsonl"), b"").map_err(ioerr("creating log"))?;
        Ok(())
    }

    /// Create a fork anchored at another fork's current state (§6: an
    /// anchor and an empty log — that is the whole file).
    pub fn create_fork(&self, name: &str, from_fork: &str) -> Result<ForkFile> {
        let state = self.current_state(from_fork)?;
        let anchor_hex = self.write_anchor_manifest(&state.manifest)?;
        let fork = ForkFile { anchor: anchor_hex.clone(), manifest: anchor_hex };
        self.create_fork_raw(name, &fork)?;
        Ok(fork)
    }

    /// Read a fork file.
    pub fn read_fork(&self, name: &str) -> Result<ForkFile> {
        validate_fork_name(name)?;
        let bytes = fs::read(self.fork_dir(name).join("fork"))
            .map_err(ioerr(format!("reading fork {name}")))?;
        ForkFile::parse(&bytes)
    }

    /// Remove a fork: delete `forks/<name>/` and everything under it.
    ///
    /// Like `git branch -d`, this refuses when the fork carries log lines no
    /// other fork's log has taken up, so deleting it would drop the only copy
    /// of that work; `force` is the `-D` escape hatch that deletes anyway.  A
    /// line counts as taken up when another fork carries it outright or
    /// carries a union line whose `origin` names it (union re-seals ids, so
    /// the origin is the linkage, §2.5).  Views are log lines (§2.6), so an
    /// uncarried view refuses too — a fused rendering is never silently
    /// dropped.  The `main` fork is never removable — the empty repository
    /// always has it (§2.3).
    pub fn remove_fork(&self, name: &str, force: bool) -> Result<()> {
        validate_fork_name(name)?;
        if name == "main" {
            return Err(Error::Invalid("refusing to remove the main fork".to_string()));
        }
        let dir = self.fork_dir(name);
        if !dir.exists() {
            return Err(Error::Invalid(format!("no such fork: {name}")));
        }
        if !force {
            let mine = self.current_state(name)?;
            if !mine.lines.is_empty() {
                let mut carried = BTreeSet::new();
                for other in self.fork_names()? {
                    if other == name {
                        continue;
                    }
                    for line in self.current_state(&other)?.lines {
                        if let Some(origin) = &line.annotation.origin
                            && origin.fork == name
                        {
                            carried.insert(origin.id.clone());
                        }
                        carried.insert(line.id);
                    }
                }
                let unmerged = mine.lines.iter().filter(|l| !carried.contains(&l.id)).count();
                if unmerged > 0 {
                    return Err(Error::Invalid(format!(
                        "fork {name} has {unmerged} line(s) not merged into another fork; \
                         pass --force to delete it anyway"
                    )));
                }
            }
        }
        // Take the lock so no writer is mid-append, then drop it before the
        // directory (and the lock file with it) goes away.
        {
            let _lock = self.lock_fork(name)?;
        }
        fs::remove_dir_all(&dir).map_err(ioerr(format!("removing fork {name}")))?;
        fsync_dir(self.dot.join("forks").as_path())?;
        Ok(())
    }

    /// Take the fork's writer lock (I8: one writer per fork log).
    pub fn lock_fork(&self, name: &str) -> Result<ForkLock> {
        let path = self.fork_dir(name).join("lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(ioerr("opening fork lock"))?;
        file.lock().map_err(ioerr("locking fork"))?;
        Ok(ForkLock { file })
    }

    ////////////////////////////////////////// log ////////////////////////////////////////////

    fn log_path(&self, fork: &str) -> PathBuf {
        self.fork_dir(fork).join("log.jsonl")
    }

    /// Read the fork's log, recovering from a torn final line by truncating
    /// it as never-committed (§2.7 crash recovery).
    pub fn read_log(&self, fork: &str) -> Result<Vec<LogLine>> {
        let path = self.log_path(fork);
        let bytes =
            fs::read(&path).map_err(ioerr(format!("reading log of fork {fork}")))?;
        let parsed = parse_log_lenient(&bytes)?;
        if parsed.valid_prefix != bytes.len() {
            let file = fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(ioerr("opening log for truncation"))?;
            file.set_len(parsed.valid_prefix as u64)
                .map_err(ioerr("truncating torn log line"))?;
            file.sync_all().map_err(ioerr("fsyncing truncated log"))?;
        }
        Ok(parsed.lines)
    }

    ///////////////////////////////////////// state ///////////////////////////////////////////

    /// The current state of a fork: the anchor manifest plus the replay of
    /// its log (§2.3).
    pub fn current_state(&self, fork: &str) -> Result<ForkState> {
        let fork_file = self.read_fork(fork)?;
        let anchor_manifest = self.read_anchor_manifest(&fork_file.manifest)?;
        if anchor_manifest.sum().hexdigest() != fork_file.anchor {
            return Err(Error::Corrupt(format!(
                "fork {fork}: anchor manifest sum disagrees with anchor"
            )));
        }
        let lines = self.read_log(fork)?;
        // The anchor may have been repointed by a snapshot; earlier log
        // lines remain (§2.4).  Replay starts after the last line whose
        // sum_after equals the anchor, or at the beginning.
        let start = last_state_position(&lines, &fork_file.anchor)
            .map(|i| i + 1)
            .unwrap_or(0);
        // Verify linkage and per-line arithmetic across the whole log.
        let mut prev = String::new();
        let mut sums: Vec<Sum> = Vec::with_capacity(lines.len());
        for (i, line) in lines.iter().enumerate() {
            if line.prev != prev {
                return Err(Error::Corrupt(format!(
                    "fork {fork} line {i} ({}): prev {} does not chain",
                    line.id, line.prev
                )));
            }
            prev = line.id.clone();
            if i > 0 {
                let expect =
                    apply_realized_to_sum(&sums[i - 1], &line.realized)?.hexdigest();
                if expect != line.sum_after {
                    return Err(Error::Corrupt(format!(
                        "fork {fork} line {i} ({}): sum_after {} disagrees with \
                         arithmetic {expect}; history stops being trustworthy here",
                        line.id, line.sum_after
                    )));
                }
            }
            sums.push(Sum::from_hexdigest(&line.sum_after)?);
        }
        if start == 0
            && let Some(first) = lines.first()
        {
            let expect = apply_realized_to_sum(
                &Sum::from_hexdigest(&fork_file.anchor)?,
                &first.realized,
            )?;
            if expect.hexdigest() != first.sum_after {
                return Err(Error::Corrupt(format!(
                    "fork {fork} line 0 ({}): sum_after disagrees with anchor arithmetic",
                    first.id
                )));
            }
        }
        let mut manifest = anchor_manifest;
        for line in &lines[start..] {
            apply_realized_to_manifest(&mut manifest, &line.realized)?;
        }
        let sum = manifest.sum();
        if let Some(last) = lines.last()
            && last.sum_after != sum.hexdigest()
        {
            return Err(Error::Corrupt(format!(
                "fork {fork}: replayed state {} disagrees with final sum_after {}",
                sum.hexdigest(),
                last.sum_after
            )));
        }
        let head_id = lines.last().map(|l| l.id.clone()).unwrap_or_default();
        Ok(ForkState { manifest, sum, head_id, lines })
    }

    /// The manifest at an arbitrary sum on a fork's history, found by
    /// forward or backward (inverse) replay — undo is the inverse.
    pub fn manifest_at(&self, fork: &str, sum_hex: &str) -> Result<Manifest> {
        let fork_file = self.read_fork(fork)?;
        let state = self.current_state(fork)?;
        if state.sum.hexdigest() == sum_hex {
            return Ok(state.manifest);
        }
        if fork_file.anchor == sum_hex {
            return self.read_anchor_manifest(&fork_file.manifest);
        }
        let pos = last_state_position(&state.lines, sum_hex).ok_or_else(|| {
            Error::Invalid(format!("sum {sum_hex} does not name a state on fork {fork}"))
        })?;
        // Walk backward from the current state, applying inverses.
        let mut manifest = state.manifest;
        for line in state.lines[pos + 1..].iter().rev() {
            for entry in line.realized.iter().rev() {
                if let Some(added) = entry.added()? {
                    manifest.remove(&added)?;
                }
                if let Some(removed) = entry.removed()? {
                    manifest.insert(removed)?;
                }
            }
        }
        if manifest.sum().hexdigest() != sum_hex {
            return Err(Error::Corrupt(format!(
                "inverse replay to {sum_hex} produced a different state"
            )));
        }
        Ok(manifest)
    }

    ////////////////////////////////////////// apply //////////////////////////////////////////

    /// §2.7 Apply: the seven steps, with the fsync'd log append as the sole
    /// linearization point (I8).
    pub fn apply(&self, fork: &str, intent: Intent, annotation: Annotation) -> Result<LogLine> {
        // 0. flock forks/<f>/lock
        let _lock = self.lock_fork(fork)?;
        // 1. LOAD current manifest.
        let state = self.current_state(fork)?;
        let mut manifest = state.manifest.clone();
        // 2. VALIDATE every op; any failure → write nothing.
        let realization: Realization =
            apply_intent(&intent, &mut manifest, &self.blobs())?;
        // 3. WRITE new blobs (tmp+rename+fsync); idempotent, uncommitted.
        let blobs = self.blobs();
        for (hash, content) in &realization.new_blobs {
            let written = blobs.put(content)?;
            debug_assert_eq!(&written, hash);
        }
        // 4. REALIZE deltas; fold the sum.
        let sum_after = apply_realized_to_sum(&state.sum, &realization.realized)?;
        let mut line = LogLine {
            id: String::new(),
            prev: state.head_id.clone(),
            intent,
            realized: realization.realized,
            sum_after: sum_after.hexdigest(),
            committed_ms: 0,
            annotation,
        };
        let bytes = line.seal(&blobs)?;
        // 5. APPEND log line; fsync file, fsync directory ← LINEARIZATION POINT.
        let path = self.log_path(fork);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(ioerr("opening log for append"))?;
        file.write_all(&bytes).map_err(ioerr("appending log line"))?;
        file.sync_all().map_err(ioerr("fsyncing log"))?;
        fsync_dir(self.fork_dir(fork).as_path())?;
        // 6. REFRESH working tree; best-effort, derived.
        let _ = self.refresh_working_tree(&line);
        // 7. unlock (drop).
        Ok(line)
    }

    /// Land an already-realized delta (union stratum 2): membership plus
    /// addition, no content inspection.  The realized delta is validated
    /// against the current manifest (I9) and appended with the same
    /// durability protocol as [`Repository::apply`].
    pub fn apply_realized(
        &self,
        fork: &str,
        intent: Intent,
        realized: Vec<crate::patch::RealizedEntry>,
        annotation: Annotation,
    ) -> Result<LogLine> {
        let _lock = self.lock_fork(fork)?;
        let state = self.current_state(fork)?;
        let mut manifest = state.manifest.clone();
        // Membership check against the target manifest, always (I9).
        apply_realized_to_manifest(&mut manifest, &realized)?;
        let blobs = self.blobs();
        for entry in &realized {
            if let Some(added) = entry.added()?
                && !blobs.has(&added.blob)?
            {
                return Err(Error::Precondition(format!(
                    "realized add references absent blob {}",
                    added.blob
                )));
            }
        }
        let sum_after = apply_realized_to_sum(&state.sum, &realized)?;
        let mut line = LogLine {
            id: String::new(),
            prev: state.head_id.clone(),
            intent,
            realized,
            sum_after: sum_after.hexdigest(),
            committed_ms: 0,
            annotation,
        };
        let bytes = line.seal(&blobs)?;
        let path = self.log_path(fork);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(ioerr("opening log for append"))?;
        file.write_all(&bytes).map_err(ioerr("appending log line"))?;
        file.sync_all().map_err(ioerr("fsyncing log"))?;
        fsync_dir(self.fork_dir(fork).as_path())?;
        let _ = self.refresh_working_tree(&line);
        Ok(line)
    }

    /// Best-effort working-tree refresh for one applied line.
    fn refresh_working_tree(&self, line: &LogLine) -> Result<()> {
        let blobs = self.blobs();
        for entry in &line.realized {
            let removed = entry.removed()?;
            let added = entry.added()?;
            if let Some(added) = added {
                self.write_tree_file(&blobs, &added)?;
            } else if let Some(removed) = removed {
                let _ = fs::remove_file(self.tree_path(&removed.path));
            }
        }
        Ok(())
    }

    fn tree_path(&self, element_path: &str) -> PathBuf {
        self.root.join(element_path.trim_start_matches('/'))
    }

    fn write_tree_file(&self, blobs: &BlobStore, record: &ElementRecord) -> Result<()> {
        let dst = self.tree_path(&record.path);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(ioerr("creating working tree directory"))?;
        }
        let content = blobs.get(&record.blob)?;
        if record.mode == "120000" {
            let _ = fs::remove_file(&dst);
            #[cfg(unix)]
            {
                let target = String::from_utf8(content).map_err(|_| {
                    Error::Corrupt(format!("symlink target not UTF-8: {}", record.path))
                })?;
                std::os::unix::fs::symlink(target, &dst)
                    .map_err(ioerr("writing symlink"))?;
            }
            return Ok(());
        }
        fs::write(&dst, &content).map_err(ioerr("writing working tree file"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if record.mode == "100755" { 0o755 } else { 0o644 };
            fs::set_permissions(&dst, fs::Permissions::from_mode(mode))
                .map_err(ioerr("setting working tree mode"))?;
        }
        Ok(())
    }

    /// Materialize a full working tree for `manifest` under the repository
    /// root (`abelian materialize`).
    pub fn materialize(&self, manifest: &Manifest) -> Result<()> {
        let blobs = self.blobs();
        for record in manifest.records() {
            self.write_tree_file(&blobs, record)?;
        }
        Ok(())
    }

    /// §1 by hand, automated: walk the working tree and produce every
    /// element record (`abelian sum`).  Blob contents are ingested into the
    /// pool so the records are always materializable.
    pub fn records_of_working_tree(&self) -> Result<Vec<ElementRecord>> {
        let ignore = match fs::read_to_string(self.root.join(".abelianignore")) {
            Ok(text) => Ignore::parse(&text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ignore::empty(),
            Err(err) => return Err(ioerr("reading .abelianignore")(err)),
        };
        let mut records = Vec::new();
        self.walk_tree(&self.root.clone(), &ignore, &mut records)?;
        records.sort();
        Ok(records)
    }

    fn walk_tree(
        &self,
        dir: &Path,
        ignore: &Ignore,
        records: &mut Vec<ElementRecord>,
    ) -> Result<()> {
        let entries = fs::read_dir(dir).map_err(ioerr(format!("walking {}", dir.display())))?;
        for entry in entries {
            let entry = entry.map_err(ioerr("walking working tree"))?;
            let path = entry.path();
            let name = entry.file_name();
            if dir == self.root && (name == ".abelian" || name == ".git") {
                continue;
            }
            let meta = fs::symlink_metadata(&path).map_err(ioerr("stat in working tree"))?;
            let rel = path
                .strip_prefix(&self.root)
                .map_err(|_| Error::Invalid("walk escaped the root".to_string()))?;
            let element_path = format!(
                "/{}",
                rel.to_str().ok_or_else(|| Error::Invalid(format!(
                    "non-UTF-8 path in working tree: {}",
                    rel.display()
                )))?
            );
            if ignore.is_ignored(&element_path[1..], meta.is_dir()) {
                continue;
            }
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&path).map_err(ioerr("readlink"))?;
                let target = target.to_str().ok_or_else(|| {
                    Error::Invalid(format!("non-UTF-8 symlink target: {}", path.display()))
                })?;
                let blob = self.blobs().put(target.as_bytes())?;
                records.push(ElementRecord::new("120000", &element_path, &blob)?);
            } else if meta.is_dir() {
                self.walk_tree(&path, ignore, records)?;
            } else {
                let content = fs::read(&path).map_err(ioerr("reading working tree file"))?;
                let blob = self.blobs().put(&content)?;
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    if meta.permissions().mode() & 0o111 != 0 { "100755" } else { "100644" }
                };
                #[cfg(not(unix))]
                let mode = "100644";
                records.push(ElementRecord::new(mode, &element_path, &blob)?);
            }
        }
        Ok(())
    }

    /// `abelian check`: recompute the working tree's sum and compare against
    /// the log's expectation.
    pub fn check(&self, fork: &str) -> Result<(Sum, Sum)> {
        let expected = self.current_state(fork)?.sum;
        let mut actual = Sum::zero();
        for record in self.records_of_working_tree()? {
            actual.insert(&record.to_bytes());
        }
        Ok((expected, actual))
    }

    /// `abelian snapshot`: write a manifest at the current state and repoint
    /// the fork file at it; earlier log lines remain (§2.4).
    pub fn snapshot(&self, fork: &str) -> Result<String> {
        let _lock = self.lock_fork(fork)?;
        let state = self.current_state(fork)?;
        let sum_hex = self.write_anchor_manifest(&state.manifest)?;
        let fork_file = ForkFile { anchor: sum_hex.clone(), manifest: sum_hex.clone() };
        fs::write(self.fork_dir(fork).join("fork"), fork_file.to_bytes())
            .map_err(ioerr("repointing fork file"))?;
        Ok(sum_hex)
    }

    ///////////////////////////////////////// views ///////////////////////////////////////////

    /// §2.6 Fuse: append a view line — provenance `view`, a span
    /// annotation, and an empty realized delta (an arithmetic identity), so
    /// it is a log line like any other: it travels through union, it counts
    /// as unmerged work, and it is ordered, so a later view supersedes an
    /// earlier one it overlaps.  A rendering, never a mutation: the fused
    /// lines remain underneath.
    pub fn fuse(
        &self,
        fork: &str,
        from: &str,
        to: &str,
        prose: Option<String>,
        author: &str,
    ) -> Result<LogLine> {
        let state = self.current_state(fork)?;
        let index_of = |id: &str| state.lines.iter().position(|l| l.id == id);
        let (Some(a), Some(b)) = (index_of(from), index_of(to)) else {
            return Err(Error::Invalid(format!(
                "fuse span {from}..{to} does not name lines on fork {fork}"
            )));
        };
        if a > b {
            return Err(Error::Invalid(format!(
                "fuse span {from}..{to} is reversed on fork {fork}"
            )));
        }
        let annotation = Annotation {
            author: author.to_string(),
            provenance: Provenance::View,
            prose,
            view: Some(ViewSpan { from: from.to_string(), to: to.to_string() }),
            ..Annotation::default()
        };
        self.apply(fork, Intent::default(), annotation)
    }

    /// The blob hashes any fork reaches: every fork's anchor manifest
    /// records, every log line's realized adds (which name every blob ever
    /// added along the history), each line's spilled read set (`reads_blob`)
    /// and andon signature (`sig`).  This is the reachability root for
    /// [`Repository::gc_blobs`].
    pub fn referenced_blobs(&self) -> Result<BTreeSet<String>> {
        let mut referenced = BTreeSet::new();
        for fork in self.fork_names()? {
            let fork_file = self.read_fork(&fork)?;
            for record in self.read_anchor_manifest(&fork_file.manifest)?.records() {
                referenced.insert(record.blob.clone());
            }
            for line in self.current_state(&fork)?.lines {
                for entry in &line.realized {
                    if let Some(added) = entry.added()? {
                        referenced.insert(added.blob);
                    }
                }
                if let Some(sig) = &line.annotation.sig {
                    referenced.insert(sig.clone());
                }
                if let Some(reads) = &line.annotation.reads
                    && let Some(blob) = reads.get("reads_blob").and_then(|v| v.as_str())
                {
                    referenced.insert(blob.to_string());
                }
            }
        }
        Ok(referenced)
    }

    /// Collect blobs no fork reaches (§2.2).  The pool is otherwise
    /// append-only (I3); this is the one sanctioned reclamation.  It removes
    /// only unreachable content — e.g. file bytes `abelian sum` ingested for a
    /// working-tree state no fork ever committed.  Returns the hashes
    /// collected, or, when `dry_run`, those that would be.
    pub fn gc_blobs(&self, dry_run: bool) -> Result<Vec<String>> {
        let referenced = self.referenced_blobs()?;
        let blobs = self.blobs();
        let mut collected = Vec::new();
        for hash in blobs.list()? {
            if referenced.contains(&hash) {
                continue;
            }
            if !dry_run {
                blobs.remove(&hash)?;
            }
            collected.push(hash);
        }
        Ok(collected)
    }
}

/// A fork's current state: manifest, sum, head line id, and the verified
/// log lines that got there.
pub struct ForkState {
    /// The materialized current state.
    pub manifest: Manifest,
    /// Its identity.
    pub sum: Sum,
    /// The id of the last log line; `""` if the log is empty.
    pub head_id: String,
    /// The fork's verified log.
    pub lines: Vec<LogLine>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ident::sha3_hex;
    use crate::patch::Op;

    fn temp_repo(name: &str) -> Repository {
        let dir = std::env::temp_dir().join(format!("abelian-repo-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Repository::init(&dir).unwrap()
    }

    fn create(path: &str, content: &[u8]) -> Intent {
        Intent {
            ops: vec![Op::Create {
                path: path.to_string(),
                mode: "100644".to_string(),
                blob: None,
                content_b64: Some(crate::b64::encode(content)),
            }],
        }
    }

    fn edit(path: &str, old: &str, new: &str) -> Intent {
        Intent {
            ops: vec![Op::Edit {
                path: path.to_string(),
                old_str: old.to_string(),
                new_str: new.to_string(),
            }],
        }
    }

    fn note(author: &str) -> Annotation {
        Annotation { author: author.to_string(), ..Annotation::default() }
    }

    #[test]
    fn abelianignore_prunes_the_walk() {
        let repo = temp_repo("ignore");
        fs::write(repo.root().join(".abelianignore"), "*.log\n/target\n").unwrap();
        fs::create_dir_all(repo.root().join("target/debug")).unwrap();
        fs::create_dir_all(repo.root().join("src")).unwrap();
        fs::write(repo.root().join("target/debug/junk"), b"x").unwrap();
        fs::write(repo.root().join("src/main.rs"), b"fn main() {}\n").unwrap();
        fs::write(repo.root().join("src/debug.log"), b"noise").unwrap();
        let paths: Vec<String> = repo
            .records_of_working_tree()
            .unwrap()
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert_eq!(paths, vec!["/.abelianignore", "/src/main.rs"]);
    }

    #[test]
    fn init_yields_empty_main() {
        let repo = temp_repo("init");
        let state = repo.current_state("main").unwrap();
        assert!(state.manifest.is_empty());
        assert_eq!(state.sum, Sum::zero());
        assert_eq!(state.head_id, "");
    }

    #[test]
    fn apply_chains_and_refreshes_the_tree() {
        let repo = temp_repo("apply");
        let l1 = repo.apply("main", create("/src/main.rs", b"fn main() {}\n"), note("t")).unwrap();
        assert_eq!(l1.prev, "");
        let l2 = repo
            .apply("main", edit("/src/main.rs", "main()", "main() /* ed */"), note("t"))
            .unwrap();
        assert_eq!(l2.prev, l1.id);
        let state = repo.current_state("main").unwrap();
        assert_eq!(state.head_id, l2.id);
        assert_eq!(state.sum.hexdigest(), l2.sum_after);
        // The working tree reflects the applied state.
        let on_disk = fs::read(repo.root().join("src/main.rs")).unwrap();
        assert!(on_disk.windows(8).any(|w| w == b"/* ed */"));
        // And abelian check agrees.
        let (expected, actual) = repo.check("main").unwrap();
        assert_eq!(expected, actual);
    }

    #[test]
    fn failed_apply_writes_nothing() {
        let repo = temp_repo("atomic");
        repo.apply("main", create("/a", b"one two\n"), note("t")).unwrap();
        let before = repo.current_state("main").unwrap();
        assert!(repo.apply("main", edit("/a", "absent", "x"), note("t")).is_err());
        let after = repo.current_state("main").unwrap();
        assert_eq!(before.sum, after.sum);
        assert_eq!(before.lines.len(), after.lines.len());
    }

    #[test]
    fn snapshot_repoints_and_replay_still_works() {
        let repo = temp_repo("snapshot");
        repo.apply("main", create("/a", b"a\n"), note("t")).unwrap();
        repo.apply("main", create("/b", b"b\n"), note("t")).unwrap();
        let sum_before = repo.current_state("main").unwrap().sum;
        let anchor = repo.snapshot("main").unwrap();
        assert_eq!(anchor, sum_before.hexdigest());
        // Earlier log lines remain; state is unchanged.
        let state = repo.current_state("main").unwrap();
        assert_eq!(state.sum, sum_before);
        assert_eq!(state.lines.len(), 2, "log lines remain after snapshot");
        // And applying continues to work.
        repo.apply("main", create("/c", b"c\n"), note("t")).unwrap();
        assert_eq!(repo.current_state("main").unwrap().manifest.len(), 3);
    }

    #[test]
    fn manifest_at_walks_backward() {
        let repo = temp_repo("backward");
        let l1 = repo.apply("main", create("/a", b"a\n"), note("t")).unwrap();
        repo.apply("main", edit("/a", "a", "b"), note("t")).unwrap();
        let m1 = repo.manifest_at("main", &l1.sum_after).unwrap();
        assert_eq!(m1.sum().hexdigest(), l1.sum_after);
        let zeros = "0".repeat(64);
        let m0 = repo.manifest_at("main", &zeros).unwrap();
        assert!(m0.is_empty());
    }

    #[test]
    fn torn_log_line_is_recovered() {
        let repo = temp_repo("torn");
        repo.apply("main", create("/a", b"a\n"), note("t")).unwrap();
        let log_path = repo.fork_dir("main").join("log.jsonl");
        let mut bytes = fs::read(&log_path).unwrap();
        bytes.extend_from_slice(b"{\"id\":\"torn");
        fs::write(&log_path, &bytes).unwrap();
        let state = repo.current_state("main").unwrap();
        assert_eq!(state.lines.len(), 1);
        // The torn tail was truncated as never-committed.
        let recovered = fs::read(&log_path).unwrap();
        assert!(recovered.ends_with(b"\n"));
        assert!(!recovered.windows(4).any(|w| w == b"torn"));
    }

    #[test]
    fn forks_anchor_at_current_state() {
        let repo = temp_repo("fork");
        repo.apply("main", create("/a", b"a\n"), note("t")).unwrap();
        let sum = repo.current_state("main").unwrap().sum;
        let fork = repo.create_fork("session-1", "main").unwrap();
        assert_eq!(fork.anchor, sum.hexdigest());
        let state = repo.current_state("session-1").unwrap();
        assert_eq!(state.sum, sum);
        assert!(state.lines.is_empty());
        // Work on the fork is invisible to main.
        repo.apply("session-1", create("/b", b"b\n"), note("t")).unwrap();
        assert_eq!(repo.current_state("main").unwrap().manifest.len(), 1);
        assert_eq!(repo.current_state("session-1").unwrap().manifest.len(), 2);
    }

    #[test]
    fn remove_fork_refuses_unmerged_but_force_deletes() {
        let repo = temp_repo("rm-fork");
        repo.apply("main", create("/a", b"a\n"), note("t")).unwrap();
        repo.create_fork("scratch", "main").unwrap();
        repo.apply("scratch", create("/b", b"b\n"), note("t")).unwrap();
        // Unmerged work: -d equivalent refuses.
        assert!(repo.remove_fork("scratch", false).is_err());
        assert!(repo.fork_names().unwrap().contains(&"scratch".to_string()));
        // -D equivalent deletes it regardless.
        repo.remove_fork("scratch", true).unwrap();
        assert!(!repo.fork_names().unwrap().contains(&"scratch".to_string()));
    }

    #[test]
    fn remove_fork_allows_merged_and_empty() {
        let repo = temp_repo("rm-merged");
        repo.apply("main", create("/a", b"a\n"), note("t")).unwrap();
        // An empty fork carries no work: safe to remove without force.
        repo.create_fork("empty", "main").unwrap();
        repo.remove_fork("empty", false).unwrap();
        // A fork whose work is unioned into main is merged: safe too.
        repo.create_fork("done", "main").unwrap();
        repo.apply("done", create("/c", b"c\n"), note("t")).unwrap();
        crate::union::union(&repo, "done", "main", "maintainer").unwrap();
        repo.remove_fork("done", false).unwrap();
        assert_eq!(repo.fork_names().unwrap(), vec!["main".to_string()]);
    }

    #[test]
    fn remove_fork_never_drops_the_only_view() {
        // A view is a log line, so the unmerged-work check protects it: a
        // fused rendering is never silently deleted with its fork.
        let repo = temp_repo("rm-view");
        repo.create_fork("s", "main").unwrap();
        let l1 = repo.apply("s", create("/a", b"a\n"), note("t")).unwrap();
        crate::union::union(&repo, "s", "main", "maintainer").unwrap();
        // Fuse after the fact: the fork now holds the only copy of the view.
        repo.fuse("s", &l1.id, &l1.id, Some("beat".to_string()), "sid").unwrap();
        assert!(repo.remove_fork("s", false).is_err(), "the view is unmerged work");
        // Union carries the view; then removal is safe.
        crate::union::union(&repo, "s", "main", "maintainer").unwrap();
        repo.remove_fork("s", false).unwrap();
    }

    #[test]
    fn remove_fork_never_removes_main() {
        let repo = temp_repo("rm-main");
        assert!(repo.remove_fork("main", false).is_err());
        assert!(repo.remove_fork("main", true).is_err());
        assert!(repo.remove_fork("nope", false).is_err());
    }

    #[test]
    fn gc_blobs_collects_only_unreachable() {
        let repo = temp_repo("gc-blobs");
        // Committed content: reachable through main's log.
        repo.apply("main", create("/lib.rs", b"committed\n"), note("t")).unwrap();
        let committed = sha3_hex(b"committed\n");
        // A spilled read set is a root.
        let spilled = repo.blobs().put(b"[]").unwrap();
        let mut annotation = note("t");
        annotation.reads = Some(serde_json::json!({"reads_blob": spilled}));
        repo.apply("main", create("/read.rs", b"r\n"), annotation).unwrap();
        // Exhaust: bytes ingested into the pool but reachable from nowhere,
        // as `abelian sum` leaves for a working-tree file no fork committed.
        let orphan = repo.blobs().put(b"never committed\n").unwrap();

        let reachable = repo.referenced_blobs().unwrap();
        assert!(reachable.contains(&committed));
        assert!(reachable.contains(&spilled));
        assert!(!reachable.contains(&orphan));

        // Dry run names the orphan without removing it.
        assert_eq!(repo.gc_blobs(true).unwrap(), vec![orphan.clone()]);
        assert!(repo.blobs().has(&orphan).unwrap());
        // Collection removes only the orphan; roots survive.
        assert_eq!(repo.gc_blobs(false).unwrap(), vec![orphan.clone()]);
        assert!(!repo.blobs().has(&orphan).unwrap());
        assert!(repo.blobs().has(&committed).unwrap());
        assert!(repo.blobs().has(&spilled).unwrap());
        // Idempotent.
        assert!(repo.gc_blobs(false).unwrap().is_empty());
    }
}
