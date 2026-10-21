//! Static analysis of test functions: structural clones, shared setup, weak assertions.

use crate::hit::{Hit, Lint};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens;
use std::collections::{BTreeMap, HashMap};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::Visit;

pub struct TestFn {
    /// Path relative to the scanned root.
    pub file: PathBuf,
    /// `module::path::name` as the libtest harness lists it, for tests under `src/`.
    pub harness_name: Option<String>,
    pub name: String,
    pub line: usize,
    pub loc: usize,
    /// Body with literals and non-keyword identifiers replaced by `$`.
    loose: String,
    /// Flattened exact body tokens, aligned position by position with `loose`.
    exact: Vec<String>,
    /// Exact tokens and line count of each top-level statement.
    stmts: Vec<(String, usize)>,
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

/// Parses every `.rs` file under `root` (skipping `target` and `.git`); unparsable files are skipped.
pub fn scan(root: &Path) -> Scan {
    let mut out = Scan {
        tests: vec![],
        hits: vec![],
        crates: BTreeMap::new(),
        test_code: HashMap::new(),
    };
    for e in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.file_name() != "target" && e.file_name() != ".git")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
    {
        let rel = e
            .path()
            .strip_prefix(root)
            .unwrap_or(e.path())
            .to_path_buf();
        let Ok(src) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let Ok(ast) = syn::parse_file(&src) else {
            continue;
        };
        let total = src.lines().count();
        let whole_file = rel.components().any(|c| {
            let c = c.as_os_str().to_string_lossy();
            matches!(
                c.trim_end_matches(".rs"),
                "tests" | "test_util" | "test_utils"
            )
        }) || is_cfg_test(&ast.attrs);
        let mut fs = FileScan {
            file: &rel,
            mod_path: file_module_path(&rel),
            in_test: whole_file,
            test_ranges: vec![],
            tests: vec![],
            hits: &mut out.hits,
        };
        fs.visit_file(&ast);
        let test_loc = if whole_file {
            total
        } else {
            fs.test_ranges
                .iter()
                .map(|r| r.end() - r.start() + 1)
                .sum::<usize>()
                .min(total)
        };
        let size = out.crates.entry(crate_dir(&rel)).or_default();
        size.prod_loc += total - test_loc;
        size.test_loc += test_loc;
        size.tests += fs.tests.len();
        let code = if whole_file {
            TestCode::WholeFile
        } else {
            TestCode::Lines(fs.test_ranges)
        };
        out.tests.extend(fs.tests);
        out.test_code.insert(rel, code);
    }
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

fn is_test_attr(a: &syn::Attribute) -> bool {
    let p = a.path().to_token_stream().to_string().replace(' ', "");
    matches!(
        p.as_str(),
        "test" | "tokio::test" | "rstest" | "test_case" | "test_log::test"
    )
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

/// Walks one test body for assertions, panic paths and pattern-level smells.
struct BodyScan<'a> {
    file: &'a Path,
    test: &'a str,
    asserts: usize,
    panic_path: bool,
    hits: &'a mut Vec<Hit>,
}

impl BodyScan<'_> {
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
}

