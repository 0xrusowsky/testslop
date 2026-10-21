//! Per-test region coverage of production code, from a `-C instrument-coverage` libtest binary.

use crate::hit::{Hit, Lint};
use crate::scan::Scan;
use serde::Deserialize;
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long one test may run before it is killed and reported as failed.
const TEST_TIMEOUT: Duration = Duration::from_secs(300);
/// llvm-cov region kind of plain code (not expansion, skipped, gap or branch).
const CODE_REGION: u64 = 0;

/// Env var that switches the binary into rustc-wrapper mode, naming the one crate to instrument.
pub const INSTRUMENT_CRATE_ENV: &str = "TESTSLOP_INSTRUMENT_CRATE";

/// Runs as `RUSTC_WORKSPACE_WRAPPER`: `args` is `rustc` and its arguments; adds
/// `-C instrument-coverage` only when compiling `krate`, so llvm-cov loads one crate's mapping.
pub fn wrap_rustc(
    krate: &str,
    args: &[std::ffi::OsString],
) -> Result<std::process::ExitStatus, std::io::Error> {
    let (rustc, rest) = args
        .split_first()
        .ok_or_else(|| std::io::Error::other("no rustc path given"))?;
    let mut cmd = Command::new(rustc);
    cmd.args(rest);
    if rest
        .windows(2)
        .any(|w| w[0] == "--crate-name" && w[1] == krate)
    {
        cmd.args(["-C", "instrument-coverage"]);
    }
    cmd.status()
}

pub struct Built {
    pub bin: PathBuf,
    /// The lib target's source directory, relative to the root.
    pub src_dir: PathBuf,
}

/// Builds `package`'s lib test binary under `<root>/target/testslop`, instrumenting only that crate.
pub fn build(root: &Path, package: &str, cargo_args: &[String]) -> Result<Built, CovError> {
    let krate = package.replace('-', "_");
    let exe = std::env::current_exe().map_err(CovError::Io)?;
    let out = Command::new("cargo")
        .args([
            "test",
            "-p",
            package,
            "--lib",
            "--no-run",
            "--message-format=json-render-diagnostics",
        ])
        .args(cargo_args)
        .current_dir(root)
        .env("RUSTC_WORKSPACE_WRAPPER", exe)
        .env(INSTRUMENT_CRATE_ENV, &krate)
        .env("CARGO_TARGET_DIR", root.join("target/testslop"))
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
                    .is_some_and(|n| n.replace('-', "_") == krate);
            let bin = PathBuf::from(m["executable"].as_str().filter(|_| is_ours)?);
            let src = Path::new(target["src_path"].as_str()?).parent()?;
            Some(Built {
                bin,
                src_dir: src.strip_prefix(root).ok()?.to_path_buf(),
            })
        })
        .ok_or(CovError::NoTestBinary)
}

pub struct CovConfig<'a> {
    pub root: &'a Path,
    pub bin: &'a Path,
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
    Timeout,
    /// cargo built nothing that is the package's lib test binary.
    NoTestBinary,
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
            Self::Timeout => write!(f, "test ran longer than {TEST_TIMEOUT:?}"),
            Self::NoTestBinary => write!(f, "cargo reported no lib test binary for the package"),
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

