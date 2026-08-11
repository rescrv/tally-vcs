//! abelian: the command over the abelian substrate.
//!
//! Named for the group.  Its merge takes after the split tally stick — the
//! twelfth century's distributed, two-party, checksummed ledger.  Every
//! subcommand here automates a step
//! the ANDON teaches by hand; with zero `abelian` binaries available, the
//! ANDON remains a complete, operable version control system.

use std::process::exit;

use arrrg::CommandLine;

use abelian::ident::Sum;
use abelian::log::{Annotation, Provenance};
use abelian::patch::Intent;
use abelian::repo::Repository;
use abelian::union::{Stratum, union};
use abelian::views::{Beat, fused_beats};
use abelian::wire::FsStore;
use abelian::{Error, Result};

const USAGE: &str = "USAGE: abelian <command> [options] [args]

repository:
  init                             create an empty repository here
  gc-blobs                         collect blobs no fork reaches (--dry-run)

git (sunsets with GitHub):
  git pull                         fast-forward the mirror fork from its bound branch
                                   (binds on first use from empty; one commit, one line;
                                   refuses non-fast-forward)
  git pr                           render a fork as a git branch and open a pull request
                                   (one commit per fused beat)
  git reanchor <commit>            recover the mirror fork after an upstream rewrite

working tree:
  sum                              print the working tree's element records and sum
  status                           the pending patch: working tree vs the fork's ref
  check                            compare the working tree against the log's expectation
  materialize <rev> [dest]         produce a working tree at a state (default: new dir)
  restore [rev] [-- path...]       rewrite working-tree paths to a state (discards edits)
                                   (does not discard untracked files)

forks:
  fork <name>                      create a fork (anchor + empty log)
  remove-fork <name>               delete a fork; refuses unsubsumed work unless --force
  union <fork>                     bring that fork's log into this one (strata 1-3)
  repoint <rev>                    move this fork's state to a prior one, non-destructively
  snapshot                         write a manifest at the current state and repoint the fork

revisions:
  rev-parse <rev>                  resolve a revision (HEAD, HEAD~N, fork, sum:S, line:ID)
  diff [rev] [rev] [-- path...]    the patch between two states (or working tree vs ref)
  blame <path>                     attribute each line to the patch that produced it

patches:
  apply [file|-]                   validate and apply an intent; append to the log
  log [--view NAME | --raw]        render history (default view: fused)
  show <id>                        render one log line
  fuse <from> <to>                 compose a span into one narrative beat (lossless)

wire:
  clone <store> <dest>             clone a packed repository from an object store
  fetch <store> <cache>            update a packed cache from an object store
  push <store>                     pack and push (put-if-absent linearization)
  pack <dir>                       pack the loose repository into a directory
  unpack <dir> <dest>              unpack a packed directory into a loose repository
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((command, rest)) = args.split_first() else {
        eprint!("{USAGE}");
        exit(64);
    };
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    let result = match command.as_str() {
        "init" => cmd_init(&rest),
        "gc-blobs" => cmd_gc_blobs(&rest),
        "git" => cmd_git(&rest),
        "sum" => cmd_sum(&rest),
        "status" => cmd_status(&rest),
        "check" => cmd_check(&rest),
        "snapshot" => cmd_snapshot(&rest),
        "materialize" => cmd_materialize(&rest),
        "restore" => cmd_restore(&rest),
        "rev-parse" => cmd_rev_parse(&rest),
        "diff" => cmd_diff(&rest),
        "blame" => cmd_blame(&rest),
        "apply" => cmd_apply(&rest),
        "log" => cmd_log(&rest),
        "show" => cmd_show(&rest),
        "fuse" => cmd_fuse(&rest),
        "fork" => cmd_fork(&rest),
        "remove-fork" => cmd_remove_fork(&rest),
        "union" => cmd_union(&rest),
        "repoint" => cmd_repoint(&rest),
        "clone" => cmd_clone(&rest),
        "fetch" => cmd_fetch(&rest),
        "push" => cmd_push(&rest),
        "pack" => cmd_pack(&rest),
        "unpack" => cmd_unpack(&rest),
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            return;
        }
        other => {
            eprintln!("abelian: unknown command {other:?}\n");
            eprint!("{USAGE}");
            exit(64);
        }
    };
    if let Err(err) = result {
        eprintln!("abelian {command}: {err}");
        exit(1);
    }
}

fn repo() -> Result<Repository> {
    let cwd = std::env::current_dir().map_err(abelian::ioerr("getting cwd"))?;
    Repository::discover(cwd)
}

//////////////////////////////////////////// options //////////////////////////////////////////////

#[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
struct ForkOptions {
    #[arrrg(optional, "The fork to operate on (default: main).", "FORK")]
    fork: Option<String>,
}

impl ForkOptions {
    fn fork(&self) -> &str {
        self.fork.as_deref().unwrap_or("main")
    }
}

