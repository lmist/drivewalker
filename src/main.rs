use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use drivewalker::{
    CleanKind, CleanOptions, DeepOptions, ScanOptions, ScanReport, apply, plan, platform::macos,
    report, scan_with_progress,
};
use serde::Serialize;
use std::{
    io::IsTerminal,
    path::PathBuf,
    process::ExitCode,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "dw",
    version,
    about = "Find disk hotspots and clean regenerable files safely"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Emit structured JSON to stdout.
    #[arg(long, global = true)]
    json: bool,
    /// Exclude a path relative to the working directory, or an absolute path. Repeatable.
    #[arg(long, global = true)]
    exclude: Vec<PathBuf>,
    /// Maximum worker threads (1–64).
    #[arg(long, global = true, default_value_t = 8)]
    jobs: usize,
}

#[derive(Subcommand)]
enum Command {
    /// Measure allocated space; default root is your home directory.
    Scan(ScanArgs),
    /// Follow the space all the way down: whole disk by default, any depth.
    Deep(DeepArgs),
    /// Preview cleanup. Files are removed only with --apply.
    Clean(CleanArgs),
}

#[derive(Args)]
struct ScanArgs {
    /// Directory to scan. Symlink components are rejected.
    #[arg(conflicts_with = "machine")]
    path: Option<PathBuf>,
    /// Scan disjoint system and home trees; includes APFS and snapshot context.
    #[arg(long)]
    machine: bool,
    /// Display depth; all descendants are measured regardless of display depth.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(0..=16))]
    depth: u8,
    /// Maximum displayed children per directory; JSON includes all measured rows.
    #[arg(long, default_value_t = 30)]
    limit: usize,
    /// Progressively expand this many largest branches using retained measurements.
    #[arg(long, default_value_t = 6, value_parser = clap::value_parser!(u8).range(0..=64))]
    drill: u8,
}

#[derive(Args)]
struct DeepArgs {
    /// Directories to analyse. Default: every folder of the Data volume.
    paths: Vec<PathBuf>,
    /// Smallest entry given its own line, e.g. 500M or 2G. Default: 0.5% of the total.
    #[arg(long, value_parser = size)]
    min_size: Option<u64>,
    /// Rows in each ranked list.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u8).range(1..=64))]
    top: u8,
}

fn size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let number: f64 = text[..split]
        .parse()
        .map_err(|_| format!("expected a size such as 500M or 2G, got {text:?}"))?;
    let unit = text[split..].trim().to_ascii_lowercase();
    let scale = match unit.trim_end_matches("ib").trim_end_matches('b') {
        "" => 1u64,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        _ => return Err(format!("unknown size unit in {text:?}; use K, M, G or T")),
    };
    Ok((number * scale as f64) as u64)
}

/// Scan while reporting the running entry count on an interactive stderr.
fn measure(options: &ScanOptions) -> Result<ScanReport> {
    if !std::io::stderr().is_terminal() {
        return scan_with_progress(options, None);
    }
    let (count, done) = (AtomicU64::new(0), AtomicBool::new(false));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                eprint!("\r  {} entries measured…", count.load(Ordering::Relaxed));
                std::thread::sleep(Duration::from_millis(200));
            }
            eprint!("\r\x1b[2K");
        });
        let result = scan_with_progress(options, Some(&count));
        done.store(true, Ordering::Relaxed);
        result
    })
}

#[derive(Serialize)]
struct DeepReport {
    volumes: Vec<macos::Volume>,
    local_snapshots: Option<Vec<String>>,
    analysis: drivewalker::Analysis,
    issues: Vec<drivewalker::Issue>,
    elapsed_seconds: f64,
    scheduling: drivewalker::Scheduling,
    accounting: &'static str,
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Caches,
    Projects,
    All,
}

#[derive(Args)]
struct CleanArgs {
    #[arg(value_enum)]
    kind: Kind,
    /// Required for projects/all; never deletes the selected root itself.
    path: Option<PathBuf>,
    /// Apply the newly validated cleanup plan.
    #[arg(long)]
    apply: bool,
}

#[derive(Serialize)]
struct MachineReport {
    overview: Vec<macos::OverviewCommand>,
    usage: drivewalker::ScanReport,
}

fn json(value: &impl Serialize) -> Result<()> {
    serde_json::to_writer_pretty(std::io::stdout().lock(), value)?;
    println!();
    Ok(())
}

