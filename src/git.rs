//! Git import: initializing an abelian repository from a git commit.
//!
//! An import walks the commit's tree, ingests every blob into the pool, and
//! anchors `main` at the resulting state.  The derivation is a pure function
//! of the commit's tree, so anyone holding the git repository can re-derive
//! the records and check them against the anchor manifest.  A zero-op first
//! log line records the provenance for humans: the ref as passed, the
//! resolved commit digest and its algorithm, and the tree oid.
//!
//! Compatibility is checked loudly before anything is written: only modes
//! `100644`, `100755`, and `120000` import (gitlinks/submodules do not);
//! paths must satisfy §1.1 and, in v0, must be ASCII, because NFC
//! normalization of arbitrary Unicode cannot be verified without tables.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::blobs::BlobStore;
use crate::fork::ForkFile;
use crate::ident::{ElementRecord, sha3_hex, validate_path};
use crate::log::{Annotation, GitImport, LogLine};
use crate::manifest::Manifest;
use crate::patch::{Intent, RealizedEntry, apply_realized_to_sum};
use crate::repo::Repository;
use crate::{Error, Result, ioerr};

/// Run `git -C dir args…`, loudly surfacing stderr on failure.
fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(ioerr("running git"))?;
    if !output.status.success() {
        return Err(Error::Invalid(format!(
            "git {} failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

/// `git rev-parse --verify spec`, validated as a hex object name.
fn rev_parse(git_dir: &Path, spec: &str) -> Result<String> {
    let out = git(git_dir, &["rev-parse", "--verify", spec])?;
    let hex = String::from_utf8(out)
        .map_err(|_| Error::Corrupt("git rev-parse produced non-UTF-8".to_string()))?
        .trim()
        .to_string();
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Corrupt(format!("git rev-parse produced a non-hex name: {hex:?}")));
    }
    Ok(hex)
}

/// Resolve a committish to its full object name.
pub fn resolve_commit(git_dir: &Path, committish: &str) -> Result<String> {
    rev_parse(git_dir, &format!("{committish}^{{commit}}"))
}

/// Resolve a commit to its tree's object name.
pub fn resolve_tree(git_dir: &Path, commit: &str) -> Result<String> {
    rev_parse(git_dir, &format!("{commit}^{{tree}}"))
}

/// The linear chain of commits ending at `commit`: first-parent ancestry,
/// oldest first, truncated at (and including) the first merge commit or the
/// root commit.  The chain is a pure function of the commit, so everyone
/// who holds it derives the same chain.
pub fn linear_chain(git_dir: &Path, commit: &str) -> Result<Vec<String>> {
    let out = git(git_dir, &["rev-list", "--first-parent", "--parents", commit])?;
    let text = String::from_utf8(out)
        .map_err(|_| Error::Corrupt("git rev-list produced non-UTF-8".to_string()))?;
    let mut chain = Vec::new();
    for line in text.lines() {
        let mut names = line.split(' ');
        let Some(hex) = names.next() else { continue };
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Corrupt(format!(
                "git rev-list produced a non-hex name: {hex:?}"
            )));
        }
        chain.push(hex.to_string());
        // Zero parents is the root; two or more is a merge.  Either way,
        // linear history stops here, this commit included.
        if names.count() != 1 {
            break;
        }
    }
    chain.reverse();
    Ok(chain)
}

/// The first-parent commits strictly after `base` up to and including
/// `commit`, oldest first.  Empty when `commit == base` (already current).
///
/// This is the fast-forward range: it errors unless `base` lies on
/// `commit`'s first-parent ancestry, so the import can only advance along
/// the same line the earlier import walked — never across a fork or a
/// history rewrite.
pub fn first_parent_since(git_dir: &Path, base: &str, commit: &str) -> Result<Vec<String>> {
    if base == commit {
        return Ok(Vec::new());
    }
    // `base` must be on `commit`'s first-parent line, or this is not a
    // fast-forward.  `commit`'s full first-parent ancestry is the ground
    // truth: excluding by all-ancestry (as `base..commit` does) would count
    // a `base` merged in through a side parent, which is not an advance of
    // this line.
    let full = git(git_dir, &["rev-list", "--first-parent", commit])?;
    let full = String::from_utf8(full)
        .map_err(|_| Error::Corrupt("git rev-list produced non-UTF-8".to_string()))?;
    if !full.lines().any(|line| line == base) {
        return Err(Error::Invalid(format!(
            "not a fast-forward: {base} is not on the first-parent history of {commit}"
        )));
    }
    // Everything on `commit`'s first-parent line that is newer than `base`,
    // oldest first.
    let out = git(
        git_dir,
        &["rev-list", "--first-parent", "--reverse", &format!("{base}..{commit}")],
    )?;
    let text = String::from_utf8(out)
        .map_err(|_| Error::Corrupt("git rev-list produced non-UTF-8".to_string()))?;
    let mut chain = Vec::new();
    for line in text.lines() {
        let hex = line.trim();
        if hex.is_empty() {
            continue;
        }
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Corrupt(format!(
                "git rev-list produced a non-hex name: {hex:?}"
            )));
        }
        chain.push(hex.to_string());
    }
    Ok(chain)
}