#[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
struct ApplyOptions {
    #[arrrg(optional, "The fork to operate on (default: main).", "FORK")]
    fork: Option<String>,
    #[arrrg(optional, "Author of the patch.", "AUTHOR")]
    author: Option<String>,
    #[arrrg(optional, "Provenance: agent or andon (default: agent).", "WHO")]
    provenance: Option<String>,
    #[arrrg(optional, "Andon: why the cord was pulled.", "REASON")]
    reason: Option<String>,
    #[arrrg(optional, "Andon: detached signature blob hash.", "HEX")]
    sig: Option<String>,
    #[arrrg(flag, "Andon: store a v0 placeholder signature blob and reference it.")]
    sign: bool,
    #[arrrg(optional, "Narrative prose for the annotation.", "PROSE")]
    prose: Option<String>,
    #[arrrg(optional, "Session identifier.", "SESSION")]
    session: Option<String>,
}

//////////////////////////////////////////// commands /////////////////////////////////////////////

fn cmd_init(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {}
    let (_, free) =
        Options::from_arguments_relaxed("USAGE: abelian init [dir]", args);
    reject_extra("init", &free, 1)?;
    let dir = match free.first() {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::env::current_dir().map_err(abelian::ioerr("getting cwd"))?,
    };
    let repo = Repository::init(&dir)?;
    println!("initialized empty abelian repository at {}", repo.root().display());
    Ok(())
}

/// The `git` namespace: the bridge that sunsets with GitHub.
fn cmd_git(args: &[&str]) -> Result<()> {
    let Some((sub, rest)) = args.split_first() else {
        return Err(Error::Invalid(
            "git requires a subcommand: pull, pr, or reanchor".to_string(),
        ));
    };
    match *sub {
        "pull" => cmd_git_pull(rest),
        "pr" => cmd_git_pr(rest),
        "reanchor" => cmd_git_reanchor(rest),
        other => Err(Error::Invalid(format!(
            "unknown git subcommand {other:?}; expected pull, pr, or reanchor"
        ))),
    }
}

fn cmd_git_pull(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "The mirror fork to bind (default: main).", "FORK")]
        fork: Option<String>,
        #[arrrg(optional, "The git repository to read (default: the repository root).", "DIR")]
        git: Option<String>,
    }
    let (options, free) = Options::from_arguments_relaxed(
        "USAGE: abelian git pull [--fork FORK] [--git DIR] [branch]",
        args,
    );
    reject_extra("git pull", &free, 1)?;
    let repo = repo()?;
    let git_dir = options.git.as_ref().map(std::path::PathBuf::from);
    // The binding is load-bearing: which fork mirrors git, and the branch it
    // fast-forwards from.  Bind on first use; thereafter it is authoritative.
    let existing = repo.read_mirror()?;
    let (fork, branch, bind_now) = match existing {
        Some(binding) => {
            // A branch argument, if given, must match the binding.
            if let Some(arg) = free.first()
                && *arg != binding.branch
            {
                return Err(Error::Invalid(format!(
                    "fork {} is bound to branch {}, not {arg}; use `git reanchor` to rebind",
                    binding.fork, binding.branch
                )));
            }
            if let Some(f) = &options.fork
                && f != &binding.fork
            {
                return Err(Error::Invalid(format!(
                    "the mirror is fork {}, not {f}",
                    binding.fork
                )));
            }
            (binding.fork, binding.branch, false)
        }
        None => {
            let branch = free.first().map(|s| s.to_string()).ok_or_else(|| {
                Error::Invalid(
                    "no mirror binding yet; name the branch to bind: git pull <branch>"
                        .to_string(),
                )
            })?;
            let fork = options.fork.as_deref().unwrap_or("main").to_string();
            (fork, branch, true)
        }
    };
    let summary = abelian::git::pull(&repo, git_dir.as_deref(), &branch, &fork)?;
    if bind_now {
        repo.write_mirror(&abelian::repo::MirrorBinding {
            fork: fork.clone(),
            branch: branch.clone(),
        })?;
    }
    // One commit, one line; report the binding — the read path status wants.
    if summary.imported.is_empty() {
        println!("mirror {fork} bound to {branch}, up to date at {}", summary.commit);
        return Ok(());
    }
    match &summary.base {
        Some(base) => println!("fast-forwarded mirror {fork} from {base} to {}", summary.commit),
        None => println!(
            "bound mirror {fork} to {branch}, imported fresh history to {}",
            summary.commit
        ),
    }
    println!("imported {} commit(s)", summary.imported.len());
    println!("head {}", repo.current_state(&fork)?.sum.hexdigest());
    Ok(())
}

fn cmd_git_reanchor(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "The mirror fork to recover (default: the bound fork).", "FORK")]
        fork: Option<String>,
        #[arrrg(optional, "The git repository to read (default: the repository root).", "DIR")]
        git: Option<String>,
    }
    let (options, free) = Options::from_arguments_relaxed(
        "USAGE: abelian git reanchor [--fork FORK] [--git DIR] <commit>",
        args,
    );
    reject_extra("git reanchor", &free, 1)?;
    let Some(committish) = free.first() else {
        return Err(Error::Invalid("git reanchor requires a commit".to_string()));
    };
    let repo = repo()?;
    let binding = repo.read_mirror()?;
    let fork = options
        .fork
        .clone()
        .or_else(|| binding.as_ref().map(|b| b.fork.clone()))
        .unwrap_or_else(|| "main".to_string());
    let git_dir = options.git.as_ref().map(std::path::PathBuf::from);
    let commit = abelian::git::reanchor(&repo, git_dir.as_deref(), committish, &fork)?;
    println!("reanchored mirror {fork} onto {commit}");
    println!("head {}", repo.current_state(&fork)?.sum.hexdigest());
    Ok(())
}

