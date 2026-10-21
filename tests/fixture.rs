//! Runs the binary on `tests/fixture`, a crate with one deliberate case per lint.

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
            (
                "parametrizable_tests",
                "double_two, double_three, double_two_again"
            ),
            (
                "missing_fixture",
                "let points = vec ! [Point { x : $ , y : $ , z : $ } ,] ; let total = points . iter () . map (| p | double (p . x)) . sum :: < u32 > () ;"
            ),
            ("copy_paste_assert_block", "double_repeated_block"),
            ("no_assertion_test", "double_smoke"),
            ("tautological_assert", "double_is_deterministic"),
            ("field_by_field_assert", "point_fields"),
            ("manual_assert_matches", "classify_pos_matches"),
            ("test_envelope", "with_ctx in ."),
            ("verbose_err_assert", "parse_rejects_words"),
            ("redundant_is_err_assert", "parse_rejects_words"),
            ("narrator_comment", "total_is_even"),
            ("divider_comment", "divider comments"),
        ])
    );
}

/// A fresh empty directory under the system temp dir, unique to this test process.
fn tempdir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("testslop-{name}-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn baseline_accepts_written_hits_and_reports_stale_entries() {
    let dir = tempdir("baseline");
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
        pairs(&[("no_assertion_test", "double_smoke")])
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
fn cov_triages_coverage_redundant_tests() {
    let cwd = tempdir("cov-cwd");
    let out = Command::new(env!("CARGO_BIN_EXE_testslop"))
        .args(["cov", FIXTURE, "-p", "demo", "--jobs", "4", "--json"])
        .current_dir(&cwd)
        .output()
        .expect("binary runs");
    assert_eq!(
        std::fs::read_dir(&cwd).unwrap().count(),
        0,
        "cov wrote files into the cwd"
    );
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
            ("duplicate_coverage", "tests::double_two_again"),
            ("subsumed_coverage", "tests::classify_neg"),
            ("same_path_different_inputs", "tests::double_two"),
            ("same_path_different_inputs", "tests::parse_rejects_sign"),
        ])
    );

    let report = testslop(&["cov", FIXTURE, "-p", "demo", "--jobs", "4"]);
    let stdout = String::from_utf8_lossy(&report.stdout);
    assert_eq!(
        stdout.lines().nth(1),
        Some(
            "   not reported: 13 covered by another test but asserting something it does not, 1 #[should_panic], 0 macro-generated"
        ),
    );
    std::fs::remove_dir_all(&cwd).unwrap();
}
