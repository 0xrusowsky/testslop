//! Per-test region coverage of production code, from a `-C instrument-coverage` libtest binary.

use crate::hit::{Hit, Lint};
use crate::scan::Scan;
use serde::Deserialize;
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long one test may run before it is killed and reported as failed.
const TEST_TIMEOUT: Duration = Duration::from_secs(300);
/// How long the whole `--against` suite may run.
const SUITE_TIMEOUT: Duration = Duration::from_secs(3600);
/// llvm-cov region kind of plain code (not expansion, skipped, gap or branch).
const CODE_REGION: u64 = 0;

/// Env var that switches the binary into rustc-wrapper mode, naming the crates to instrument
/// (comma-separated).
pub const INSTRUMENT_CRATE_ENV: &str = "TESTSLOP_INSTRUMENT_CRATE";

/// Runs as `RUSTC_WORKSPACE_WRAPPER`: `args` is `rustc` and its arguments; adds
/// `-C instrument-coverage` only when compiling one of `krates` (comma-separated), so llvm-cov
/// loads only their mappings.
pub fn wrap_rustc(
    krates: &str,
    args: &[std::ffi::OsString],
) -> Result<std::process::ExitStatus, std::io::Error> {
    let (rustc, rest) = args
        .split_first()
        .ok_or_else(|| std::io::Error::other("no rustc path given"))?;
    let mut cmd = Command::new(rustc);
    cmd.args(rest);
    if rest
        .windows(2)
        .any(|w| w[0] == "--crate-name" && krates.split(',').any(|k| w[1] == k))
    {
        cmd.args(["-C", "instrument-coverage"]);
    }
    cmd.status()
}

pub struct Built {
    pub bin: PathBuf,
    /// The instrumented crate's rustc name.
    pub krate: String,
    /// The lib target's source directory, relative to the root.
    pub src_dir: PathBuf,
}

/// The target whose unit tests `cov` runs: the package's lib, or its only bin.
enum UnitTarget {
    Lib(String),
    Bin(String),
}

impl UnitTarget {
    /// Crate name rustc compiles the target under.
    fn crate_name(&self) -> String {
        let (Self::Lib(n) | Self::Bin(n)) = self;
        n.replace('-', "_")
    }
}

/// Reads `package`'s targets from `cargo metadata`.
fn unit_target(root: &Path, package: &str) -> Result<UnitTarget, CovError> {
    let out = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .map_err(|source| CovError::Spawn {
            tool: "cargo metadata".into(),
            source,
        })?;
    if !out.status.success() {
        return Err(CovError::ToolFailed {
            tool: "cargo metadata".into(),
            status: out.status,
            stderr: String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(400)
                .collect(),
        });
    }
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(CovError::Json)?;
    let targets = meta["packages"]
        .as_array()
        .and_then(|ps| ps.iter().find(|p| p["name"] == package))
        .and_then(|p| p["targets"].as_array())
        .ok_or_else(|| CovError::NoUnitTarget {
            package: package.into(),
            bins: vec![],
        })?;
    let of_kind = |kind: &str| -> Vec<String> {
        targets
            .iter()
            .filter(|t| {
                t["kind"]
                    .as_array()
                    .is_some_and(|k| k.iter().any(|k| k == kind))
            })
            .filter_map(|t| t["name"].as_str().map(str::to_owned))
            .collect()
    };
    if let Some(lib) = of_kind("lib").into_iter().next() {
        return Ok(UnitTarget::Lib(lib));
    }
    match <[String; 1]>::try_from(of_kind("bin")) {
        Ok([bin]) => Ok(UnitTarget::Bin(bin)),
        Err(bins) => Err(CovError::NoUnitTarget {
            package: package.into(),
            bins,
        }),
    }
}

