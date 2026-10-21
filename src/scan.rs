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
    /// `module::path::name` as the libtest harness lists it, for tests under `src/`.
    pub harness_name: Option<String>,
    pub name: String,
    pub line: usize,
    pub loc: usize,
    /// `#[should_panic]`: its expectation is a panic that coverage cannot see.
    pub should_panic: bool,
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

#[derive(Default)]
pub struct CrateSize {
    pub prod_loc: usize,
    pub test_loc: usize,
    pub tests: usize,
}

pub struct Scan {
    pub tests: Vec<TestFn>,
    pub hits: Vec<Hit>,
    /// Keyed by the crate directory relative to the root.
    pub crates: BTreeMap<String, CrateSize>,
    /// Keyed by path relative to the root.
    pub test_code: HashMap<PathBuf, TestCode>,
}

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

/// Names of non-test functions in test code whose bodies assert: calls to them are assertions.
fn asserting_helpers(root: &Path) -> HashSet<String> {
    struct Helpers {
        in_test: bool,
        names: HashSet<String>,
    }
    impl Helpers {
        fn check(&mut self, attrs: &[syn::Attribute], ident: &syn::Ident, body: &syn::Block) {
            if self.in_test && !attrs.iter().any(is_test_attr) && asserts_anything(body) {
                self.names.insert(ident.to_string());
            }
        }
    }
    impl<'a> Visit<'a> for Helpers {
        fn visit_item_mod(&mut self, m: &'a syn::ItemMod) {
            let was = self.in_test;
            self.in_test |= is_cfg_test(&m.attrs);
            syn::visit::visit_item_mod(self, m);
            self.in_test = was;
        }
        fn visit_item_fn(&mut self, f: &'a syn::ItemFn) {
            self.check(&f.attrs, &f.sig.ident, &f.block);
            syn::visit::visit_item_fn(self, f);
        }
        fn visit_impl_item_fn(&mut self, f: &'a syn::ImplItemFn) {
            self.check(&f.attrs, &f.sig.ident, &f.block);
            syn::visit::visit_impl_item_fn(self, f);
        }
    }
    let mut h = Helpers {
        in_test: false,
        names: HashSet::new(),
    };
    for (rel, _, ast) in rust_files(root) {
        h.in_test = is_test_file(&rel, &ast);
        h.visit_file(&ast);
    }
    h.names
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

/// Parses every `.rs` file under `root` (skipping `target` and `.git`); unparsable files are skipped.
pub fn scan(root: &Path) -> Scan {
    let helpers = asserting_helpers(root);
    let mut out = Scan {
        tests: vec![],
        hits: vec![],
        crates: BTreeMap::new(),
        test_code: HashMap::new(),
    };
    let mut envelopes: BTreeMap<(String, String), Vec<EnvelopeUse>> = BTreeMap::new();
    for (rel, src, ast) in rust_files(root) {
        let src_lines: Vec<&str> = src.lines().collect();
        let whole_file = is_test_file(&rel, &ast);
        let krate = crate_dir(&rel);
        let mut fs = FileScan {
            file: &rel,
            src_lines: &src_lines,
            mod_path: file_module_path(&rel),
            in_test: whole_file,
            test_ranges: vec![],
            tests: vec![],
            envelopes: vec![],
            helpers: &helpers,
            hits: &mut out.hits,
        };
        fs.visit_file(&ast);
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
        let size = out.crates.entry(krate).or_default();
        size.prod_loc += total - test_loc;
        size.test_loc += test_loc;
        size.tests += fs.tests.len();
        out.tests.extend(fs.tests);
        out.test_code.insert(rel, code);
    }
    envelope_hits(envelopes, &mut out.hits);
    clone_hits(&out.tests, &mut out.hits);
    fixture_hits(&out.tests, &mut out.hits);
    out
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

fn crate_dir(rel: &Path) -> String {
    let comps: Vec<_> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    let cut = comps
        .iter()
        .position(|c| c == "src" || c == "tests" || c == "benches");
    comps[..cut.unwrap_or(comps.len().saturating_sub(1))].join("/")
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

/// Walks one test body for assertions, inputs, panic paths and assertion-level smells.
struct BodyScan<'a> {
    file: &'a Path,
    test: &'a str,
    asserts: BTreeSet<String>,
    inputs: BTreeSet<String>,
    panic_path: bool,
    repeated: Option<(usize, usize)>,
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
    src_lines: &'a [&'a str],
    /// `None` outside a crate's `src/`, where the harness name is not derivable.
    mod_path: Option<Vec<String>>,
    in_test: bool,
    test_ranges: Vec<RangeInclusive<usize>>,
    tests: Vec<TestFn>,
    envelopes: Vec<EnvelopeUse>,
    /// See `asserting_helpers`.
    helpers: &'a HashSet<String>,
    hits: &'a mut Vec<Hit>,
}

impl FileScan<'_> {
    fn hit(&mut self, lint: Lint, line: usize, item: &str, saved_loc: usize, detail: String) {
        self.hits.push(Hit {
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
            return syn::visit::visit_item_fn(self, f);
        }
        let should_panic = f.attrs.iter().any(|a| attr_path(a) == "should_panic");
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
            should_panic,
            sig_inputs,
        );
    }
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        self.macro_tests(m.tokens.clone());
    }
}

impl FileScan<'_> {
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
                                false,
                                sig_inputs,
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
        should_panic: bool,
        sig_inputs: BTreeSet<String>,
    ) {
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
            helpers: self.helpers,
            hits: self.hits,
        };
        body.visit_block(block);
        let BodyScan {
            asserts,
            inputs,
            panic_path,
            repeated,
            ..
        } = body;
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
            harness_name,
            name,
            line,
            loc,
            should_panic,
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
        let helpers = ["execute_case".to_owned()].into();
        let mut fs = FileScan {
            file: Path::new("src/lib.rs"),
            src_lines: &lines,
            mod_path: Some(vec![]),
            in_test: true,
            test_ranges: vec![],
            tests: vec![],
            envelopes: vec![],
            helpers: &helpers,
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