fn cmd_git_pr(_args: &[&str]) -> Result<()> {
    // Rendering a fork as a git branch (one commit per fused beat) and
    // opening the pull request is a separate bridge component; the command
    // exists so the namespace is complete, but the mechanism is not wired.
    Err(Error::Invalid(
        "git pr is not yet implemented: rendering a fork as a git branch and \
         opening a pull request lands next"
            .to_string(),
    ))
}

fn cmd_sum(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(flag, "Print only the sum, not the records.")]
        quiet: bool,
    }
    let (options, free) = Options::from_arguments_relaxed("USAGE: abelian sum [--quiet]", args);
    reject_extra("sum", &free, 0)?;
    let repo = repo()?;
    let records = repo.records_of_working_tree()?;
    let mut sum = Sum::zero();
    for record in &records {
        if !options.quiet {
            print!("{}", String::from_utf8_lossy(&record.to_bytes()));
        }
        sum.insert(&record.to_bytes());
    }
    println!("{}", sum.hexdigest());
    Ok(())
}

fn cmd_status(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "The fork to compare against (default: main).", "FORK")]
        fork: Option<String>,
        #[arrrg(flag, "Print machine-readable lines: <code> <TAB> <path>.")]
        porcelain: bool,
    }
    let (options, free) =
        Options::from_arguments_relaxed("USAGE: abelian status [--fork FORK] [--porcelain]", args);
    reject_extra("status", &free, 0)?;
    let fork = options.fork.as_deref().unwrap_or("main");
    let repo = repo()?;
    // The pending patch: what the working tree differs from the ref by.  Not
    // index-vs-worktree (there is no index) but materialized-vs-committed.
    let committed = repo.current_state(fork)?;
    let working = repo.working_tree_manifest()?;
    let changes = abelian::diff::diff_manifests(&committed.manifest, &working);
    let pending = abelian::diff::pending_sum(&committed.sum, &working.sum());
    if options.porcelain {
        for change in &changes {
            println!("{}\t{}", change.code(), change.path);
        }
        return Ok(());
    }
    println!("fork {fork}");
    // The mirror binding is load-bearing state (the bridge-ownership bit),
    // and phase-1 users live here: name which fork mirrors git and the
    // branch it is bound to, so the binding has a read path.
    match repo.read_mirror()? {
        Some(binding) if binding.fork == fork => {
            println!("mirror  bound to git branch {}", binding.branch);
        }
        Some(binding) => {
            println!("mirror  fork {} (bound to git branch {})", binding.fork, binding.branch);
        }
        None => {}
    }
    println!("ref     {}", committed.sum.hexdigest());
    println!("working {}", working.sum().hexdigest());
    if changes.is_empty() {
        println!("clean (nothing pending)");
        return Ok(());
    }
    // The setsum difference is a verifiable checksum of the symmetric
    // difference: it is zero exactly when the states agree, and it names
    // this pending patch independently of the order the edits were made.
    println!("pending {}", pending.hexdigest());
    println!();
    for change in &changes {
        println!("  {}  {}", change.code(), change.path);
    }
    println!();
    let (a, d, m) = changes.iter().fold((0, 0, 0), |(a, d, m), c| match c.code() {
        'A' => (a + 1, d, m),
        'D' => (a, d + 1, m),
        _ => (a, d, m + 1),
    });
    println!("{a} added, {d} deleted, {m} modified");
    Ok(())
}

fn cmd_check(args: &[&str]) -> Result<()> {
    let (options, free) =
        ForkOptions::from_arguments_relaxed("USAGE: abelian check [--fork FORK]", args);
    reject_extra("check", &free, 0)?;
    let repo = repo()?;
    let (expected, actual) = repo.check(options.fork())?;
    println!("log expects   {}", expected.hexdigest());
    println!("working tree  {}", actual.hexdigest());
    if expected != actual {
        return Err(Error::Corrupt(
            "the working tree does not match the log's expectation".to_string(),
        ));
    }
    println!("ok");
    Ok(())
}

fn cmd_snapshot(args: &[&str]) -> Result<()> {
    let (options, free) =
        ForkOptions::from_arguments_relaxed("USAGE: abelian snapshot [--fork FORK]", args);
    reject_extra("snapshot", &free, 0)?;
    let repo = repo()?;
    let sum = repo.snapshot(options.fork())?;
    println!("anchored {} at {sum}", options.fork());
    Ok(())
}

fn cmd_materialize(args: &[&str]) -> Result<()> {
    let (options, free) = ForkOptions::from_arguments_relaxed(
        "USAGE: abelian materialize [--fork FORK] <rev> [dest]",
        args,
    );
    reject_extra("materialize", &free, 2)?;
    let Some(spec) = free.first() else {
        return Err(Error::Invalid("materialize requires a revision".to_string()));
    };
    let repo = repo()?;
    // Any revision resolves: HEAD, a fork, sum:S, line:ID.  materialize no
    // longer demands a raw sum — rev-parse exists, let it resolve.
    let resolved = abelian::revision::resolve(&repo, spec, options.fork())?;
    let manifest = repo.manifest_at_lineage(&resolved.fork, &resolved.sum)?;
    // Whole-tree, elsewhere, non-destructive: default to a fresh directory
    // beside the repository rather than over the working tree (that is
    // restore's job).
    let dest = match free.get(1) {
        Some(dest) => std::path::PathBuf::from(dest),
        None => {
            let name = format!("abelian-{}", short(&resolved.sum));
            std::path::PathBuf::from(name)
        }
    };
    if dest.exists() {
        return Err(Error::Invalid(format!(
            "destination {} already exists; name an empty or new directory",
            dest.display()
        )));
    }
    repo.materialize_into(&manifest, &dest)?;
    println!(
        "materialized {} elements at {} into {}",
        manifest.len(),
        short(&resolved.sum),
        dest.display()
    );
    Ok(())
}