/// Builds the unit-test binary of `package`'s lib (or only bin) under
/// `<root>/target/testslop/<crate>`, instrumenting only that crate.
pub fn build(root: &Path, package: &str, cargo_args: &[String]) -> Result<Built, CovError> {
    let target = unit_target(root, package)?;
    let krate = target.crate_name();
    let target_args = match &target {
        UnitTarget::Lib(_) => vec!["--lib".to_owned()],
        UnitTarget::Bin(b) => vec!["--bin".to_owned(), b.clone()],
    };
    let (bin, src_path) =
        cargo_test_build(root, package, &target_args, &krate, &krate, cargo_args)?;
    let src_dir = src_path
        .parent()
        .and_then(|p| p.strip_prefix(root).ok())
        .ok_or(CovError::NoTestBinary)?;
    Ok(Built {
        bin,
        krate,
        src_dir: src_dir.to_path_buf(),
    })
}

/// Another package's test target whose combined coverage `cov --against` compares unit tests with.
#[derive(Clone, Debug)]
pub struct Against {
    pub package: String,
    pub test: String,
}

impl std::str::FromStr for Against {
    type Err = String;
    /// `PACKAGE:TEST`, where TEST is a `cargo test --test` target.
    fn from_str(s: &str) -> Result<Self, String> {
        match s.split_once(':') {
            Some((p, t)) if !p.is_empty() && !t.is_empty() => Ok(Self {
                package: p.into(),
                test: t.into(),
            }),
            _ => Err(format!("expected PACKAGE:TEST, got `{s}`")),
        }
    }
}

impl std::fmt::Display for Against {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.package, self.test)
    }
}

/// Builds the `--against` test target with the same crate (`krate`) instrumented. The test crate
/// is instrumented too, so the profiler runtime gets linked; its regions are test code.
pub fn build_against(
    root: &Path,
    against: &Against,
    krate: &str,
    cargo_args: &[String],
) -> Result<PathBuf, CovError> {
    let args = ["--test".to_owned(), against.test.clone()];
    let test_crate = against.test.replace('-', "_");
    let instrument = format!("{krate},{test_crate}");
    Ok(cargo_test_build(
        root,
        &against.package,
        &args,
        &instrument,
        &test_crate,
        cargo_args,
    )?
    .0)
}

/// Runs `cargo test --no-run` for `target_args` with only the `krate` crates (comma-separated)
/// instrumented, and returns the test
/// executable of the target named `target_name` (dashes normalized) with that target's `src_path`.
fn cargo_test_build(
    root: &Path,
    package: &str,
    target_args: &[String],
    krate: &str,
    target_name: &str,
    cargo_args: &[String],
) -> Result<(PathBuf, PathBuf), CovError> {
    let exe = std::env::current_exe().map_err(CovError::Io)?;
    let out = Command::new("cargo")
        .args([
            "test",
            "-p",
            package,
            "--no-run",
            "--message-format=json-render-diagnostics",
        ])
        .args(target_args)
        .args(cargo_args)
        .current_dir(root)
        .env("RUSTC_WORKSPACE_WRAPPER", exe)
        .env(INSTRUMENT_CRATE_ENV, krate)
        // cargo does not track the wrapper's env, so each instrumented set gets its own target dir.
        .env(
            "CARGO_TARGET_DIR",
            root.join("target/testslop").join(krate.replace(',', "+")),
        )
        .stderr(Stdio::inherit())
        .output()
        .map_err(|source| CovError::Spawn {
            tool: "cargo".into(),
            source,
        })?;
    if !out.status.success() {
        return Err(CovError::ToolFailed {
            tool: "cargo test --no-run".into(),
            status: out.status,
            stderr: String::new(),
        });
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|m| {
            let target = &m["target"];
            let is_ours = m["reason"] == "compiler-artifact"
                && m["profile"]["test"] == true
                && target["name"]
                    .as_str()
                    .is_some_and(|n| n.replace('-', "_") == target_name);
            let bin = PathBuf::from(m["executable"].as_str().filter(|_| is_ours)?);
            Some((bin, PathBuf::from(target["src_path"].as_str()?)))
        })
        .ok_or(CovError::NoTestBinary)
}

pub struct CovConfig<'a> {
    pub root: &'a Path,
    pub bin: &'a Path,
    /// The `--against` suite and its built test executable.
    pub against: Option<(&'a Against, &'a Path)>,
    /// Directories relative to `root` whose non-test code counts as production code.
    pub prod_dirs: &'a [PathBuf],
    pub llvm_bin: &'a Path,
    pub jobs: usize,
    pub work_dir: &'a Path,
}

