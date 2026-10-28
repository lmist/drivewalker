use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use drivewalker::{
    CleanKind, CleanOptions, ScanOptions, apply, plan, platform::macos, report, scan,
};
use serde::Serialize;
use std::{path::PathBuf, process::ExitCode};

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
            let mut usage = scan(&ScanOptions {
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
