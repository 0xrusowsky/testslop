//! Suite-level findings: tests of test tooling, duplicated or heavy test doubles, orphaned helpers.

use crate::hit::{Hit, Lint};
use crate::scan::{Scan, Support, SupportKind, TestFn};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Name tokens of crates that are test tooling (split on `-`, `_` and `/`).
const TOOLING_TOKENS: &[&str] = &[
    "dst",
    "sim",
    "simulator",
    "simulation",
    "harness",
    "e2e",
    "testkit",
    "testing",
    "test",
    "tests",
    "testutils",
    "testutil",
    "fuzz",
    "bench",
    "benches",
    "integration",
    "workload",
    "workloads",
];

/// A test double is heavy when it reaches this many lines ...
const HEAVY_DOUBLE_LOC: usize = 60;
/// ... and this share (1/n) of its file's test code.
const HEAVY_DOUBLE_SHARE: usize = 4;

/// Hits that need the whole scan: `meta_tests`, `duplicate_test_double`, `fake_heavy_tests`, and
/// `orphaned_test_helper` for support code nothing references.
pub fn static_hits(scan: &Scan) -> Vec<Hit> {
    let mut hits = meta_test_hits(scan);
    hits.extend(double_hits(scan));
    hits.extend(orphan_hits(scan, &[]));
    hits.extend(upstream_only_hits(scan));
    hits
}

/// Tests that reach no name production code defines, directly or through test helpers they call.
fn upstream_only_hits(scan: &Scan) -> Vec<Hit> {
    let helpers: BTreeMap<&str, &Support> = scan
        .support
        .iter()
        .filter(|s| s.kind == SupportKind::Helper)
        .map(|s| (s.name.as_str(), s))
        .collect();
    let mut hits = vec![];
    for t in &scan.tests {
        let mut reach: HashSet<&str> = t.idents().collect();
        let mut frontier: Vec<&str> = reach
            .iter()
            .copied()
            .filter(|i| helpers.contains_key(i))
            .collect();
        while let Some(h) = frontier.pop() {
            for i in helpers[h].idents() {
                if reach.insert(i) && helpers.contains_key(i) {
                    frontier.push(i);
                }
            }
        }
        if reach.iter().any(|i| scan.index.prod_names.contains(*i)) {
            continue;
        }
        hits.push(Hit {
            lint: Lint::TestsUpstreamOnly,
            file: t.file.clone(),
            line: t.line,
            item: t.name.clone(),
            saved_loc: t.loc,
            saved_ms: None,
            detail: "reaches nothing production code defines: it tests std, a dependency, or test helpers".into(),
        });
    }
    hits
}

/// The token of the crate directory or package name that marks it as test tooling.
fn tooling_token(krate: &str, package: Option<&str>) -> Option<String> {
    krate
        .split(['-', '_', '/'])
        .chain(package.into_iter().flat_map(|p| p.split(['-', '_'])))
        .find(|t| TOOLING_TOKENS.contains(t))
        .map(str::to_owned)
}

fn meta_test_hits(scan: &Scan) -> Vec<Hit> {
    let mut hits = vec![];
    for (krate, size) in &scan.crates {
        let Some(token) = tooling_token(krate, size.package.as_deref()) else {
            continue;
        };
        // A crate other crates build on is product code, whatever its name (`fuzz`, `dst`).
        if size.unit_tests == 0 || size.used_by > 0 {
            continue;
        }
        let src = src_dir(krate);
        let unit: Vec<&TestFn> = scan
            .tests
            .iter()
            .filter(|t| &t.krate == krate && t.file.starts_with(&src))
            .collect();
        hits.push(Hit {
            saved_ms: None,
            lint: Lint::MetaTests,
            file: PathBuf::from(krate),
            line: 0,
            item: krate.clone(),
            saved_loc: unit.iter().map(|t| t.loc).sum::<usize>() + size.fixture_loc,
            detail: format!(
                "{} unit tests and {} fixture lines in test tooling (`{token}`): verify the tooling by running it",
                unit.len(),
                size.fixture_loc
            ),
        });
    }
    // Test-support modules inside ordinary crates.
    let mut by_file: BTreeMap<&Path, Vec<&TestFn>> = BTreeMap::new();
    for t in &scan.tests {
        let tooling_crate = tooling_token(
            &t.krate,
            scan.crates.get(&t.krate).and_then(|c| c.package.as_deref()),
        )
        .is_some();
        let in_support_module = t.file.components().any(|c| {
            matches!(
                c.as_os_str().to_string_lossy().trim_end_matches(".rs"),
                "test_util" | "test_utils"
            )
        });
        if in_support_module && !tooling_crate {
            by_file.entry(&t.file).or_default().push(t);
        }
    }
    for (file, tests) in by_file {
        hits.push(Hit {
            saved_ms: None,
            lint: Lint::MetaTests,
            file: file.into(),
            line: tests[0].line,
            item: file.display().to_string(),
            saved_loc: tests.iter().map(|t| t.loc).sum(),
            detail: format!("{} tests of a test-support module", tests.len()),
        });
    }
    hits
}

