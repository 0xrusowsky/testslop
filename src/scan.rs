//! Static analysis of test functions: structural clones, shared setup, boilerplate, weak assertions.

use crate::hit::{Hit, Lint};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::Visit;

/// Tests in one crate that must share an envelope call before it is reported.
const MIN_ENVELOPE_TESTS: usize = 5;

pub struct TestFn {
    /// Path relative to the scanned root.
    pub file: PathBuf,
    /// Directory of the crate (nearest `Cargo.toml`) relative to the root; `.` for the root.
    pub krate: String,
    /// `module::path::name` as the libtest harness lists it, for tests under `src/`.
    pub harness_name: Option<String>,
    pub name: String,
    pub line: usize,
    pub loc: usize,
    /// `#[should_panic]`: its expectation is a panic that coverage cannot see.
    pub should_panic: bool,
    /// (line, 0-based column, macro or helper name) of each assertion in the body.
    pub assert_sites: Vec<(usize, usize, String)>,
    /// Body with literals and non-keyword identifiers replaced by `$`.
    loose: String,
    /// Flattened exact body tokens, aligned position by position with `loose`.
    exact: Vec<String>,
    /// Literal-blind tokens and code lines (`code_lines`) of each statement of the effective body.
    stmts: Vec<(String, usize)>,
    /// Normalized assertions: assert macros without their message, and `assert_*`/`expect_*`/
    /// `check_*`/`verify_*` helper calls.
    asserts: BTreeSet<String>,
    /// Literals, qualified paths (`Kind::Rotate`, `Address::ZERO`) and SCREAMING_CASE names outside
    /// assertion messages: what the test feeds in.
    inputs: BTreeSet<String>,
}

impl TestFn {
    /// Differing token count when `other` has the same shape and differs in at most 8 tokens (and <= 10%).
    pub fn near_clone_of(&self, other: &TestFn) -> Option<usize> {
        if self.loose != other.loose || self.exact.len() != other.exact.len() {
            return None;
        }
        let holes = self
            .exact
            .iter()
            .zip(&other.exact)
            .filter(|(a, b)| a != b)
            .count();
        (holes <= 8 && holes * 10 <= self.exact.len()).then_some(holes)
    }

    /// Every assertion of this test also appears in `other`; false for a test with none.
    pub fn asserts_repeated_in(&self, other: &TestFn) -> bool {
        !self.asserts.is_empty() && self.asserts.is_subset(&other.asserts)
    }

    /// Every input of this test also appears in `other`.
    pub fn inputs_repeated_in(&self, other: &TestFn) -> bool {
        self.inputs.is_subset(&other.inputs)
    }

    /// Occurrences of `ident` in the body.
    pub fn uses(&self, ident: &str) -> usize {
        self.exact.iter().filter(|t| *t == ident).count()
    }

    /// Identifiers in the body.
    pub fn idents(&self) -> impl Iterator<Item = &str> {
        self.exact
            .iter()
            .map(String::as_str)
            .filter(|t| t.starts_with(|c: char| c.is_alphabetic() || c == '_'))
    }
}

/// What a test's attributes and signature say.
struct TestAttrs {
    should_panic: bool,
    /// `#[ignore]` with no `= "reason"`.
    unreasoned_ignore: bool,
    /// Inputs outside the body, from proptest parameters or `#[case(..)]`-style attributes.
    sig_inputs: BTreeSet<String>,
}

/// What a piece of test support code is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SupportKind {
    /// A non-test function in test code.
    Helper,
    /// A `Fake*`/`Mock*`/`Stub*`/`Dummy*`/`Spy*`/`Noop*` type in test code, with its impl blocks.
    Double,
}

/// A test helper or test double.
pub struct Support {
    pub kind: SupportKind,
    pub name: String,
    pub file: PathBuf,
    pub krate: String,
    pub line: usize,
    pub loc: usize,
    /// Identifier occurrences inside the definition (and, for doubles, their impl blocks).
    idents: HashMap<String, usize>,
}

impl Support {
    /// Occurrences of `ident` inside this item.
    pub fn uses(&self, ident: &str) -> usize {
        self.idents.get(ident).copied().unwrap_or(0)
    }

    /// Identifiers inside this item.
    pub fn idents(&self) -> impl Iterator<Item = &str> {
        self.idents.keys().map(String::as_str)
    }
}

/// Lines of one file that are test code.
pub enum TestCode {
    WholeFile,
    Lines(Vec<RangeInclusive<usize>>),
}

impl TestCode {
    pub fn contains(&self, line: usize) -> bool {
        match self {
            Self::WholeFile => true,
            Self::Lines(r) => r.iter().any(|r| r.contains(&line)),
        }
    }
}

/// One crate's production and test code, for the `crates` report.
#[derive(Default, serde::Serialize)]
pub struct CrateSize {
    /// `[package] name` from the crate's `Cargo.toml`.
    pub package: Option<String>,
    pub prod_loc: usize,
    /// `.rs` test code: test files, `#[cfg(test)]` modules and items, test functions.
    pub test_loc: usize,
    /// Non-`.rs` files tests read: `testdata/`, `fixtures/`, `snapshots/`, files under `tests/`,
    /// and `include_str!`/`include_bytes!` targets in test code.
    pub fixture_loc: usize,
    /// `#[cfg(test)]` items (helpers, impls, types) inside production files.
    pub test_only_items_loc: usize,
    pub tests: usize,
    /// Tests under the crate's `src/`: unit tests, as opposed to `tests/` integration tests.
    pub unit_tests: usize,
    /// Tests that compare against a snapshot (`characterization_golden`).
    pub goldens: usize,
    pub doubles: usize,
    pub doubles_loc: usize,
    /// Workspace crates that list this one under normal or build dependencies (not dev).
    pub used_by: usize,
}

pub struct Scan {
    pub tests: Vec<TestFn>,
    pub support: Vec<Support>,
    pub hits: Vec<Hit>,
    /// Keyed by the crate directory relative to the root (`.` for the root).
    pub crates: BTreeMap<String, CrateSize>,
    /// Keyed by path relative to the root.
    pub test_code: HashMap<PathBuf, TestCode>,
    /// Test lines per file, keyed by path relative to the root.
    pub file_test_loc: HashMap<PathBuf, usize>,
    pub index: Index,
}

/// Workspace-wide facts the per-file pass needs.
#[derive(Default)]
pub struct Index {
    /// Non-test functions in test code whose bodies assert: calls to them are assertions.
    asserting: HashSet<String>,
    /// Non-test functions in test code that compare against or rewrite a snapshot, with the marker.
    golden: HashMap<String, String>,
    /// Occurrences of each identifier across every token of every file, definitions included.
    pub ident_counts: HashMap<String, usize>,
    /// Names production code defines: types, traits, free functions, methods (minus `COMMON_NAMES`),
    /// and identifiers inside item-level macros like `sol!`.
    pub prod_names: HashSet<String>,
    /// Traits each type `#[derive]`s.
    derives: HashMap<String, HashSet<String>>,
    /// Traits each type implements by hand (`impl Trait for Type`).
    manual_impls: HashMap<String, HashSet<String>>,
    /// Types whose serde derives are configured by `#[serde(..)]` attributes (on the type, a
    /// variant or a field): their serialized form is a declared format, not trivial derive code.
    serde_configured: HashSet<String>,
}

/// Function and method names too generic to show that a test calls workspace code.
const COMMON_NAMES: &[&str] = &[
    "new",
    "from",
    "into",
    "default",
    "clone",
    "len",
    "get",
    "get_mut",
    "iter",
    "iter_mut",
    "map",
    "unwrap",
    "expect",
    "as_ref",
    "as_mut",
    "to_string",
    "eq",
    "ne",
    "fmt",
    "hash",
    "cmp",
    "partial_cmp",
    "deref",
    "deref_mut",
    "drop",
    "next",
    "try_from",
    "try_into",
    "from_str",
    "is_empty",
    "push",
    "pop",
    "insert",
    "remove",
    "contains",
    "with_capacity",
    "build",
    "parse",
    "read",
    "write",
    "call",
    "run",
    "set",
    "update",
    "init",
    "open",
    "close",
    "start",
    "stop",
    "send",
    "recv",
    "encode",
    "decode",
    "serialize",
    "deserialize",
    "as_str",
    "as_bytes",
    "to_vec",
    "to_owned",
    "borrow",
    "index",
    "add",
    "sub",
    "mul",
    "div",
    "main",
    "flush",
    "poll",
    "extend",
    "clear",
    "first",
    "last",
    "keys",
    "values",
    "entry",
    "and_then",
    "ok",
    "err",
    "is_some",
    "is_none",
    "is_ok",
    "is_err",
    "len_hint",
    "size_hint",
    "into_iter",
    "collect",
    "filter",
    "fold",
    "sum",
];

/// A test whose tail expression is `callee(args.., |..| { body })`.
struct Envelope {
    callee: String,
    /// Lines the envelope adds: the call line, its closing line, a trailing `Ok(())`, and the
    /// `let`s that only bind the call's arguments.
    lines: usize,
}

/// One test wrapped by an envelope call.
struct EnvelopeUse {
    callee: String,
    file: PathBuf,
    line: usize,
    /// `Envelope::lines` of this test.
    lines: usize,
}

/// Every parsable `.rs` file under `root` (skipping `target` and `.git`), with its path relative to `root`.
fn rust_files(root: &Path) -> impl Iterator<Item = (PathBuf, String, syn::File)> + '_ {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.file_name() != "target" && e.file_name() != ".git")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
        .filter_map(move |e| {
            let src = std::fs::read_to_string(e.path()).ok()?;
            let ast = syn::parse_file(&src).ok()?;
            Some((
                e.path()
                    .strip_prefix(root)
                    .unwrap_or(e.path())
                    .to_path_buf(),
                src,
                ast,
            ))
        })
}

/// Files under `tests/` or a `test_util(s)` module, or gated by an inner `#![cfg(test)]`.
fn is_test_file(rel: &Path, ast: &syn::File) -> bool {
    rel.components().any(|c| {
        let c = c.as_os_str().to_string_lossy();
        matches!(
            c.trim_end_matches(".rs"),
            "tests" | "test_util" | "test_utils"
        )
    }) || is_cfg_test(&ast.attrs)
}

/// `#[serde(..)]`, or `#[cfg_attr(feature = "serde", serde(..))]`.
fn is_serde_attr(a: &syn::Attribute) -> bool {
    a.path().is_ident("serde")
        || (a.path().is_ident("cfg_attr")
            && a.meta.to_token_stream().to_string().contains("serde ("))
}