/// Non-ignored test names the binary lists.
fn list_tests(bin: &Path) -> Result<Vec<String>, CovError> {
    let list = |ignored: bool| -> Result<HashSet<String>, CovError> {
        let mut cmd = Command::new(bin);
        cmd.args(["--list", "--format", "terse"]);
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

/// Runs one test alone and returns the production regions it executed.
fn cover_one(
    cfg: &CovConfig,
    scan: &Scan,
    idx: usize,
    test: &str,
) -> Result<HashSet<Region>, CovError> {
    let raw = cfg.work_dir.join(format!("{idx}.profraw"));
    let data = cfg.work_dir.join(format!("{idx}.profdata"));
    let mut child = Command::new(cfg.bin)
        .args(["--exact", test, "--test-threads=1", "-q"])
        .env("LLVM_PROFILE_FILE", &raw)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| CovError::Spawn {
            tool: cfg.bin.display().to_string(),
            source,
        })?;
    let deadline = Instant::now() + TEST_TIMEOUT;
    let status = loop {
        if let Some(s) = child.try_wait().map_err(CovError::Io)? {
            break s;
        }
        if Instant::now() > deadline {
            child.kill().map_err(CovError::Io)?;
            child.wait().map_err(CovError::Io)?;
            return Err(CovError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        let _ = std::fs::remove_file(&raw);
        return Err(CovError::ToolFailed {
            tool: test.into(),
            status,
            stderr: String::new(),
        });
    }

    let profdata = cfg.llvm_bin.join("llvm-profdata");
    run(
        &profdata,
        Command::new(&profdata)
            .arg("merge")
            .arg("-sparse")
            .arg(&raw)
            .arg("-o")
            .arg(&data),
    )?;
    std::fs::remove_file(&raw).map_err(CovError::Io)?;

    let llvm_cov = cfg.llvm_bin.join("llvm-cov");
    let mut export = Command::new(&llvm_cov)
        .args(["export", "-format=text", "-skip-expansions"])
        .arg(format!("-instr-profile={}", data.display()))
        .arg("-object")
        .arg(cfg.bin)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| CovError::Spawn {
            tool: llvm_cov.display().to_string(),
            source,
        })?;
    let stdout = export.stdout.take().expect("stdout is piped");
    let mut covered = HashSet::new();
    let mut sink = |f: ExportFn| {
        for r in &f.regions {
            let [ls, cs, le, ce, count, file_id, _, kind] = *r;
            if count == 0 || kind != CODE_REGION {
                continue;
            }
            let Some(rel) = usize::try_from(file_id)
                .ok()
                .and_then(|i| f.filenames.get(i))
                .and_then(|p| p.strip_prefix(cfg.root).ok())
            else {
                continue;
            };
            if !cfg.prod_dirs.iter().any(|d| rel.starts_with(d)) {
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
    std::fs::remove_file(&data).map_err(CovError::Io)?;
    if !status.success() {
        return Err(CovError::ToolFailed {
            tool: llvm_cov.display().to_string(),
            status,
            stderr: String::new(),
        });
    }
    parsed.map_err(CovError::Json)?;
    Ok(covered)
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
    /// Hits not reported because the test is generated by a macro the scanner cannot expand.
    pub macro_generated: usize,
}

pub fn analyze(cfg: &CovConfig, scan: &Scan) -> Result<CovReport, CovError> {
    let tests = list_tests(cfg.bin)?;
    std::fs::create_dir_all(cfg.work_dir).map_err(CovError::Io)?;
    let next = Mutex::new(0usize);
    let results = Mutex::new(Vec::<(usize, Result<HashSet<Region>, CovError>)>::new());
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
                    let r = cover_one(cfg, scan, idx, test);
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
    for (idx, r) in results.into_inner().expect("workers have exited") {
        match r {
            Ok(regions) => {
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

    let by_harness: HashMap<&str, &crate::scan::TestFn> = scan
        .tests
        .iter()
        .filter_map(|t| Some((t.harness_name.as_deref()?, t)))
        .collect();
    let mut hits = vec![];
    let mut macro_generated = 0;
    // Tests the scanner did not find are generated by `macro_rules!`: already one definition, not deletable alone.
    let mut push = |i: usize, lint: Lint, detail: String| {
        let name = &tests[sets[i].0];
        match by_harness.get(name.as_str()) {
            Some(t) => hits.push(Hit {
                lint,
                file: t.file.clone(),
                line: t.line,
                item: name.clone(),
                saved_loc: t.loc,
                detail,
            }),
            None => macro_generated += 1,
        }
    };

    // Identical sets: keep the first test of each group, report the rest.
    let mut first_with: BTreeMap<&[usize], usize> = BTreeMap::new();
    let mut dup = vec![false; sets.len()];
    for (i, (_, v)) in sets.iter().enumerate() {
        if v.is_empty() {
            push(
                i,
                Lint::NoProdCoverage,
                "runs no production code: tests generated, upstream or test-only code".into(),
            );
            dup[i] = true;
            continue;
        }
        match first_with.get(v.as_slice()) {
            Some(&j) => {
                dup[i] = true;
                push(
                    i,
                    Lint::DuplicateCoverage,
                    format!("same {} regions as {}", v.len(), tests[sets[j].0]),
                );
            }
            None => {
                first_with.insert(v, i);
            }
        }
    }
    for i in (0..sets.len()).filter(|&i| !dup[i]) {
        // The smallest strict superset is the closest, most reviewable witness.
        let witness = (0..sets.len())
            .filter(|&j| j != i && !dup[j] && sets[j].1.len() > sets[i].1.len())
            .filter(|&j| bits[i].iter().zip(&bits[j]).all(|(a, b)| a & !b == 0))
            .min_by_key(|&j| sets[j].1.len());
        if let Some(j) = witness {
            push(
                i,
                Lint::SubsumedCoverage,
                format!(
                    "{} of {} regions, all covered by {}",
                    sets[i].1.len(),
                    sets[j].1.len(),
                    tests[sets[j].0]
                ),
            );
        }
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
        macro_generated,
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