/// A commit's committer time (milliseconds), author (`Name <email>`), and
/// subject line — the deterministic annotation material an import stamps.
/// Committer time (ms), author, and the *full* commit message (`%B`: subject
/// and body).  `%B` is the raw body, so a multi-line message round-trips
/// verbatim; only git's trailing newline is trimmed.
fn commit_meta(git_dir: &Path, commit: &str) -> Result<(u64, String, String)> {
    let out = git(git_dir, &["show", "-s", "--format=%ct%x00%an <%ae>%x00%B", commit])?;
    let text = String::from_utf8(out)
        .map_err(|_| Error::Corrupt("git show produced non-UTF-8".to_string()))?;
    let text = text.trim_end_matches('\n');
    let mut parts = text.splitn(3, '\0');
    let (Some(seconds), Some(author), Some(message)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err(Error::Corrupt(format!("git show produced bad metadata: {text:?}")));
    };
    let seconds: u64 = seconds
        .parse()
        .map_err(|_| Error::Corrupt(format!("bad committer time: {seconds:?}")))?;
    Ok((seconds * 1000, author.to_string(), message.to_string()))
}

/// The git repository's object hash algorithm (`sha1` or `sha256`).
pub fn object_format(git_dir: &Path) -> Result<String> {
    let out = git(git_dir, &["rev-parse", "--show-object-format"])?;
    let name = String::from_utf8(out)
        .map_err(|_| Error::Corrupt("git rev-parse produced non-UTF-8".to_string()))?
        .trim()
        .to_string();
    if name.is_empty() {
        return Err(Error::Corrupt("git reported an empty object format".to_string()));
    }
    Ok(name)
}

/// One entry of a commit's tree, validated as importable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitEntry {
    /// The abelian mode: `100644`, `100755`, or `120000`.
    pub mode: String,
    /// The git object name of the blob.
    pub oid: String,
    /// The element path, absolute from the repository root.
    pub path: String,
}

/// Map a git tree path to an element path, erroring loudly on anything
/// §1.1 cannot carry.
fn element_path(git_path: &str) -> Result<String> {
    if !git_path.is_ascii() {
        return Err(Error::Invalid(format!(
            "incompatible git path (non-ASCII; v0 cannot verify NFC): {git_path:?}"
        )));
    }
    let path = format!("/{git_path}");
    validate_path(&path)?;
    let first = path[1..].split('/').next().unwrap_or("");
    if first == ".abelian" || first == ".git" {
        return Err(Error::Invalid(format!(
            "incompatible git path (reserved component): {path:?}"
        )));
    }
    Ok(path)
}

/// The importable entries of `commit`'s tree, or a loud error naming the
/// first incompatible one.  Nothing is written.
pub fn commit_entries(git_dir: &Path, commit: &str) -> Result<Vec<GitEntry>> {
    let out = git(git_dir, &["ls-tree", "-r", "-z", commit])?;
    let mut entries = Vec::new();
    for chunk in out.split(|b| *b == 0) {
        if chunk.is_empty() {
            continue;
        }
        // <mode> SP <type> SP <oid> TAB <path>
        let tab = chunk
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| Error::Corrupt("git ls-tree entry has no TAB".to_string()))?;
        let meta = std::str::from_utf8(&chunk[..tab])
            .map_err(|_| Error::Corrupt("git ls-tree metadata is not UTF-8".to_string()))?;
        let path_bytes = &chunk[tab + 1..];
        let git_path = std::str::from_utf8(path_bytes).map_err(|_| {
            Error::Invalid(format!(
                "incompatible git path (not UTF-8): {:?}",
                String::from_utf8_lossy(path_bytes)
            ))
        })?;
        let fields: Vec<&str> = meta.split(' ').collect();
        let [mode, typ, oid] = fields[..] else {
            return Err(Error::Corrupt(format!("bad git ls-tree entry: {meta:?}")));
        };
        match (mode, typ) {
            ("100644" | "100755" | "120000", "blob") => {}
            ("160000", _) => {
                return Err(Error::Invalid(format!(
                    "incompatible git entry (gitlink/submodule): {git_path:?}"
                )));
            }
            _ => {
                return Err(Error::Invalid(format!(
                    "incompatible git entry (mode {mode}, type {typ}): {git_path:?}"
                )));
            }
        }
        entries.push(GitEntry {
            mode: mode.to_string(),
            oid: oid.to_string(),
            path: element_path(git_path)?,
        });
    }
    Ok(entries)
}

/// Read one git blob's content.
pub fn read_git_blob(git_dir: &Path, oid: &str) -> Result<Vec<u8>> {
    git(git_dir, &["cat-file", "blob", oid])
}

/// Derive the element records of `commit`'s tree, read-only: the pure
/// function an import is checked against.
pub fn derive_records(git_dir: &Path, commit: &str) -> Result<Vec<ElementRecord>> {
    let entries = commit_entries(git_dir, commit)?;
    let mut records = Vec::with_capacity(entries.len());
    for entry in &entries {
        let content = read_git_blob(git_dir, &entry.oid)?;
        records.push(ElementRecord::new(&entry.mode, &entry.path, &sha3_hex(&content))?);
    }
    records.sort();
    Ok(records)
}