/// A source region: path relative to the root, start line and column, end line and column.
type Region = (PathBuf, [u64; 4]);

/// `regions` entries are `[line_start, col_start, line_end, col_end, count, file_id, expanded_file_id, kind]`.
#[derive(Deserialize)]
struct ExportFn {
    regions: Vec<[u64; 8]>,
    filenames: Vec<PathBuf>,
}

type FnSink<'a> = &'a mut dyn FnMut(ExportFn);

/// Streams `data[*].functions[*]` of an llvm-cov export into a sink, skipping everything else.
enum ExportSeed<'a> {
    Root(FnSink<'a>),
    DataList(FnSink<'a>),
    Data(FnSink<'a>),
    Functions(FnSink<'a>),
}

impl<'de> DeserializeSeed<'de> for ExportSeed<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        match self {
            Self::Root(_) | Self::Data(_) => d.deserialize_map(self),
            Self::DataList(_) | Self::Functions(_) => d.deserialize_seq(self),
        }
    }
}

impl<'de> Visitor<'de> for ExportSeed<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("llvm-cov export json")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let wanted = if matches!(self, Self::Root(_)) {
            "data"
        } else {
            "functions"
        };
        let (Self::Root(sink) | Self::Data(sink) | Self::DataList(sink) | Self::Functions(sink)) =
            self;
        while let Some(k) = map.next_key::<String>()? {
            if k != wanted {
                map.next_value::<IgnoredAny>()?;
            } else if wanted == "data" {
                map.next_value_seed(ExportSeed::DataList(&mut *sink))?;
            } else {
                map.next_value_seed(ExportSeed::Functions(&mut *sink))?;
            }
        }
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        match self {
            Self::DataList(sink) => {
                while seq
                    .next_element_seed(ExportSeed::Data(&mut *sink))?
                    .is_some()
                {}
            }
            Self::Functions(sink) => {
                while let Some(f) = seq.next_element::<ExportFn>()? {
                    sink(f);
                }
            }
            Self::Root(_) | Self::Data(_) => {
                return Err(serde::de::Error::custom("expected an object"));
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum CovError {
    Spawn {
        tool: String,
        source: std::io::Error,
    },
    ToolFailed {
        tool: String,
        status: std::process::ExitStatus,
        stderr: String,
    },
    Timeout(Duration),
    /// cargo built nothing that is the package's unit-test binary.
    NoTestBinary,
    /// The package has no lib and not exactly one bin (`bins` lists them).
    NoUnitTarget {
        package: String,
        bins: Vec<String>,
    },
    Json(serde_json::Error),
    Io(std::io::Error),
}

impl std::fmt::Display for CovError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn { tool, source } => write!(f, "spawning {tool}: {source}"),
            Self::ToolFailed {
                tool,
                status,
                stderr,
            } => write!(f, "{tool} exited with {status}: {stderr}"),
            Self::Timeout(t) => write!(f, "ran longer than {t:?}"),
            Self::NoTestBinary => write!(f, "cargo reported no unit-test binary for the package"),
            Self::NoUnitTarget { package, bins } => {
                write!(
                    f,
                    "package `{package}` has no lib and {} bins {bins:?}; cov needs a lib or exactly one bin",
                    bins.len()
                )
            }
            Self::Json(e) => write!(f, "llvm-cov json: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for CovError {}

fn run(tool: &Path, cmd: &mut Command) -> Result<(), CovError> {
    let out = cmd.output().map_err(|source| CovError::Spawn {
        tool: tool.display().to_string(),
        source,
    })?;
    if out.status.success() {
        return Ok(());
    }
    Err(CovError::ToolFailed {
        tool: tool.display().to_string(),
        status: out.status,
        stderr: String::from_utf8_lossy(&out.stderr)
            .chars()
            .take(400)
            .collect(),
    })
}

/// Non-ignored test names the binary lists. Its listing profiles go to `work_dir`, not the cwd.
fn list_tests(bin: &Path, work_dir: &Path) -> Result<Vec<String>, CovError> {
    let list = |ignored: bool| -> Result<HashSet<String>, CovError> {
        let mut cmd = Command::new(bin);
        cmd.args(["--list", "--format", "terse"])
            .env("LLVM_PROFILE_FILE", work_dir.join("list-%p.profraw"));
        if ignored {
            cmd.arg("--ignored");
        }
        let out = cmd.output().map_err(|source| CovError::Spawn {
            tool: bin.display().to_string(),
            source,
        })?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.strip_suffix(": test"))
            .map(str::to_owned)
            .collect())
    };
    let ignored = list(true)?;
    let mut tests: Vec<String> = list(false)?
        .into_iter()
        .filter(|t| !ignored.contains(t))
        .collect();
    tests.sort();
    Ok(tests)
}

/// Runs `cmd` to completion, killing it after `timeout`.
fn run_bounded(
    cmd: &mut Command,
    timeout: Duration,
    tool: &str,
) -> Result<std::process::ExitStatus, CovError> {
    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| CovError::Spawn {
            tool: tool.into(),
            source,
        })?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(s) = child.try_wait().map_err(CovError::Io)? {
            return Ok(s);
        }
        if Instant::now() > deadline {
            child.kill().map_err(CovError::Io)?;
            child.wait().map_err(CovError::Io)?;
            return Err(CovError::Timeout(timeout));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Merges raw profiles into `out` and deletes them.
fn merge(cfg: &CovConfig, raws: &[PathBuf], out: &Path) -> Result<(), CovError> {
    let profdata = cfg.llvm_bin.join("llvm-profdata");
    run(
        &profdata,
        Command::new(&profdata)
            .arg("merge")
            .arg("-sparse")
            .args(raws)
            .arg("-o")
            .arg(out),
    )?;
    for r in raws {
        std::fs::remove_file(r).map_err(CovError::Io)?;
    }
    Ok(())
}

/// A test function's own source: its file and line range.
type Focus<'a> = (&'a Path, RangeInclusive<usize>);
/// Code regions of a test function's own source, with their execution counts.
type OwnRegions = Vec<([u64; 4], u64)>;

/// Production regions `profdata` counts as executed in `object`, and every code region (with its
/// count) inside `focus`; deletes `profdata`.
fn export_regions(
    cfg: &CovConfig,
    scan: &Scan,
    profdata: &Path,
    object: &Path,
    focus: Option<&Focus>,
) -> Result<(HashSet<Region>, OwnRegions), CovError> {
    let llvm_cov = cfg.llvm_bin.join("llvm-cov");
    let mut export = Command::new(&llvm_cov)
        .args(["export", "-format=text", "-skip-expansions"])
        .arg(format!("-instr-profile={}", profdata.display()))
        .arg("-object")
        .arg(object)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| CovError::Spawn {
            tool: llvm_cov.display().to_string(),
            source,
        })?;
    let stdout = export.stdout.take().expect("stdout is piped");
    let mut covered = HashSet::new();
    let mut own = vec![];
    let mut sink = |f: ExportFn| {
        for r in &f.regions {
            let [ls, cs, le, ce, count, file_id, _, kind] = *r;
            if kind != CODE_REGION {
                continue;
            }
            let Some(rel) = usize::try_from(file_id)
                .ok()
                .and_then(|i| f.filenames.get(i))
                .and_then(|p| p.strip_prefix(cfg.root).ok())
            else {
                continue;
            };
            if let Some((file, lines)) = focus
                && rel == *file
                && usize::try_from(ls).is_ok_and(|l| lines.contains(&l))
            {
                own.push(([ls, cs, le, ce], count));
            }
            if count == 0 || !cfg.prod_dirs.iter().any(|d| rel.starts_with(d)) {
                continue;
            }
            let is_test = usize::try_from(ls)
                .is_ok_and(|l| scan.test_code.get(rel).is_some_and(|t| t.contains(l)));
            if !is_test {
                covered.insert((rel.to_path_buf(), [ls, cs, le, ce]));
            }
        }
    };
    let mut de = serde_json::Deserializer::from_reader(BufReader::with_capacity(1 << 20, stdout));
    let parsed = ExportSeed::Root(&mut sink).deserialize(&mut de);
    let status = export.wait().map_err(CovError::Io)?;
    std::fs::remove_file(profdata).map_err(CovError::Io)?;
    if !status.success() {
        return Err(CovError::ToolFailed {
            tool: llvm_cov.display().to_string(),
            status,
            stderr: String::new(),
        });
    }
    parsed.map_err(CovError::Json)?;
    Ok((covered, own))
}

/// One test's run: the production regions it executed, the code regions of its own function with
/// their counts, and its wall time.
struct TestRun {
    regions: HashSet<Region>,
    own: OwnRegions,
    wall: Duration,
}

/// Runs one test alone; `focus` is its own function's source, whose regions are kept with counts.
fn cover_one(
    cfg: &CovConfig,
    scan: &Scan,
    idx: usize,
    test: &str,
    focus: Option<&Focus>,
) -> Result<TestRun, CovError> {
    let raw = cfg.work_dir.join(format!("{idx}.profraw"));
    let data = cfg.work_dir.join(format!("{idx}.profdata"));
    let started = Instant::now();
    let status = run_bounded(
        Command::new(cfg.bin)
            .args(["--exact", test, "--test-threads=1", "-q"])
            .env("LLVM_PROFILE_FILE", &raw),
        TEST_TIMEOUT,
        &cfg.bin.display().to_string(),
    )?;
    let wall = started.elapsed();
    if !status.success() {
        if raw.exists() {
            std::fs::remove_file(&raw).map_err(CovError::Io)?;
        }
        return Err(CovError::ToolFailed {
            tool: test.into(),
            status,
            stderr: String::new(),
        });
    }
    merge(cfg, std::slice::from_ref(&raw), &data)?;
    let (regions, own) = export_regions(cfg, scan, &data, cfg.bin, focus)?;
    Ok(TestRun { regions, own, wall })
}

/// Runs the whole `--against` suite once and returns the production regions it executed.
fn cover_suite(
    cfg: &CovConfig,
    scan: &Scan,
    against: &Against,
    bin: &Path,
) -> Result<HashSet<Region>, CovError> {
    let status = run_bounded(
        Command::new(bin).env(
            "LLVM_PROFILE_FILE",
            cfg.work_dir.join("against-%p-%m.profraw"),
        ),
        SUITE_TIMEOUT,
        &against.to_string(),
    )?;
    if !status.success() {
        return Err(CovError::ToolFailed {
            tool: against.to_string(),
            status,
            stderr: String::new(),
        });
    }
    let raws: Vec<PathBuf> = std::fs::read_dir(cfg.work_dir)
        .map_err(CovError::Io)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("against-"))
        })
        .collect();
    let data = cfg.work_dir.join("against.profdata");
    merge(cfg, &raws, &data)?;
    Ok(export_regions(cfg, scan, &data, bin, None)?.0)
}