/// Builds the `Index` in a first pass over every file.
fn index(root: &Path) -> Index {
    struct Pass {
        in_test: bool,
        idx: Index,
    }
    impl Pass {
        /// `method`: an impl or trait fn, whose generic names (`COMMON_NAMES`) do not count.
        fn check(
            &mut self,
            attrs: &[syn::Attribute],
            ident: &syn::Ident,
            body: &syn::Block,
            method: bool,
        ) {
            if !self.in_test {
                let name = ident.to_string();
                if !method || !COMMON_NAMES.contains(&name.as_str()) {
                    self.idx.prod_names.insert(name);
                }
                return;
            }
            if attrs.iter().any(is_test_attr) {
                return;
            }
            if asserts_anything(body) {
                self.idx.asserting.insert(ident.to_string());
            }
            if let Some(m) = golden_marker(body, &HashMap::new()) {
                self.idx.golden.insert(ident.to_string(), m);
            }
        }
        fn ty(&mut self, attrs: &[syn::Attribute], ident: &syn::Ident) {
            let name = ident.to_string();
            let derived: HashSet<String> = attrs
                .iter()
                .filter(|a| a.path().is_ident("derive"))
                .filter_map(|a| {
                    a.parse_args_with(Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)
                        .ok()
                })
                .flatten()
                .filter_map(|p| p.segments.last().map(|s| s.ident.to_string()))
                .collect();
            self.idx
                .derives
                .entry(name.clone())
                .or_default()
                .extend(derived);
            if !self.in_test {
                self.idx.prod_names.insert(name);
            }
        }
        /// Records `name` as serde-configured when any of `attr_sets` has `#[serde(..)]`.
        fn serde_attrs<'f>(
            &mut self,
            name: &syn::Ident,
            mut attr_sets: impl Iterator<Item = &'f [syn::Attribute]>,
        ) {
            if attr_sets.any(|attrs| attrs.iter().any(is_serde_attr)) {
                self.idx.serde_configured.insert(name.to_string());
            }
        }
    }
    impl<'a> Visit<'a> for Pass {
        fn visit_item_mod(&mut self, m: &'a syn::ItemMod) {
            let was = self.in_test;
            self.in_test |= is_cfg_test(&m.attrs);
            syn::visit::visit_item_mod(self, m);
            self.in_test = was;
        }
        fn visit_item_fn(&mut self, f: &'a syn::ItemFn) {
            let was = self.in_test;
            self.in_test |= is_cfg_test(&f.attrs);
            self.check(&f.attrs, &f.sig.ident, &f.block, false);
            syn::visit::visit_item_fn(self, f);
            self.in_test = was;
        }
        fn visit_impl_item_fn(&mut self, f: &'a syn::ImplItemFn) {
            self.check(&f.attrs, &f.sig.ident, &f.block, true);
            syn::visit::visit_impl_item_fn(self, f);
        }
        fn visit_trait_item_fn(&mut self, f: &'a syn::TraitItemFn) {
            if !self.in_test {
                let name = f.sig.ident.to_string();
                if !COMMON_NAMES.contains(&name.as_str()) {
                    self.idx.prod_names.insert(name);
                }
            }
            syn::visit::visit_trait_item_fn(self, f);
        }
        fn visit_item_struct(&mut self, i: &'a syn::ItemStruct) {
            self.ty(&i.attrs, &i.ident);
            let fields = i.fields.iter().map(|f| f.attrs.as_slice());
            self.serde_attrs(&i.ident, std::iter::once(i.attrs.as_slice()).chain(fields));
            syn::visit::visit_item_struct(self, i);
        }
        fn visit_item_enum(&mut self, i: &'a syn::ItemEnum) {
            self.ty(&i.attrs, &i.ident);
            let variants = i.variants.iter().flat_map(|v| {
                std::iter::once(v.attrs.as_slice())
                    .chain(v.fields.iter().map(|f| f.attrs.as_slice()))
            });
            self.serde_attrs(
                &i.ident,
                std::iter::once(i.attrs.as_slice()).chain(variants),
            );
            syn::visit::visit_item_enum(self, i);
        }
        fn visit_item_trait(&mut self, i: &'a syn::ItemTrait) {
            if !self.in_test {
                self.idx.prod_names.insert(i.ident.to_string());
            }
            syn::visit::visit_item_trait(self, i);
        }
        fn visit_item_type(&mut self, i: &'a syn::ItemType) {
            if !self.in_test {
                self.idx.prod_names.insert(i.ident.to_string());
            }
        }
        fn visit_item_impl(&mut self, i: &'a syn::ItemImpl) {
            if let (Some((_, tr, _)), syn::Type::Path(ty)) = (&i.trait_, &*i.self_ty)
                && let (Some(t), Some(s)) = (tr.segments.last(), ty.path.segments.last())
            {
                self.idx
                    .manual_impls
                    .entry(s.ident.to_string())
                    .or_default()
                    .insert(t.ident.to_string());
            }
            syn::visit::visit_item_impl(self, i);
        }
        fn visit_item_macro(&mut self, m: &'a syn::ItemMacro) {
            // `sol! { interface IFoo { function bar(); } }` defines `IFoo` and `bar`.
            if !self.in_test {
                let mut stack = vec![m.mac.tokens.clone()];
                while let Some(ts) = stack.pop() {
                    for tt in ts {
                        match tt {
                            TokenTree::Ident(i) => {
                                let n = i.to_string();
                                if !COMMON_NAMES.contains(&n.as_str())
                                    && !KEYWORDS.contains(&n.as_str())
                                {
                                    self.idx.prod_names.insert(n);
                                }
                            }
                            TokenTree::Group(g) => stack.push(g.stream()),
                            _ => {}
                        }
                    }
                }
            }
            syn::visit::visit_item_macro(self, m);
        }
    }
    fn count_idents(ts: TokenStream, counts: &mut HashMap<String, usize>) {
        for tt in ts {
            match tt {
                TokenTree::Ident(i) => *counts.entry(i.to_string()).or_default() += 1,
                TokenTree::Group(g) => count_idents(g.stream(), counts),
                _ => {}
            }
        }
    }
    let mut p = Pass {
        in_test: false,
        idx: Index::default(),
    };
    for (rel, _, ast) in rust_files(root) {
        p.in_test = is_test_file(&rel, &ast);
        p.visit_file(&ast);
        count_idents(ast.to_token_stream(), &mut p.idx.ident_counts);
    }
    p.idx
}

/// insta and expect-test macros that compare against a stored snapshot.
const SNAPSHOT_MACROS: &[&str] = &[
    "assert_snapshot",
    "assert_debug_snapshot",
    "assert_json_snapshot",
    "assert_yaml_snapshot",
    "assert_toml_snapshot",
    "assert_ron_snapshot",
    "assert_csv_snapshot",
    "assert_compact_debug_snapshot",
    "assert_compact_json_snapshot",
    "expect_file",
];

/// A string literal that regenerates goldens (`UPDATE_GOLDEN`-style env var) or names a snapshot file.
fn golden_literal(v: &str) -> Option<String> {
    // `INSTA_UPDATE`, `TEMPO_DST_UPDATE_GOLDEN`, `BLESS`; a bare word like `"UPDATE"` may be SQL.
    let env_like = (v.contains('_') || v == "BLESS")
        && v.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if env_like
        && ["UPDATE", "BLESS", "GOLDEN", "REGENERATE", "OVERWRITE"]
            .iter()
            .any(|k| v.contains(k))
    {
        return Some(format!("env `{v}`"));
    }
    let l = v.to_lowercase();
    // `SNAPSHOT` alone and `.snapshot` files are too often product terms (DB or gas snapshots).
    let snapshot_file = l.contains("golden")
        || l.ends_with(".snap")
        || l.ends_with(".snap.new")
        || (l.contains("expected")
            && [".json", ".ndjson", ".txt", ".out", ".stdout"]
                .iter()
                .any(|e| l.ends_with(e)));
    (snapshot_file && !l.contains(' ')).then(|| format!("`{v}`"))
}

/// What makes `b` a snapshot comparison: a snapshot macro, a golden env var or path, or a call to
/// one of `golden_helpers`.
fn golden_marker(b: &syn::Block, golden_helpers: &HashMap<String, String>) -> Option<String> {
    struct Find<'h> {
        helpers: &'h HashMap<String, String>,
        found: Option<String>,
    }
    impl Find<'_> {
        fn tokens(&mut self, ts: TokenStream) {
            for tt in ts {
                match tt {
                    TokenTree::Literal(l) => {
                        if let Ok(s) = syn::parse_str::<syn::LitStr>(&l.to_string()) {
                            self.found = self.found.take().or_else(|| golden_literal(&s.value()));
                        }
                    }
                    TokenTree::Group(g) => self.tokens(g.stream()),
                    _ => {}
                }
            }
        }
        fn call(&mut self, name: &str) {
            if let Some(m) = self.helpers.get(name) {
                self.found = self
                    .found
                    .take()
                    .or_else(|| Some(format!("`{name}` ({m})")));
            }
        }
    }
    impl<'a> Visit<'a> for Find<'_> {
        fn visit_macro(&mut self, m: &'a syn::Macro) {
            let n = macro_name(m);
            if SNAPSHOT_MACROS.contains(&n.as_str()) {
                self.found = self.found.take().or(Some(format!("`{n}!`")));
            }
            self.tokens(m.tokens.clone());
        }
        fn visit_lit_str(&mut self, l: &'a syn::LitStr) {
            self.found = self.found.take().or_else(|| golden_literal(&l.value()));
        }
        fn visit_expr_call(&mut self, c: &'a syn::ExprCall) {
            if let syn::Expr::Path(p) = &*c.func
                && let Some(s) = p.path.segments.last()
            {
                self.call(&s.ident.to_string());
            }
            syn::visit::visit_expr_call(self, c);
        }
        fn visit_expr_method_call(&mut self, c: &'a syn::ExprMethodCall) {
            self.call(&c.method.to_string());
            syn::visit::visit_expr_method_call(self, c);
        }
    }
    let mut f = Find {
        helpers: golden_helpers,
        found: None,
    };
    f.visit_block(b);
    f.found
}

/// The block invokes an assert macro or an `assert_*`-style helper.
fn asserts_anything(b: &syn::Block) -> bool {
    struct Finds(bool);
    impl<'a> Visit<'a> for Finds {
        fn visit_macro(&mut self, m: &'a syn::Macro) {
            self.0 |= is_assert_macro(&macro_name(m));
        }
        fn visit_expr_call(&mut self, c: &'a syn::ExprCall) {
            if let syn::Expr::Path(p) = &*c.func {
                self.0 |= p
                    .path
                    .segments
                    .last()
                    .is_some_and(|s| is_assert_helper(&s.ident.to_string()));
            }
            syn::visit::visit_expr_call(self, c);
        }
        fn visit_expr_method_call(&mut self, c: &'a syn::ExprMethodCall) {
            self.0 |= is_assert_helper(&c.method.to_string());
            syn::visit::visit_expr_method_call(self, c);
        }
    }
    let mut f = Finds(false);
    f.visit_block(b);
    f.0
}

/// Directory names whose non-`.rs` files are test data.
const FIXTURE_DIRS: &[&str] = &[
    "testdata",
    "test-data",
    "test_data",
    "fixtures",
    "snapshots",
    "goldens",
    "golden",
];

/// Maps paths to their crate: the nearest ancestor directory with a `Cargo.toml`.
struct Crates<'r> {
    root: &'r Path,
    cache: HashMap<PathBuf, String>,
}

impl Crates<'_> {
    /// Crate directory of `rel` relative to the root, `.` when only the root (or nothing) has a manifest.
    fn of(&mut self, rel: &Path) -> String {
        let mut dir = rel.parent().unwrap_or(Path::new("")).to_path_buf();
        let mut walked = vec![];
        let found = loop {
            if let Some(k) = self.cache.get(&dir) {
                break k.clone();
            }
            walked.push(dir.clone());
            if self.root.join(&dir).join("Cargo.toml").is_file() {
                let s = dir.to_string_lossy().into_owned();
                break if s.is_empty() { ".".to_owned() } else { s };
            }
            if !dir.pop() {
                break ".".to_owned();
            }
        };
        for d in walked {
            self.cache.insert(d, found.clone());
        }
        found
    }
}

/// `[package] name` of `<root>/<krate>/Cargo.toml`, and the workspace package names it lists under
/// normal or build dependencies (including `[target.*]` ones, by `package =` when renamed).
fn manifest(root: &Path, krate: &str) -> (Option<String>, Vec<String>) {
    let Some(t) = std::fs::read_to_string(root.join(krate).join("Cargo.toml"))
        .ok()
        .and_then(|m| m.parse::<toml::Table>().ok())
    else {
        return (None, vec![]);
    };
    let name = t
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_owned);
    fn dep_tables(owner: &toml::Table) -> impl Iterator<Item = &toml::Table> {
        ["dependencies", "build-dependencies"]
            .into_iter()
            .filter_map(|k| owner.get(k)?.as_table())
    }
    let targets = t.get("target").and_then(|t| t.as_table());
    let tables = dep_tables(&t).chain(
        targets
            .into_iter()
            .flat_map(|ts| ts.values().filter_map(|v| v.as_table()))
            .flat_map(dep_tables),
    );
    let deps = tables
        .flat_map(|d| d.iter())
        .map(|(k, v)| {
            v.get("package")
                .and_then(|p| p.as_str())
                .unwrap_or(k)
                .to_owned()
        })
        .collect();
    (name, deps)
}