/// Initialize a repository at `root` whose `main` fork is anchored at the
/// tree of a git commit.  Returns the repository and the resolved commit.
///
/// `git_dir` names the git repository to read; it defaults to `root` (the
/// common case: `abelian init --from-git HEAD` inside a checkout).  Every
/// entry is validated before anything is written; an incompatible entry
/// errors loudly and leaves no `.abelian` behind.  The working tree is not
/// touched — `abelian materialize` produces one if wanted.
pub fn init_from_git(
    root: impl Into<PathBuf>,
    git_dir: Option<&Path>,
    committish: &str,
) -> Result<(Repository, String)> {
    let root = root.into();
    let git_dir = git_dir.unwrap_or(&root).to_path_buf();
    let commit = resolve_commit(&git_dir, committish)?;
    // Validate the whole tree before writing anything (error loudly,
    // import nothing).
    let entries = commit_entries(&git_dir, &commit)?;
    let repo = Repository::init_bare(&root)?;
    match import(&repo, &git_dir, &entries)
        .and_then(|()| annotate_import(&repo, &git_dir, committish, &commit))
    {
        Ok(()) => Ok((repo, commit)),
        Err(err) => {
            // The layout was ours alone (init_bare refuses an existing
            // `.abelian`); a failed import leaves nothing behind.
            let _ = std::fs::remove_dir_all(root.join(".abelian"));
            Err(err)
        }
    }
}

/// Initialize a repository at `root` whose `main` fork carries one log line
/// per git commit of `committish`'s linear history: first-parent ancestry
/// back to (and including) the first merge commit or the root commit.  The
/// oldest commit's tree anchors the fork; each later commit's line realizes
/// the delta from its parent's tree.  Every line is stamped with the
/// commit's committer time, author, and subject, so the whole log is a pure
/// function of the git history: everyone imports the same bytes.  Returns
/// the repository and the chain, oldest first.
pub fn init_from_git_linear(
    root: impl Into<PathBuf>,
    git_dir: Option<&Path>,
    committish: &str,
) -> Result<(Repository, Vec<String>)> {
    let root = root.into();
    let git_dir = git_dir.unwrap_or(&root).to_path_buf();
    let commit = resolve_commit(&git_dir, committish)?;
    let chain = linear_chain(&git_dir, &commit)?;
    // Validate every commit's tree before writing anything (error loudly,
    // import nothing).
    let mut trees = Vec::with_capacity(chain.len());
    for c in &chain {
        trees.push(commit_entries(&git_dir, c)?);
    }
    let repo = Repository::init_bare(&root)?;
    match import_linear(&repo, &git_dir, &chain, &trees) {
        Ok(()) => Ok((repo, chain)),
        Err(err) => {
            // The layout was ours alone (init_bare refuses an existing
            // `.abelian`); a failed import leaves nothing behind.
            let _ = std::fs::remove_dir_all(root.join(".abelian"));
            Err(err)
        }
    }
}

/// The outcome of an incremental import: the base commit fast-forwarded
/// from, the resolved target commit, and the commits imported (oldest
/// first, empty when already current).
#[derive(Clone, Debug)]
pub struct ImportSummary {
    /// The last git commit the fork already carried (the fast-forward base).
    pub base: String,
    /// The resolved target commit the fork now carries.
    pub commit: String,
    /// The commits imported this run, oldest first.
    pub imported: Vec<String>,
}

/// Fast-forward an existing fork to a later git commit, mirroring
/// `--import-linear-history` but as a `--ff-only` advance rather than a
/// fresh init.  This is the "pull main from GitHub" step: people prepare
/// patches in abelian and send them upstream; once merged to GitHub's main,
/// this pulls the new commits back in, one log line per commit.
///
/// The fork's last git-import line names the commit the fork already sits
/// at.  `committish` must be a descendant of that commit along its
/// first-parent history, or the import errors and writes nothing (a
/// fast-forward never rewrites history or crosses a fork).  The fork must
/// also sit exactly at that commit's tree state — no local drift past the
/// import — or it is not a clean fast-forward.  Each newer commit's line
/// realizes the delta from its parent's tree and is stamped with the
/// commit's committer time, author, and message, exactly as a linear init
/// would produce.
pub fn import_from_git(
    repo: &Repository,
    git_dir: Option<&Path>,
    committish: &str,
    fork: &str,
) -> Result<ImportSummary> {
    let root = repo.root().to_path_buf();
    let git_dir = git_dir.unwrap_or(&root).to_path_buf();
    let commit = resolve_commit(&git_dir, committish)?;

    // The fork's fast-forward base: the commit named by its most recent
    // git-import line.
    let state = repo.current_state(fork)?;
    let base = state
        .lines
        .iter()
        .rev()
        .find_map(|line| line.annotation.import.as_ref().map(|i| i.commit.clone()))
        .ok_or_else(|| {
            Error::Invalid(format!(
                "fork {fork} carries no git import to fast-forward from; \
                 initialize with `init --from-git COMMIT --import-linear-history`"
            ))
        })?;

    // A clean fast-forward requires the fork to sit exactly at the base
    // commit's tree state: no local edits past the import.
    let base_entries = commit_entries(&git_dir, &base)?;
    let base_manifest = commit_manifest(&repo.blobs(), &git_dir, &base_entries)?;
    if base_manifest.sum() != state.sum {
        return Err(Error::Invalid(format!(
            "not a fast-forward: fork {fork} has drifted from its imported commit {base}; \
             its state no longer matches that commit's tree"
        )));
    }

    // The new first-parent commits, oldest first (errors unless `base` is on
    // the target's first-parent line).
    let new_commits = first_parent_since(&git_dir, &base, &commit)?;
    if new_commits.is_empty() {
        return Ok(ImportSummary { base, commit, imported: Vec::new() });
    }

    import_run(repo, &git_dir, base_manifest, &new_commits, fork)?;
    Ok(ImportSummary { base, commit, imported: new_commits })
}