/// Count of the smallest region in `own` containing `line` and 0-based `col` (llvm columns are
/// 1-based); `None` when no region contains it (macro expansions, untracked code).
fn innermost_count(own: &[([u64; 4], u64)], line: usize, col: usize) -> Option<u64> {
    let (l, c) = (u64::try_from(line).ok()?, u64::try_from(col).ok()? + 1);
    own.iter()
        .filter(|([ls, cs, le, ce], _)| (*ls, *cs) <= (l, c) && (l, c) <= (*le, *ce))
        .min_by_key(|([ls, cs, le, ce], _)| (le - ls, ce.abs_diff(*cs)))
        .map(|(_, n)| *n)
}

/// Delete sorts before Table, so the strongest verdict wins.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Verdict {
    Delete,
    Table,
}

/// Tests the coverage lints do not report, by reason.
#[derive(Default)]
pub struct Triage {
    /// Generated by a macro the scanner cannot expand; never triaged.
    pub macro_generated: usize,
    /// `#[should_panic]`, whose expected panic is invisible to coverage; never triaged.
    pub should_panic: usize,
    /// Covered by another test but asserting something it does not, on a different shape.
    pub kept_distinct: usize,
}

pub struct CovReport {
    pub hits: Vec<Hit>,
    pub tests: usize,
    pub failed: Vec<(String, CovError)>,
    pub regions_covered: usize,
    /// Tests a greedy set cover keeps to cover every region the whole suite covers.
    pub min_cover: usize,
    /// Tests that are the only one covering at least one region.
    pub unique_contributors: usize,
    pub triage: Triage,
}