/// Parses every `.rs` file under `root` (skipping `target` and `.git`); unparsable files are skipped.
pub fn scan(root: &Path) -> Scan {
    let index = index(root);
    let mut crates = Crates {
        root,
        cache: HashMap::new(),
    };
    let mut out = Scan {
        tests: vec![],
        support: vec![],
        hits: vec![],
        crates: BTreeMap::new(),
        test_code: HashMap::new(),
        file_test_loc: HashMap::new(),
        index: Index::default(),
    };
    let mut envelopes: BTreeMap<(String, String), Vec<EnvelopeUse>> = BTreeMap::new();
    let mut includes: Vec<(String, PathBuf)> = vec![];
    for (rel, src, ast) in rust_files(root) {
        let src_lines: Vec<&str> = src.lines().collect();
        let whole_file = is_test_file(&rel, &ast);
        let krate = crates.of(&rel);
        let mut fs = FileScan {
            file: &rel,
            krate: &krate,
            src_lines: &src_lines,
            mod_path: file_module_path(&rel),
            in_test: whole_file,
            test_ranges: vec![],
            tests: vec![],
            envelopes: vec![],
            support: vec![],
            impls: vec![],
            includes: vec![],
            test_only_items_loc: 0,
            goldens: 0,
            index: &index,
            hits: &mut out.hits,
        };
        fs.visit_file(&ast);
        fs.attach_impls();
        let code = if whole_file {
            TestCode::WholeFile
        } else {
            TestCode::Lines(std::mem::take(&mut fs.test_ranges))
        };
        divider_hits(&rel, &src_lines, &code, fs.hits);
        let total = src_lines.len();
        let test_loc = match &code {
            TestCode::WholeFile => total,
            TestCode::Lines(r) => r
                .iter()
                .map(|r| r.end() - r.start() + 1)
                .sum::<usize>()
                .min(total),
        };
        for u in fs.envelopes {
            envelopes
                .entry((krate.clone(), u.callee.clone()))
                .or_default()
                .push(u);
        }
        includes.extend(fs.includes.into_iter().map(|p| (krate.clone(), p)));
        let size = out.crates.entry(krate.clone()).or_default();
        size.prod_loc += total - test_loc;
        size.test_loc += test_loc;
        size.tests += fs.tests.len();
        let src_dir = if krate == "." {
            PathBuf::from("src")
        } else {
            Path::new(&krate).join("src")
        };
        size.unit_tests += fs
            .tests
            .iter()
            .filter(|t| t.file.starts_with(&src_dir))
            .count();
        size.test_only_items_loc += fs.test_only_items_loc;
        size.goldens += fs.goldens;
        for d in fs.support.iter().filter(|s| s.kind == SupportKind::Double) {
            size.doubles += 1;
            size.doubles_loc += d.loc;
        }
        out.tests.extend(fs.tests);
        out.support.extend(fs.support);
        out.file_test_loc.insert(rel.clone(), test_loc);
        out.test_code.insert(rel, code);
    }
    count_fixtures(root, &mut crates, includes, &mut out.crates);
    let manifests: Vec<(String, Option<String>, Vec<String>)> = out
        .crates
        .keys()
        .map(|k| {
            let (name, deps) = manifest(root, k);
            (k.clone(), name, deps)
        })
        .collect();
    for (krate, name, _) in &manifests {
        let size = out
            .crates
            .get_mut(krate)
            .expect("manifests are read for existing crates");
        size.package = name.clone();
        size.used_by = name.as_ref().map_or(0, |n| {
            manifests
                .iter()
                .filter(|(k, _, deps)| k != krate && deps.contains(n))
                .count()
        });
    }
    out.index = index;
    envelope_hits(envelopes, &mut out.hits);
    clone_hits(&out.tests, &mut out.hits);
    fixture_hits(&out.tests, &mut out.hits);
    out.hits.extend(crate::suite::static_hits(&out));
    out
}

/// Adds the lines of every test data file to its crate's `fixture_loc`: non-`.rs` files under a
/// `FIXTURE_DIRS` directory or `tests/`, and the `include_str!`/`include_bytes!` targets of test
/// code (resolved paths relative to the root, with the including crate). Non-UTF-8 files are skipped.
fn count_fixtures(
    root: &Path,
    crates: &mut Crates,
    includes: Vec<(String, PathBuf)>,
    sizes: &mut BTreeMap<String, CrateSize>,
) {
    let mut counted: HashSet<PathBuf> = HashSet::new();
    let data_files = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.file_name() != "target" && e.file_name() != ".git")
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension().is_none_or(|x| x != "rs"))
        .filter_map(|e| e.path().strip_prefix(root).ok().map(Path::to_path_buf))
        .filter(|rel| {
            rel.components().any(|c| {
                let c = c.as_os_str().to_string_lossy();
                FIXTURE_DIRS.contains(&c.as_ref()) || c == "tests"
            })
        })
        .map(|rel| (crates.of(&rel), rel))
        .collect::<Vec<_>>();
    for (krate, rel) in data_files.into_iter().chain(includes) {
        if !counted.insert(rel.clone()) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(root.join(&rel)) {
            sizes.entry(krate).or_default().fixture_loc += text.lines().count();
        }
    }
}

/// Module path of a file under a crate's `src/`, `None` elsewhere.
fn file_module_path(rel: &Path) -> Option<Vec<String>> {
    let comps: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let src = comps.iter().rposition(|c| c == "src")?;
    let mut path: Vec<String> = comps[src + 1..].to_vec();
    let last = path.pop()?;
    let stem = last.strip_suffix(".rs")?;
    if !matches!(stem, "lib" | "main" | "mod") {
        path.push(stem.to_owned());
    }
    Some(path)
}

fn attr_path(a: &syn::Attribute) -> String {
    a.path().to_token_stream().to_string().replace(' ', "")
}

/// `#[test]`, `#[tokio::test]`, `#[rstest]`, `#[test_case(..)]`, and project test attributes like
/// `#[forgetest]`: the last path segment names a test.
fn is_test_attr(a: &syn::Attribute) -> bool {
    a.path()
        .segments
        .last()
        .is_some_and(|s| s.ident.to_string().contains("test"))
}

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg") && {
            let s = a.meta.to_token_stream().to_string();
            s.contains("test") && !s.contains("not (test")
        }
    })
}

const KEYWORDS: &[&str] = &[
    "let", "mut", "if", "else", "match", "for", "in", "while", "loop", "return", "fn", "move",
    "async", "await", "ref", "true", "false", "Some", "None", "Ok", "Err", "self", "Self", "as",
    "break", "continue", "unsafe", "impl", "dyn", "where", "const", "static", "struct", "enum",
];

/// Flattens tokens into a loose shape (identifiers and literals as `$`) and the exact tokens.
fn flatten(ts: TokenStream, loose: &mut String, exact: &mut Vec<String>) {
    for tt in ts {
        match tt {
            TokenTree::Group(g) => {
                let (o, c) = match g.delimiter() {
                    Delimiter::Parenthesis => ("(", ")"),
                    Delimiter::Brace => ("{", "}"),
                    Delimiter::Bracket => ("[", "]"),
                    Delimiter::None => ("", ""),
                };
                loose.push_str(o);
                exact.push(o.into());
                flatten(g.stream(), loose, exact);
                loose.push_str(c);
                exact.push(c.into());
            }
            TokenTree::Ident(i) => {
                let s = i.to_string();
                loose.push_str(if KEYWORDS.contains(&s.as_str()) {
                    &s
                } else {
                    "$"
                });
                loose.push(' ');
                exact.push(s);
            }
            TokenTree::Literal(l) => {
                loose.push_str("$ ");
                exact.push(l.to_string());
            }
            TokenTree::Punct(p) => {
                loose.push(p.as_char());
                exact.push(p.as_char().to_string());
            }
        }
    }
}

fn loose_of(ts: TokenStream) -> String {
    let mut l = String::new();
    flatten(ts, &mut l, &mut vec![]);
    l
}

/// Tokens with every literal replaced by `$`, identifiers kept.
fn literal_blind(ts: TokenStream) -> String {
    fn blind(ts: TokenStream) -> TokenStream {
        ts.into_iter()
            .map(|tt| match tt {
                TokenTree::Literal(_) => {
                    TokenTree::Punct(proc_macro2::Punct::new('$', proc_macro2::Spacing::Alone))
                }
                TokenTree::Group(g) => {
                    TokenTree::Group(proc_macro2::Group::new(g.delimiter(), blind(g.stream())))
                }
                other => other,
            })
            .collect()
    }
    blind(ts).to_string()
}

/// Newlines inside literals (embedded sources, snapshots): lines a table or fixture cannot share.
fn literal_newlines(ts: TokenStream) -> usize {
    ts.into_iter()
        .map(|tt| match tt {
            TokenTree::Literal(l) => l.to_string().matches('\n').count(),
            TokenTree::Group(g) => literal_newlines(g.stream()),
            _ => 0,
        })
        .sum()
}

/// Lines of `s` outside multi-line literals.
fn code_lines(s: &(impl Spanned + ToTokens)) -> usize {
    lines(s).saturating_sub(literal_newlines(s.to_token_stream()))
}

fn lines(s: &impl Spanned) -> usize {
    let sp = s.span();
    sp.end().line - sp.start().line + 1
}

fn macro_name(m: &syn::Macro) -> String {
    m.path
        .segments
        .last()
        .map(|s| s.ident.to_string())
        .unwrap_or_default()
}

fn macro_args(m: &syn::Macro) -> Option<Punctuated<syn::Expr, syn::Token![,]>> {
    m.parse_body_with(Punctuated::parse_terminated).ok()
}

/// Arguments an assert macro checks; the rest are its message.
fn checked_args(name: &str) -> usize {
    match name {
        "assert" | "prop_assert" | "debug_assert" => 1,
        _ => 2,
    }
}

fn is_assert_macro(name: &str) -> bool {
    name.starts_with("assert") || name.starts_with("prop_assert")
}

/// A function or method whose name says it asserts, like `assert_rejected` or `expect_revert`.
fn is_assert_helper(name: &str) -> bool {
    ["assert_", "expect_", "check_", "verify_"]
        .iter()
        .any(|p| name.starts_with(p))
        && !matches!(name, "expect_err")
}

fn compact(ts: impl ToTokens) -> String {
    ts.to_token_stream().to_string().replace(' ', "")
}

/// The pattern binds no variable, so a panicking `let .. else` only asserts the match.
fn binds_nothing(p: &syn::Pat) -> bool {
    struct Binds(bool);
    impl<'a> Visit<'a> for Binds {
        fn visit_pat_ident(&mut self, i: &'a syn::PatIdent) {
            let s = i.ident.to_string();
            self.0 |= s != "_" && s.starts_with(|c: char| c.is_lowercase() || c == '_');
            syn::visit::visit_pat_ident(self, i);
        }
    }
    let mut b = Binds(false);
    b.visit_pat(p);
    !b.0
}

fn panics(e: &syn::Expr) -> bool {
    let s = e.to_token_stream().to_string();
    (s.contains("panic !") || s.contains("unreachable !")) && s.len() < 200
}

/// Checked assert arguments (compact tokens) that inspect an error's value, not just `is_err()`.
fn is_err_check(text: &str) -> bool {
    ["unwrap_err", "expect_err", "Err(", "Error::", "Error("]
        .iter()
        .any(|p| text.contains(p))
}

/// rustfmt's default `max_width`.
const MAX_WIDTH: usize = 100;
/// rustfmt's default `fn_call_width`: wider argument lists go one per line.
const FN_CALL_WIDTH: usize = 60;

