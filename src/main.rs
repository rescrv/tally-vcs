//! tally: the command over the abelian substrate.
//!
//! Named for the split tally stick — the twelfth century's distributed,
//! two-party, checksummed ledger.  Every subcommand here automates a step
//! the README teaches by hand; with zero `tally` binaries available, the
//! README remains a complete, operable version control system.

use std::process::exit;

use arrrg::CommandLine;

use abelian::claims::Claim;
use abelian::ident::Sum;
use abelian::log::{Annotation, Provenance};
use abelian::patch::Intent;
use abelian::repo::Repository;
use abelian::union::{Stratum, union};
use abelian::views::{Beat, View, ViewAnnotation, fused_beats};
use abelian::wire::FsStore;
use abelian::{Error, Result};

const USAGE: &str = "USAGE: tally <command> [options] [args]

repository:
  init                             create a repository here
  sum                              print the working tree's element records and sum
  check                            compare the working tree against the log's expectation
  snapshot                         write a manifest at the current state and repoint the fork
  materialize <sum>                produce a working tree at a prior state

patches:
  apply <patch.json>               validate and apply an intent; append to the log
  log                              render the chain
  show <id>                        render one log line
  read                             render history (--fused, --raw, --claims)
  fuse <from-id> <to-id>           compose a span into one narrative beat (lossless)

claims:
  claim <cmd>                      run a command attested; file the claim
  claims                           list claims (--stale for drifted ones)

forks:
  fork <name>                      create a fork (anchor + empty log)
  union <fork>                     bring a fork's log into another (strata 1-4)

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
        "sum" => cmd_sum(&rest),
        "check" => cmd_check(&rest),
        "snapshot" => cmd_snapshot(&rest),
        "materialize" => cmd_materialize(&rest),
        "apply" => cmd_apply(&rest),
        "log" => cmd_log(&rest),
        "show" => cmd_show(&rest),
        "read" => cmd_read(&rest),
        "fuse" => cmd_fuse(&rest),
        "claim" => cmd_claim(&rest),
        "claims" => cmd_claims(&rest),
        "fork" => cmd_fork(&rest),
        "union" => cmd_union(&rest),
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
            eprintln!("tally: unknown command {other:?}\n");
            eprint!("{USAGE}");
            exit(64);
        }
    };
    if let Err(err) = result {
        eprintln!("tally {command}: {err}");
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
    let (_, free) = Options::from_arguments_relaxed("USAGE: tally init [dir]", args);
    let dir = match free.first() {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::env::current_dir().map_err(abelian::ioerr("getting cwd"))?,
    };
    let repo = Repository::init(&dir)?;
    println!("initialized empty abelian repository at {}", repo.root().display());
    Ok(())
}

fn cmd_sum(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(flag, "Print only the sum, not the records.")]
        quiet: bool,
    }
    let (options, _) = Options::from_arguments_relaxed("USAGE: tally sum [--quiet]", args);
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

fn cmd_check(args: &[&str]) -> Result<()> {
    let (options, _) =
        ForkOptions::from_arguments_relaxed("USAGE: tally check [--fork FORK]", args);
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
    let (options, _) =
        ForkOptions::from_arguments_relaxed("USAGE: tally snapshot [--fork FORK]", args);
    let repo = repo()?;
    let sum = repo.snapshot(options.fork())?;
    println!("anchored {} at {sum}", options.fork());
    Ok(())
}

fn cmd_materialize(args: &[&str]) -> Result<()> {
    let (options, free) = ForkOptions::from_arguments_relaxed(
        "USAGE: tally materialize [--fork FORK] <sum>",
        args,
    );
    let Some(sum_hex) = free.first() else {
        return Err(Error::Invalid("materialize requires a sum".to_string()));
    };
    let repo = repo()?;
    let manifest = repo.manifest_at(options.fork(), sum_hex)?;
    repo.materialize(&manifest)?;
    println!("materialized {} elements at {sum_hex}", manifest.len());
    Ok(())
}

fn cmd_apply(args: &[&str]) -> Result<()> {
    let (options, free) = ApplyOptions::from_arguments_relaxed(
        "USAGE: tally apply [options] <patch.json>",
        args,
    );
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
        claims: Vec::new(),
        origin: None,
    };
    let line = repo.apply(options.fork.as_deref().unwrap_or("main"), intent, annotation)?;
    println!("{} {}", line.id, line.sum_after);
    Ok(())
}