pub fn analyze(cfg: &CovConfig, scan: &Scan) -> Result<CovReport, CovError> {
    std::fs::create_dir_all(cfg.work_dir).map_err(CovError::Io)?;
    let tests = list_tests(cfg.bin, cfg.work_dir)?;
    let by_harness: HashMap<&str, &crate::scan::TestFn> = scan
        .tests
        .iter()
        .filter_map(|t| Some((t.harness_name.as_deref()?, t)))
        .collect();
    let next = Mutex::new(0usize);
    let results = Mutex::new(Vec::<(usize, Result<TestRun, CovError>)>::new());
    std::thread::scope(|s| {
        for _ in 0..cfg.jobs.max(1) {
            s.spawn(|| {
                loop {
                    let idx = {
                        let mut n = next
                            .lock()
                            .expect("no worker panics while holding the lock");
                        let i = *n;
                        *n += 1;
                        i
                    };
                    let Some(test) = tests.get(idx) else { break };
                    let focus = by_harness
                        .get(test.as_str())
                        .map(|t| (t.file.as_path(), t.line..=t.line + t.loc - 1));
                    let r = cover_one(cfg, scan, idx, test, focus.as_ref());
                    let mut res = results
                        .lock()
                        .expect("no worker panics while holding the lock");
                    res.push((idx, r));
                    if res.len().is_multiple_of(100) {
                        eprintln!("cov: {}/{}", res.len(), tests.len());
                    }
                }
            });
        }
    });

    let mut ids: HashMap<Region, usize> = HashMap::new();
    let mut sets: Vec<(usize, Vec<usize>)> = vec![];
    let mut failed = vec![];
    // Per test index: its own function's regions with counts, and its wall time.
    let mut runs: HashMap<usize, (OwnRegions, Duration)> = HashMap::new();
    for (idx, r) in results.into_inner().expect("workers have exited") {
        match r {
            Ok(TestRun { regions, own, wall }) => {
                runs.insert(idx, (own, wall));
                let mut v: Vec<usize> = regions
                    .into_iter()
                    .map(|r| {
                        let n = ids.len();
                        *ids.entry(r).or_insert(n)
                    })
                    .collect();
                v.sort_unstable();
                sets.push((idx, v));
            }
            Err(e) => failed.push((tests[idx].clone(), e)),
        }
    }
    sets.sort_by_key(|s| s.0);
    let words = ids.len().div_ceil(64);
    let bits: Vec<Vec<u64>> = sets
        .iter()
        .map(|(_, v)| {
            let mut b = vec![0u64; words];
            for &r in v {
                b[r / 64] |= 1 << (r % 64);
            }
            b
        })
        .collect();

    let mut hits = vec![];
    let mut triage = Triage::default();
    for i in 0..sets.len() {
        // Tests the scanner did not find are generated by `macro_rules!`: one definition, not deletable alone.
        let Some(t) = by_harness.get(tests[sets[i].0].as_str()) else {
            triage.macro_generated += 1;
            continue;
        };
        // A test merged into a table still costs a row of about three lines.
        let hit = |lint, detail| Hit {
            saved_ms: None,
            lint,
            file: t.file.clone(),
            line: t.line,
            item: tests[sets[i].0].clone(),
            saved_loc: if lint == Lint::SamePathDifferentInputs {
                t.loc.saturating_sub(3)
            } else {
                t.loc
            },
            detail,
        };
        if sets[i].1.is_empty() {
            hits.push(hit(
                Lint::NoProdCoverage,
                "runs no production code: tests generated, upstream or test-only code".into(),
            ));
            continue;
        }
        if t.should_panic {
            triage.should_panic += 1;
            continue;
        }
        // Covering tests: strict supersets, or equal sets earlier in order so two equal tests never
        // name each other. Closest (smallest) first.
        let mut covering: Vec<usize> = (0..sets.len())
            .filter(|&j| j != i)
            .filter(|&j| {
                sets[j].1.len() > sets[i].1.len() || (sets[j].1.len() == sets[i].1.len() && j < i)
            })
            .filter(|&j| bits[i].iter().zip(&bits[j]).all(|(a, b)| a & !b == 0))
            .collect();
        covering.sort_by_key(|&j| sets[j].1.len());
        let verdicts = covering.iter().filter_map(|&j| {
            let w = by_harness.get(tests[sets[j].0].as_str())?;
            let clone = t.near_clone_of(w);
            let same_inputs = t.inputs_repeated_in(w);
            let verdict = match (clone, t.asserts_repeated_in(w)) {
                (Some(0), _) => Verdict::Delete,
                (None, true) if same_inputs => Verdict::Delete,
                (Some(_), _) | (None, true) => Verdict::Table,
                (None, false) => return None,
            };
            Some((verdict, j, clone))
        });
        let best = verdicts.min_by_key(|v| v.0);
        let Some((verdict, j, clone)) = best else {
            triage.kept_distinct += usize::from(!covering.is_empty());
            continue;
        };
        let witness = &tests[sets[j].0];
        let equal = sets[j].1.len() == sets[i].1.len();
        hits.push(match (verdict, equal) {
            (Verdict::Delete, true) => hit(
                Lint::DuplicateCoverage,
                format!(
                    "same {} regions, inputs and assertions as {witness}",
                    sets[i].1.len()
                ),
            ),
            (Verdict::Delete, false) => hit(
                Lint::SubsumedCoverage,
                format!(
                    "{} of {} regions; inputs and assertions repeated in {witness}",
                    sets[i].1.len(),
                    sets[j].1.len()
                ),
            ),
            (Verdict::Table, _) => hit(
                Lint::SamePathDifferentInputs,
                match clone {
                    Some(n) => {
                        format!("runs no code beyond {witness} and differs from it in {n} tokens")
                    }
                    None => format!(
                        "runs no code beyond {witness} and repeats its assertions on other inputs"
                    ),
                },
            ),
        });
    }

    // Helpers and doubles that only deletion candidates use go with them; ones nothing uses at
    // all are `scan` hits already.
    let deleted: Vec<&crate::scan::TestFn> = hits
        .iter()
        .filter(|h| matches!(h.lint, Lint::DuplicateCoverage | Lint::SubsumedCoverage))
        .filter_map(|h| by_harness.get(h.item.as_str()).copied())
        .collect();
    if !deleted.is_empty() {
        let unused: HashSet<String> = crate::suite::orphan_hits(scan, &[])
            .into_iter()
            .map(|h| h.item)
            .collect();
        hits.extend(
            crate::suite::orphan_hits(scan, &deleted)
                .into_iter()
                .filter(|h| !unused.contains(&h.item)),
        );
    }

    if let Some((against, bin)) = cfg.against {
        let suite = cover_suite(cfg, scan, against, bin)?;
        let mut regions: Vec<Option<&Region>> = vec![None; ids.len()];
        for (r, &id) in &ids {
            regions[id] = Some(r);
        }
        let reported: HashSet<&str> = hits.iter().map(|h| h.item.as_str()).collect();
        let mut covered_hits = vec![];
        for (idx, v) in &sets {
            let name = tests[*idx].as_str();
            let Some(t) = by_harness.get(name) else {
                continue;
            };
            let in_suite = !v.is_empty()
                && v.iter()
                    .all(|&r| regions[r].is_some_and(|r| suite.contains(r)));
            if in_suite && !t.should_panic && !reported.contains(name) {
                covered_hits.push(Hit {
                    saved_ms: None,
                    lint: Lint::CoveredBySuite,
                    file: t.file.clone(),
                    line: t.line,
                    item: name.to_owned(),
                    saved_loc: t.loc,
                    detail: format!("all {} of its regions also run in {against}; keep it only if it asserts what that suite does not", v.len()),
                });
            }
        }
        hits.extend(covered_hits);
    }

    // Assertions whose innermost code region never ran while the test passed.
    for (idx, _) in &sets {
        let (Some(t), Some((own, _))) = (by_harness.get(tests[*idx].as_str()), runs.get(idx))
        else {
            continue;
        };
        if t.should_panic {
            continue;
        }
        let unexecuted: Vec<&(usize, usize, String)> = t
            .assert_sites
            .iter()
            .filter(|(line, col, _)| innermost_count(own, *line, *col) == Some(0))
            .collect();
        if let Some((line, _, what)) = unexecuted.first() {
            hits.push(Hit {
                lint: Lint::UnexecutedAssertion,
                file: t.file.clone(),
                line: *line,
                item: tests[*idx].clone(),
                saved_loc: 0,
                saved_ms: None,
                detail: format!(
                    "{} of {} assertions never ran (first `{what}` at line {line}); the test passes without checking them",
                    unexecuted.len(),
                    t.assert_sites.len()
                ),
            });
        }
    }

    // Run time each removal saves: wall time above the fastest test, which is process overhead.
    let fastest = runs.values().map(|r| r.1).min().unwrap_or_default();
    let wall_of: HashMap<&str, Duration> = runs
        .iter()
        .map(|(i, r)| (tests[*i].as_str(), r.1))
        .collect();
    for h in hits.iter_mut().filter(|h| {
        matches!(
            h.lint,
            Lint::DuplicateCoverage
                | Lint::SubsumedCoverage
                | Lint::NoProdCoverage
                | Lint::CoveredBySuite
        )
    }) {
        h.saved_ms = wall_of
            .get(h.item.as_str())
            .map(|w| u64::try_from(w.saturating_sub(fastest).as_millis()).unwrap_or(u64::MAX));
    }

    let mut cover_count = vec![0u32; ids.len()];
    for (_, v) in &sets {
        for &r in v {
            cover_count[r] += 1;
        }
    }
    let unique_contributors = sets
        .iter()
        .filter(|(_, v)| v.iter().any(|&r| cover_count[r] == 1))
        .count();

    let mut left: Vec<u64> = vec![0; words];
    for b in &bits {
        for (l, w) in left.iter_mut().zip(b) {
            *l |= w;
        }
    }
    let mut min_cover = 0;
    loop {
        let gain = |b: &Vec<u64>| {
            b.iter()
                .zip(&left)
                .map(|(a, l)| (a & l).count_ones())
                .sum::<u32>()
        };
        let Some((best, g)) = bits
            .iter()
            .enumerate()
            .map(|(i, b)| (i, gain(b)))
            .max_by_key(|x| x.1)
        else {
            break;
        };
        if g == 0 {
            break;
        }
        min_cover += 1;
        for (l, w) in left.iter_mut().zip(&bits[best]) {
            *l &= !w;
        }
    }

    Ok(CovReport {
        hits,
        tests: tests.len(),
        failed,
        regions_covered: ids.len(),
        min_cover,
        unique_contributors,
        triage,
    })
}

