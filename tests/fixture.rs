//! Runs the binary on `tests/fixture`, a crate with one deliberate case per lint.

use std::path::Path;
use std::process::{Command, Output};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixture");

fn testslop(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_testslop"))
        .args(args)
        .output()
        .expect("binary runs")
}

/// `(lint, item)` of every hit, in the binary's sorted order.
fn hits(out: &Output) -> Vec<(String, String)> {
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json output");
    v.as_array()
        .expect("array of hits")
        .iter()
        .map(|h| {
            (
                h["lint"].as_str().unwrap().to_owned(),
                h["item"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

#[test]
fn scan_reports_each_static_lint_once() {
    let out = testslop(&["scan", FIXTURE, "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        hits(&out),
        pairs(&[
            ("parametrizable_tests", "double_two, double_three"),
            (
                "missing_fixture",
                "let points = vec ! [Point { x : 1 , y : 2 , z : 3 } ,] ; let total = points . iter () . map (| p | double (p . x)) . sum :: < u32 > () ;"
            ),
            ("copy_paste_assert_block", "double_repeated_block"),
            ("no_assertion_test", "double_via_helper"),
            ("tautological_assert", "double_is_deterministic"),
            ("field_by_field_assert", "point_fields"),
            ("manual_assert_matches", "classify_pos_matches"),
        ])
    );
}

#[test]
fn baseline_accepts_written_hits_and_reports_stale_entries() {
    let dir = std::env::temp_dir().join(format!("testslop-baseline-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("baseline.json");
    let path_s = path.to_str().unwrap();

    let written = testslop(&[
        "scan",
        FIXTURE,
        "--only",
        "no-assertion-test",
        "--json",
        "--write-baseline",
        path_s,
    ]);
    assert_eq!(
        hits(&written),
        pairs(&[("no_assertion_test", "double_via_helper")])
    );

    let accepted = testslop(&[
        "scan",
        FIXTURE,
        "--only",
        "no-assertion-test",
        "--json",
        "--baseline",
        path_s,
    ]);
    assert_eq!(accepted.status.code(), Some(0));
    assert_eq!(hits(&accepted), vec![]);

    let other = testslop(&[
        "scan",
        FIXTURE,
        "--only",
        "tautological-assert",
        "--json",
        "--baseline",
        path_s,
    ]);
    assert_eq!(
        hits(&other),
        pairs(&[("tautological_assert", "double_is_deterministic")])
    );
    assert_eq!(
        String::from_utf8_lossy(&other.stderr).trim(),
        "baseline: 1 entries no longer hit; rewrite it with --write-baseline"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Needs the `llvm-tools` rustup component of the toolchain the fixture selects.
#[test]
fn cov_reports_empty_duplicate_and_subsumed_coverage() {
    assert!(Path::new(FIXTURE).join("src/lib.rs").exists());
    let out = testslop(&["cov", FIXTURE, "-p", "demo", "--jobs", "4", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        hits(&out),
        pairs(&[
            ("no_prod_coverage", "tests::point_fields"),
            ("duplicate_coverage", "tests::double_two"),
            ("duplicate_coverage", "tests::double_three"),
            ("duplicate_coverage", "tests::double_via_helper"),
            ("duplicate_coverage", "tests::double_repeated_block"),
            ("duplicate_coverage", "tests::total_is_positive"),
            ("duplicate_coverage", "tests::total_is_even"),
            ("subsumed_coverage", "tests::double_is_deterministic"),
            ("subsumed_coverage", "tests::classify_neg"),
            ("subsumed_coverage", "tests::classify_pos_matches"),
        ])
    );
}
