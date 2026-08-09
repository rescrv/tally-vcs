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
use crate::log::{Annotation, LogLine};
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

/// A commit's committer time (milliseconds), author (`Name <email>`), and
/// subject line — the deterministic annotation material an import stamps.
fn commit_meta(git_dir: &Path, commit: &str) -> Result<(u64, String, String)> {
    let out = git(git_dir, &["show", "-s", "--format=%ct%x00%an <%ae>%x00%s", commit])?;
    let text = String::from_utf8(out)
        .map_err(|_| Error::Corrupt("git show produced non-UTF-8".to_string()))?;
    let text = text.trim_end_matches('\n');
    let mut parts = text.splitn(3, '\0');
    let (Some(seconds), Some(author), Some(subject)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err(Error::Corrupt(format!("git show produced bad metadata: {text:?}")));
    };
    let seconds: u64 = seconds
        .parse()
        .map_err(|_| Error::Corrupt(format!("bad committer time: {seconds:?}")))?;
    Ok((seconds * 1000, author.to_string(), subject.to_string()))
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

/// Ingest one commit's tree into the pool and derive its manifest.
fn commit_manifest(
    blobs: &BlobStore,
    git_dir: &Path,
    entries: &[GitEntry],
) -> Result<Manifest> {
    let mut manifest = Manifest::new();
    for entry in entries {
        let content = read_git_blob(git_dir, &entry.oid)?;
        let blob = blobs.put(&content)?;
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
        let (committed_ms, author, subject) = commit_meta(git_dir, commit)?;
        let tree = resolve_tree(git_dir, commit)?;
        let annotation = Annotation {
            author,
            prose: Some(format!(
                "git import: commit {algorithm}:{commit} (tree {algorithm}:{tree}): {subject}"
            )),
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
        let blob = blobs.put(&content)?;
        manifest.insert(ElementRecord::new(&entry.mode, &entry.path, &blob)?)?;
    }
    let sum = manifest.sum();
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
    let annotation = Annotation {
        author: "git-import".to_string(),
        prose: Some(format!(
            "git import: ref {committish} -> commit {algorithm}:{commit} (tree {algorithm}:{tree})"
        )),
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
        let prose = line.annotation.prose.as_deref().unwrap();
        assert_eq!(
            prose,
            format!(
                "git import: ref HEAD -> commit {algorithm}:{commit} (tree {algorithm}:{tree})"
            )
        );
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
        // Annotations carry the commit's subject.
        let prose = state.lines[2].annotation.prose.as_deref().unwrap();
        assert!(prose.ends_with(": edit a"), "{prose}");
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