fn cmd_log(args: &[&str]) -> Result<()> {
    let (options, _) = ForkOptions::from_arguments_relaxed("USAGE: tally log [--fork FORK]", args);
    let repo = repo()?;
    let state = repo.current_state(options.fork())?;
    for line in state.lines.iter().rev() {
        print_line_brief(line);
    }
    Ok(())
}

fn print_line_brief(line: &abelian::log::LogLine) {
    let provenance = match line.annotation.provenance {
        Provenance::Agent => "agent",
        Provenance::Andon => "ANDON",
        Provenance::Union => "union",
    };
    println!(
        "{}  {}  {}  {}",
        line.id,
        provenance,
        line.annotation.author,
        line.annotation.prose.as_deref().unwrap_or(""),
    );
}

fn cmd_show(args: &[&str]) -> Result<()> {
    let (options, free) =
        ForkOptions::from_arguments_relaxed("USAGE: tally show [--fork FORK] <id>", args);
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

fn cmd_read(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "The fork to operate on (default: main).", "FORK")]
        fork: Option<String>,
        #[arrrg(flag, "Render the human narrative (fused spans collapse).")]
        fused: bool,
        #[arrrg(flag, "Render the raw tool-call stream (full lines).")]
        raw: bool,
        #[arrrg(flag, "Render what was proven when (claims).")]
        claims: bool,
    }
    let (options, _) = Options::from_arguments_relaxed(
        "USAGE: tally read [--fork FORK] [--fused|--raw|--claims]",
        args,
    );
    let repo = repo()?;
    let state = repo.current_state(options.fork.as_deref().unwrap_or("main"))?;
    if options.claims {
        for line in &state.lines {
            for claim_id in &line.annotation.claims {
                let claim = repo.get_claim(claim_id)?;
                let stale = claim.is_stale_at(&state.manifest)?;
                println!(
                    "{}  exit={}  {}  {}",
                    claim.id,
                    claim.exit,
                    if stale { "STALE" } else { "fresh" },
                    claim.cmd,
                );
            }
        }
        return Ok(());
    }
    if options.raw {
        for line in &state.lines {
            println!("{}", serde_json::to_string(line)?);
        }
        return Ok(());
    }
    // Default zoom: fused — the human view of history is a zoom level, not
    // a different interface.
    let views = repo.read_views(options.fork.as_deref().unwrap_or("main"))?;
    for beat in fused_beats(&state.lines, &views) {
        match beat {
            Beat::Fused { view, lines } => {
                let View::Fuse { annotation, .. } = view;
                println!(
                    "{}  fuse({} lines)  {}  {}",
                    lines.last().map(|l| l.id.as_str()).unwrap_or(""),
                    lines.len(),
                    annotation.author,
                    annotation.prose,
                );
            }
            Beat::Single(line) => print_line_brief(line),
        }
    }
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
        "USAGE: tally fuse [--fork FORK] [--prose P] <from-id> <to-id>",
        args,
    );
    let (Some(from), Some(to)) = (free.first(), free.get(1)) else {
        return Err(Error::Invalid("fuse requires <from-id> <to-id>".to_string()));
    };
    let repo = repo()?;
    let view = View::Fuse {
        from: from.clone(),
        to: to.clone(),
        annotation: ViewAnnotation {
            prose: options.prose.unwrap_or_default(),
            author: options.author.unwrap_or_else(whoami),
        },
    };
    repo.append_view(options.fork.as_deref().unwrap_or("main"), &view)?;
    println!("fused {from}..{to} (lossless: the fine structure remains underneath)");
    Ok(())
}