fn cmd_restore(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "The fork to restore from (default: main).", "FORK")]
        fork: Option<String>,
    }
    // A trailing `--` restricts to a pathspec, as diff does.
    let (left, paths): (Vec<&str>, Vec<String>) = match args.iter().position(|a| *a == "--") {
        Some(i) => (
            args[..i].to_vec(),
            args[i + 1..].iter().map(|p| normalize_path_filter(p)).collect(),
        ),
        None => (args.to_vec(), Vec::new()),
    };
    let (options, revs) = Options::from_arguments_relaxed(
        "USAGE: abelian restore [--fork FORK] [rev] [-- path...]",
        &left,
    );
    reject_extra("restore", &revs, 1)?;
    let fork = options.fork.as_deref().unwrap_or("main");
    let repo = repo()?;
    // Default to HEAD: discard uncommitted working-tree edits.
    let spec = revs.first().map(String::as_str).unwrap_or("HEAD");
    let resolved = abelian::revision::resolve(&repo, spec, fork)?;
    let target = repo.manifest_at_lineage(&resolved.fork, &resolved.sum)?;
    let filters = if paths.is_empty() { None } else { Some(paths.as_slice()) };
    let actions = repo.restore(&target, filters)?;
    for (code, path) in &actions {
        println!("{code}  {path}");
    }
    println!("{} path(s) restored to {}", actions.len(), short(&resolved.sum));
    Ok(())
}

fn cmd_repoint(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "The fork to repoint (default: main).", "FORK")]
        fork: Option<String>,
        #[arrrg(optional, "Author of the repoint line.", "AUTHOR")]
        author: Option<String>,
        #[arrrg(optional, "Narrative prose for the repoint.", "PROSE")]
        prose: Option<String>,
    }
    let (options, free) =
        Options::from_arguments_relaxed("USAGE: abelian repoint [--fork FORK] <rev>", args);
    reject_extra("repoint", &free, 1)?;
    let Some(spec) = free.first() else {
        return Err(Error::Invalid("repoint requires a revision".to_string()));
    };
    let fork = options.fork.as_deref().unwrap_or("main");
    let repo = repo()?;
    let resolved = abelian::revision::resolve(&repo, spec, fork)?;
    let target = repo.manifest_at_lineage(&resolved.fork, &resolved.sum)?;
    let author = options.author.unwrap_or_else(whoami);
    let line = repo.repoint(fork, &target, &author, options.prose)?;
    println!("{} {}", line.id, line.sum_after);
    println!(
        "repointed {fork} to {} (non-destructive: the prior state remains reachable)",
        short(&resolved.sum)
    );
    Ok(())
}

fn cmd_rev_parse(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "The fork to resolve against (default: main).", "FORK")]
        fork: Option<String>,
        #[arrrg(flag, "Print the resolving line id instead of the state sum.")]
        line: bool,
        #[arrrg(flag, "Print sum, line id, and fork, one field per line.")]
        verbose: bool,
    }
    let (options, free) = Options::from_arguments_relaxed(
        "USAGE: abelian rev-parse [--fork FORK] [--line|--verbose] <rev>",
        args,
    );
    reject_extra("rev-parse", &free, 1)?;
    let Some(spec) = free.first() else {
        return Err(Error::Invalid("rev-parse requires a revision".to_string()));
    };
    let repo = repo()?;
    let resolved =
        abelian::revision::resolve(&repo, spec, options.fork.as_deref().unwrap_or("main"))?;
    if options.verbose {
        println!("sum  {}", resolved.sum);
        println!("line {}", resolved.line.as_deref().unwrap_or("(anchor: no line)"));
        println!("fork {}", resolved.fork);
    } else if options.line {
        match &resolved.line {
            Some(id) => println!("{id}"),
            None => {
                return Err(Error::Invalid(format!(
                    "revision {spec:?} names the base anchor, which no line produced"
                )));
            }
        }
    } else {
        println!("{}", resolved.sum);
    }
    Ok(())
}