/// Ingest `new_commits` (oldest first) as one log line per commit onto
/// `fork`, each line realizing the delta from the previous tree, starting
/// from `base_manifest`.  Validates every tree before writing a single byte,
/// then appends the whole run in one linearizing batch.
fn import_run(
    repo: &Repository,
    git_dir: &Path,
    base_manifest: Manifest,
    new_commits: &[String],
    fork: &str,
) -> Result<()> {
    // Validate every new commit's tree before writing anything (error
    // loudly, import nothing).
    let mut trees = Vec::with_capacity(new_commits.len());
    for c in new_commits {
        trees.push(commit_entries(git_dir, c)?);
    }
    // Ingest blobs and build one realized delta per commit, from the base
    // tree forward.
    let blobs = repo.blobs();
    let algorithm = object_format(git_dir)?;
    let mut prev_manifest = base_manifest;
    let mut batch = Vec::with_capacity(new_commits.len());
    for (i, c) in new_commits.iter().enumerate() {
        let manifest = commit_manifest(&blobs, git_dir, &trees[i])?;
        let changes = crate::diff::diff_manifests(&prev_manifest, &manifest);
        prev_manifest = manifest;
        let realized: Vec<RealizedEntry> = changes
            .iter()
            .map(|change| RealizedEntry {
                remove: change.before.as_ref().map(|r| r.to_line()),
                add: change.after.as_ref().map(|r| r.to_line()),
            })
            .collect();
        let (committed_ms, author, message) = commit_meta(git_dir, c)?;
        let tree = resolve_tree(git_dir, c)?;
        let annotation = Annotation {
            author,
            prose: Some(message),
            import: Some(GitImport {
                algorithm: algorithm.clone(),
                commit: c.clone(),
                tree,
                reference: None,
            }),
            ..Annotation::default()
        };
        batch.push((realized, annotation, committed_ms));
    }
    repo.append_realized_batch(fork, batch)?;
    Ok(())
}

/// The outcome of a `git pull`.
#[derive(Clone, Debug)]
pub struct PullSummary {
    /// The mirror fork the pull fast-forwarded.
    pub fork: String,
    /// The upstream branch it is bound to.
    pub branch: String,
    /// Whether this pull established the binding for the first time.
    pub bound_now: bool,
    /// The commit the fork already sat at, if it carried a prior import.
    /// `None` on a fresh import (anchored against empty).
    pub base: Option<String>,
    /// The resolved target commit the fork now carries.
    pub commit: String,
    /// The commits imported this run, oldest first (empty when up to date).
    pub imported: Vec<String>,
}

/// Fast-forward the mirror fork from its bound branch (`abelian git pull`).
///
/// Every pull is `--ff-only`.  When the fork carries no git import yet, this
/// is a fresh import: the fork must be empty (anchored against empty), and
/// the branch's whole first-parent history lands as one line per commit.
/// Otherwise it is an incremental fast-forward from the commit the fork
/// already sits at; the target must be a first-parent descendant of that
/// commit or the pull refuses (a fast-forward never rewrites history).
pub fn pull(
    repo: &Repository,
    git_dir: Option<&Path>,
    branch: &str,
    fork: &str,
) -> Result<PullSummary> {
    let root = repo.root().to_path_buf();
    let git_dir = git_dir.unwrap_or(&root).to_path_buf();
    let commit = resolve_commit(&git_dir, branch)?;
    let state = repo.current_state(fork)?;

    // The commit the fork already sits at: its most recent git-import line.
    let base = state
        .lines
        .iter()
        .rev()
        .find_map(|line| line.annotation.import.as_ref().map(|i| i.commit.clone()));

    let Some(base) = base else {
        // Fresh import: anchor against empty.  The fork must be empty — no
        // local work ahead of an import that never happened.
        if !state.lines.is_empty() || state.sum != Manifest::new().sum() {
            return Err(Error::Invalid(format!(
                "fork {fork} carries local work but no git import; \
                 cannot bind it to branch {branch} as a fresh mirror"
            )));
        }
        let chain = linear_chain(&git_dir, &commit)?;
        import_run(repo, &git_dir, Manifest::new(), &chain, fork)?;
        return Ok(PullSummary {
            fork: fork.to_string(),
            branch: branch.to_string(),
            bound_now: false,
            base: None,
            commit,
            imported: chain,
        });
    };

    // A clean fast-forward requires the fork to sit exactly at the base
    // commit's tree state: no local edits past the import.
    let base_entries = commit_entries(&git_dir, &base)?;
    let base_manifest = commit_manifest(&repo.blobs(), &git_dir, &base_entries)?;
    if base_manifest.sum() != state.sum {
        return Err(Error::Invalid(format!(
            "not a fast-forward: fork {fork} has drifted from its imported commit {base}; \
             its state no longer matches that commit's tree"
        )));
    }

    let new_commits = first_parent_since(&git_dir, &base, &commit)?;
    if !new_commits.is_empty() {
        import_run(repo, &git_dir, base_manifest, &new_commits, fork)?;
    }
    Ok(PullSummary {
        fork: fork.to_string(),
        branch: branch.to_string(),
        bound_now: false,
        base: Some(base),
        commit,
        imported: new_commits,
    })
}

