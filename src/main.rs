//! Survey of test slop in a Rust workspace: structural clones, shared setup, weak tests.

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use syn::spanned::Spanned;
use syn::visit::Visit;

struct TestFn {
    file: PathBuf,
    name: String,
    line: usize,
    loc: usize,
    /// Body tokens with every literal replaced by a placeholder.
    shape: String,
    /// Body tokens with literals and non-keyword identifiers replaced.
    loose: String,
    /// Flattened exact body tokens, aligned with `loose` positions.
    exact: Vec<String>,
    /// Best intra-test repeated block: (repeats, LOC saved).
    repeated_block: Option<(usize, usize)>,
    /// Exact tokens of each top-level statement, with its line count.
    stmts: Vec<(String, usize)>,
    asserts: usize,
    has_panic_path: bool,
    calls: Vec<String>,
}

#[derive(Default)]
struct Findings {
    no_assertion: Vec<(PathBuf, usize, String)>,
    tautological: Vec<(PathBuf, usize, String)>,
    field_by_field: Vec<(PathBuf, usize, usize)>,
    manual_assert_matches: Vec<(PathBuf, usize)>,
}

fn is_test_attr(a: &syn::Attribute) -> bool {
    let p = a.path().to_token_stream().to_string().replace(' ', "");
    matches!(
        p.as_str(),
        "test" | "tokio::test" | "rstest" | "test_case" | "test_log::test" | "test_strategy::proptest"
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

fn shape(ts: TokenStream, out: &mut String) {
    for tt in ts {
        match tt {
            TokenTree::Literal(_) => out.push_str("$L "),
            TokenTree::Group(g) => {
                let (o, c) = match g.delimiter() {
                    Delimiter::Parenthesis => ("(", ")"),
                    Delimiter::Brace => ("{", "}"),
                    Delimiter::Bracket => ("[", "]"),
                    Delimiter::None => ("", ""),
                };
                out.push_str(o);
                shape(g.stream(), out);
                out.push_str(c);
                out.push(' ');
            }
            other => {
                out.push_str(&other.to_string());
                out.push(' ');
            }
        }
    }
}

const KEYWORDS: &[&str] = &[
    "let", "mut", "if", "else", "match", "for", "in", "while", "loop", "return", "fn", "move",
    "async", "await", "ref", "true", "false", "Some", "None", "Ok", "Err", "self", "Self", "as",
    "break", "continue", "unsafe", "impl", "dyn", "where", "const", "static", "struct", "enum",
];

/// Flattens tokens, emitting a loose shape (idents and literals as holes) and exact tokens.
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
                if KEYWORDS.contains(&s.as_str()) { loose.push_str(&s) } else { loose.push('$') }
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

/// Pattern binds no variable (so `let P = e else { panic!() }` is a plain `assert_matches!`).
fn binds_nothing(p: &syn::Pat) -> bool {
    struct B(bool);
    impl<'a> Visit<'a> for B {
        fn visit_pat_ident(&mut self, i: &'a syn::PatIdent) {
            if i.ident.to_string().starts_with(|c: char| c.is_lowercase() || c == '_') && i.ident != "_" {
                self.0 = true;
            }
            syn::visit::visit_pat_ident(self, i);
        }
    }
    let mut b = B(false);
    b.visit_pat(p);
    !b.0
}

fn lines(s: &impl Spanned) -> usize {
    let sp = s.span();
    sp.end().line - sp.start().line + 1
}

fn macro_name(m: &syn::Macro) -> String {
    m.path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default()
}

/// Scans one test body for assertions, panics, calls and pattern-level smells.
struct BodyScan<'a> {
    file: &'a Path,
    asserts: usize,
    panics: bool,
    calls: Vec<String>,
    findings: &'a mut Findings,
    test_name: &'a str,
}

impl<'ast> Visit<'ast> for BodyScan<'_> {
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        let n = macro_name(m);
        if n.starts_with("assert") || n.starts_with("prop_assert") || n == "expect_test" {
            self.asserts += 1;
            if n == "assert_eq" || n == "assert_ne" {
                if let Ok(args) = m.parse_body_with(
                    syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated,
                ) {
                    let v: Vec<_> = args.iter().collect();
                    if v.len() >= 2 {
                        let a = v[0].to_token_stream().to_string();
                        let b = v[1].to_token_stream().to_string();
                        if a == b {
                            self.findings.tautological.push((
                                self.file.into(),
                                m.span().start().line,
                                format!("{n}!({a}, {b})"),
                            ));
                        }
                    }
                }
            }
        }
        if matches!(n.as_str(), "panic" | "unreachable" | "bail") {
            self.panics = true;
        }
        // Descend into macro bodies that parse as expressions (vec!, format!, ...).
        if let Ok(args) = m.parse_body_with(
            syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated,
        ) {
            for e in &args {
                self.visit_expr(e);
            }
        }
    }
    fn visit_expr_method_call(&mut self, c: &'ast syn::ExprMethodCall) {
        let m = c.method.to_string();
        if matches!(
            m.as_str(),
            "unwrap" | "expect" | "unwrap_err" | "expect_err" | "unwrap_none"
        ) {
            self.panics = true;
        }
        self.calls.push(m);
        syn::visit::visit_expr_method_call(self, c);
    }
    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        self.calls.push(c.func.to_token_stream().to_string().replace(' ', ""));
        syn::visit::visit_expr_call(self, c);
    }
    fn visit_expr_try(&mut self, e: &'ast syn::ExprTry) {
        self.panics = true;
        syn::visit::visit_expr_try(self, e);
    }
    fn visit_expr_match(&mut self, m: &'ast syn::ExprMatch) {
        let panicking_wild = m.arms.iter().any(|a| {
            matches!(a.pat, syn::Pat::Wild(_) | syn::Pat::Ident(_))
                && arm_panics(&a.body)
        });
        let empty_arm = m.arms.iter().any(|a| {
            let b = a.body.to_token_stream().to_string();
            (b == "{ }" || b == "()") && binds_nothing(&a.pat)
        });
        if panicking_wild && empty_arm && m.arms.len() == 2 {
            self.findings
                .manual_assert_matches
                .push((self.file.into(), m.span().start().line));
            self.panics = true;
        }
        syn::visit::visit_expr_match(self, m);
    }
    fn visit_expr_let(&mut self, e: &'ast syn::ExprLet) {
        syn::visit::visit_expr_let(self, e);
    }
    fn visit_local(&mut self, l: &'ast syn::Local) {
        if let Some(init) = &l.init {
            if let Some((_, div)) = &init.diverge {
                if expr_panics(div) && binds_nothing(&l.pat) {
                    self.findings
                        .manual_assert_matches
                        .push((self.file.into(), l.span().start().line));
                    self.panics = true;
                }
            }
        }
        let _ = self.test_name;
        syn::visit::visit_local(self, l);
    }
}

