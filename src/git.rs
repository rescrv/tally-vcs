//! Git predecessors: initializing an abelian repository from a git commit.
//!
//! An import walks the commit's tree, ingests every blob into the pool, and
//! anchors `main` at the resulting state.  The predecessor is recorded as a
//! claim — abelian's artifact for verifiable facts about states — whose
//! `cmd` is `git-predecessor <commit>`, whose `at_sum` is the derived state,
//! whose `inputs` are every imported element record (so `input_sum` equals
//! `at_sum` by arithmetic), and whose transcript is the anchor manifest's
//! bytes in the blob pool.  Claims are byte-preserved (I4), id-verified
//! (§1.3), and travel over the existing wire format unchanged.
//!
//! Compatibility is checked loudly before anything is written: only modes
//! `100644`, `100755`, and `120000` import (gitlinks/submodules do not);
//! paths must satisfy §1.1 and, in v0, must be ASCII, because NFC
//! normalization of arbitrary Unicode cannot be verified without tables.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::claims::Claim;
use crate::fork::ForkFile;
use crate::ident::{ElementRecord, sha3_hex, validate_path};
use crate::manifest::Manifest;
use crate::repo::Repository;
use crate::{Error, Result, ioerr};

/// The `cmd` prefix of a predecessor claim: `git-predecessor <commit hex>`.
pub const GIT_PREDECESSOR_PREFIX: &str = "git-predecessor ";

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

/// Resolve a committish to its full object name.
pub fn resolve_commit(git_dir: &Path, committish: &str) -> Result<String> {
    let spec = format!("{committish}^{{commit}}");
    let out = git(git_dir, &["rev-parse", "--verify", &spec])?;
    let hex = String::from_utf8(out)
        .map_err(|_| Error::Corrupt("git rev-parse produced non-UTF-8".to_string()))?
        .trim()
        .to_string();
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Corrupt(format!("git rev-parse produced a non-hex name: {hex:?}")));
    }
    Ok(hex)
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
/// function a predecessor claim is verified against.
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
/// tree of a git commit, recording the predecessor as a claim.
///
/// `git_dir` names the git repository to read; it defaults to `root` (the
/// common case: `tally init --from-git HEAD` inside a checkout).  Every
/// entry is validated before anything is written; an incompatible entry
/// errors loudly and leaves no `.abelian` behind.  The working tree is not
/// touched — `tally materialize` produces one if wanted.
pub fn init_from_git(
    root: impl Into<PathBuf>,
    git_dir: Option<&Path>,
    committish: &str,
) -> Result<(Repository, Claim)> {
    let root = root.into();
    let git_dir = git_dir.unwrap_or(&root).to_path_buf();
    let commit = resolve_commit(&git_dir, committish)?;
    // Validate the whole tree before writing anything (error loudly,
    // import nothing).
    let entries = commit_entries(&git_dir, &commit)?;
    let repo = Repository::init_bare(&root)?;
    match import(&repo, &git_dir, &commit, &entries) {
        Ok(claim) => Ok((repo, claim)),
        Err(err) => {
            // The layout was ours alone (init_bare refuses an existing
            // `.abelian`); a failed import leaves nothing behind.
            let _ = std::fs::remove_dir_all(root.join(".abelian"));
            Err(err)
        }
    }
}

fn import(
    repo: &Repository,
    git_dir: &Path,
    commit: &str,
    entries: &[GitEntry],
) -> Result<Claim> {
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
    // The predecessor claim: at_sum is the derived state, inputs are every
    // imported record (input_sum == at_sum by arithmetic), and the
    // transcript is the anchor manifest's bytes.
    let transcript_sha3 = blobs.put(&manifest.to_bytes())?;
    let cmd = format!("{GIT_PREDECESSOR_PREFIX}{commit}");
    let inputs: Vec<ElementRecord> = manifest.records().cloned().collect();
    let claim = Claim::new(&sum, &cmd, inputs, 0, &transcript_sha3)?;
    repo.put_claim(&claim)?;
    Ok(claim)
}

/// The git commit a predecessor claim names.
pub fn predecessor_commit(claim: &Claim) -> Option<&str> {
    let commit = claim.cmd.strip_prefix(GIT_PREDECESSOR_PREFIX)?;
    if !commit.is_empty() && commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(commit)
    } else {
        None
    }
}