/// Lines an `assert_err!(expr, Variant)` call needs at `indent`, given its arguments as
/// (compact tokens, current lines). Absorbed wrappers (`matches!(`, `Err(`, `.unwrap_err()`,
/// `.into()`) are dropped. Past `FN_CALL_WIDTH` or `MAX_WIDTH`, rustfmt puts each argument on its
/// own line, and an argument keeps its current height unless it is short and has no struct literal.
fn err_helper_lines(indent: usize, args: &[(String, usize)]) -> usize {
    let width = |t: &str| {
        let absorbed: usize = ["matches!(", "Err(", ".unwrap_err()", ".into()"]
            .iter()
            .map(|w| t.matches(w).count() * (w.len() + usize::from(w.ends_with('('))))
            .sum();
        (t.len() + t.matches(',').count()).saturating_sub(absorbed)
    };
    let args_width: usize = args.iter().map(|a| width(&a.0) + 2).sum();
    if args_width <= FN_CALL_WIDTH && indent + "assert_err!(".len() + args_width <= MAX_WIDTH {
        return 1;
    }
    let short = |t: &str| width(t) <= FN_CALL_WIDTH && !t.contains('{');
    2 + args
        .iter()
        .map(|(t, l)| if short(t) { 1 } else { *l })
        .sum::<usize>()
}

/// Receiver `x` of `assert!(x.is_err())`.
fn is_err_receiver(s: &syn::Stmt) -> Option<String> {
    let m = stmt_macro(s)?;
    if macro_name(m) != "assert" {
        return None;
    }
    match macro_args(m)?.first()? {
        syn::Expr::MethodCall(c) if c.method == "is_err" => Some(compact(&c.receiver)),
        _ => None,
    }
}

fn stmt_macro(s: &syn::Stmt) -> Option<&syn::Macro> {
    match s {
        syn::Stmt::Macro(m) => Some(&m.mac),
        syn::Stmt::Expr(syn::Expr::Macro(m), _) => Some(&m.mac),
        _ => None,
    }
}

/// Base of `assert_eq!(base.field, ..)`, if the statement is one.
fn field_assert_base(s: &syn::Stmt) -> Option<String> {
    let m = stmt_macro(s)?;
    if macro_name(m) != "assert_eq" {
        return None;
    }
    match macro_args(m)?.first()? {
        syn::Expr::Field(f) => Some(f.base.to_token_stream().to_string()),
        _ => None,
    }
}

/// Best (repeats, LOC saved) over blocks of 1..=6 statements that repeat inside one body.
fn repeated_block(stmts: &[syn::Stmt]) -> Option<(usize, usize)> {
    let shapes: Vec<(String, usize)> = stmts
        .iter()
        .map(|s| (loose_of(s.to_token_stream()), code_lines(s)))
        .collect();
    let mut best: Option<(usize, usize)> = None;
    for k in 1..=6.min(shapes.len() / 2) {
        // Non-overlapping occurrences per window shape: (count, end of last occurrence, loc).
        let mut seen: HashMap<Vec<&str>, (usize, usize, usize)> = HashMap::new();
        for i in 0..=shapes.len() - k {
            let w = &shapes[i..i + k];
            let loc: usize = w.iter().map(|x| x.1).sum();
            if loc < 3 {
                continue;
            }
            let e = seen
                .entry(w.iter().map(|x| x.0.as_str()).collect())
                .or_insert((0, 0, loc));
            if e.0 == 0 || i >= e.1 {
                e.0 += 1;
                e.1 = i + k;
            }
        }
        for (n, _, loc) in seen.into_values() {
            if n >= 3 || (n >= 2 && loc >= 6) {
                let saved = ((n - 1) * loc).saturating_sub(n + 2);
                if saved >= 4 && best.is_none_or(|(_, s)| saved > s) {
                    best = Some((n, saved));
                }
            }
        }
    }
    best
}

/// The block's tail `callee(.., |..| { .. })` call and the closure's statements, if it has one.
fn envelope(block: &syn::Block) -> Option<(Envelope, &[syn::Stmt])> {
    let Some(syn::Stmt::Expr(tail, None)) = block.stmts.last() else {
        return None;
    };
    let (callee, args) = match tail {
        syn::Expr::Call(c) => (compact(&c.func), &c.args),
        syn::Expr::MethodCall(c) => (format!(".{}", c.method), &c.args),
        _ => return None,
    };
    let Some(syn::Expr::Closure(closure)) = args.last() else {
        return None;
    };
    let syn::Expr::Block(body) = &*closure.body else {
        return None;
    };
    let inner = &body.block.stmts;
    let ok_tail = inner.last().is_some_and(|s| compact(s) == "Ok(())");
    let arg_idents: BTreeSet<String> = args
        .iter()
        .take(args.len() - 1)
        .flat_map(|a| a.to_token_stream())
        .filter_map(|t| match t {
            TokenTree::Ident(i) => Some(i.to_string()),
            _ => None,
        })
        .collect();
    let setup: usize = block.stmts[..block.stmts.len() - 1]
        .iter()
        .filter_map(|s| match s {
            syn::Stmt::Local(l) => match &l.pat {
                syn::Pat::Ident(p) if arg_idents.contains(&p.ident.to_string()) => Some(lines(l)),
                _ => None,
            },
            _ => None,
        })
        .sum();
    let inner_stmts = if ok_tail {
        &inner[..inner.len() - 1]
    } else {
        &inner[..]
    };
    Some((
        Envelope {
            callee,
            lines: setup + 2 + usize::from(ok_tail),
        },
        inner_stmts,
    ))
}