fn src_dir(krate: &str) -> PathBuf {
    if krate == "." {
        PathBuf::from("src")
    } else {
        Path::new(krate).join("src")
    }
}

/// Heavy when a file's doubles reach `HEAVY_DOUBLE_LOC` lines and 1/`HEAVY_DOUBLE_SHARE` of its test code.
fn fake_heavy(doubles_loc: usize, test_loc: usize) -> bool {
    doubles_loc >= HEAVY_DOUBLE_LOC && doubles_loc * HEAVY_DOUBLE_SHARE >= test_loc
}

fn double_hits(scan: &Scan) -> Vec<Hit> {
    let doubles = || {
        scan.support
            .iter()
            .filter(|s| s.kind == SupportKind::Double)
    };
    let mut hits = vec![];
    let mut by_name: BTreeMap<(&str, &str), Vec<&Support>> = BTreeMap::new();
    for d in doubles() {
        by_name.entry((&d.krate, &d.name)).or_default().push(d);
    }
    for ((_, name), defs) in by_name {
        let files: BTreeMap<&Path, usize> = defs.iter().fold(BTreeMap::new(), |mut m, d| {
            *m.entry(d.file.as_path()).or_default() += d.loc;
            m
        });
        if files.len() < 2 {
            continue;
        }
        let total: usize = files.values().sum();
        let largest = files.values().copied().max().unwrap_or(0);
        hits.push(Hit {
            saved_ms: None,
            lint: Lint::DuplicateTestDouble,
            file: defs[0].file.clone(),
            line: defs[0].line,
            item: name.to_owned(),
            saved_loc: total - largest,
            detail: format!(
                "defined in {} files: {}",
                files.len(),
                files
                    .keys()
                    .map(|f| f.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    let mut by_file: BTreeMap<&Path, Vec<&Support>> = BTreeMap::new();
    for d in doubles() {
        by_file.entry(&d.file).or_default().push(d);
    }
    for (file, ds) in by_file {
        let loc: usize = ds.iter().map(|d| d.loc).sum();
        let test_loc = scan.file_test_loc.get(file).copied().unwrap_or(0);
        if fake_heavy(loc, test_loc) {
            hits.push(Hit {
                saved_ms: None,
                lint: Lint::FakeHeavyTests,
                file: file.into(),
                line: ds[0].line,
                item: ds.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", "),
                saved_loc: 0,
                detail: format!(
                    "test doubles are {loc} of {test_loc} test lines: the tests check the code against a hand-written model"
                ),
            });
        }
    }
    hits
}

/// Support items every use of which (outside their own definition) lies in `removed` tests or in
/// other orphaned support, iterated to a fixed point. With `removed` empty: items nothing uses.
/// Names are matched across the whole workspace, so a shared name keeps an item alive.
pub fn orphan_hits(scan: &Scan, removed: &[&TestFn]) -> Vec<Hit> {
    let mut orphaned: HashSet<usize> = HashSet::new();
    loop {
        let before = orphaned.len();
        for (i, s) in scan.support.iter().enumerate() {
            if orphaned.contains(&i) {
                continue;
            }
            let total = scan.index.ident_counts.get(&s.name).copied().unwrap_or(0);
            let own = s.uses(&s.name);
            let from_removed: usize = removed.iter().map(|t| t.uses(&s.name)).sum::<usize>()
                + orphaned
                    .iter()
                    .map(|&j| scan.support[j].uses(&s.name))
                    .sum::<usize>();
            if total.saturating_sub(own) <= from_removed {
                orphaned.insert(i);
            }
        }
        if orphaned.len() == before {
            break;
        }
    }
    let mut idx: Vec<usize> = orphaned.into_iter().collect();
    idx.sort_unstable();
    idx.into_iter()
        .map(|i| {
            let s = &scan.support[i];
            let users: Vec<&str> = removed
                .iter()
                .filter(|t| t.uses(&s.name) > 0)
                .map(|t| t.name.as_str())
                .collect();
            Hit {
                saved_ms: None,
                lint: Lint::OrphanedTestHelper,
                file: s.file.clone(),
                line: s.line,
                item: s.name.clone(),
                saved_loc: s.loc,
                detail: if users.is_empty() {
                    "nothing references it".into()
                } else {
                    format!("used only by deletion candidates: {}", users.join(", "))
                },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooling_is_named_by_a_path_or_package_token() {
        assert_eq!(
            tooling_token("workloads/tempo-dst", None),
            Some("workloads".into())
        );
        assert_eq!(
            tooling_token("crates/foo", Some("foo-test-utils")),
            Some("test".into())
        );
        assert_eq!(
            tooling_token("crates/precompiles", Some("tempo-precompiles")),
            None
        );
        assert_eq!(tooling_token("crates/latest", None), None);
    }

    #[test]
    fn fake_heavy_needs_size_and_share() {
        assert!(fake_heavy(60, 240));
        assert!(!fake_heavy(59, 100));
        assert!(!fake_heavy(60, 241));
    }
}