fn cmd_diff(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "The fork to resolve revisions against (default: main).", "FORK")]
        fork: Option<String>,
        #[arrrg(flag, "Summarize changes per file instead of showing hunks.")]
        stat: bool,
        #[arrrg(flag, "Do not pair a delete with an add of the same blob as a rename.")]
        no_renames: bool,
        #[arrrg(optional, "Lines of context in hunks (default: 3).", "N")]
        context: Option<usize>,
    }
    // Split off an explicit pathspec: everything after `--` restricts the
    // diff to matching element paths.
    let (left, paths): (Vec<&str>, Vec<String>) = match args.iter().position(|a| *a == "--") {
        Some(i) => (
            args[..i].to_vec(),
            args[i + 1..].iter().map(|p| normalize_path_filter(p)).collect(),
        ),
        None => (args.to_vec(), Vec::new()),
    };
    let (options, revs) = Options::from_arguments_relaxed(
        "USAGE: abelian diff [--fork FORK] [--stat] [--no-renames] [rev [rev]] [-- path...]",
        &left,
    );
    let fork = options.fork.as_deref().unwrap_or("main");
    let context = options.context.unwrap_or(3);
    let repo = repo()?;
    // Resolve the two sides.  Zero revs: ref vs working tree.  One rev: that
    // rev vs working tree.  Two revs: rev vs rev.
    let (before, after, label_a, label_b) = match revs.as_slice() {
        [] => {
            let state = repo.current_state(fork)?;
            (state.manifest, repo.working_tree_manifest()?, "ref".to_string(), "working".to_string())
        }
        [a] => {
            let ra = abelian::revision::resolve(&repo, a, fork)?;
            let ma = repo.manifest_at_lineage(&ra.fork, &ra.sum)?;
            (ma, repo.working_tree_manifest()?, short(&ra.sum), "working".to_string())
        }
        [a, b] => {
            let ra = abelian::revision::resolve(&repo, a, fork)?;
            let rb = abelian::revision::resolve(&repo, b, fork)?;
            let ma = repo.manifest_at_lineage(&ra.fork, &ra.sum)?;
            let mb = repo.manifest_at_lineage(&rb.fork, &rb.sum)?;
            (ma, mb, short(&ra.sum), short(&rb.sum))
        }
        _ => {
            return Err(Error::Invalid("diff takes at most two revisions".to_string()));
        }
    };
    let mut changes = abelian::diff::diff_manifests(&before, &after);
    if !paths.is_empty() {
        changes.retain(|c| path_matches(&c.path, &paths));
    }
    let (renames, changes) = if options.no_renames {
        (Vec::new(), changes)
    } else {
        abelian::diff::detect_renames(&changes)
    };
    let blobs = repo.blobs();
    let read = |hash: &str| -> Result<Vec<u8>> { blobs.get(hash) };

    if options.stat {
        let mut total_ins = 0;
        let mut total_del = 0;
        for r in &renames {
            println!(" {} => {} (rename)", r.from.path, r.to.path);
        }
        for c in &changes {
            let old = match &c.before {
                Some(rec) => read(&rec.blob)?,
                None => Vec::new(),
            };
            let new = match &c.after {
                Some(rec) => read(&rec.blob)?,
                None => Vec::new(),
            };
            let (ins, del) = abelian::diff::line_stat(&old, &new);
            total_ins += ins;
            total_del += del;
            println!(" {} | +{ins} -{del}", c.path);
        }
        println!(
            "{} file(s) changed, {total_ins} insertion(s), {total_del} deletion(s)",
            changes.len() + renames.len()
        );
        return Ok(());
    }

    for r in &renames {
        println!("rename {} => {}", r.from.path, r.to.path);
        if r.from.mode != r.to.mode {
            println!("  mode {} => {}", r.from.mode, r.to.mode);
        }
    }
    for c in &changes {
        let old = match &c.before {
            Some(rec) => read(&rec.blob)?,
            None => Vec::new(),
        };
        let new = match &c.after {
            Some(rec) => read(&rec.blob)?,
            None => Vec::new(),
        };
        // Mode-only change with identical blob: report it, no content hunk.
        if let (Some(b), Some(a)) = (&c.before, &c.after)
            && b.blob == a.blob
            && b.mode != a.mode
        {
            println!("mode {} {} => {}", c.path, b.mode, a.mode);
            continue;
        }
        let la = format!("{}/{}", label_a, c.path.trim_start_matches('/'));
        let lb = format!("{}/{}", label_b, c.path.trim_start_matches('/'));
        print!("{}", abelian::diff::unified(&old, &new, &la, &lb, context));
    }
    Ok(())
}

fn cmd_blame(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "The fork to blame against (default: main).", "FORK")]
        fork: Option<String>,
    }
    let (options, free) =
        Options::from_arguments_relaxed("USAGE: abelian blame [--fork FORK] <path>", args);
    reject_extra("blame", &free, 1)?;
    let Some(raw) = free.first() else {
        return Err(Error::Invalid("blame requires a path".to_string()));
    };
    let path = normalize_path_filter(raw);
    let fork = options.fork.as_deref().unwrap_or("main");
    let repo = repo()?;
    let blamed = repo.blame(fork, &path)?;
    // Join owner ids with their annotations so blame reads as provenance,
    // not just ids: short id, author, one line of text.
    let state = repo.current_state(fork)?;
    let history = repo.continuity_log(fork)?;
    let author_of = |id: &str| -> String {
        history
            .iter()
            .map(|(_, l)| l)
            .chain(state.lines.iter())
            .find(|l| l.id == id)
            .map(|l| l.annotation.author.clone())
            .unwrap_or_else(|| "?".to_string())
    };
    let width = blamed.len().to_string().len();
    for (n, bl) in blamed.iter().enumerate() {
        let short_id = bl.owner.chars().take(8).collect::<String>();
        let author = if bl.owner.is_empty() { "?".to_string() } else { author_of(&bl.owner) };
        println!("{short_id:8}  {author:>12}  {:>width$}  {}", n + 1, bl.text);
    }
    Ok(())
}