impl<'ast> Visit<'ast> for BodyScan<'_> {
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        let n = macro_name(m);
        let args = macro_args(m);
        if n.starts_with("assert") || n.starts_with("prop_assert") {
            self.asserts += 1;
        }
        if let (Some(args), "assert_eq" | "assert_ne") = (&args, n.as_str()) {
            let mut it = args.iter().map(|e| e.to_token_stream().to_string());
            if let (Some(a), Some(b)) = (it.next(), it.next())
                && a == b
            {
                self.hit(
                    Lint::TautologicalAssert,
                    m.span().start().line,
                    lines(m),
                    format!("{n}!({a}, {b})"),
                );
            }
        }
        self.panic_path |= matches!(n.as_str(), "panic" | "unreachable" | "bail");
        for e in args.iter().flatten() {
            self.visit_expr(e);
        }
    }
    fn visit_expr_method_call(&mut self, c: &'ast syn::ExprMethodCall) {
        self.panic_path |= matches!(
            c.method.to_string().as_str(),
            "unwrap" | "expect" | "unwrap_err" | "expect_err"
        );
        syn::visit::visit_expr_method_call(self, c);
    }
    fn visit_expr_try(&mut self, e: &'ast syn::ExprTry) {
        self.panic_path = true;
        syn::visit::visit_expr_try(self, e);
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

/// Base of `assert_eq!(base.field, ..)`, if the statement is one.
fn field_assert_base(s: &syn::Stmt) -> Option<String> {
    let m = match s {
        syn::Stmt::Macro(m) => &m.mac,
        syn::Stmt::Expr(syn::Expr::Macro(m), _) => &m.mac,
        _ => return None,
    };
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
        .map(|s| (loose_of(s.to_token_stream()), lines(s)))
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

struct FileScan<'a> {
    file: &'a Path,
    /// `None` outside a crate's `src/`, where the harness name is not derivable.
    mod_path: Option<Vec<String>>,
    in_test: bool,
    test_ranges: Vec<RangeInclusive<usize>>,
    tests: Vec<TestFn>,
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
        self.record_test(
            f.sig.ident.to_string(),
            f.span().start().line..=f.span().end().line,
            &f.block,
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
                    let body = (j + 2..tts.len()).find(|&k| matches!(&tts[k], TokenTree::Group(g) if g.delimiter() == Delimiter::Brace));
                    if let Some(k) = body {
                        let end = tts[k].span().end().line;
                        if let Ok(block) = syn::parse2::<syn::Block>(tts[k].clone().into()) {
                            self.record_test(name, p.span().start().line..=end, &block);
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

    fn record_test(&mut self, name: String, span: RangeInclusive<usize>, block: &syn::Block) {
        let (line, loc) = (*span.start(), span.end() - span.start() + 1);
        if !self.in_test {
            self.test_ranges.push(span);
        }

        let mut body = BodyScan {
            file: self.file,
            test: &name,
            asserts: 0,
            panic_path: false,
            hits: self.hits,
        };
        body.visit_block(block);
        if body.asserts == 0 && !body.panic_path {
            self.hit(
                Lint::NoAssertionTest,
                line,
                &name,
                0,
                "assertions, if any, live in a helper".into(),
            );
        }

        let sentinel = syn::Stmt::Expr(syn::parse_quote!(()), None);
        let mut run: Option<(String, usize, usize)> = None;
        for st in block.stmts.iter().chain([&sentinel]) {
            match (&mut run, field_assert_base(st)) {
                (Some((base, _, n)), Some(b)) if *base == b => *n += 1,
                (r, b) => {
                    if let Some((base, at, n)) = r.take().filter(|r| r.2 >= 3) {
                        self.hit(
                            Lint::FieldByFieldAssert,
                            at,
                            &name,
                            n - 1,
                            format!("{n} fields of `{base}`"),
                        );
                    }
                    *r = b.map(|b| (b, st.span().start().line, 1));
                }
            }
        }

        if let Some((n, saved)) = repeated_block(&block.stmts) {
            self.hit(
                Lint::CopyPasteAssertBlock,
                line,
                &name,
                saved,
                format!("block repeated x{n}"),
            );
        }

        let (mut loose, mut exact) = (String::new(), vec![]);
        flatten(block.to_token_stream(), &mut loose, &mut exact);
        let stmts = block
            .stmts
            .iter()
            .map(|s| (s.to_token_stream().to_string(), lines(s)))
            .collect();
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
            loose,
            exact,
            stmts,
        });
    }
}

/// Clusters same-file tests whose bodies match in shape and differ in at most 8 tokens (and <= 10%).
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
                saved_loc: ((cluster.len() - 1) * seed.loc).saturating_sub(cluster.len() + 2),
                detail: format!(
                    "{} tests x {} LOC, {n_holes} differing tokens",
                    cluster.len(),
                    seed.loc
                ),
            });
        }
    }
}

/// Per file, the exact statement prefix shared by >= 3 tests that saves the most lines as a fixture.
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