/// All predecessor claims in the repository.
pub fn predecessor_claims(repo: &Repository) -> Result<Vec<Claim>> {
    let mut out = Vec::new();
    for id in repo.claim_ids()? {
        let claim = repo.get_claim(&id)?;
        if predecessor_commit(&claim).is_some() {
            out.push(claim);
        }
    }
    Ok(out)
}

/// Verify a predecessor claim against a git repository: re-derive the
/// commit's tree and check every assertion the claim makes.  Returns the
/// verified commit.  (`Claim::parse` has already verified the claim's id
/// and its `input_sum` arithmetic.)
pub fn verify_predecessor(claim: &Claim, git_dir: &Path) -> Result<String> {
    let Some(commit) = predecessor_commit(claim) else {
        return Err(Error::Invalid(format!("not a git-predecessor claim: {}", claim.cmd)));
    };
    if claim.input_sum != claim.at_sum {
        return Err(Error::Corrupt(format!(
            "predecessor claim {}: inputs (sum {}) do not cover the state {}",
            claim.id, claim.input_sum, claim.at_sum
        )));
    }
    let derived = derive_records(git_dir, commit)?;
    let mut claimed = claim.input_records()?;
    claimed.sort();
    if derived != claimed {
        return Err(Error::Corrupt(format!(
            "predecessor claim {}: state {} does not re-derive from git commit {commit}",
            claim.id, claim.at_sum
        )));
    }
    let manifest = Manifest::from_records(derived)?;
    if manifest.sum().hexdigest() != claim.at_sum {
        return Err(Error::Corrupt(format!(
            "predecessor claim {}: derived sum {} disagrees with at_sum {}",
            claim.id,
            manifest.sum().hexdigest(),
            claim.at_sum
        )));
    }
    if sha3_hex(&manifest.to_bytes()) != claim.transcript_sha3 {
        return Err(Error::Corrupt(format!(
            "predecessor claim {}: transcript is not the anchor manifest",
            claim.id
        )));
    }
    Ok(commit.to_string())
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
    fn init_from_git_imports_and_records_a_verifiable_predecessor() {
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

        let (repo, claim) = init_from_git(&dir, None, "HEAD").unwrap();

        // The fork is anchored at the derived state and the claim agrees.
        let state = repo.current_state("main").unwrap();
        assert_eq!(state.sum.hexdigest(), claim.at_sum);
        assert_eq!(claim.input_sum, claim.at_sum);
        assert_eq!(claim.cmd, format!("{GIT_PREDECESSOR_PREFIX}{commit}"));
        assert_eq!(state.manifest.get("/a.txt").unwrap().mode, "100644");
        #[cfg(unix)]
        {
            assert_eq!(state.manifest.get("/tools/run").unwrap().mode, "100755");
            assert_eq!(state.manifest.get("/link").unwrap().mode, "120000");
        }
        // Imported blobs are materializable.
        let a = state.manifest.get("/a.txt").unwrap();
        assert_eq!(repo.blobs().get(&a.blob).unwrap(), b"alpha\n");

        // The claim round-trips byte-preserved and verifies against git.
        let back = repo.get_claim(&claim.id).unwrap();
        assert_eq!(back, claim);
        assert_eq!(predecessor_claims(&repo).unwrap(), vec![claim.clone()]);
        assert_eq!(verify_predecessor(&claim, &dir).unwrap(), commit);

        // A claim naming a different commit fails loudly.
        std::fs::write(dir.join("a.txt"), b"beta\n").unwrap();
        run_git(&dir, &["add", "a.txt"]);
        commit_index(&dir, "drift");
        let drifted = resolve_commit(&dir, "HEAD").unwrap();
        assert_ne!(drifted, commit);
        let inputs: Vec<ElementRecord> = state.manifest.records().cloned().collect();
        let forged = Claim::new(
            &state.sum,
            &format!("{GIT_PREDECESSOR_PREFIX}{drifted}"),
            inputs,
            0,
            &claim.transcript_sha3,
        )
        .unwrap();
        assert!(matches!(verify_predecessor(&forged, &dir), Err(Error::Corrupt(_))));
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