fn is_screaming(id: &str) -> bool {
    id.len() > 1
        && id
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Literals and SCREAMING_CASE names of a macro body that does not parse as expressions (`vec![0; 64]`).
fn token_inputs(ts: TokenStream, inputs: &mut BTreeSet<String>) {
    for tt in ts {
        match tt {
            TokenTree::Literal(l) => {
                inputs.insert(l.to_string());
            }
            TokenTree::Ident(i) if is_screaming(&i.to_string()) => {
                inputs.insert(i.to_string());
            }
            TokenTree::Group(g) => token_inputs(g.stream(), inputs),
            _ => {}
        }
    }
}

/// Every identifier and literal of a proptest parameter list: the strategies are the inputs.
fn param_inputs(tt: TokenTree, inputs: &mut BTreeSet<String>) {
    match tt {
        TokenTree::Group(g) => g.stream().into_iter().for_each(|t| param_inputs(t, inputs)),
        TokenTree::Ident(i) => {
            inputs.insert(i.to_string());
        }
        TokenTree::Literal(l) => {
            inputs.insert(l.to_string());
        }
        TokenTree::Punct(_) => {}
    }
}

/// The condition of an environment-dependent early exit among `stmts`: `if <check> { ..; return; }`
/// or `let .. = <check> else { return };`, where the check reads an env var, a tool's availability,
/// a path's existence, `cfg!`, or a CI flag.
fn silent_skip(stmts: &[syn::Stmt]) -> Option<String> {
    fn block_returns(b: &syn::Block) -> bool {
        b.stmts
            .iter()
            .any(|s| matches!(s, syn::Stmt::Expr(syn::Expr::Return(_), _)))
    }
    const MARKERS: &[&str] = &[
        "env ::",
        "var (",
        "var_os",
        "available",
        "installed",
        "enabled",
        "which",
        "exists ()",
        "is_ci",
        "\"CI\"",
        "skip",
        "cfg !",
        "Command",
    ];
    stmts.iter().find_map(|st| {
        let (cond, exits) = match st {
            syn::Stmt::Expr(syn::Expr::If(i), _) => {
                (i.cond.to_token_stream(), block_returns(&i.then_branch))
            }
            syn::Stmt::Local(syn::Local {
                init: Some(init), ..
            }) => match &init.diverge {
                Some((_, div)) => (
                    init.expr.to_token_stream(),
                    match &**div {
                        syn::Expr::Return(_) => true,
                        syn::Expr::Block(b) => block_returns(&b.block),
                        _ => false,
                    },
                ),
                None => return None,
            },
            _ => return None,
        };
        let c = cond.to_string();
        // `if !is_ci() { return }` skips locally but runs in CI; skipping *in* CI is the silent one.
        let ci_only = c.contains("is_ci") && c.trim_start().starts_with('!');
        (exits && !ci_only && MARKERS.iter().any(|m| c.contains(m))).then_some(c)
    })
}

/// Why a test exercises nothing but trivial code: every assertion reads back a field or getter of a
/// value it built from the same literal, or it only uses impls its types `#[derive]` (none written by hand).
fn trivial_reason(stmts: &[syn::Stmt], n_asserts: usize, index: &Index) -> Option<String> {
    if n_asserts == 0 {
        return None;
    }
    // (a) Read-backs: `let v = T { f: 1 }` or `T::new(1)`, then `assert_eq!(v.f, 1)` / `v.f()`.
    let mut built: HashMap<String, HashSet<String>> = HashMap::new();
    for st in stmts {
        if let syn::Stmt::Local(syn::Local {
            pat: syn::Pat::Ident(p),
            init: Some(init),
            ..
        }) = st
        {
            let lits: Option<HashSet<String>> = match &*init.expr {
                syn::Expr::Struct(e) => e
                    .fields
                    .iter()
                    .map(|f| match &f.expr {
                        syn::Expr::Lit(l) => Some(compact(l)),
                        _ => None,
                    })
                    .collect(),
                syn::Expr::Call(c) if matches!(&*c.func, syn::Expr::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "new" || s.ident == "from")) => {
                    c.args
                        .iter()
                        .map(|a| match a {
                            syn::Expr::Lit(l) => Some(compact(l)),
                            _ => None,
                        })
                        .collect()
                }
                _ => None,
            };
            if let Some(l) = lits.filter(|l| !l.is_empty()) {
                built.insert(p.ident.to_string(), l);
            }
        }
    }
    let reads_back = |a: &syn::Expr, b: &syn::Expr| {
        let base = match a {
            syn::Expr::Field(f) => &f.base,
            syn::Expr::MethodCall(m) if m.args.is_empty() => &m.receiver,
            _ => return false,
        };
        matches!((&**base, b), (syn::Expr::Path(p), syn::Expr::Lit(l))
            if built.get(&compact(p)).is_some_and(|lits| lits.contains(&compact(l))))
    };
    let readbacks = stmts
        .iter()
        .filter_map(stmt_macro)
        .filter(|m| macro_name(m) == "assert_eq")
        .filter_map(macro_args)
        .filter(|a| {
            let v: Vec<&syn::Expr> = a.iter().collect();
            v.len() >= 2 && (reads_back(v[0], v[1]) || reads_back(v[1], v[0]))
        })
        .count();
    if readbacks == n_asserts && !built.is_empty() {
        return Some("only reads back fields it set from the same literals".into());
    }

    // (b) Derived impls only: build workspace types, then only clone, `Default`, serde round trips,
    // `{:?}` and `assert_eq!`.
    struct Uses {
        types: BTreeSet<String>,
        traits: BTreeSet<&'static str>,
        other_call: bool,
    }
    impl<'a> Visit<'a> for Uses {
        fn visit_expr_struct(&mut self, e: &'a syn::ExprStruct) {
            if let Some(s) = e.path.segments.last() {
                self.types.insert(s.ident.to_string());
            }
            syn::visit::visit_expr_struct(self, e);
        }
        fn visit_expr_call(&mut self, c: &'a syn::ExprCall) {
            match &*c.func {
                syn::Expr::Path(p) => {
                    let segs: Vec<String> = p
                        .path
                        .segments
                        .iter()
                        .map(|s| s.ident.to_string())
                        .collect();
                    let last = segs.last().map(String::as_str).unwrap_or_default();
                    let serde_crate = segs.iter().any(|s| {
                        matches!(
                            s.as_str(),
                            "serde_json" | "bincode" | "toml" | "postcard" | "serde_yaml"
                        )
                    });
                    if last == "default" {
                        self.traits.insert("Default");
                        if let [.., ty, _] = segs.as_slice() {
                            self.types.insert(ty.clone());
                        }
                    } else if serde_crate {
                        self.traits.extend(["Serialize", "Deserialize"]);
                    } else if let [ty] = segs.as_slice()
                        && ty.starts_with(|c: char| c.is_uppercase())
                    {
                        // Tuple-struct construction `T(..)`.
                        self.types.insert(ty.clone());
                    } else {
                        self.other_call = true;
                    }
                }
                _ => self.other_call = true,
            }
            syn::visit::visit_expr_call(self, c);
        }
        fn visit_expr_method_call(&mut self, c: &'a syn::ExprMethodCall) {
            match c.method.to_string().as_str() {
                "clone" => {
                    self.traits.insert("Clone");
                }
                "unwrap" | "expect" => {}
                _ => self.other_call = true,
            }
            syn::visit::visit_expr_method_call(self, c);
        }
        fn visit_macro(&mut self, m: &'a syn::Macro) {
            let n = macro_name(m);
            if matches!(n.as_str(), "assert_eq" | "assert_ne") {
                self.traits.extend(["PartialEq", "Debug"]);
            }
            if m.tokens.to_string().contains(":?") {
                self.traits.insert("Debug");
            }
            if let Some(args) = macro_args(m) {
                args.iter().for_each(|e| self.visit_expr(e));
            }
        }
    }
    let mut u = Uses {
        types: BTreeSet::new(),
        traits: BTreeSet::new(),
        other_call: false,
    };
    for st in stmts {
        u.visit_stmt(st);
    }
    if u.other_call || u.traits.is_empty() {
        return None;
    }
    let serde = u.traits.contains("Serialize") || u.traits.contains("Deserialize");
    let derived_only = |ty: &String| {
        !(serde && index.serde_configured.contains(ty))
            && index
                .derives
                .get(ty)
                .is_some_and(|d| u.traits.iter().all(|t| d.contains(*t)))
            && index
                .manual_impls
                .get(ty)
                .is_none_or(|m| u.traits.iter().all(|t| !m.contains(*t)))
    };
    let types: Vec<&String> = u
        .types
        .iter()
        .filter(|t| index.derives.contains_key(*t))
        .collect();
    (!types.is_empty() && types.iter().all(|t| derived_only(t))).then(|| {
        format!(
            "only exercises derived {} of {}",
            u.traits.iter().copied().collect::<Vec<_>>().join(", "),
            types
                .iter()
                .map(|t| format!("`{t}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// Walks one test body for assertions, inputs, panic paths and assertion-level smells.
struct BodyScan<'a> {
    file: &'a Path,
    test: &'a str,
    asserts: BTreeSet<String>,
    inputs: BTreeSet<String>,
    panic_path: bool,
    repeated: Option<(usize, usize)>,
    sites: Vec<(usize, usize, String)>,
    helpers: &'a HashSet<String>,
    hits: &'a mut Vec<Hit>,
}

impl BodyScan<'_> {
    fn is_assert_call(&self, name: &str) -> bool {
        is_assert_helper(name) || self.helpers.contains(name)
    }

    /// Visits the `{ .. }` groups of a macro body that does not parse as expressions, like
    /// `proptest!(config, |(x in s)| { .. })`.
    fn visit_macro_blocks(&mut self, ts: TokenStream) {
        for tt in ts {
            if let TokenTree::Group(g) = tt {
                match syn::parse2::<syn::Block>(TokenTree::Group(g.clone()).into()) {
                    Ok(b) if g.delimiter() == Delimiter::Brace => self.visit_block(&b),
                    _ => self.visit_macro_blocks(g.stream()),
                }
            }
        }
    }

    fn hit(&mut self, lint: Lint, line: usize, saved_loc: usize, detail: String) {
        self.hits.push(Hit {
            saved_ms: None,
            lint,
            file: self.file.into(),
            line,
            item: self.test.into(),
            saved_loc,
            detail,
        });
    }

    /// Statement-sequence checks for one block: field-by-field asserts, `is_err` before a variant check, repeats.
    fn scan_stmts(&mut self, stmts: &[syn::Stmt]) {
        let mut run: Option<(String, usize, usize)> = None;
        for (i, st) in stmts.iter().enumerate() {
            match (&mut run, field_assert_base(st)) {
                (Some((base, _, n)), Some(b)) if *base == b => *n += 1,
                (r, b) => {
                    if let Some((base, at, n)) = r.take().filter(|r| r.2 >= 3) {
                        self.hit(
                            Lint::FieldByFieldAssert,
                            at,
                            n - 1,
                            format!("{n} fields of `{base}`"),
                        );
                    }
                    *r = b.map(|b| (b, st.span().start().line, 1));
                }
            }
            if let Some(recv) = is_err_receiver(st) {
                let checked_next = stmts[i + 1..].iter().take(2).any(|n| {
                    let t = compact(n);
                    t.contains(&format!("{recv}.unwrap_err()"))
                        || t.contains(&format!("{recv}.expect_err("))
                        || (t.contains(&format!("matches!({recv},Err(")))
                });
                if checked_next {
                    self.hit(
                        Lint::RedundantIsErrAssert,
                        st.span().start().line,
                        lines(st),
                        format!("`{recv}` is unwrapped as an error next"),
                    );
                }
            }
        }
        if let Some((base, at, n)) = run.filter(|r| r.2 >= 3) {
            self.hit(
                Lint::FieldByFieldAssert,
                at,
                n - 1,
                format!("{n} fields of `{base}`"),
            );
        }
        if let Some((n, saved)) = repeated_block(stmts)
            && self.repeated.is_none_or(|(_, s)| saved > s)
        {
            self.repeated = Some((n, saved));
        }
    }
}

impl<'ast> Visit<'ast> for BodyScan<'_> {
    fn visit_block(&mut self, b: &'ast syn::Block) {
        self.scan_stmts(&b.stmts);
        syn::visit::visit_block(self, b);
    }
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        let n = macro_name(m);
        let args = macro_args(m);
        let checked: Vec<&syn::Expr> = match (&args, is_assert_macro(&n)) {
            (Some(a), true) => a.iter().take(checked_args(&n)).collect(),
            (Some(a), false) => a.iter().collect(),
            (None, _) => vec![],
        };
        if is_assert_macro(&n) {
            let text = checked.iter().map(compact).collect::<Vec<_>>().join(",");
            self.asserts.insert(format!("{n}!({text})"));
            let at = m.span().start();
            self.sites.push((at.line, at.column, format!("{n}!")));
            if let ("assert_eq" | "assert_ne", [a, b, ..]) = (n.as_str(), checked.as_slice()) {
                let (a, b) = (
                    a.to_token_stream().to_string(),
                    b.to_token_stream().to_string(),
                );
                if a == b {
                    self.hit(
                        Lint::TautologicalAssert,
                        m.span().start().line,
                        lines(m),
                        format!("{n}!({a}, {b})"),
                    );
                }
            }
            // `matches!(e, P)` passes `e` and `P` to the helper.
            let inner = match checked.as_slice() {
                [syn::Expr::Macro(mm)] if macro_name(&mm.mac) == "matches" => {
                    macro_args(&mm.mac).map(|a| a.into_iter().collect::<Vec<_>>())
                }
                _ => None,
            };
            let args: Vec<(String, usize)> = match &inner {
                Some(a) => a.iter().map(|e| (compact(e), lines(e))).collect(),
                None => checked.iter().map(|e| (compact(e), lines(*e))).collect(),
            };
            let saved = lines(m).saturating_sub(err_helper_lines(m.span().start().column, &args));
            if lines(m) >= 3 && is_err_check(&text) && saved > 0 {
                self.hit(
                    Lint::VerboseErrAssert,
                    m.span().start().line,
                    saved,
                    String::new(),
                );
            }
        }
        self.panic_path |= matches!(n.as_str(), "panic" | "unreachable" | "bail");
        if args.is_none() {
            token_inputs(m.tokens.clone(), &mut self.inputs);
            self.visit_macro_blocks(m.tokens.clone());
        }
        for e in checked {
            self.visit_expr(e);
        }
    }
    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*c.func
            && p.path
                .segments
                .last()
                .is_some_and(|s| self.is_assert_call(&s.ident.to_string()))
        {
            self.asserts.insert(compact(c));
            let at = c.span().start();
            self.sites.push((at.line, at.column, compact(&c.func)));
        }
        syn::visit::visit_expr_call(self, c);
    }
    fn visit_expr_method_call(&mut self, c: &'ast syn::ExprMethodCall) {
        let m = c.method.to_string();
        self.panic_path |= matches!(
            m.as_str(),
            "unwrap" | "expect" | "unwrap_err" | "expect_err"
        );
        if self.is_assert_call(&m) {
            self.asserts.insert(compact(c));
            let at = c.method.span().start();
            self.sites.push((at.line, at.column, format!(".{m}")));
        }
        syn::visit::visit_expr_method_call(self, c);
    }
    fn visit_expr_try(&mut self, e: &'ast syn::ExprTry) {
        self.panic_path = true;
        syn::visit::visit_expr_try(self, e);
    }
    fn visit_lit(&mut self, l: &'ast syn::Lit) {
        self.inputs.insert(l.to_token_stream().to_string());
    }
    fn visit_path(&mut self, p: &'ast syn::Path) {
        if p.segments.len() > 1
            || p.segments
                .first()
                .is_some_and(|s| is_screaming(&s.ident.to_string()))
        {
            // Module prefixes (`crate::`, `orderbook::`) do not change what is fed in; type qualifiers do.
            let segs: Vec<String> = p.segments.iter().map(compact).collect();
            let keep = segs
                .iter()
                .position(|s| !s.starts_with(|c: char| c.is_lowercase()))
                .unwrap_or(segs.len() - 1);
            self.inputs.insert(segs[keep..].join("::"));
        }
        syn::visit::visit_path(self, p);
    }
    fn visit_expr_match(&mut self, m: &'ast syn::ExprMatch) {
        let panicking_wild = m
            .arms
            .iter()
            .any(|a| matches!(a.pat, syn::Pat::Wild(_) | syn::Pat::Ident(_)) && panics(&a.body));
        let empty_arm = m.arms.iter().any(|a| {
            let b = a.body.to_token_stream().to_string();
            (b == "{ }" || b == "()") && binds_nothing(&a.pat)
        });
        if m.arms.len() == 2 && panicking_wild && empty_arm {
            self.panic_path = true;
            self.hit(
                Lint::ManualAssertMatches,
                m.span().start().line,
                lines(m).saturating_sub(1),
                String::new(),
            );
        }
        syn::visit::visit_expr_match(self, m);
    }
    fn visit_local(&mut self, l: &'ast syn::Local) {
        if let Some((_, div)) = l.init.as_ref().and_then(|i| i.diverge.as_ref())
            && panics(div)
            && binds_nothing(&l.pat)
        {
            self.panic_path = true;
            self.hit(
                Lint::ManualAssertMatches,
                l.span().start().line,
                lines(l).saturating_sub(1),
                String::new(),
            );
        }
        syn::visit::visit_local(self, l);
    }
}

/// A one-line `//` comment, short, opening with an action verb, giving no reason.
fn is_narrator(text: &str) -> bool {
    const VERBS: &[&str] = &[
        "create",
        "creating",
        "verify",
        "verifies",
        "check",
        "checks",
        "test",
        "testing",
        "set",
        "setup",
        "add",
        "call",
        "deploy",
        "mint",
        "now",
        "try",
        "get",
        "initialize",
        "init",
        "approve",
        "transfer",
        "should",
        "assert",
        "ensure",
        "make",
        "build",
        "send",
        "fund",
        "give",
        "register",
        "execute",
        "run",
        "read",
        "write",
        "store",
        "fetch",
        "wait",
        "start",
        "spawn",
        "insert",
        "update",
        "remove",
        "configure",
        "prepare",
        "generate",
        "sign",
        "encode",
        "decode",
        "submit",
        "expect",
        "confirm",
        "advance",
        "commit",
    ];
    const REASONS: &[&str] = &[
        "because",
        "so that",
        "so the",
        "since",
        "otherwise",
        "why",
        "note",
        "todo",
        "fixme",
        "safety",
        "in order",
        "instead",
        "unlike",
        "regression",
        "workaround",
        "must",
        "cannot",
        "can't",
        "would",
        "which",
        "tip-",
        "eip",
        "issue",
        "#",
    ];
    let lower = text.to_lowercase();
    let first = lower
        .trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | ')' | ':' | ' '))
        .trim_start_matches("step ")
        .trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | ')' | ':' | ' '))
        .split(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap_or_default();
    text.split_whitespace().count() <= 10
        && VERBS.contains(&first)
        && !REASONS.iter().any(|r| lower.contains(r))
}

