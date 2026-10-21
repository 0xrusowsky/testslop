//! testslop: finds tests to delete, merge into tables, or share setup.

mod cov;
mod hit;
mod scan;
mod suite;

use clap::{Args, Parser, Subcommand};
use hit::{Baseline, Hit, Lint};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Static checks over every test in the workspace.
    Scan {
        root: PathBuf,
        #[command(flatten)]
        out: Output,
    },
    /// What each crate's tests cost: test code, fixtures, test-only items, doubles, snapshots.
    Crates {
        root: PathBuf,
        /// Print every crate as JSON instead of the table.
        #[arg(long)]
        json: bool,
        /// Crates shown in the table, most test code first.
        #[arg(long, default_value_t = 25)]
        limit: usize,
    },
    /// Builds a package's unit tests (lib, or its only bin) with coverage, runs each test alone, and
    /// compares the production code each covers.
    Cov {
        root: PathBuf,
        #[arg(short, long)]
        package: String,
        /// Directory, relative to the root, whose non-test code is production code; repeatable.
        /// Defaults to the package's lib source directory.
        #[arg(long = "prod")]
        prod_dirs: Vec<PathBuf>,
        /// Directory holding llvm-profdata and llvm-cov; defaults to the root toolchain's llvm-tools.
        #[arg(long)]
        llvm_bin: Option<PathBuf>,
        /// Tests run at once; defaults to the available parallelism.
        #[arg(long)]
        jobs: Option<usize>,
        /// `PACKAGE:TEST`: also run this test target once, with the same crate instrumented, and report
        /// unit tests whose production regions it already runs.
        #[arg(long)]
        against: Option<cov::Against>,
        #[command(flatten)]
        out: Output,
        /// Extra `cargo test` arguments, such as `--features test-utils`.
        #[arg(last = true)]
        cargo_args: Vec<String>,
    },
}

#[derive(Args)]
struct Output {
    /// Print every hit as JSON instead of the report.
    #[arg(long)]
    json: bool,
    /// Hits shown per lint in the report.
    #[arg(long, default_value_t = 15)]
    limit: usize,
    /// Only these lints; repeatable.
    #[arg(long = "only", value_enum)]
    only: Vec<Lint>,
    /// Drop hits this baseline accepts.
    #[arg(long)]
    baseline: Option<PathBuf>,
    /// Write a baseline accepting every reported hit to this path.
    #[arg(long)]
    write_baseline: Option<PathBuf>,
}

fn main() -> ExitCode {
    if let Ok(krate) = std::env::var(cov::INSTRUMENT_CRATE_ENV) {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        return match cov::wrap_rustc(&krate, &args) {
            Ok(s) => s
                .code()
                .and_then(|c| u8::try_from(c).ok())
                .map_or(ExitCode::FAILURE, ExitCode::from),
            Err(e) => {
                eprintln!("testslop rustc wrapper: {e}");
                ExitCode::FAILURE
            }
        };
    }
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("testslop: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match cli.cmd {
        Cmd::Scan { root, out } => {
            let s = scan::scan(&root);
            if !out.json {
                print_crates(&s, 10);
            }
            emit(s.hits, &out)
        }
        Cmd::Crates { root, json, limit } => {
            let s = scan::scan(&root);
            if json {
                println!("{}", serde_json::to_string_pretty(&s.crates)?);
            } else {
                print_crates(&s, limit);
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Cov {
            root,
            package,
            prod_dirs,
            llvm_bin,
            jobs,
            against,
            out,
            cargo_args,
        } => {
            let root = root.canonicalize()?;
            let built = cov::build(&root, &package, &cargo_args)?;
            let against_bin = match &against {
                Some(a) => Some(cov::build_against(&root, a, &built.krate, &cargo_args)?),
                None => None,
            };
            let prod_dirs = if prod_dirs.is_empty() {
                vec![built.src_dir]
            } else {
                prod_dirs
            };
            let jobs =
                jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from));
            let s = scan::scan(&root);
            let llvm_bin = match llvm_bin {
                Some(p) => p,
                None => cov::default_llvm_bin(&root)?,
            };
            let work_dir =
                std::env::temp_dir().join(format!("testslop-cov-{}", std::process::id()));
            let cfg = cov::CovConfig {
                root: &root,
                bin: &built.bin,
                against: against.as_ref().zip(against_bin.as_deref()),
                prod_dirs: &prod_dirs,
                llvm_bin: &llvm_bin,
                jobs,
                work_dir: &work_dir,
            };
            let r = cov::analyze(&cfg, &s);
            std::fs::remove_dir_all(&work_dir)?;
            let r = r?;
            for (t, e) in &r.failed {
                eprintln!("cov: skipped {t}: {e}");
            }
            if !out.json {
                let ran = r.tests - r.failed.len();
                let t = &r.triage;
                println!(
                    "== cov: {ran}/{} tests ran; {} production regions covered; greedy cover keeps {} tests; {} tests are the only cover of some region",
                    r.tests, r.regions_covered, r.min_cover, r.unique_contributors,
                );
                println!(
                    "   not reported: {} covered by another test but asserting something it does not, {} #[should_panic], {} macro-generated",
                    t.kept_distinct, t.should_panic, t.macro_generated,
                );
            }
            emit(r.hits, &out)
        }
    }
}