fn expr_panics(e: &syn::Expr) -> bool {
    let s = e.to_token_stream().to_string();
    s.contains("panic !") || s.contains("unreachable !")
}

fn arm_panics(e: &syn::Expr) -> bool {
    expr_panics(e) && e.to_token_stream().to_string().len() < 200
}

/// Base of an `assert_eq!(base.field, ..)` first argument, if it is a field access.
fn field_assert_base(s: &syn::Stmt) -> Option<String> {
    let m = match s {
        syn::Stmt::Macro(m) => &m.mac,
        syn::Stmt::Expr(syn::Expr::Macro(m), _) => &m.mac,
        _ => return None,
    };
    if macro_name(m) != "assert_eq" {
        return None;
    }
    let args = m
        .parse_body_with(syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated)
        .ok()?;
    match args.first()? {
        syn::Expr::Field(f) => Some(f.base.to_token_stream().to_string()),
        _ => None,
    }
}

struct FileScan<'a> {
    file: &'a Path,
    in_test: bool,
    test_loc: usize,
    tests: Vec<TestFn>,
    findings: &'a mut Findings,
}

impl<'ast> Visit<'ast> for FileScan<'_> {
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        let was = self.in_test;
        if is_cfg_test(&m.attrs) && !was {
            self.in_test = true;
            self.test_loc += lines(m);
        }
        syn::visit::visit_item_mod(self, m);
        self.in_test = was;
    }
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        if !f.attrs.iter().any(is_test_attr) {
            return syn::visit::visit_item_fn(self, f);
        }
        let name = f.sig.ident.to_string();
        let mut s = String::new();
        shape(f.block.to_token_stream(), &mut s);
        let mut loose = String::new();
        let mut exact = vec![];
        flatten(f.block.to_token_stream(), &mut loose, &mut exact);
        let stmt_shapes: Vec<(String, usize)> = f
            .block
            .stmts
            .iter()
            .map(|st| (loose_of(st.to_token_stream()), lines(st)))
            .collect();
        let mut repeated_block = None;
        for k in 1..=6.min(stmt_shapes.len() / 2) {
            let mut seen: HashMap<Vec<&str>, (usize, usize, usize)> = HashMap::new(); // count, last end, loc
            for i in 0..=stmt_shapes.len() - k {
                let w = &stmt_shapes[i..i + k];
                let loc: usize = w.iter().map(|x| x.1).sum();
                if loc < 3 { continue }
                let e = seen.entry(w.iter().map(|x| x.0.as_str()).collect()).or_insert((0, 0, loc));
                if e.0 == 0 || i >= e.1 {
                    e.0 += 1;
                    e.1 = i + k;
                }
            }
            for (_, (n, _, loc)) in seen {
                if n >= 3 || (n >= 2 && loc >= 6) {
                    let saved = ((n - 1) * loc).saturating_sub(n + 2);
                    if saved >= 4 && repeated_block.is_none_or(|(_, s)| saved > s) {
                        repeated_block = Some((n, saved));
                    }
                }
            }
        }
        let stmts = f
            .block
            .stmts
            .iter()
            .map(|st| (st.to_token_stream().to_string(), lines(st)))
            .collect();
        let mut scan = BodyScan {
            file: self.file,
            asserts: 0,
            panics: false,
            calls: vec![],
            findings: self.findings,
            test_name: &name,
        };
        scan.visit_block(&f.block);
        let (asserts, has_panic_path, calls) = (scan.asserts, scan.panics, scan.calls);

        // Runs of >= 3 consecutive assert_eq!(x.field, ..) on the same base.
        let mut run: Option<(String, usize, usize)> = None;
        for st in f.block.stmts.iter().chain(std::iter::once(&syn::Stmt::Expr(
            syn::parse_quote!(()),
            None,
        ))) {
            let b = field_assert_base(st);
            match (&mut run, b) {
                (Some((rb, _, n)), Some(b)) if *rb == b => *n += 1,
                (r, b) => {
                    if let Some((_, line, n)) = r.take() {
                        if n >= 3 {
                            self.findings.field_by_field.push((self.file.into(), line, n));
                        }
                    }
                    *r = b.map(|b| (b, st.span().start().line, 1));
                }
            }
        }

        if asserts == 0 && !has_panic_path {
            self.findings
                .no_assertion
                .push((self.file.into(), f.span().start().line, name.clone()));
        }
        if !self.in_test {
            self.test_loc += lines(f);
        }
        self.tests.push(TestFn {
            file: self.file.into(),
            name,
            line: f.span().start().line,
            loc: lines(f),
            shape: s,
            loose,
            exact,
            repeated_block,
            stmts,
            asserts,
            has_panic_path,
            calls,
        });
    }
}