/// Recover the mirror fork after an upstream rewrite (`abelian git
/// reanchor`).  A force-push replaces the commit the fork imported from with
/// one that shares no ancestry, so a fast-forward can no longer bridge the
/// two.  reanchor repoints the fork's state onto `committish`'s tree,
/// non-destructively (one appended line whose realized delta carries the
/// current state to the new tree), and records the new commit as the import
/// base so subsequent pulls fast-forward from it.  The prior state stays
/// reachable, exactly as `repoint` promises.
pub fn reanchor(
    repo: &Repository,
    git_dir: Option<&Path>,
    committish: &str,
    fork: &str,
) -> Result<String> {
    let root = repo.root().to_path_buf();
    let git_dir = git_dir.unwrap_or(&root).to_path_buf();
    let commit = resolve_commit(&git_dir, committish)?;
    let entries = commit_entries(&git_dir, &commit)?;
    let manifest = commit_manifest(&repo.blobs(), &git_dir, &entries)?;
    let current = repo.current_state(fork)?;
    let changes = crate::diff::diff_manifests(&current.manifest, &manifest);
    let realized: Vec<RealizedEntry> = changes
        .iter()
        .map(|change| RealizedEntry {
            remove: change.before.as_ref().map(|r| r.to_line()),
            add: change.after.as_ref().map(|r| r.to_line()),
        })
        .collect();
    let algorithm = object_format(&git_dir)?;
    let (committed_ms, author, message) = commit_meta(&git_dir, &commit)?;
    let tree = resolve_tree(&git_dir, &commit)?;
    let annotation = Annotation {
        author,
        prose: Some(message),
        import: Some(GitImport {
            algorithm,
            commit: commit.clone(),
            tree,
            reference: Some(committish.to_string()),
        }),
        ..Annotation::default()
    };
    repo.append_realized_batch(fork, vec![(realized, annotation, committed_ms)])?;
    Ok(commit)
}

/// Ingest one commit's tree into the pool and derive its manifest.  Blobs
/// land unsynced: the imports below write thousands per run, so they defer
/// to one device sync before the commit that references them (see
/// [`BlobStore::put_unsynced`]) rather than fsyncing each blob.
fn commit_manifest(
    blobs: &BlobStore,
    git_dir: &Path,
    entries: &[GitEntry],
) -> Result<Manifest> {
    let mut manifest = Manifest::new();
    for entry in entries {
        let content = read_git_blob(git_dir, &entry.oid)?;
        let blob = blobs.put_unsynced(&content)?;
        manifest.insert(ElementRecord::new(&entry.mode, &entry.path, &blob)?)?;
    }
    Ok(manifest)
}

fn import_linear(
    repo: &Repository,
    git_dir: &Path,
    chain: &[String],
    trees: &[Vec<GitEntry>],
) -> Result<()> {
    let blobs = repo.blobs();
    let algorithm = object_format(git_dir)?;
    let anchor = commit_manifest(&blobs, git_dir, &trees[0])?;
    let anchor_sum = anchor.sum();
    // The anchor commits the blobs commit_manifest pooled unsynced.
    blobs.sync()?;
    repo.write_anchor_manifest(&anchor)?;
    // One line per commit.  The oldest commit's line is zero-op (the anchor
    // carries its state); each later line's realized delta takes the parent
    // commit's tree to this commit's tree, sorted by path.
    let mut log_bytes = Vec::new();
    let mut prev_id = String::new();
    let mut prev_manifest = anchor;
    let mut sum = anchor_sum.clone();
    for (i, commit) in chain.iter().enumerate() {
        let realized: Vec<RealizedEntry> = if i == 0 {
            Vec::new()
        } else {
            let manifest = commit_manifest(&blobs, git_dir, &trees[i])?;
            let changes = crate::diff::diff_manifests(&prev_manifest, &manifest);
            prev_manifest = manifest;
            changes
                .iter()
                .map(|c| RealizedEntry {
                    remove: c.before.as_ref().map(|r| r.to_line()),
                    add: c.after.as_ref().map(|r| r.to_line()),
                })
                .collect()
        };
        sum = apply_realized_to_sum(&sum, &realized)?;
        let (committed_ms, author, message) = commit_meta(git_dir, commit)?;
        let tree = resolve_tree(git_dir, commit)?;
        let annotation = Annotation {
            author,
            prose: Some(message),
            import: Some(GitImport {
                algorithm: algorithm.clone(),
                commit: commit.clone(),
                tree,
                reference: None,
            }),
            ..Annotation::default()
        };
        let mut line = LogLine {
            id: String::new(),
            prev: prev_id.clone(),
            intent: Intent::default(),
            realized,
            sum_after: sum.hexdigest(),
            committed_ms,
            annotation,
        };
        log_bytes.extend_from_slice(&line.seal(&blobs)?);
        prev_id = line.id.clone();
    }
    repo.restore_fork("main", &ForkFile::at(&anchor_sum), &log_bytes)?;
    Ok(())
}

