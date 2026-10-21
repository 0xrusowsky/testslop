# testslop

Finds Rust tests to delete, merge into a table, or give a shared fixture. Every hit is a suggestion
for an agent or a human to review; nothing is auto-applied.

## Usage

```bash
cargo build --release
testslop scan <repo>                                             # static checks over every test
testslop crates <repo>                                           # per-crate test cost: code, fixtures, cfg(test) items, doubles, snapshots
testslop cov <repo> -p <package> [-- --features x]               # per-test coverage of one package's unit tests
testslop cov <repo> -p <package> --against <pkg>:<test>          # ... plus unit tests an integration suite already covers
testslop scan <repo> --json                                      # every hit, sorted, as JSON
testslop scan <repo> --only parametrizable-tests --limit 50
testslop scan <repo> --write-baseline .testslop.json             # accept every current hit
testslop scan <repo> --baseline .testslop.json                   # report only new hits (exit 1 if any)
```

Exit codes: 0 no hits, 1 hits, 2 error.

`cov` builds the unit-test binary of `<package>`'s lib (or its only bin) into
`<repo>/target/testslop/<crate>`, instrumenting only that crate (testslop acts as its own
`RUSTC_WORKSPACE_WRAPPER`; one target dir per instrumented set, since cargo does not track the wrapper's env),
runs each non-ignored test alone, and diffs the production regions each one executes. `--against PKG:TEST`
builds that test target with the same crate instrumented, runs it once, and reports unit tests whose regions
it all runs. It needs the `llvm-tools` rustup component of the repo's toolchain.
Production code is the package's `src/` minus `#[cfg(test)]` modules, test functions, and `test_util(s)` files.

## Lints

| Lint | Source | Means |
|---|---|---|
| `parametrizable_tests` | scan | same-file tests whose bodies differ in at most 8 tokens: one table (rows keep their own multi-line literals) |
| `missing_fixture` | scan | a statement prefix (literals ignored) that 3 or more tests in a file open with |
| `test_envelope` | scan | 5+ tests in a crate wrap their body in the same `call(.., \|..\| { .. })`: a test attribute drops the wrapper and one indent level |
| `copy_paste_assert_block` | scan | a block repeated inside one test: a loop |
| `verbose_err_assert` | scan | a multi-line assertion on an error that an `assert_err!`-style helper fits in fewer lines (rustfmt widths modeled) |
| `redundant_is_err_assert` | scan | `assert!(x.is_err())` right before `x.unwrap_err()` or a `matches!(x, Err(..))` |
| `no_assertion_test` | scan | no assert, panic path, or call to an asserting helper |
| `tautological_assert` | scan | `assert_eq!(x, x)` |
| `field_by_field_assert` | scan | three or more `assert_eq!(v.f, ..)` in a row |
| `manual_assert_matches` | scan | a `match` or `let .. else` that only panics on a miss |
| `narrator_comment` | scan | one-line comments that open with an action verb and give no reason ("Create the token") |
| `divider_comment` | scan | decorative divider comments in test code |
| `meta_tests` | scan | unit tests of test tooling: a crate named like a harness/simulation/e2e/test-utils crate that no workspace crate builds on (normal or build dependency), or a `test_util(s)` module |
| `characterization_golden` | scan | a test that compares against a stored snapshot: insta/expect-test macros, an `*_UPDATE*`/`*GOLDEN*`/`BLESS`-style env var, or a golden/`.snap` path, directly or through a helper |
| `duplicate_test_double` | scan | the same `Fake*`/`Mock*`/`Stub*`/`Dummy*`/`Spy*`/`Noop*` type defined in several files of a crate |
| `fake_heavy_tests` | scan | a file whose test doubles are 60+ lines and a quarter or more of its test code |
| `orphaned_test_helper` | scan, cov | a test helper or double nothing references (`pub` ones rustc does not warn about); with `cov`, also those only deletion candidates use |
| `silent_skip` | scan | returns early on an environment check (env var, tool availability, path existence, `cfg!`, CI) or is `#[ignore]`d without a reason; `if !is_ci() { return }` (CI-only) is fine |
| `tests_trivial_code` | scan | every assertion reads back a field or getter of a value built from the same literal, or the test only uses impls its types `#[derive]` (not hand-written, and serde not configured by `#[serde(..)]`) |
| `tests_upstream_only` | scan | reaches no name production code defines (types, free functions, distinctive methods, identifiers in item macros like `sol!`), even through test helpers |
| `no_prod_coverage` | cov | runs no production code: tests generated code (derives, `thiserror`, `sol!`), upstream crates, or test helpers |
| `duplicate_coverage` | cov | same regions as another test, with the same inputs and only assertions that test repeats: delete |
| `subsumed_coverage` | cov | as above, but the other test covers strictly more |
| `same_path_different_inputs` | cov | runs no code beyond another test, with other inputs (a near-clone, or the same assertions): one table |
| `covered_by_suite` | cov `--against` | every region the unit test runs, the `--against` suite runs too |
| `unexecuted_assertion` | cov | an assertion whose innermost code region never ran while its test passed (untaken branch, empty loop, closure never called) |

`cov` also times each test; removal findings (`no_prod_coverage`, `duplicate_coverage`, `subsumed_coverage`,
`covered_by_suite`) carry `saved_ms`, the wall time above the fastest test (process overhead), and the report
totals it per lint.

`testslop crates` reports, per crate (nearest `Cargo.toml`): `.rs` test code, fixture lines (non-`.rs` files
under `testdata/`, `fixtures/`, `snapshots/`, `tests/`, and `include_str!` targets of test code), `#[cfg(test)]`
items in production files, test doubles, snapshot tests, and how many workspace crates build on it. It is the
view for suite-level decisions like deleting a harness's unit tests wholesale, which no per-test lint makes.

Statement-level lints look inside a test's envelope closure, so `StorageCtx::enter(.., || { .. })`-style
tests get the same analysis as plain ones.

What counts as an assertion: assert macros (minus their message), calls whose name starts with
`assert_`/`expect_`/`check_`/`verify_`, and calls to any function in test code whose body asserts (found in
a first pass). Inputs are literals, qualified paths minus module prefixes (`Kind::Rotate`, `Address::ZERO`),
SCREAMING_CASE names, proptest strategies, and `#[case(..)]` values.

`cov` triage: for each test, the tests covering all its regions are compared closest first. Same inputs and
repeated assertions, or an exact clone, make a deletion candidate; a near-clone or repeated assertions on
other inputs make a table candidate; anything else is not reported (counted in the summary), and so are
`#[should_panic]` tests and macro-generated tests. Deletion candidates remain hazy: a test can feed the same
inputs through a different variable (another caller) and syntax cannot tell. Selector and storage-layout
guards show up as `no_prod_coverage` on purpose.

## Sweeps

`scan` (seconds per repo, no build):

| Repo | Tests | Test LOC | Flagged LOC |
|---|---|---|---|
| tempo | 4,242 | 200k | 11.0k |
| reth | 4,175 | 172k | 6.6k |
| alloy | 1,465 | 38k | 0.8k |
| foundry | 6,210 | 258k | 13.8k |

`cov` on `tempo-precompiles` (1,065 tests, under a minute on 32 cores): 17 run no production code, 11 deletion
candidates (8 confirmed by manual review), 55 table candidates; 292 coverage-redundant tests are not
reported because they assert something extra. Without triage, coverage flagged 360 tests and about 1 in 10
held up.

## Known gaps

- Syntax only: no types, so derived-impl tests are found only by `cov`.
- Clone clusters need identical token shapes; reordered statements are missed.
- Tests generated by `macro_rules!` are invisible to `scan`; `#[test] fn` inside macros (`proptest!`) and
  project test attributes (`#[forgetest]`, `#[test_traced]`) are seen.
- No mutation testing yet; it is the confirmation step deletion candidates need.
- Test-tooling detection is by name plus "nothing builds on it": a product binary named like a harness (a
  DST tool's own CLI) is still reported.
- Orphaned helpers are matched by name across the workspace, so a helper sharing a name with anything else is
  kept; helpers marked `#[allow/expect(dead_code)]` or named `_*` are skipped.