/// `<sysroot>/lib/rustlib/<host>/bin` of the toolchain `root` selects, where `llvm-tools` installs.
pub fn default_llvm_bin(root: &Path) -> Result<PathBuf, CovError> {
    let rustc = |args: &[&str]| -> Result<String, CovError> {
        let out = Command::new("rustc")
            .args(args)
            .current_dir(root)
            .output()
            .map_err(|source| CovError::Spawn {
                tool: "rustc".into(),
                source,
            })?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let sysroot = rustc(&["--print", "sysroot"])?;
    let vv = rustc(&["-vV"])?;
    let host = vv
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .unwrap_or_default();
    Ok(Path::new(&sysroot)
        .join("lib/rustlib")
        .join(host)
        .join("bin"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn innermost_count_takes_the_smallest_containing_region() {
        // A body (count 1) with an untaken branch (count 0) on lines 3..=5.
        let own = [([1, 1, 9, 2], 1), ([3, 10, 5, 6], 0)];
        assert_eq!(innermost_count(&own, 4, 8), Some(0));
        assert_eq!(innermost_count(&own, 2, 4), Some(1));
        assert_eq!(
            innermost_count(&own, 3, 2),
            Some(1),
            "before the branch opens on line 3"
        );
        assert_eq!(innermost_count(&own, 12, 0), None);
    }
}