/// Normalize a pathspec argument to an absolute element path for matching.
fn normalize_path_filter(p: &str) -> String {
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
}

/// True iff `path` is or is under one of the filters.
fn path_matches(path: &str, filters: &[String]) -> bool {
    filters
        .iter()
        .any(|f| path == f || path.starts_with(&format!("{}/", f.trim_end_matches('/'))))
}

/// A short, human-readable form of a sum for diff labels.
fn short(sum: &str) -> String {
    sum.chars().take(12).collect()
}

/// Reject positional arguments a command does not consume, rather than
/// silently ignoring them.  `max` is how many leading free args the command
/// uses; anything past that is an error.  We parse with arrrg's `relaxed`
/// variant, which only relaxes the enforcement that flags appear in arrrg's
/// canonical (sorted) order — the strict variant panics on a non-canonical
/// command line.  Relaxing that check does not change how leftover positional
/// arguments are returned, so it is still on each command to reject the
/// extras; an unrecognized argument almost always means a mistyped flag or a
/// misplaced word, and swallowing it hides the bug.
fn reject_extra(command: &str, free: &[String], max: usize) -> Result<()> {
    if free.len() > max {
        return Err(Error::Invalid(format!(
            "{command}: unexpected argument(s): {}",
            free[max..].join(" ")
        )));
    }
    Ok(())
}

fn cmd_apply(args: &[&str]) -> Result<()> {
    let (options, free) = ApplyOptions::from_arguments_relaxed(
        "USAGE: abelian apply [options] <patch.json>",
        args,
    );
    reject_extra("apply", &free, 1)?;
    let Some(patch_path) = free.first() else {
        return Err(Error::Invalid("apply requires a patch file (or - for stdin)".to_string()));
    };
    let bytes = if *patch_path == "-" {
        use std::io::Read;
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf).map_err(abelian::ioerr("reading stdin"))?;
        buf
    } else {
        std::fs::read(patch_path).map_err(abelian::ioerr(format!("reading {patch_path}")))?
    };
    let intent: Intent = serde_json::from_slice(&bytes)?;
    let repo = repo()?;
    let provenance = match options.provenance.as_deref() {
        None | Some("agent") => Provenance::Agent,
        Some("andon") => Provenance::Andon,
        Some(other) => {
            return Err(Error::Invalid(format!(
                "provenance {other:?} cannot be requested; union lines come from union"
            )));
        }
    };
    let sig = match (&options.sig, options.sign) {
        (Some(sig), _) => Some(sig.clone()),
        (None, true) => {
            // v0 placeholder scheme: a detached signature blob; deliberately
            // under-specified (§2.5).
            let author = options.author.as_deref().unwrap_or("anonymous");
            let body = format!("abelian detached signature v0\nsigned-by: {author}\n");
            Some(repo.blobs().put(body.as_bytes())?)
        }
        (None, false) => None,
    };
    let annotation = Annotation {
        author: options.author.clone().unwrap_or_else(whoami),
        provenance,
        reason: options.reason.clone(),
        sig,
        session: options.session.clone(),
        prose: options.prose.clone(),
        reads: None,
        origin: None,
        view: None,
        import: None,
    };
    let line = repo.apply(options.fork.as_deref().unwrap_or("main"), intent, annotation)?;
    println!("{} {}", line.id, line.sum_after);
    Ok(())
}

fn cmd_log(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "The fork to operate on (default: main).", "FORK")]
        fork: Option<String>,
        #[arrrg(optional, "The named view to render (default: fused).", "NAME")]
        view: Option<String>,
        #[arrrg(flag, "Render the un-fused chain: every line, full.")]
        raw: bool,
    }
    let (options, free) = Options::from_arguments_relaxed(
        "USAGE: abelian log [--fork FORK] [--view NAME | --raw]",
        args,
    );
    reject_extra("log", &free, 0)?;
    if options.raw && options.view.is_some() {
        return Err(Error::Invalid("--raw and --view are mutually exclusive".to_string()));
    }
    let fork = options.fork.as_deref().unwrap_or("main");
    let repo = repo()?;
    // Follow the fork across its lineage: its own log, then—when exhausted—
    // the log of the fork it was forked from, and so on to the root.
    let history = repo.continuity_log(fork)?;
    if options.raw {
        // The un-fused chain: every line, full, git-log style.
        let mut shown = fork.to_string();
        for (fork, line) in history.iter().rev() {
            if fork != &shown {
                println!("--- {fork} ---");
                shown = fork.clone();
            }
            print_line_full(line);
        }
        return Ok(());
    }
    // A named view is a zoom level, not a different interface.  `fused` is
    // the default view; agent-maintained views slot in here by name.
    let view = options.view.as_deref().unwrap_or("fused");
    if view != "fused" {
        return Err(Error::Invalid(format!(
            "unknown view {view:?}; the only view is `fused` (--raw shows the un-fused chain)"
        )));
    }
    let lines: Vec<abelian::log::LogLine> =
        history.into_iter().map(|(_, line)| line).collect();
    for beat in fused_beats(&lines) {
        match beat {
            Beat::Fused { view, lines } => {
                let header = format!(
                    "{}  fuse({} lines)  {}",
                    lines.last().map(|l| l.id.as_str()).unwrap_or(""),
                    lines.len(),
                    view.annotation.author,
                );
                print_prose(&header, view.annotation.prose.as_deref().unwrap_or(""));
            }
            Beat::Single(line) => print_line_brief(line),
        }
    }
    Ok(())
}