fn cmd_claim(args: &[&str]) -> Result<()> {
    let (options, free) =
        ForkOptions::from_arguments_relaxed("USAGE: tally claim [--fork FORK] <cmd>...", args);
    if free.is_empty() {
        return Err(Error::Invalid("claim requires a command".to_string()));
    }
    let cmd = free.join(" ");
    let repo = repo()?;
    let state = repo.current_state(options.fork())?;
    // Run in the most hermetic environment we have; record what it read as
    // honestly as the sandbox allows.  v0 is honest about its limits: the
    // recorded read set is the whole state.
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .current_dir(repo.root())
        .output()
        .map_err(abelian::ioerr("running claimed command"))?;
    let mut transcript = output.stdout.clone();
    transcript.extend_from_slice(&output.stderr);
    let transcript_sha3 = repo.blobs().put(&transcript)?;
    let exit = output.status.code().unwrap_or(-1) as i64;
    let inputs: Vec<_> = state.manifest.records().cloned().collect();
    let claim = Claim::new(&state.sum, &cmd, inputs, exit, &transcript_sha3)?;
    repo.put_claim(&claim)?;
    println!("{}  exit={}  {}", claim.id, exit, cmd);
    Ok(())
}

fn cmd_claims(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "The fork to operate on (default: main).", "FORK")]
        fork: Option<String>,
        #[arrrg(flag, "List only claims whose inputs have drifted.")]
        stale: bool,
    }
    let (options, _) =
        Options::from_arguments_relaxed("USAGE: tally claims [--fork FORK] [--stale]", args);
    let repo = repo()?;
    let state = repo.current_state(options.fork.as_deref().unwrap_or("main"))?;
    for id in repo.claim_ids()? {
        let claim = repo.get_claim(&id)?;
        let stale = claim.is_stale_at(&state.manifest)?;
        if options.stale && !stale {
            continue;
        }
        println!(
            "{}  exit={}  {}  {}",
            claim.id,
            claim.exit,
            if stale { "STALE" } else { "fresh" },
            claim.cmd,
        );
    }
    Ok(())
}

fn cmd_fork(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {
        #[arrrg(optional, "The fork to anchor at (default: main).", "FORK")]
        from: Option<String>,
    }
    let (options, free) =
        Options::from_arguments_relaxed("USAGE: tally fork [--from FORK] <name>", args);
    let Some(name) = free.first() else {
        return Err(Error::Invalid("fork requires a name".to_string()));
    };
    let repo = repo()?;
    let fork = repo.create_fork(name, options.from.as_deref().unwrap_or("main"))?;
    println!("fork {name} anchored at {}", fork.anchor);
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
        "USAGE: tally union [--into FORK] <fork>",
        args,
    );
    let Some(source) = free.first() else {
        return Err(Error::Invalid("union requires a source fork".to_string()));
    };
    let repo = repo()?;
    let target = options.into.as_deref().unwrap_or("main");
    let author = options.author.unwrap_or_else(whoami);
    let outcome = union(&repo, source, target, &author)?;
    if outcome.already_identical {
        println!("already identical (stratum 1: arithmetic)");
        return Ok(());
    }
    for landed in &outcome.landed {
        let stratum = match landed.stratum {
            Stratum::RealizedReplay => "stratum 2: realized replay",
            Stratum::IntentReplay => "stratum 3: intent replay",
        };
        println!("landed {} <- {}  ({stratum})", landed.line.id, landed.origin_id);
    }
    for claim_id in &outcome.stale_claims {
        println!("stale claim {claim_id} (stratum 4: refresh with `tally claim`; CPU, not tokens)");
    }
    if let Some((line_id, evidence)) = &outcome.needs_reenactment {
        return Err(Error::NeedsReenactment(format!(
            "line {line_id} of fork {source}: {evidence} \
             (stratum 5 costs tokens and is never automatic)"
        )));
    }
    Ok(())
}

fn cmd_clone(args: &[&str]) -> Result<()> {
    #[derive(Debug, Default, Eq, PartialEq, arrrg_derive::CommandLine)]
    struct Options {}
    let (_, free) = Options::from_arguments_relaxed("USAGE: tally clone <store> <dest>", args);
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
    let (_, free) = Options::from_arguments_relaxed("USAGE: tally fetch <store> <cache>", args);
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
        Options::from_arguments_relaxed("USAGE: tally push [--level N] <store>", args);
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
        Options::from_arguments_relaxed("USAGE: tally pack [--level N] <dir>", args);
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
    let (_, free) = Options::from_arguments_relaxed("USAGE: tally unpack <dir> <dest>", args);
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