fn crate_of(root: &Path, file: &Path) -> String {
    let rel = file.strip_prefix(root).unwrap_or(file);
    let comps: Vec<_> = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect();
    let cut = comps.iter().position(|c| c == "src" || c == "tests" || c == "benches");
    comps[..cut.unwrap_or(comps.len().saturating_sub(1))].join("/")
}

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).expect("usage: testslop <dir>"));
    let mut findings = Findings::default();
    let mut tests = vec![];
    let mut per_crate: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    for e in walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_entry(|e| e.file_name() != "target" && e.file_name() != ".git")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
    {
        let p = e.path();
        let Ok(src) = std::fs::read_to_string(p) else { continue };
        let total = src.lines().count();
        let Ok(ast) = syn::parse_file(&src) else { continue };
        let whole_file_test = p.components().any(|c| c.as_os_str() == "tests")
            || p.file_name().is_some_and(|f| f == "tests.rs")
            || is_cfg_test(&ast.attrs);
        let mut fs = FileScan {
            file: p,
            in_test: whole_file_test,
            test_loc: 0,
            tests: vec![],
            findings: &mut findings,
        };
        fs.visit_file(&ast);
        let tloc = if whole_file_test { total } else { fs.test_loc.min(total) };
        let ent = per_crate.entry(crate_of(&root, p)).or_default();
        ent.0 += total - tloc;
        ent.1 += tloc;
        ent.2 += fs.tests.len();
        tests.extend(fs.tests);
    }

    let (prod, tloc, n): (usize, usize, usize) =
        per_crate.values().fold((0, 0, 0), |a, v| (a.0 + v.0, a.1 + v.1, a.2 + v.2));
    println!("== totals: {n} tests, test LOC {tloc}, prod LOC {prod}, ratio {:.2}", tloc as f64 / prod.max(1) as f64);
    println!("\n== per crate (test/prod ratio, top 25 by test LOC)");
    let mut pc: Vec<_> = per_crate.iter().filter(|(_, v)| v.1 > 0).collect();
    pc.sort_by_key(|(_, v)| std::cmp::Reverse(v.1));
    for (c, (p, t, n)) in pc.iter().take(25) {
        println!("{:>6} test {:>6} prod {:>5.2}x {:>5} tests  {c}", t, p, *t as f64 / (*p).max(1) as f64, n);
    }

    // Structural clones: same loose shape within one file, clustered by differing token count.
    let mut by_loose: HashMap<(&Path, &str), Vec<&TestFn>> = HashMap::new();
    for t in &tests {
        if t.loc >= 4 {
            by_loose.entry((&t.file, &t.loose)).or_default().push(t);
        }
    }
    let mut clones: Vec<Vec<&TestFn>> = vec![];
    for (_, mut g) in by_loose {
        while let Some(seed) = g.pop() {
            let mut cluster = vec![seed];
            let mut holes = vec![false; seed.exact.len()];
            g.retain(|t| {
                let mut h = holes.clone();
                for (i, (a, b)) in seed.exact.iter().zip(&t.exact).enumerate() {
                    if a != b { h[i] = true }
                }
                let n = h.iter().filter(|x| **x).count();
                if n <= 8 && n * 10 <= seed.exact.len() {
                    holes = h;
                    cluster.push(t);
                    false
                } else {
                    true
                }
            });
            if cluster.len() >= 2 {
                clones.push(cluster);
            }
        }
    }
    let saved = |g: &Vec<&TestFn>| ((g.len() - 1) * g[0].loc).saturating_sub(g.len() + 2);
    clones.sort_by_key(|g| std::cmp::Reverse(saved(g)));
    let total_saved: usize = clones.iter().map(saved).sum();
    println!(
        "\n== parametrizable_tests: {} groups, {} tests, ~{} LOC saved as tables",
        clones.len(),
        clones.iter().map(Vec::len).sum::<usize>(),
        total_saved
    );
    for g in clones.iter().take(20) {
        let names: Vec<_> = g.iter().map(|t| t.name.as_str()).collect();
        println!(
            "  -{:<4} {}:{} x{} ({} LOC each) {}",
            saved(g),
            g[0].file.strip_prefix(&root).unwrap().display(),
            g[0].line,
            g.len(),
            g[0].loc,
            names.join(", ")
        );
    }

    // Shared setup: longest exact statement prefix shared by >= 3 tests in a file.
    let mut by_file: HashMap<&Path, Vec<&TestFn>> = HashMap::new();
    for t in &tests {
        by_file.entry(&t.file).or_default().push(t);
    }
    let mut fixtures = vec![];
    for (f, ts) in &by_file {
        let mut prefixes: HashMap<Vec<&str>, Vec<&TestFn>> = HashMap::new();
        for t in ts {
            for k in 2..=t.stmts.len().saturating_sub(1) {
                let key: Vec<&str> = t.stmts[..k].iter().map(|s| s.0.as_str()).collect();
                prefixes.entry(key).or_default().push(t);
            }
        }
        // Keep the best-saving prefix per file.
        let best = prefixes
            .into_iter()
            .filter(|(_, v)| v.len() >= 3)
            .map(|(k, v)| {
                let ploc: usize = v[0].stmts[..k.len()].iter().map(|s| s.1).sum();
                ((v.len() - 1) * ploc).saturating_sub(v.len() + 2) // one call per test + fn header
            })
            .max();
        if let Some(s) = best.filter(|s| *s >= 6) {
            fixtures.push((s, *f, ts.len()));
        }
    }
    fixtures.sort_by_key(|x| std::cmp::Reverse(x.0));
    println!(
        "\n== missing-fixtures: {} files, ~{} LOC saved (best shared prefix per file)",
        fixtures.len(),
        fixtures.iter().map(|x| x.0).sum::<usize>()
    );
    for (s, f, n) in fixtures.iter().take(15) {
        println!("  -{:<4} {} ({} tests)", s, f.strip_prefix(&root).unwrap().display(), n);
    }

    let mut rb: Vec<_> = tests.iter().filter_map(|t| t.repeated_block.map(|r| (r, t))).collect();
    rb.sort_by_key(|((_, s), _)| std::cmp::Reverse(*s));
    println!(
        "\n== copy_paste_assert_block: {} tests, ~{} LOC saved (best repeated block per test)",
        rb.len(),
        rb.iter().map(|((_, s), _)| s).sum::<usize>()
    );
    for ((n, s), t) in rb.iter().take(15) {
        println!("  -{:<4} {}:{} {} (block x{})", s, t.file.strip_prefix(&root).unwrap().display(), t.line, t.name, n);
    }

    let show = |title: &str, v: &[(PathBuf, usize, String)]| {
        println!("\n== {title}: {}", v.len());
        for (f, l, s) in v.iter().take(12) {
            println!("  {}:{} {}", f.strip_prefix(&root).unwrap().display(), l, s);
        }
    };
    show("no_assertion_test (no assert, unwrap, ?, or panic)", &findings.no_assertion);
    show("tautological_assert", &findings.tautological);
    println!("\n== field_by_field_assert (>=3 in a row): {}", findings.field_by_field.len());
    for (f, l, n) in findings.field_by_field.iter().take(10) {
        println!("  {}:{} x{}", f.strip_prefix(&root).unwrap().display(), l, n);
    }
    println!("\n== manual_assert_matches: {}", findings.manual_assert_matches.len());
    for (f, l) in findings.manual_assert_matches.iter().take(10) {
        println!("  {}:{}", f.strip_prefix(&root).unwrap().display(), l);
    }

    let mut sizes: Vec<_> = tests.iter().map(|t| t.loc).collect();
    sizes.sort();
    let weak = tests.iter().filter(|t| t.asserts == 0 && t.has_panic_path).count();
    let calls: usize = tests.iter().map(|t| t.calls.len()).sum();
    println!(
        "\n== shape: median test {} LOC, p90 {}; {} tests assert only via unwrap/?; avg {} calls/test",
        sizes.get(sizes.len() / 2).unwrap_or(&0),
        sizes.get(sizes.len() * 9 / 10).unwrap_or(&0),
        weak,
        calls / tests.len().max(1)
    );
}