fn comment_text(line: &str) -> Option<&str> {
    let t = line.trim();
    (t.starts_with("//") && !t.starts_with("///") && !t.starts_with("//!")).then(|| t[2..].trim())
}

fn is_divider(text: &str) -> bool {
    let t = text.trim_matches(|c: char| c == '-' || c == '=' || c == ' ' || c == '*');
    text.len() >= 3
        && (text.starts_with("---")
            || text.starts_with("===")
            || (text.starts_with("-- ") && text.ends_with("--") && !t.is_empty()))
}

/// One hit per file with decorative divider comments in its test code.
fn divider_hits(file: &Path, src: &[&str], code: &TestCode, hits: &mut Vec<Hit>) {
    let at: Vec<usize> = (1..=src.len())
        .filter(|&l| code.contains(l) && comment_text(src[l - 1]).is_some_and(is_divider))
        .collect();
    if let Some(&first) = at.first() {
        hits.push(Hit {
            saved_ms: None,
            lint: Lint::DividerComment,
            file: file.into(),
            line: first,
            item: "divider comments".into(),
            saved_loc: at.len(),
            detail: format!("{} lines", at.len()),
        });
    }
}

struct FileScan<'a> {
    file: &'a Path,
    krate: &'a str,
    src_lines: &'a [&'a str],
    /// `None` outside a crate's `src/`, where the harness name is not derivable.
    mod_path: Option<Vec<String>>,
    in_test: bool,
    test_ranges: Vec<RangeInclusive<usize>>,
    tests: Vec<TestFn>,
    envelopes: Vec<EnvelopeUse>,
    support: Vec<Support>,
    /// (self type, lines, identifier counts) of impl blocks in test code, attached to doubles.
    impls: Vec<(String, usize, HashMap<String, usize>)>,
    /// Paths (relative to the root) that test code `include_str!`s or `include_bytes!`s.
    includes: Vec<PathBuf>,
    test_only_items_loc: usize,
    goldens: usize,
    index: &'a Index,
    hits: &'a mut Vec<Hit>,
}

impl FileScan<'_> {
    fn hit(&mut self, lint: Lint, line: usize, item: &str, saved_loc: usize, detail: String) {
        self.hits.push(Hit {
            saved_ms: None,
            lint,
            file: self.file.into(),
            line,
            item: item.into(),
            saved_loc,
            detail,
        });
    }
}

impl<'ast> Visit<'ast> for FileScan<'_> {
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        let was = self.in_test;
        if is_cfg_test(&m.attrs) && !was {
            self.in_test = true;
            let sp = m.span();
            self.test_ranges.push(sp.start().line..=sp.end().line);
        }
        if let Some(p) = &mut self.mod_path {
            p.push(m.ident.to_string());
        }
        syn::visit::visit_item_mod(self, m);
        if let Some(p) = &mut self.mod_path {
            p.pop();
        }
        self.in_test = was;
    }
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        if !f.attrs.iter().any(is_test_attr) {
            let was = self.gate(&f.attrs, f);
            if self.in_test && !intentionally_unused(&f.attrs, &f.sig.ident) {
                self.support.push(Support {
                    kind: SupportKind::Helper,
                    name: f.sig.ident.to_string(),
                    file: self.file.into(),
                    krate: self.krate.into(),
                    line: f.span().start().line,
                    loc: lines(f),
                    idents: ident_counts(f.to_token_stream()),
                });
            }
            syn::visit::visit_item_fn(self, f);
            self.in_test = was;
            return;
        }
        let should_panic = f.attrs.iter().any(|a| attr_path(a) == "should_panic");
        let unreasoned_ignore = f
            .attrs
            .iter()
            .any(|a| attr_path(a) == "ignore" && matches!(a.meta, syn::Meta::Path(_)));
        let mut sig_inputs = BTreeSet::new();
        for a in f
            .attrs
            .iter()
            .filter(|a| matches!(attr_path(a).as_str(), "case" | "values" | "test_case"))
        {
            token_inputs(a.meta.to_token_stream(), &mut sig_inputs);
        }
        for arg in &f.sig.inputs {
            token_inputs(arg.to_token_stream(), &mut sig_inputs);
        }
        self.record_test(
            f.sig.ident.to_string(),
            f.span().start().line..=f.span().end().line,
            &f.block,
            TestAttrs {
                should_panic,
                unreasoned_ignore,
                sig_inputs,
            },
        );
    }
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        let was = self.gate(&i.attrs, i);
        self.double(&i.ident, i);
        syn::visit::visit_item_struct(self, i);
        self.in_test = was;
    }
    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        let was = self.gate(&i.attrs, i);
        self.double(&i.ident, i);
        syn::visit::visit_item_enum(self, i);
        self.in_test = was;
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let was = self.gate(&i.attrs, i);
        if self.in_test
            && let syn::Type::Path(p) = &*i.self_ty
            && let Some(seg) = p.path.segments.last()
        {
            self.impls.push((
                seg.ident.to_string(),
                lines(i),
                ident_counts(i.to_token_stream()),
            ));
        }
        syn::visit::visit_item_impl(self, i);
        self.in_test = was;
    }
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        self.macro_tests(m.tokens.clone());
        if self.in_test && matches!(macro_name(m).as_str(), "include_str" | "include_bytes") {
            self.include(m.tokens.clone());
        }
    }
}

/// Identifier occurrences in `ts`.
fn ident_counts(ts: TokenStream) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    let mut stack = vec![ts];
    while let Some(ts) = stack.pop() {
        for tt in ts {
            match tt {
                TokenTree::Ident(i) => *counts.entry(i.to_string()).or_default() += 1,
                TokenTree::Group(g) => stack.push(g.stream()),
                _ => {}
            }
        }
    }
    counts
}

/// `#[allow(dead_code)]`/`#[expect(dead_code)]` or a `_` name: unused on purpose, like a
/// compile-time `Send` check.
fn intentionally_unused(attrs: &[syn::Attribute], ident: &syn::Ident) -> bool {
    ident.to_string().starts_with('_')
        || attrs.iter().any(|a| {
            matches!(attr_path(a).as_str(), "allow" | "expect")
                && a.meta.to_token_stream().to_string().contains("dead_code")
        })
}

/// `Fake*`, `Mock*`, `Stub*`, `Dummy*`, `Spy*`, `Noop*` (or exactly one of those words).
fn is_double_name(name: &str) -> bool {
    ["Fake", "Mock", "Stub", "Dummy", "Spy", "Noop"]
        .iter()
        .any(|p| {
            name.strip_prefix(p).is_some_and(|rest| {
                rest.is_empty()
                    || rest.starts_with(|c: char| c.is_ascii_uppercase() || c.is_ascii_digit())
            })
        })
}