fn line_header(line: &abelian::log::LogLine) -> String {
    let provenance = match line.annotation.provenance {
        Provenance::Agent => "agent",
        Provenance::Andon => "ANDON",
        Provenance::Union => "union",
        Provenance::View => "view",
    };
    format!("{}  {}  {}", line.id, provenance, line.annotation.author)
}

fn print_line_brief(line: &abelian::log::LogLine) {
    let header = line_header(line);
    // git-log style: only the subject (first line of the message) shares the
    // header line; the body is omitted for a one-line-per-commit summary.
    let prose = line.annotation.prose.as_deref().unwrap_or("");
    let subject = prose.split('\n').next().unwrap_or("");
    println!("{header}  {subject}");
}

/// Format a commit time (milliseconds since the Unix epoch) git-log style:
/// `Sun Aug 9 09:48:51 2026 -0700`, in the local timezone.
fn format_committed(committed_ms: u64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(committed_ms as i64) {
        chrono::LocalResult::Single(dt) => dt.format("%a %b %-d %H:%M:%S %Y %z").to_string(),
        _ => String::new(),
    }
}

fn print_line_full(line: &abelian::log::LogLine) {
    let provenance = match line.annotation.provenance {
        Provenance::Agent => "agent",
        Provenance::Andon => "ANDON",
        Provenance::Union => "union",
        Provenance::View => "view",
    };
    // git-log style: the id line stands alone, then Author and Date each on
    // their own line, mirroring `git log`'s commit/Author/Date block.
    let header = format!(
        "{}  {}\nAuthor: {}\nDate:   {}",
        line.id,
        provenance,
        line.annotation.author,
        format_committed(line.committed_ms),
    );
    print_prose(&header, line.annotation.prose.as_deref().unwrap_or(""));
}

/// Render a header line followed by prose, git-log style: the header stands
/// alone, then a blank line, then every prose line—subject included—is
/// indented four spaces.  Blank prose lines stay blank (no trailing
/// whitespace).  A trailing blank line closes the entry.
fn print_prose(header: &str, prose: &str) {
    println!("{header}");
    println!();
    for line in prose.trim_end_matches('\n').split('\n') {
        if line.is_empty() {
            println!();
        } else {
            println!("    {line}");
        }
    }
    println!();
}

fn cmd_show(args: &[&str]) -> Result<()> {
    let (options, free) =
        ForkOptions::from_arguments_relaxed("USAGE: abelian show [--fork FORK] <id>", args);
    reject_extra("show", &free, 1)?;
    let Some(id) = free.first() else {
        return Err(Error::Invalid("show requires a line id".to_string()));
    };
    let repo = repo()?;
    let state = repo.current_state(options.fork())?;
    let line = state
        .lines
        .iter()
        .find(|l| l.id == *id || l.id.starts_with(id.as_str()))
        .ok_or_else(|| Error::Invalid(format!("no line {id} on fork {}", options.fork())))?;
    println!("{}", serde_json::to_string_pretty(line)?);
    Ok(())
}

fn cmd_fuse(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "The fork to operate on (default: main).", "FORK")]
        fork: Option<String>,
        #[arrrg(optional, "The narrative beat.", "PROSE")]
        prose: Option<String>,
        #[arrrg(optional, "Author of the fuse.", "AUTHOR")]
        author: Option<String>,
    }
    let (options, free) = Options::from_arguments_relaxed(
        "USAGE: abelian fuse [--fork FORK] [--prose P] <from-id> <to-id>",
        args,
    );
    reject_extra("fuse", &free, 2)?;
    let (Some(from), Some(to)) = (free.first(), free.get(1)) else {
        return Err(Error::Invalid("fuse requires <from-id> <to-id>".to_string()));
    };
    let repo = repo()?;
    let line = repo.fuse(
        options.fork.as_deref().unwrap_or("main"),
        from,
        to,
        options.prose,
        &options.author.unwrap_or_else(whoami),
    )?;
    println!(
        "{} fused {from}..{to} (lossless: the fine structure remains underneath)",
        line.id
    );
    Ok(())
}

fn cmd_gc_blobs(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(flag, "Report what would be collected without removing anything.")]
        dry_run: bool,
    }
    let (options, free) =
        Options::from_arguments_relaxed("USAGE: abelian gc-blobs [--dry-run]", args);
    reject_extra("gc-blobs", &free, 0)?;
    let repo = repo()?;
    let collected = repo.gc_blobs(options.dry_run)?;
    for hash in &collected {
        if options.dry_run {
            println!("would collect {hash}");
        } else {
            println!("collected {hash}");
        }
    }
    println!("{} unreachable blob(s)", collected.len());
    Ok(())
}

fn cmd_fork(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "The fork to anchor at (default: main).", "FORK")]
        from: Option<String>,
    }
    let (options, free) =
        Options::from_arguments_relaxed("USAGE: abelian fork [--from FORK] <name>", args);
    reject_extra("fork", &free, 1)?;
    let Some(name) = free.first() else {
        return Err(Error::Invalid("fork requires a name".to_string()));
    };
    let repo = repo()?;
    let fork = repo.create_fork(name, options.from.as_deref().unwrap_or("main"))?;
    println!("fork {name} anchored at {}", fork.anchor);
    Ok(())
}