/// Filters, prints, and optionally baselines hits; exit 1 when any hit is left.
fn emit(mut hits: Vec<Hit>, out: &Output) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if !out.only.is_empty() {
        hits.retain(|h| out.only.contains(&h.lint));
    }
    if let Some(p) = &out.baseline {
        let stale = Baseline::load(p)?.filter(&mut hits);
        if stale > 0 {
            eprintln!("baseline: {stale} entries no longer hit; rewrite it with --write-baseline");
        }
    }
    if let Some(p) = &out.write_baseline {
        Baseline::write(p, &hits)?;
    }
    hits.sort_by(|a, b| {
        (a.lint, &a.file, a.line, &a.item).cmp(&(b.lint, &b.file, b.line, &b.item))
    });
    if out.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        hit::print_report(&hits, out.limit);
    }
    Ok(if hits.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Totals, then the `limit` crates with the most test code (`.rs` plus fixtures).
fn print_crates(s: &scan::Scan, limit: usize) {
    let (prod, test, n) = s.crates.values().fold((0, 0, 0), |a, c| {
        (a.0 + c.prod_loc, a.1 + c.test_loc, a.2 + c.tests)
    });
    println!(
        "== {n} tests, test LOC {test}, prod LOC {prod} ({:.2}x)",
        test as f64 / prod.max(1) as f64
    );
    let mut crates: Vec<_> = s
        .crates
        .iter()
        .filter(|(_, c)| c.tests > 0 || c.test_loc > 0)
        .collect();
    crates.sort_by_key(|(_, c)| std::cmp::Reverse(c.test_loc + c.fixture_loc));
    println!(
        "  {:>7} {:>7} {:>7} {:>7} {:>6} {:>6} {:>9} {:>6}  crate",
        "test", "fixture", "cfgtest", "prod", "ratio", "tests", "doubles", "golden"
    );
    for (dir, c) in crates.iter().take(limit) {
        let ratio = match c.prod_loc {
            0 => "-".to_owned(),
            p => format!("{:.2}x", (c.test_loc + c.fixture_loc) as f64 / p as f64),
        };
        println!(
            "  {:>7} {:>7} {:>7} {:>7} {ratio:>6} {:>6} {:>4}/{:<4} {:>6}  {}",
            c.test_loc,
            c.fixture_loc,
            c.test_only_items_loc,
            c.prod_loc,
            c.tests,
            c.doubles,
            c.doubles_loc,
            c.goldens,
            Path::new(dir).display()
        );
    }
}
