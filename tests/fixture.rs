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
            ("meta_tests", "src/test_util.rs"),
            ("characterization_golden", "render_matches_golden"),
            ("duplicate_test_double", "FakeStore"),
            ("orphaned_test_helper", "unused_helper"),
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
            ("no_prod_coverage", "tests::fake_store_reads_origin"),
            ("no_prod_coverage", "test_util::tests::sample_is_21"),
            ("duplicate_coverage", "tests::double_two_again"),
            ("duplicate_coverage", "tests::parse_ok_via_helper"),
            ("subsumed_coverage", "tests::classify_neg"),
            ("same_path_different_inputs", "tests::double_two"),
            ("same_path_different_inputs", "tests::parse_rejects_sign"),
            ("orphaned_test_helper", "seven"),
        ])
    );

    let report = testslop(&["cov", FIXTURE, "-p", "demo", "--jobs", "4"]);
    let stdout = String::from_utf8_lossy(&report.stdout);
    assert_eq!(
        stdout.lines().nth(1),
        Some(
            "   not reported: 15 covered by another test but asserting something it does not, 1 #[should_panic], 0 macro-generated"
        ),
    );
    std::fs::remove_dir_all(&cwd).unwrap();
}

#[test]
fn cov_runs_the_unit_tests_of_a_bin_only_package() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixture-bin");
    let out = testslop(&["cov", dir, "-p", "demo-harness", "--jobs", "2", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        hits(&out),
        pairs(&[("duplicate_coverage", "tests::triple_two_again")])
    );
}

#[test]
fn cov_rejects_a_package_with_several_bins_and_no_lib() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixture-bins");
    let out = testslop(&["cov", dir, "-p", "demo-bins"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        r#"testslop: package `demo-bins` has no lib and 2 bins ["demo-bins", "extra"]; cov needs a lib or exactly one bin"#
    );
}

#[test]
fn cov_against_reports_unit_tests_the_suite_already_runs() {
    let out = testslop(&[
        "cov",
        FIXTURE,
        "-p",
        "demo",
        "--jobs",
        "4",
        "--against",
        "demo:it",
        "--only",
        "covered-by-suite",
        "--json",
    ]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let covered: Vec<(String, String)> = [
        "double_three",
        "double_is_even",
        "double_smoke",
        "double_is_deterministic",
        "double_via_helper",
        "double_halves",
        "render_matches_golden",
        "double_repeated_block",
        "total_is_positive",
        "total_is_even",
    ]
    .iter()
    .map(|t| ("covered_by_suite".to_owned(), format!("tests::{t}")))
    .collect();
    assert_eq!(hits(&out), covered);
}

#[test]
fn crates_reports_fixtures_test_only_items_doubles_and_goldens() {
    let out = testslop(&["crates", FIXTURE, "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    let c = &v["."];
    assert_eq!(c["package"], "demo");
    assert_eq!(c["fixture_loc"], 1, "testdata/render.golden");
    assert_eq!(
        c["test_only_items_loc"], 6,
        "the #[cfg(test)] impl of Point"
    );
    assert_eq!(c["unit_tests"], 30);
    assert_eq!(c["tests"], 31);
    assert_eq!(c["goldens"], 1);
    assert_eq!(c["doubles"], 2);
}

#[test]
fn scan_reports_unit_tests_of_a_tooling_crate() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixture-bin");
    let out = testslop(&["scan", dir, "--only", "meta-tests", "--json"]);
    assert_eq!(hits(&out), pairs(&[("meta_tests", ".")]));
}

#[test]
fn meta_tests_skip_tooling_named_crates_other_crates_build_on() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixture-tooling");
    let out = testslop(&["scan", dir, "--only", "meta-tests", "--json"]);
    assert_eq!(hits(&out), pairs(&[("meta_tests", "app-harness")]));
}