fn import(repo: &Repository, git_dir: &Path, entries: &[GitEntry]) -> Result<()> {
    let blobs = repo.blobs();
    let mut manifest = Manifest::new();
    for entry in entries {
        let content = read_git_blob(git_dir, &entry.oid)?;
        let blob = blobs.put_unsynced(&content)?;
        manifest.insert(ElementRecord::new(&entry.mode, &entry.path, &blob)?)?;
    }
    let sum = manifest.sum();
    // The anchor and fork file commit the blobs pooled unsynced above.
    blobs.sync()?;
    repo.write_anchor_manifest(&manifest)?;
    repo.create_fork_raw("main", &ForkFile::at(&sum))?;
    Ok(())
}

/// Record where the anchor came from: a zero-op line on `main` whose
/// annotation names the ref as the user passed it, the resolved commit
/// digest and its algorithm, and the tree the derivation is a pure
/// function of.  Anyone reading the log can re-derive the anchor from it.
fn annotate_import(
    repo: &Repository,
    git_dir: &Path,
    committish: &str,
    commit: &str,
) -> Result<()> {
    let algorithm = object_format(git_dir)?;
    let tree = resolve_tree(git_dir, commit)?;
    let (_committed_ms, _author, message) = commit_meta(git_dir, commit)?;
    let annotation = Annotation {
        author: "git-import".to_string(),
        prose: Some(message),
        import: Some(GitImport {
            algorithm,
            commit: commit.to_string(),
            tree,
            reference: Some(committish.to_string()),
        }),
        ..Annotation::default()
    };
    repo.apply("main", Intent::default(), annotation)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) -> Vec<u8> {
        git(dir, args).unwrap()
    }

    fn commit_all(dir: &Path, msg: &str) {
        run_git(dir, &["add", "-A"]);
        commit_index(dir, msg);
    }

    /// Commit the index as-is (add -A would drop --cacheinfo entries that
    /// have no working-tree file).
    fn commit_index(dir: &Path, msg: &str) {
        run_git(
            dir,
            &[
                "-c",
                "user.name=abelian",
                "-c",
                "user.email=abelian@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                msg,
            ],
        );
    }

    fn temp_git_repo(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("abelian-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run_git(&dir, &["init", "-q"]);
        dir
    }

    #[test]
    fn init_from_git_imports_a_rederivable_anchor() {
        let dir = temp_git_repo("import");
        std::fs::write(dir.join("a.txt"), b"alpha\n").unwrap();
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        std::fs::write(dir.join("tools/run"), b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                dir.join("tools/run"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            std::os::unix::fs::symlink("a.txt", dir.join("link")).unwrap();
        }
        commit_all(&dir, "genesis");
        let commit = resolve_commit(&dir, "HEAD").unwrap();

        let (repo, resolved) = init_from_git(&dir, None, "HEAD").unwrap();
        assert_eq!(resolved, commit);

        // The fork is anchored at the derived state.
        let state = repo.current_state("main").unwrap();

        // The first log line names the provenance: the ref as passed, the
        // resolved digest and its algorithm, and the tree oid.
        let algorithm = object_format(&dir).unwrap();
        let tree = resolve_tree(&dir, &commit).unwrap();
        let [line] = &state.lines[..] else { panic!("expected one provenance line") };
        assert!(line.intent.ops.is_empty());
        assert!(line.realized.is_empty());
        assert_eq!(line.annotation.author, "git-import");
        // The commit message rides verbatim in prose; the derivation facts
        // are structured, not fused into the prose string.
        assert_eq!(line.annotation.prose.as_deref(), Some("genesis"));
        let import = line.annotation.import.as_ref().expect("import provenance");
        assert_eq!(import.algorithm, algorithm);
        assert_eq!(import.commit, commit);
        assert_eq!(import.tree, tree);
        assert_eq!(import.reference.as_deref(), Some("HEAD"));
        assert_eq!(state.manifest.get("/a.txt").unwrap().mode, "100644");
        #[cfg(unix)]
        {
            assert_eq!(state.manifest.get("/tools/run").unwrap().mode, "100755");
            assert_eq!(state.manifest.get("/link").unwrap().mode, "120000");
        }
        // Imported blobs are materializable.
        let a = state.manifest.get("/a.txt").unwrap();
        assert_eq!(repo.blobs().get(&a.blob).unwrap(), b"alpha\n");

        // The anchor re-derives from the commit's tree: the import is a
        // pure function of it.
        let derived = derive_records(&dir, &commit).unwrap();
        let mut anchored: Vec<ElementRecord> = state.manifest.records().cloned().collect();
        anchored.sort();
        assert_eq!(derived, anchored);

        // A drifted commit no longer re-derives the anchored state.
        std::fs::write(dir.join("a.txt"), b"beta\n").unwrap();
        run_git(&dir, &["add", "a.txt"]);
        commit_index(&dir, "drift");
        let drifted = resolve_commit(&dir, "HEAD").unwrap();
        assert_ne!(drifted, commit);
        assert_ne!(derive_records(&dir, &drifted).unwrap(), anchored);
    }

    #[test]
    fn linear_history_imports_one_line_per_commit_deterministically() {
        let dir = temp_git_repo("linear");
        std::fs::write(dir.join("a.txt"), b"alpha\n").unwrap();
        commit_all(&dir, "genesis");
        std::fs::write(dir.join("b.txt"), b"beta\n").unwrap();
        commit_all(&dir, "add b");
        std::fs::write(dir.join("a.txt"), b"alpha 2\n").unwrap();
        commit_all(&dir, "edit a");
        let commit = resolve_commit(&dir, "HEAD").unwrap();

        // The chain is first-parent ancestry, oldest first.
        let chain = linear_chain(&dir, &commit).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain.last().unwrap(), &commit);

        let root_a = dir.join("import-a");
        let (repo, chain_a) = init_from_git_linear(&root_a, Some(&dir), &commit).unwrap();
        assert_eq!(chain_a, chain);

        // One line per commit; the head state re-derives from the target
        // commit's tree.
        let state = repo.current_state("main").unwrap();
        assert_eq!(state.lines.len(), 3);
        assert!(state.lines[0].realized.is_empty(), "the anchor carries the first commit");
        let derived = derive_records(&dir, &commit).unwrap();
        let mut anchored: Vec<ElementRecord> = state.manifest.records().cloned().collect();
        anchored.sort();
        assert_eq!(derived, anchored);
        // Annotations carry the commit message verbatim, with structured
        // derivation facts alongside.
        assert_eq!(state.lines[2].annotation.prose.as_deref(), Some("edit a"));
        let import = state.lines[2].annotation.import.as_ref().expect("import provenance");
        assert_eq!(import.commit, commit);
        assert_eq!(import.reference, None);
        assert_eq!(state.lines[0].annotation.author, "abelian <abelian@example.com>");

        // Determinism: a second import produces byte-identical log lines.
        let root_b = dir.join("import-b");
        let (repo_b, _) = init_from_git_linear(&root_b, Some(&dir), &commit).unwrap();
        assert_eq!(
            repo.log_bytes("main").unwrap(),
            repo_b.log_bytes("main").unwrap(),
            "everyone imports the same"
        );

        // A merge commit truncates the chain: it becomes the anchor.
        run_git(&dir, &["checkout", "-q", "-b", "side", "HEAD~2"]);
        std::fs::write(dir.join("c.txt"), b"gamma\n").unwrap();
        commit_all(&dir, "side c");
        run_git(&dir, &["checkout", "-q", "-"]);
        run_git(
            &dir,
            &[
                "-c",
                "user.name=abelian",
                "-c",
                "user.email=abelian@example.com",
                "-c",
                "commit.gpgsign=false",
                "merge",
                "-q",
                "--no-ff",
                "-m",
                "merge side",
                "side",
            ],
        );
        std::fs::write(dir.join("d.txt"), b"delta\n").unwrap();
        commit_all(&dir, "after merge");
        let head = resolve_commit(&dir, "HEAD").unwrap();
        let merge = resolve_commit(&dir, "HEAD~1").unwrap();
        let chain = linear_chain(&dir, &head).unwrap();
        assert_eq!(chain, vec![merge, head]);
    }

    #[test]
    fn import_from_git_fast_forwards_and_matches_a_fresh_import() {
        let dir = temp_git_repo("ff");
        std::fs::write(dir.join("a.txt"), b"alpha\n").unwrap();
        commit_all(&dir, "genesis");
        std::fs::write(dir.join("b.txt"), b"beta\n").unwrap();
        commit_all(&dir, "add b");
        let early = resolve_commit(&dir, "HEAD").unwrap();

        // Initialize abelian at the early commit, in a root outside the git
        // tree so later `add -A` commits do not ingest it.
        let root = temp_git_repo("ff-abelian");
        std::fs::remove_dir_all(&root).unwrap();
        let (repo, _) = init_from_git_linear(&root, Some(&dir), &early).unwrap();
        assert_eq!(repo.current_state("main").unwrap().lines.len(), 2);

        // Advance git's history, then fast-forward the abelian fork.
        std::fs::write(dir.join("a.txt"), b"alpha 2\n").unwrap();
        commit_all(&dir, "edit a");
        std::fs::write(dir.join("c.txt"), b"gamma\n").unwrap();
        commit_all(&dir, "add c");
        let head = resolve_commit(&dir, "HEAD").unwrap();

        let summary = import_from_git(&repo, Some(&dir), &head, "main").unwrap();
        assert_eq!(summary.base, early);
        assert_eq!(summary.commit, head);
        assert_eq!(summary.imported.len(), 2);

        // The head state re-derives from the target commit's tree.
        let state = repo.current_state("main").unwrap();
        assert_eq!(state.lines.len(), 4);
        let derived = derive_records(&dir, &head).unwrap();
        let mut anchored: Vec<ElementRecord> = state.manifest.records().cloned().collect();
        anchored.sort();
        assert_eq!(derived, anchored);
        // Per-commit provenance rides on the appended lines.
        assert_eq!(state.lines[3].annotation.prose.as_deref(), Some("add c"));
        assert_eq!(
            state.lines[3].annotation.import.as_ref().unwrap().commit,
            head
        );

        // A second import is a no-op: already up to date.
        let again = import_from_git(&repo, Some(&dir), &head, "main").unwrap();
        assert!(again.imported.is_empty());
        assert_eq!(repo.current_state("main").unwrap().lines.len(), 4);

        // The incremental import replays to the same state a fresh linear
        // import produces (the head sums agree; each line derives its tree).
        let fresh_root = temp_git_repo("ff-fresh");
        std::fs::remove_dir_all(&fresh_root).unwrap();
        let (fresh, _) = init_from_git_linear(&fresh_root, Some(&dir), &head).unwrap();
        assert_eq!(
            repo.current_state("main").unwrap().sum.hexdigest(),
            fresh.current_state("main").unwrap().sum.hexdigest(),
        );
    }

    #[test]
    fn import_from_git_refuses_a_non_fast_forward() {
        let dir = temp_git_repo("nonff");
        std::fs::write(dir.join("a.txt"), b"alpha\n").unwrap();
        commit_all(&dir, "genesis");
        let genesis = resolve_commit(&dir, "HEAD").unwrap();
        std::fs::write(dir.join("a.txt"), b"alpha 2\n").unwrap();
        commit_all(&dir, "edit a");
        let mainline = resolve_commit(&dir, "HEAD").unwrap();

        // The fork is imported at the mainline tip.
        let root = temp_git_repo("nonff-abelian");
        std::fs::remove_dir_all(&root).unwrap();
        let (repo, _) = init_from_git_linear(&root, Some(&dir), &mainline).unwrap();

        // A divergent branch off genesis: the fork's base (mainline) is not
        // on this commit's first-parent history, so importing it is not a
        // fast-forward.
        run_git(&dir, &["checkout", "-q", "-b", "side", &genesis]);
        std::fs::write(dir.join("d.txt"), b"delta\n").unwrap();
        commit_all(&dir, "side d");
        let side = resolve_commit(&dir, "HEAD").unwrap();

        let err = import_from_git(&repo, Some(&dir), &side, "main").unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err}");
        assert!(err.to_string().contains("not a fast-forward"), "{err}");
        // Nothing was appended: the fork still sits at the mainline tip.
        let state = repo.current_state("main").unwrap();
        assert_eq!(state.lines.len(), 2);
    }

    #[test]
    fn gitlinks_error_loudly_and_write_nothing() {
        let dir = temp_git_repo("gitlink");
        std::fs::write(dir.join("a.txt"), b"alpha\n").unwrap();
        run_git(&dir, &["add", "a.txt"]);
        run_git(
            &dir,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000,a94a8fe5ccb19ba61c4c0873d391e987982fbbd3,sub",
            ],
        );
        commit_index(&dir, "with submodule");
        let Err(err) = init_from_git(&dir, None, "HEAD") else {
            panic!("expected a loud error")
        };
        assert!(matches!(err, Error::Invalid(_)), "{err}");
        assert!(err.to_string().contains("gitlink"), "{err}");
        assert!(!dir.join(".abelian").exists(), "a failed import must write nothing");
    }

    #[test]
    fn reserved_and_non_ascii_paths_are_rejected() {
        let dir = temp_git_repo("badpaths");
        std::fs::write(dir.join("x"), b"x\n").unwrap();
        let oid = String::from_utf8(run_git(&dir, &["hash-object", "-w", "x"]))
            .unwrap()
            .trim()
            .to_string();
        run_git(
            &dir,
            &["update-index", "--add", "--cacheinfo", &format!("100644,{oid},.abelian/config")],
        );
        commit_index(&dir, "reserved");
        let Err(err) = init_from_git(&dir, None, "HEAD") else {
            panic!("expected a loud error")
        };
        assert!(err.to_string().contains("reserved"), "{err}");
        assert!(!dir.join(".abelian").exists());

        run_git(&dir, &["rm", "-q", "--cached", ".abelian/config"]);
        run_git(
            &dir,
            &["update-index", "--add", "--cacheinfo", &format!("100644,{oid},caf\u{e9}")],
        );
        commit_index(&dir, "non-ascii");
        let Err(err) = init_from_git(&dir, None, "HEAD") else {
            panic!("expected a loud error")
        };
        assert!(err.to_string().contains("non-ASCII"), "{err}");
        assert!(!dir.join(".abelian").exists());
    }
}
