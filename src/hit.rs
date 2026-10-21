//! The one finding type every analysis emits, its report, and the baseline that accepts known hits.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum Lint {
    /// Tests in one file that differ in a few literals or identifiers: one table test.
    ParametrizableTests,
    /// A statement prefix that several tests in one file share: one fixture.
    MissingFixture,
    /// A block of statements repeated inside one test: one loop or local closure.
    CopyPasteAssertBlock,
    /// A test with no assertion, unwrap, `?`, panic, or call to an asserting helper.
    NoAssertionTest,
    /// `assert_eq!(x, x)` or `assert_ne!(x, x)`.
    TautologicalAssert,
    /// Three or more `assert_eq!(v.field, ..)` in a row on one value.
    FieldByFieldAssert,
    /// `match`/`let else` whose only job is to panic on a pattern miss: `assert_matches!`.
    ManualAssertMatches,
    /// A test that runs none of the production code under analysis.
    NoProdCoverage,
    /// Covers the same production regions as another test, with the same inputs and only
    /// assertions that test repeats: delete it.
    DuplicateCoverage,
    /// Like `DuplicateCoverage`, but the other test covers strictly more.
    SubsumedCoverage,
    /// Runs no production code beyond another test, with other inputs: one table with it.
    SamePathDifferentInputs,
    /// Many tests in a crate wrap their body in the same closure-taking call: one test attribute or helper.
    TestEnvelope,
    /// A multi-line assertion on an error: one `assert_err!`-style helper line.
    VerboseErrAssert,
    /// `assert!(x.is_err())` right before a check of the error's variant.
    RedundantIsErrAssert,
    /// A one-line comment that restates the code below it.
    NarratorComment,
    /// A decorative divider comment in test code.
    DividerComment,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hit {
    pub lint: Lint,
    /// Path relative to the scanned root.
    pub file: PathBuf,
    pub line: usize,
    /// The test (or comma-separated tests) the hit is about; the baseline key with `lint` and `file`.
    pub item: String,
    /// Rough lines of code the suggested change removes.
    pub saved_loc: usize,
    pub detail: String,
}

/// Prints hits grouped by lint, largest savings first, `limit` per lint.
pub fn print_report(hits: &[Hit], limit: usize) {
    let mut by_lint: BTreeMap<Lint, Vec<&Hit>> = BTreeMap::new();
    for h in hits {
        by_lint.entry(h.lint).or_default().push(h);
    }
    for (lint, mut hs) in by_lint {
        hs.sort_by_key(|h| std::cmp::Reverse(h.saved_loc));
        let saved: usize = hs.iter().map(|h| h.saved_loc).sum();
        let name = serde_json::to_value(lint).expect("unit enum serializes");
        println!(
            "\n== {} : {} hits, ~{saved} LOC",
            name.as_str().unwrap_or_default(),
            hs.len()
        );
        for h in hs.iter().take(limit) {
            println!(
                "  -{:<4} {}:{} {}  {}",
                h.saved_loc,
                h.file.display(),
                h.line,
                h.item,
                h.detail
            );
        }
    }
}

type Key = (Lint, PathBuf, String);

#[derive(Default, Serialize, Deserialize)]
pub struct Baseline {
    accepted: BTreeSet<Key>,
}

impl Baseline {
    pub fn load(path: &Path) -> Result<Self, BaselineError> {
        let s = std::fs::read_to_string(path).map_err(BaselineError::Io)?;
        serde_json::from_str(&s).map_err(BaselineError::Json)
    }

    /// Overwrites `path` with a baseline that accepts exactly `hits`.
    pub fn write(path: &Path, hits: &[Hit]) -> Result<(), BaselineError> {
        let b = Baseline {
            accepted: hits.iter().map(key).collect(),
        };
        let s = serde_json::to_string_pretty(&b).map_err(BaselineError::Json)?;
        std::fs::write(path, s + "\n").map_err(BaselineError::Io)
    }

    /// Drops accepted hits; returns how many baseline entries no current hit matched.
    pub fn filter(&self, hits: &mut Vec<Hit>) -> usize {
        let seen: BTreeSet<Key> = hits.iter().map(key).collect();
        hits.retain(|h| !self.accepted.contains(&key(h)));
        self.accepted.difference(&seen).count()
    }
}

fn key(h: &Hit) -> Key {
    (h.lint, h.file.clone(), h.item.clone())
}

#[derive(Debug)]
pub enum BaselineError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for BaselineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "baseline io: {e}"),
            Self::Json(e) => write!(f, "baseline json: {e}"),
        }
    }
}

impl std::error::Error for BaselineError {}