impl FileScan<'_> {
    /// Enters a `#[cfg(test)]` item outside test code: counts it as test code and returns the
    /// previous `in_test` for the caller to restore.
    fn gate(&mut self, attrs: &[syn::Attribute], item: &impl Spanned) -> bool {
        let was = self.in_test;
        if !was && is_cfg_test(attrs) {
            let sp = item.span();
            self.test_ranges.push(sp.start().line..=sp.end().line);
            self.test_only_items_loc += lines(item);
            self.in_test = true;
        }
        was
    }

    fn double(&mut self, ident: &syn::Ident, item: &(impl Spanned + ToTokens)) {
        let name = ident.to_string();
        if self.in_test && is_double_name(&name) {
            self.support.push(Support {
                kind: SupportKind::Double,
                name,
                file: self.file.into(),
                krate: self.krate.into(),
                line: item.span().start().line,
                loc: lines(item),
                idents: ident_counts(item.to_token_stream()),
            });
        }
    }

    /// Adds each impl block of a double's type in this file to the double.
    fn attach_impls(&mut self) {
        for (ty, loc, idents) in std::mem::take(&mut self.impls) {
            if let Some(d) = self
                .support
                .iter_mut()
                .find(|s| s.kind == SupportKind::Double && s.name == ty)
            {
                d.loc += loc;
                for (k, v) in idents {
                    *d.idents.entry(k).or_default() += v;
                }
            }
        }
    }

    /// Records the file an `include_str!`/`include_bytes!` reads: a literal relative to this file,
    /// or `concat!(env!("CARGO_MANIFEST_DIR"), "/rel")` relative to the crate.
    fn include(&mut self, ts: TokenStream) {
        let text = ts.to_string();
        let lits: Vec<String> = ts
            .into_iter()
            .flat_map(|tt| match tt {
                TokenTree::Group(g) => g.stream().into_iter().collect::<Vec<_>>(),
                other => vec![other],
            })
            .filter_map(|tt| match tt {
                TokenTree::Literal(l) => syn::parse_str::<syn::LitStr>(&l.to_string())
                    .ok()
                    .map(|s| s.value()),
                _ => None,
            })
            .filter(|v| v != "CARGO_MANIFEST_DIR")
            .collect();
        let Some(path) = lits.last() else { return };
        let base = if text.contains("CARGO_MANIFEST_DIR") {
            PathBuf::from(if self.krate == "." { "" } else { self.krate })
        } else {
            self.file.parent().unwrap_or(Path::new("")).to_path_buf()
        };
        let joined = base.join(path.trim_start_matches('/'));
        // Lexically drop `..` so the path matches the walk's relative paths.
        let mut norm = PathBuf::new();
        for c in joined.components() {
            match c {
                std::path::Component::ParentDir => {
                    norm.pop();
                }
                std::path::Component::CurDir => {}
                other => norm.push(other),
            }
        }
        self.includes.push(norm);
    }
    /// Records `#[test] fn name(..) { .. }` items inside a macro body, as `proptest!` writes them.
    fn macro_tests(&mut self, ts: TokenStream) {
        let tts: Vec<TokenTree> = ts.into_iter().collect();
        let mut i = 0;
        while i < tts.len() {
            if let (TokenTree::Punct(p), Some(TokenTree::Group(g))) = (&tts[i], tts.get(i + 1))
                && p.as_char() == '#'
                && g.delimiter() == Delimiter::Bracket
                && g.stream().to_string() == "test"
            {
                let fn_at = (i + 2..tts.len())
                    .find(|&j| matches!(&tts[j], TokenTree::Ident(id) if id == "fn"));
                let name = fn_at.and_then(|j| match tts.get(j + 1) {
                    Some(TokenTree::Ident(id)) => Some((j, id.to_string())),
                    _ => None,
                });
                if let Some((j, name)) = name {
                    let body = (j + 2..tts.len())
                        .find(|&k| matches!(&tts[k], TokenTree::Group(g) if g.delimiter() == Delimiter::Brace));
                    if let Some(k) = body {
                        let end = tts[k].span().end().line;
                        if let Ok(block) = syn::parse2::<syn::Block>(tts[k].clone().into()) {
                            // Parameters like `s in arb_short_string()` name the generated inputs.
                            let mut sig_inputs = BTreeSet::new();
                            for t in &tts[j + 2..k] {
                                param_inputs(t.clone(), &mut sig_inputs);
                            }
                            self.record_test(
                                name,
                                p.span().start().line..=end,
                                &block,
                                TestAttrs {
                                    should_panic: false,
                                    unreasoned_ignore: false,
                                    sig_inputs,
                                },
                            );
                        }
                        i = k + 1;
                        continue;
                    }
                }
            }
            if let TokenTree::Group(g) = &tts[i] {
                self.macro_tests(g.stream());
            }
            i += 1;
        }
    }

    /// `sig_inputs`: inputs outside the body, from proptest parameters or `#[case(..)]`-style attributes.
    fn record_test(
        &mut self,
        name: String,
        span: RangeInclusive<usize>,
        block: &syn::Block,
        attrs: TestAttrs,
    ) {
        let TestAttrs {
            should_panic,
            unreasoned_ignore,
            sig_inputs,
        } = attrs;
        let (line, loc) = (*span.start(), span.end() - span.start() + 1);
        if !self.in_test {
            self.test_ranges.push(span.clone());
        }

        let mut body = BodyScan {
            file: self.file,
            test: &name,
            asserts: BTreeSet::new(),
            inputs: sig_inputs,
            panic_path: false,
            repeated: None,
            sites: vec![],
            helpers: &self.index.asserting,
            hits: self.hits,
        };
        body.visit_block(block);
        let BodyScan {
            asserts,
            inputs,
            panic_path,
            repeated,
            sites,
            ..
        } = body;
        let effective = match envelope(block) {
            Some((env, inner)) => {
                self.envelopes.push(EnvelopeUse {
                    callee: env.callee,
                    file: self.file.into(),
                    line,
                    lines: env.lines,
                });
                inner
            }
            None => &block.stmts[..],
        };
        if let Some(cond) = silent_skip(effective) {
            self.hit(
                Lint::SilentSkip,
                line,
                &name,
                0,
                format!("returns early when `{cond}`: passes without testing"),
            );
        } else if unreasoned_ignore {
            self.hit(
                Lint::SilentSkip,
                line,
                &name,
                0,
                "`#[ignore]` without a reason: nobody knows when to run it".into(),
            );
        }
        if let Some(why) = trivial_reason(effective, asserts.len(), self.index) {
            self.hit(Lint::TestsTrivialCode, line, &name, loc, why);
        }
        if asserts.is_empty() && !panic_path && !should_panic {
            self.hit(Lint::NoAssertionTest, line, &name, 0, String::new());
        }
        if let Some((n, saved)) = repeated {
            self.hit(
                Lint::CopyPasteAssertBlock,
                line,
                &name,
                saved,
                format!("block repeated x{n}"),
            );
        }
        if let Some(marker) = golden_marker(block, &self.index.golden) {
            self.goldens += 1;
            self.hit(
                Lint::CharacterizationGolden,
                line,
                &name,
                loc,
                format!("compares against a stored snapshot via {marker}: it detects change, so keep it only for output that is a contract"),
            );
        }

        let narrated: Vec<usize> = span
            .clone()
            .filter(|&l| {
                let comment_at = |l: usize| {
                    l.checked_sub(1)
                        .and_then(|i| self.src_lines.get(i))
                        .and_then(|s| comment_text(s))
                };
                comment_at(l).is_some_and(|t| {
                    is_narrator(t) && comment_at(l - 1).is_none() && comment_at(l + 1).is_none()
                })
            })
            .collect();
        if let Some(&first) = narrated.first() {
            let text = comment_text(self.src_lines[first - 1])
                .unwrap_or_default()
                .to_owned();
            self.hit(
                Lint::NarratorComment,
                first,
                &name,
                narrated.len(),
                format!("{} lines, e.g. `{text}`", narrated.len()),
            );
        }

        let stmts = effective
            .iter()
            .map(|s| (literal_blind(s.to_token_stream()), code_lines(s)))
            .collect();
        let (mut loose, mut exact) = (String::new(), vec![]);
        flatten(block.to_token_stream(), &mut loose, &mut exact);
        let harness_name = self.mod_path.as_ref().map(|p| {
            p.iter()
                .map(String::as_str)
                .chain([name.as_str()])
                .collect::<Vec<_>>()
                .join("::")
        });
        self.tests.push(TestFn {
            file: self.file.into(),
            krate: self.krate.into(),
            harness_name,
            name,
            line,
            loc,
            should_panic,
            assert_sites: sites,
            loose,
            exact,
            stmts,
            asserts,
            inputs,
        });
    }
}

/// One hit per (crate, callee) wrapping at least `MIN_ENVELOPE_TESTS` tests.
fn envelope_hits(envelopes: BTreeMap<(String, String), Vec<EnvelopeUse>>, hits: &mut Vec<Hit>) {
    for ((krate, callee), users) in envelopes {
        if users.len() < MIN_ENVELOPE_TESTS {
            continue;
        }
        let saved: usize = users.iter().map(|u| u.lines).sum();
        let EnvelopeUse { file, line, .. } = &users[0];
        let krate = if krate.is_empty() { "." } else { &krate };
        hits.push(Hit {
            saved_ms: None,
            lint: Lint::TestEnvelope,
            file: file.clone(),
            line: *line,
            item: format!("{callee} in {krate}"),
            saved_loc: saved,
            detail: format!(
                "{} tests wrap their body in `{callee}(.., || {{ .. }})`; a test attribute drops the wrapper lines and one indent level",
                users.len()
            ),
        });
    }
}

/// Lines saved by turning `cluster` into one body plus a row per test. A row costs one line plus
/// the lines of its own differing literals; the body loses those lines; the loop adds two.
fn table_saving(cluster: &[&TestFn], holes: &[bool]) -> usize {
    let hole_lines = |t: &TestFn| -> usize {
        t.exact
            .iter()
            .zip(holes)
            .filter(|(_, h)| **h)
            .map(|(tok, _)| tok.matches('\n').count())
            .sum()
    };
    let before: usize = cluster.iter().map(|t| t.loc).sum();
    let body = cluster[0].loc.saturating_sub(hole_lines(cluster[0]));
    let rows: usize = cluster.iter().map(|t| 1 + hole_lines(t)).sum();
    before.saturating_sub(body + rows + 2)
}

/// Clusters same-file near-clone tests (see `TestFn::near_clone_of`).
fn clone_hits(tests: &[TestFn], hits: &mut Vec<Hit>) {
    let mut by_shape: HashMap<(&Path, &str), Vec<&TestFn>> = HashMap::new();
    for t in tests.iter().filter(|t| t.loc >= 4) {
        by_shape.entry((&t.file, &t.loose)).or_default().push(t);
    }
    for mut group in by_shape.into_values() {
        while let Some(seed) = group.pop() {
            let mut cluster = vec![seed];
            let mut holes = vec![false; seed.exact.len()];
            group.retain(|t| {
                let mut h = holes.clone();
                for (i, (a, b)) in seed.exact.iter().zip(&t.exact).enumerate() {
                    h[i] |= a != b;
                }
                let n = h.iter().filter(|x| **x).count();
                let fits = n <= 8 && n * 10 <= seed.exact.len();
                if fits {
                    holes = h;
                    cluster.push(t);
                }
                !fits
            });
            if cluster.len() < 2 {
                continue;
            }
            cluster.sort_by_key(|t| t.line);
            let n_holes = holes.iter().filter(|x| **x).count();
            hits.push(Hit {
                saved_ms: None,
                lint: Lint::ParametrizableTests,
                file: seed.file.clone(),
                line: cluster[0].line,
                item: cluster
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                saved_loc: table_saving(&cluster, &holes),
                detail: format!(
                    "{} tests x {} LOC, {n_holes} differing tokens",
                    cluster.len(),
                    seed.loc
                ),
            });
        }
    }
}