fn run(cli: Cli) -> Result<bool> {
    ensure!(
        (1..=64).contains(&cli.jobs),
        "--jobs must be between 1 and 64"
    );
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    match cli.command {
        Command::Scan(args) => {
            let roots = if args.machine {
                let mut roots = vec![home.clone()];
                for path in [
                    "/Applications",
                    "/opt",
                    "/usr/local",
                    "/Library",
                    "/private",
                ] {
                    if std::path::Path::new(path).is_dir() {
                        roots.push(path.into());
                    }
                }
                roots
            } else {
                vec![args.path.unwrap_or(home)]
            };
            eprintln!("Measuring {} root(s)…", roots.len());
            let mut usage = measure(&ScanOptions {
                roots,
                excludes: cli.exclude,
                depth: args.depth as usize,
                jobs: cli.jobs,
            })?;
            usage.expand_hotspots(args.drill as usize, args.limit);
            let partial = !usage.issues.is_empty();
            if args.machine {
                let result = MachineReport {
                    overview: macos::overview(),
                    usage,
                };
                let partial = partial || result.overview.iter().any(|c| !c.success);
                if cli.json {
                    json(&result)?;
                } else {
                    for command in &result.overview {
                        println!("$ {}\n{}", command.command, command.output);
                    }
                    print!("{}", report::scan(&result.usage, args.limit));
                }
                Ok(partial)
            } else {
                if cli.json {
                    json(&usage)?;
                } else {
                    print!("{}", report::scan(&usage, args.limit));
                }
                Ok(partial)
            }
        }
        Command::Deep(args) => {
            let layout = report::Layout::detect(home.clone());
            let machine = args.paths.is_empty();
            let roots = if machine {
                let roots = macos::machine_roots();
                if roots.is_empty() { vec![home] } else { roots }
            } else {
                args.paths
            };
            eprintln!("Measuring {} root(s) to full depth…", roots.len());
            let usage = measure(&ScanOptions {
                roots,
                excludes: cli.exclude,
                depth: 0,
                jobs: cli.jobs,
            })?;
            let analysis = usage.deep(DeepOptions {
                min_bytes: args.min_size,
                top: args.top as usize,
            });
            let volumes = usage
                .roots
                .first()
                .map(|root| macos::volumes(&root.path))
                .unwrap_or_default();
            let snapshots = machine.then(macos::local_snapshots).flatten();
            let partial = !usage.issues.is_empty();
            if cli.json {
                json(&DeepReport {
                    volumes,
                    local_snapshots: snapshots,
                    analysis,
                    issues: usage.issues,
                    elapsed_seconds: usage.elapsed_seconds,
                    scheduling: usage.scheduling,
                    accounting: usage.accounting,
                })?;
            } else {
                print!(
                    "{}",
                    report::deep(&analysis, &volumes, snapshots.as_deref(), &usage, &layout)
                );
            }
            Ok(partial)
        }
        Command::Clean(args) => {
            let kind = match args.kind {
                Kind::Caches => CleanKind::Caches,
                Kind::Projects => CleanKind::Projects,
                Kind::All => CleanKind::All,
            };
            ensure!(
                kind != CleanKind::Caches || args.path.is_none(),
                "caches takes no project path; use clean all PATH"
            );
            ensure!(
                kind == CleanKind::Caches || args.path.is_some(),
                "projects and all require an explicit project path"
            );
            eprintln!("Discovering and measuring cleanup candidates…");
            let plan = plan(&CleanOptions {
                kind,
                home,
                project_root: args.path,
                excludes: cli.exclude,
                jobs: cli.jobs,
            })?;
            if args.apply {
                eprintln!("Applying {} candidates…", plan.candidates().len());
                let result = apply(&plan)?;
                if cli.json {
                    json(&result)?;
                } else {
                    print!("{}", report::clean(&result));
                }
                Ok(result.partial)
            } else {
                if cli.json {
                    json(&plan)?;
                } else {
                    print!("{}", report::plan(&plan, 40));
                }
                Ok(!plan.issues().is_empty())
            }
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(false) => ExitCode::SUCCESS,
        Ok(true) => ExitCode::from(2),
        Err(error) => {
            eprintln!("dw: {error:#}");
            ExitCode::FAILURE
        }
    }
}