fn cmd_remove_fork(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(flag, "Delete even work no other fork has taken up (git branch -D).")]
        force: bool,
    }
    let (options, free) =
        Options::from_arguments_relaxed("USAGE: abelian remove-fork [--force] <name>", args);
    reject_extra("remove-fork", &free, 1)?;
    let Some(name) = free.first() else {
        return Err(Error::Invalid("remove-fork requires a fork name".to_string()));
    };
    let repo = repo()?;
    repo.remove_fork(name, options.force)?;
    println!("removed fork {name}");
    Ok(())
}

fn cmd_union(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "The target fork (default: main).", "FORK")]
        into: Option<String>,
        #[arrrg(optional, "Author of the union lines.", "AUTHOR")]
        author: Option<String>,
    }
    let (options, free) = Options::from_arguments_relaxed(
        "USAGE: abelian union [--into FORK] <fork>",
        args,
    );
    reject_extra("union", &free, 1)?;
    let Some(source) = free.first() else {
        return Err(Error::Invalid("union requires a source fork".to_string()));
    };
    let repo = repo()?;
    let target = options.into.as_deref().unwrap_or("main");
    let author = options.author.unwrap_or_else(whoami);
    let outcome = union(&repo, source, target, &author)?;
    if outcome.already_identical {
        println!("already identical (stratum 1: arithmetic)");
    }
    for landed in &outcome.landed {
        let stratum = match landed.stratum {
            Stratum::RealizedReplay => "stratum 2: realized replay",
            Stratum::IntentReplay => "stratum 3: intent replay",
        };
        println!("landed {} <- {}  ({stratum})", landed.line.id, landed.origin_id);
    }
    if let Some((line_id, evidence)) = &outcome.needs_reenactment {
        return Err(Error::NeedsReenactment(format!(
            "line {line_id} of fork {source}: {evidence} \
             (stratum 4 costs tokens and is never automatic)"
        )));
    }
    Ok(())
}

fn cmd_clone(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {}
    let (_, free) = Options::from_arguments_relaxed("USAGE: abelian clone <store> <dest>", args);
    reject_extra("clone", &free, 2)?;
    let (Some(store), Some(dest)) = (free.first(), free.get(1)) else {
        return Err(Error::Invalid("clone requires <store> <dest>".to_string()));
    };
    let store = FsStore::open(store)?;
    let repo = abelian::wire::clone(&store, dest)?;
    println!("cloned into {}", repo.root().display());
    Ok(())
}

fn cmd_fetch(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {}
    let (_, free) = Options::from_arguments_relaxed("USAGE: abelian fetch <store> <cache>", args);
    reject_extra("fetch", &free, 2)?;
    let (Some(store), Some(cache)) = (free.first(), free.get(1)) else {
        return Err(Error::Invalid("fetch requires <store> <cache>".to_string()));
    };
    let store = FsStore::open(store)?;
    match abelian::wire::fetch(&store, std::path::Path::new(cache))? {
        Some(manifest) => println!("fetched manifest seq {} ({})", manifest.seq, manifest.id),
        None => println!("the store has no manifest"),
    }
    Ok(())
}

fn cmd_push(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "zstd level for pack-for-push (default: 3, per SPEC 5).", "LEVEL")]
        level: Option<i32>,
    }
    let (options, free) =
        Options::from_arguments_relaxed("USAGE: abelian push [--level N] <store>", args);
    reject_extra("push", &free, 1)?;
    let Some(store) = free.first() else {
        return Err(Error::Invalid("push requires <store>".to_string()));
    };
    let repo = repo()?;
    let store = FsStore::open(store)?;
    let manifest = abelian::wire::push(&repo, &store, options.level.unwrap_or(3))?;
    println!("pushed manifest seq {} ({})", manifest.seq, manifest.id);
    Ok(())
}

fn cmd_pack(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "zstd level for pack-at-rest (default: 19, per SPEC 5).", "LEVEL")]
        level: Option<i32>,
    }
    let (options, free) =
        Options::from_arguments_relaxed("USAGE: abelian pack [--level N] <dir>", args);
    reject_extra("pack", &free, 1)?;
    let Some(dir) = free.first() else {
        return Err(Error::Invalid("pack requires an output directory".to_string()));
    };
    let repo = repo()?;
    let manifest = abelian::serve::pack(
        &repo,
        std::path::Path::new(dir),
        1,
        "",
        options.level.unwrap_or(19),
    )?;
    println!("packed manifest seq {} ({})", manifest.seq, manifest.id);
    Ok(())
}

fn cmd_unpack(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {}
    let (_, free) = Options::from_arguments_relaxed("USAGE: abelian unpack <dir> <dest>", args);
    reject_extra("unpack", &free, 2)?;
    let (Some(dir), Some(dest)) = (free.first(), free.get(1)) else {
        return Err(Error::Invalid("unpack requires <dir> <dest>".to_string()));
    };
    let repo =
        abelian::serve::unpack_dir(std::path::Path::new(dir), std::path::Path::new(dest))?;
    println!("unpacked into {}", repo.root().display());
    Ok(())
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "anonymous".to_string())
}