/// Per file, the statement prefix (literals ignored) shared by >= 3 tests that saves the most lines as a fixture.
fn fixture_hits(tests: &[TestFn], hits: &mut Vec<Hit>) {
    let mut by_file: HashMap<&Path, Vec<&TestFn>> = HashMap::new();
    for t in tests {
        by_file.entry(&t.file).or_default().push(t);
    }
    for (file, ts) in by_file {
        let mut prefixes: HashMap<Vec<&str>, Vec<&TestFn>> = HashMap::new();
        for t in &ts {
            for k in 2..t.stmts.len() {
                prefixes
                    .entry(t.stmts[..k].iter().map(|s| s.0.as_str()).collect())
                    .or_default()
                    .push(t);
            }
        }
        let best = prefixes
            .into_iter()
            .filter(|(_, users)| users.len() >= 3)
            .map(|(prefix, users)| {
                let ploc: usize = users[0].stmts[..prefix.len()].iter().map(|s| s.1).sum();
                // Each user keeps one call line; the fixture adds a header and a closing brace.
                let saved = ((users.len() - 1) * ploc).saturating_sub(users.len() + 2);
                (saved, users.len(), prefix.join(" "), users)
            })
            .max_by(|a, b| (a.0, a.1, &b.2).cmp(&(b.0, b.1, &a.2)));
        if let Some((saved, n_users, setup, users)) = best.filter(|b| b.0 >= 6) {
            let first = users
                .iter()
                .min_by_key(|t| t.line)
                .expect("filtered to >= 3 users");
            hits.push(Hit {
                saved_ms: None,
                lint: Lint::MissingFixture,
                file: file.into(),
                line: first.line,
                item: setup.chars().take(160).collect(),
                saved_loc: saved,
                detail: format!("{n_users} tests open with this setup, first {}", first.name),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests `FileScan` records from `src`, as if it were `src/lib.rs`.
    fn tests_of(src: &str) -> Vec<TestFn> {
        let ast = syn::parse_file(src).expect("fixture parses");
        let lines: Vec<&str> = src.lines().collect();
        let mut hits = vec![];
        let index = Index {
            asserting: ["execute_case".to_owned()].into(),
            golden: [("check_golden".to_owned(), "env `UPDATE_GOLDEN`".to_owned())].into(),
            ..Index::default()
        };
        let mut fs = FileScan {
            file: Path::new("src/lib.rs"),
            krate: ".",
            src_lines: &lines,
            mod_path: Some(vec![]),
            in_test: true,
            test_ranges: vec![],
            tests: vec![],
            envelopes: vec![],
            support: vec![],
            impls: vec![],
            includes: vec![],
            test_only_items_loc: 0,
            goldens: 0,
            index: &index,
            hits: &mut hits,
        };
        fs.visit_file(&ast);
        fs.tests
    }

    #[test]
    fn narrator_needs_a_leading_verb_and_no_reason() {
        assert!(is_narrator("Create the token"));
        assert!(is_narrator("Step 2: verify the balance"));
        assert!(!is_narrator("Create it first so the cache is warm"));
        assert!(!is_narrator("Edge case: zero amount"));
        assert!(!is_narrator(
            "Create a token with a very long name and symbol and a lot of words"
        ));
    }

    #[test]
    fn divider_is_a_rule_or_a_dashed_title() {
        assert!(is_divider("--- helpers ---"));
        assert!(is_divider("-- BASIC TESTS ------"));
        assert!(!is_divider("-- not a divider"));
    }

    #[test]
    fn err_helper_fits_one_line_only_when_short() {
        let short = [
            ("parse(\"x\")".to_owned(), 1),
            ("ParseError::NotANumber(_)".to_owned(), 1),
        ];
        assert_eq!(err_helper_lines(8, &short), 1);
        let long_call =
            "token.burn_at(burner,ITIP20::burnAtCall{from:holder,amount:U256::ONE})".to_owned();
        let long = [
            (long_call, 7),
            (
                "Err(TIP20Error::insufficient_balance(U256::ZERO).into())".to_owned(),
                4,
            ),
        ];
        assert_eq!(err_helper_lines(16, &long), 2 + 7 + 1);
    }

    #[test]
    fn envelope_counts_call_closing_ok_and_argument_lets() {
        let block: syn::Block = syn::parse_quote!({
            let mut storage = Storage::new();
            let admin = random();
            StorageCtx::enter(&mut storage, || {
                admin.call();
                Ok(())
            })
        });
        let (env, inner) = envelope(&block).expect("tail call takes a closure");
        assert_eq!(env.callee, "StorageCtx::enter");
        // `let mut storage` (1) + call line + closing line + `Ok(())`; `admin` is not a call argument.
        assert_eq!(env.lines, 4);
        assert_eq!(inner.len(), 1);
        assert!(
            envelope(&syn::parse_quote!({
                run(1);
            }))
            .is_none()
        );
    }

    #[test]
    fn inputs_keep_type_qualifiers_and_drop_module_prefixes() {
        let t = tests_of(
            r#"
            #[test] fn a() { check(orderbook::MAX_PRICE, Address::ZERO, vec![0u8; 64]); }
            #[test] fn b() { check(MAX_PRICE, B256::ZERO, vec![]); }
            "#,
        );
        assert_eq!(
            t[0].inputs,
            ["0u8", "64", "Address::ZERO", "MAX_PRICE"]
                .map(String::from)
                .into()
        );
        assert_eq!(
            t[1].inputs,
            ["B256::ZERO", "MAX_PRICE"].map(String::from).into()
        );
        assert!(!t[0].inputs_repeated_in(&t[1]));
    }

    #[test]
    fn proptest_parameters_are_inputs_and_messages_are_not_asserts() {
        let t = tests_of(
            r#"
            proptest! {
                #[test] fn short(s in arb_short()) { prop_assert_eq!(s.len(), 1, "msg {}", 1); }
                #[test] fn long(s in arb_long()) { prop_assert_eq!(s.len(), 1, "other msg"); }
            }
            "#,
        );
        assert_eq!(t[0].asserts, t[1].asserts);
        assert!(t[0].asserts_repeated_in(&t[1]));
        assert!(t[0].inputs.contains("arb_short"));
        assert!(!t[0].inputs_repeated_in(&t[1]));
    }

    #[test]
    fn table_saving_keeps_differing_multiline_literals_in_rows() {
        let t = tests_of(
            "#[test] fn a() {\n    run(r#\"\nline 1\nline 2\n\"#);\n    done();\n}\n\
             #[test] fn b() {\n    run(r#\"\nline 3\nline 4\n\"#);\n    done();\n}\n",
        );
        let holes: Vec<bool> = t[0]
            .exact
            .iter()
            .zip(&t[1].exact)
            .map(|(a, b)| a != b)
            .collect();
        // 7 + 7 lines before; body 7 - 3 literal lines, two rows of 1 + 3, loop 2: 14 - 14.
        assert_eq!(table_saving(&[&t[0], &t[1]], &holes), 0);
        let same = vec![false; holes.len()];
        // Identical bodies: 14 - (7 + 2 + 2).
        assert_eq!(table_saving(&[&t[0], &t[1]], &same), 3);
    }

    #[test]
    fn golden_literals_are_update_env_vars_or_snapshot_paths() {
        assert_eq!(
            golden_literal("TEMPO_DST_UPDATE_GOLDEN"),
            Some("env `TEMPO_DST_UPDATE_GOLDEN`".into())
        );
        assert_eq!(
            golden_literal("/testdata/expected.ndjson"),
            Some("`/testdata/expected.ndjson`".into())
        );
        assert_eq!(
            golden_literal("snapshots/a.snap"),
            Some("`snapshots/a.snap`".into())
        );
        assert_eq!(golden_literal("UPDATE"), None);
        assert_eq!(golden_literal("expected a golden value"), None);
        assert_eq!(golden_literal("RUST_LOG"), None);
        assert_eq!(golden_literal("FORGE_SNAPSHOT_CHECK"), None);
        assert_eq!(golden_literal("db/new.snapshot"), None);
        assert_eq!(
            golden_literal("INSTA_UPDATE"),
            Some("env `INSTA_UPDATE`".into())
        );
    }

    #[test]
    fn doubles_are_named_by_a_prefix_word() {
        assert!(is_double_name("FakeChain"));
        assert!(is_double_name("Mock"));
        assert!(is_double_name("Stub2"));
        assert!(!is_double_name("Faker"));
        assert!(!is_double_name("TestEnv"));
    }

    #[test]
    fn includes_resolve_relative_to_the_file_or_the_crate() {
        let src = r#"
            #[cfg(test)]
            mod t {
                const A: &str = include_str!("../testdata/a.json");
                const B: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/b.json"));
            }
        "#;
        let ast = syn::parse_file(src).expect("parses");
        let lines: Vec<&str> = src.lines().collect();
        let index = Index::default();
        let mut hits = vec![];
        let mut fs = FileScan {
            file: Path::new("crates/x/src/a.rs"),
            krate: "crates/x",
            src_lines: &lines,
            mod_path: None,
            in_test: false,
            test_ranges: vec![],
            tests: vec![],
            envelopes: vec![],
            support: vec![],
            impls: vec![],
            includes: vec![],
            test_only_items_loc: 0,
            goldens: 0,
            index: &index,
            hits: &mut hits,
        };
        fs.visit_file(&ast);
        assert_eq!(
            fs.includes,
            [
                PathBuf::from("crates/x/testdata/a.json"),
                PathBuf::from("crates/x/testdata/b.json")
            ]
        );
    }

    #[test]
    fn manifest_lists_normal_and_build_dependencies_by_package() {
        let dir = std::env::temp_dir().join(format!("testslop-manifest-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("c")).unwrap();
        std::fs::write(
            dir.join("c/Cargo.toml"),
            r#"
            [package]
            name = "c"
            [dependencies]
            a = "1"
            b = { package = "real-b", version = "1" }
            [dev-dependencies]
            dev-only = "1"
            [build-dependencies]
            builder = "1"
            [target.'cfg(unix)'.dependencies]
            unix-only = "1"
            "#,
        )
        .unwrap();
        let (name, mut deps) = manifest(&dir, "c");
        deps.sort();
        assert_eq!(name.as_deref(), Some("c"));
        assert_eq!(deps, ["a", "builder", "real-b", "unix-only"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dead_code_allowances_and_underscore_names_are_intentionally_unused() {
        let f: syn::ItemFn = syn::parse_quote! { #[expect(dead_code)] fn send_check() {} };
        assert!(intentionally_unused(&f.attrs, &f.sig.ident));
        let f: syn::ItemFn = syn::parse_quote! { fn _compile_check() {} };
        assert!(intentionally_unused(&f.attrs, &f.sig.ident));
        let f: syn::ItemFn = syn::parse_quote! { #[allow(clippy::foo)] fn helper() {} };
        assert!(!intentionally_unused(&f.attrs, &f.sig.ident));
    }

    #[test]
    fn silent_skip_needs_an_environment_check_and_a_return() {
        let stmts = |b: syn::Block| b.stmts;
        assert!(
            silent_skip(&stmts(syn::parse_quote!({
                if !z3_available() {
                    return;
                }
                run();
            })))
            .is_some()
        );
        assert!(
            silent_skip(&stmts(syn::parse_quote!({
                let Ok(p) = std::env::var("BIN") else {
                    return Ok(());
                };
            })))
            .is_some()
        );
        assert!(
            silent_skip(&stmts(syn::parse_quote!({
                if n == 0 {
                    return;
                }
                run();
            })))
            .is_none()
        );
        assert!(
            silent_skip(&stmts(syn::parse_quote!({
                if std::env::var("X").is_ok() {
                    run();
                }
            })))
            .is_none()
        );
        let ci_only = stmts(syn::parse_quote!({
            if !ci_info::is_ci() {
                return;
            }
            run();
        }));
        assert!(silent_skip(&ci_only).is_none());
        let skipped_in_ci = stmts(syn::parse_quote!({
            if ci_info::is_ci() {
                return;
            }
            run();
        }));
        assert!(silent_skip(&skipped_in_ci).is_some());
    }

    #[test]
    fn trivial_tests_read_back_literals_or_use_only_derived_impls() {
        let mut index = Index::default();
        index.derives.insert(
            "Config".into(),
            ["Clone", "Debug", "PartialEq"].map(String::from).into(),
        );
        index.derives.insert(
            "Celsius".into(),
            ["Clone", "Debug"].map(String::from).into(),
        );
        index
            .manual_impls
            .insert("Celsius".into(), ["PartialEq".to_owned()].into());
        let stmts = |b: syn::Block| b.stmts;
        let readback = stmts(syn::parse_quote!({
            let p = P { x: 1, y: 2 };
            assert_eq!(p.x, 1);
        }));
        assert_eq!(
            trivial_reason(&readback, 1, &index).as_deref(),
            Some("only reads back fields it set from the same literals")
        );
        let computed = stmts(syn::parse_quote!({
            let p = P { x: 1 };
            assert_eq!(p.x, 1);
            assert_eq!(p.area(), 0);
        }));
        assert_eq!(trivial_reason(&computed, 2, &index), None);
        let derived = stmts(syn::parse_quote!({
            let c = Config { n: 1 };
            assert_eq!(c.clone(), c);
        }));
        assert_eq!(
            trivial_reason(&derived, 1, &index).as_deref(),
            Some("only exercises derived Clone, Debug, PartialEq of `Config`")
        );
        let manual = stmts(syn::parse_quote!({
            assert_eq!(Celsius(1), Celsius(1).clone());
        }));
        assert_eq!(trivial_reason(&manual, 1, &index), None);
        let calls_code = stmts(syn::parse_quote!({
            let c = Config { n: 1 };
            assert_eq!(c.scaled(), c);
        }));
        assert_eq!(trivial_reason(&calls_code, 1, &index), None);
        index.derives.insert(
            "Wire".into(),
            ["Debug", "PartialEq", "Serialize", "Deserialize"]
                .map(String::from)
                .into(),
        );
        index.serde_configured.insert("Wire".into());
        let wire = stmts(syn::parse_quote!({
            let w = Wire { a: 1 };
            assert_eq!(
                serde_json::from_str::<Wire>(&serde_json::to_string(&w).unwrap()).unwrap(),
                w
            );
        }));
        assert_eq!(
            trivial_reason(&wire, 1, &index),
            None,
            "#[serde(..)] declares a format"
        );
    }

    #[test]
    fn serde_attributes_count_behind_cfg_attr() {
        let s: syn::ItemStruct = syn::parse_quote! {
            #[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
            #[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
            #[serde(default)]
            struct S;
        };
        assert_eq!(
            s.attrs.iter().map(is_serde_attr).collect::<Vec<_>>(),
            [true, false, true]
        );
    }

    #[test]
    fn project_test_attributes_mark_tests() {
        let t = tests_of(
            r#"
            #[forgetest_init] fn a(prj: P, cmd: C) { cmd.assert_success(); }
            #[tokio::test] async fn b() { assert!(true); }
            #[inline] fn not_a_test() {}
            "#,
        );
        assert_eq!(
            t.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[test]
    fn helper_calls_count_as_asserts() {
        let t = tests_of(
            r#"
            #[test] fn a() { assert_rejected(&fixture.bad); }
            #[test] fn b() { let x = run().expect("ran"); }
            #[test] fn c() { execute_case(case); }
            #[test] fn d() { proptest!(cfg, |(x in any::<u8>())| { prop_assert!(x < 2); }); }
            "#,
        );
        assert_eq!(
            t[0].asserts,
            ["assert_rejected(&fixture.bad)".to_owned()].into()
        );
        assert!(t[1].asserts.is_empty());
        assert_eq!(t[2].asserts, ["execute_case(case)".to_owned()].into());
        assert_eq!(t[3].asserts, ["prop_assert!(x<2)".to_owned()].into());
    }
}
