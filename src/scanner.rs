//! The definition scanner for the languages of [`crate::langs`]: one
//! engine, driven by each language's [`ScanRow`].
//!
//! It is what a root has when no SCIP indexer runs for it, and what keeps
//! an edited file's ranges right between full runs ([`crate::code::reindex_file`]).
//! Definitions only: names, kinds, containers and line ranges, no
//! references.
//!
//! # How a file is scanned
//!
//! (A row whose blocks are indentation, Python, is read the same way with
//! indentation blocks in place of brackets: see `Scanner::scan_indented`.)
//!
//! 1. Comments and string literals are blanked to spaces, byte for byte
//!    (so columns stay exact), by the row's [`crate::langs::Quote`] forms.
//!    A line that starts inside a block comment or a multi-line string is
//!    not code ([`Scanner::code_lines`]).
//! 2. Lines are read in order with a stack of open blocks (`{`, `(`, `[`).
//!    Each block has a kind: the kind a rule gave it (a Go struct body, a
//!    `const (` group), else opaque (a function body, a composite literal).
//!    Rules of scope `""` apply at the top level, rules of a kind inside a
//!    block of that kind, and no rule inside an opaque block, so a local is
//!    never taken for a member.
//! 3. A definition owns the block its line leaves open: its last line is
//!    the line that closes it (a multi-line parameter list hands ownership
//!    to the body opened on the line that closes it). A definition that
//!    leaves no block open ends on its own line, unless the row puts bodies
//!    on the next line ([`crate::langs::ScanRow::allman`], C#): then a line
//!    that starts with `{` after it (past lines indented deeper, such as a
//!    base list, `where` constraints or a constructor initializer) opens
//!    its body.

use std::sync::OnceLock;

use crate::langs::{Does, Lang, Quote, Rule, LANGS};
use crate::regex::Regex;

/// One definition the scanner found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Def {
    /// The name.
    pub name: String,
    /// The rule's kind word (`fn`, `method`, `struct`, `field`, ...).
    pub kind: &'static str,
    /// The enclosing block's name (a struct for a field), the rule's
    /// `container` group (a Go receiver), else the file's default (a Go
    /// package); empty when none is known.
    pub container: String,
    /// Line, 1-based.
    pub line: u32,
    /// Column of the name, 0-based bytes.
    pub col: u32,
    /// Last line, 1-based (at least `line`).
    pub end: u32,
}

/// A language's compiled scanner.
#[derive(Debug)]
pub struct Scanner {
    quotes: &'static [Quote],
    /// Each rule, its scopes and its compiled pattern.
    rules: Vec<(&'static Rule, Vec<&'static str>, Regex)>,
    file_container: bool,
    continued_by: Option<Regex>,
    header_continued: Option<Regex>,
    allman: bool,
    not_names: &'static [&'static str],
    indent: bool,
    once: &'static [&'static str],
}

/// The compiled scanner of a language row (compiled once per process).
///
/// # Panics
///
/// When a row's pattern does not compile: the rows are fixed data, and a
/// unit test compiles every one.
pub fn of(lang: &'static Lang) -> &'static Scanner {
    static ALL: OnceLock<Vec<Scanner>> = OnceLock::new();
    let all = ALL.get_or_init(|| {
        LANGS
            .iter()
            .map(|l| Scanner {
                quotes: l.scanner.quotes,
                rules: l
                    .scanner
                    .rules
                    .iter()
                    .map(|r| (r, r.scope.split('|').collect(), Regex::new(r.re).expect("scanner pattern")))
                    .collect(),
                file_container: l.scanner.file_container,
                continued_by: (!l.scanner.continued_by.is_empty()).then(|| Regex::new(l.scanner.continued_by).expect("scanner pattern")),
                header_continued: (!l.scanner.header_continued.is_empty())
                    .then(|| Regex::new(l.scanner.header_continued).expect("scanner pattern")),
                allman: l.scanner.allman,
                not_names: l.scanner.not_names,
                indent: l.scanner.indent,
                once: l.scanner.once,
            })
            .collect()
    });
    let i = LANGS.iter().position(|l| std::ptr::eq(l, lang)).expect("a row of LANGS");
    &all[i]
}

/// A file name without its folder and extension (`.d.ts` as one), as
/// scip-typescript names a module.
fn file_stem(path: &str) -> &str {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    let stem = stem.strip_suffix(".d").filter(|s| !s.is_empty()).unwrap_or(stem);
    if stem.is_empty() {
        name
    } else {
        stem
    }
}

/// An open block.
struct Block {
    /// The byte that closes it.
    close: u8,
    /// Its kind (`""`: opaque).
    kind: &'static str,
    /// The name its member definitions take as container.
    container: String,
    /// The definition it belongs to (index into the output).
    owner: Option<usize>,
    /// Other definitions whose body it is too: the same signature written
    /// in each branch of an `#if` (C#), all ending where the body does.
    twins: Vec<usize>,
}

/// A body expected on a later line ([`crate::langs::ScanRow::allman`]).
struct Pending {
    /// What the block it opens is: kind, container, owner.
    what: (&'static str, String, Option<usize>),
    /// The indent of the line that declared it.
    indent: usize,
    /// A preprocessor directive came between (`#else`): a declaration
    /// after it is the same item's other branch.
    directive: bool,
    /// Earlier branches' definitions.
    twins: Vec<usize>,
}

impl Scanner {
    /// The text with every comment and string literal blanked to spaces,
    /// and for each line whether it starts in code.
    pub fn blank(&self, text: &str) -> (String, Vec<bool>) {
        let b = text.as_bytes();
        let mut out = b.to_vec();
        let mut starts = vec![true];
        let mut inside: Option<&Quote> = None;
        // Inside a quote's template expression ([`Quote::template`]): its
        // open braces, and whether a string it holds is open.
        let mut depth = 0usize;
        let mut nested = false;
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if c == b'\n' {
                if depth == 0 && inside.is_some_and(|q| !q.multiline) {
                    inside = None;
                }
                starts.push(inside.is_none());
                i += 1;
                continue;
            }
            match inside {
                Some(_) if depth > 0 => {
                    out[i] = b' ';
                    if nested {
                        if c == b'\\' && i + 1 < b.len() && b[i + 1] != b'\n' {
                            out[i + 1] = b' ';
                            i += 1;
                        } else if c == b'"' {
                            nested = false;
                        }
                    } else {
                        match c {
                            b'"' => nested = true,
                            b'{' => depth += 1,
                            b'}' => depth -= 1,
                            _ => {}
                        }
                    }
                }
                Some(q) if !q.template.is_empty() && b[i..].starts_with(q.template.as_bytes()) => {
                    out[i..i + q.template.len()].fill(b' ');
                    i += q.template.len() - 1;
                    depth = 1;
                    nested = false;
                }
                Some(q) => {
                    if q.escape && c == b'\\' {
                        out[i] = b' ';
                        if i + 1 < b.len() && b[i + 1] != b'\n' {
                            out[i + 1] = b' ';
                            i += 1;
                        }
                    } else if !q.close.is_empty() && b[i..].starts_with(q.close.as_bytes()) {
                        out[i..i + q.close.len()].fill(b' ');
                        i += q.close.len() - 1;
                        inside = None;
                    } else {
                        out[i] = b' ';
                    }
                }
                None => {
                    if let Some(q) = self.quotes.iter().find(|q| b[i..].starts_with(q.open.as_bytes())) {
                        out[i..i + q.open.len()].fill(b' ');
                        i += q.open.len() - 1;
                        inside = Some(q);
                    }
                }
            }
            i += 1;
        }
        // Only ASCII bytes and whole UTF-8 sequences were replaced, so the
        // result is UTF-8; the fallback keeps a pathological text scannable.
        let s = String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
        (s, starts)
    }

    /// For each line, whether it starts in code (not inside a block comment
    /// or a multi-line string).
    pub fn code_lines(&self, text: &str) -> Vec<bool> {
        self.blank(text).1
    }

    /// Every definition in a file's text, in line order.
    pub fn scan(&self, text: &str) -> Vec<Def> {
        self.scan_file("", text)
    }

    /// Every definition in a file's text, in line order, for a row whose
    /// top-level definitions take the file's name as container
    /// ([`crate::langs::ScanRow::file_container`]).
    ///
    /// # Arguments
    ///
    /// * `path` - the file's path (`/` or `\`-separated; only its last
    ///   component is read).
    /// * `text` - its text.
    pub fn scan_file(&self, path: &str, text: &str) -> Vec<Def> {
        if self.indent {
            let defs = self.scan_indented(path, text);
            return self.first_only(defs);
        }
        let (clean, _) = self.blank(text);
        let mut out: Vec<Def> = Vec::new();
        let mut stack: Vec<Block> = Vec::new();
        let mut file_container = if self.file_container { file_stem(path).to_string() } else { String::new() };
        let lines: Vec<&str> = clean.lines().collect();
        let raw: Vec<&str> = text.lines().collect();
        let indent = |l: &str| l.len() - l.trim_start().len();
        let mut pending: Option<Pending> = None;
        for (i, &line) in lines.iter().enumerate() {
            let n = i as u32 + 1;
            let body = line.trim_start().starts_with('{');
            // An earlier branch's declaration, when this line may be the
            // same item's next branch.
            let mut twins: Vec<usize> = Vec::new();
            // A deeper line that ends by opening a brace (a Java `throws`
            // or `implements` clause) opens the body too. A deeper line
            // under a declaration waiting for its body, or a line the row
            // says continues a header at any indent (Kotlin's `private
            // constructor(`), continues that declaration: no rule reads it
            // as a declaration (a Java `throws X,` line is no field `X`).
            // (A header line continues only a declaration whose block is a
            // scope, a class: a secondary constructor is a member.)
            let header = self.header_continued.as_ref().is_some_and(|re| re.is_match(line))
                && pending.as_ref().is_some_and(|p| !p.what.0.is_empty());
            let deeper = |p: &Pending| !line.trim().is_empty() && (indent(line) > p.indent || header);
            let mut continuing = false;
            let mut header_take = false;
            let inherit = match pending.take() {
                Some(p) if body => Some(p),
                // A header line (`private constructor(`): its first bracket
                // is the declaration's, whatever it is.
                Some(p) if header => {
                    continuing = true;
                    header_take = true;
                    Some(p)
                }
                Some(p) if deeper(&p) && line.trim_end().ends_with('{') => {
                    continuing = true;
                    Some(p)
                }
                // A deeper line ending in `;` ends a declaration without a
                // body (an abstract method's `throws` clause).
                Some(p) if deeper(&p) && line.trim_end().ends_with(';') => {
                    continuing = true;
                    for &o in p.what.2.iter().chain(&p.twins) {
                        out[o].end = n;
                    }
                    None
                }
                // Blank lines (a blanked comment or directive) and deeper
                // ones (a base list, `where`) keep waiting.
                Some(mut p) if line.trim().is_empty() || deeper(&p) => {
                    continuing = deeper(&p);
                    p.directive |= raw.get(i).is_some_and(|r| r.trim_start().starts_with('#'));
                    pending = Some(p);
                    None
                }
                Some(p) => {
                    if p.directive {
                        twins = p.twins;
                        twins.extend(p.what.2);
                    }
                    None
                }
                None => None,
            };
            // A line continuing a declaration is read as an expression, as
            // the inside of an opaque block is.
            let scope = match stack.last() {
                _ if continuing => crate::langs::OPAQUE,
                None => "",
                Some(b) if !b.kind.is_empty() => b.kind,
                Some(_) => crate::langs::OPAQUE,
            };
            let outer = stack.last().map_or(file_container.as_str(), |b| b.container.as_str()).to_string();
            // What the line opens: (kind, container, owner).
            let opens = self.rules_on(line, n, scope, &outer, &mut file_container, &mut out);
            // A body the line takes from the declaration it continues is
            // what the line opens, when the declaration's is opaque (a
            // field's value on the next line: an anonymous class's body).
            let inherit = match (inherit, &opens) {
                (Some(mut p), Some((kind, _, None))) if continuing && p.what.0.is_empty() && !kind.is_empty() => {
                    p.what.0 = kind;
                    Some(p)
                }
                (i, _) => i,
            };
            // Brackets: the blocks this line leaves open, and the owner of
            // one it closes (a parameter list handing over to the body).
            let mut depth = stack.len();
            let mut closed_owner: Option<(usize, &'static str, String)> = None;
            // A body on a later line: its `{` is the line's first brace.
            let inherited = inherit.is_some();
            let mut inherit = inherit;
            for &c in line.as_bytes() {
                let close = match c {
                    b'{' => Some(b'}'),
                    b'(' => Some(b')'),
                    b'[' => Some(b']'),
                    _ => None,
                };
                if let Some(close) = close {
                    let taken = if c == b'{' || header_take { inherit.take() } else { None };
                    let (kind, container, owner, twins) = match taken {
                        Some(p) => (p.what.0, p.what.1, p.what.2, p.twins),
                        // A block no rule named takes the container of the
                        // block around it (an anonymous class in a call
                        // inside a method body: the method).
                        None => ("", stack.last().map_or_else(String::new, |b| b.container.clone()), None, Vec::new()),
                    };
                    stack.push(Block { close, kind, container, owner, twins });
                    continue;
                }
                if matches!(c, b'}' | b')' | b']') {
                    let Some(at) = stack.iter().rposition(|b| b.close == c) else { continue };
                    let mut popped = stack.split_off(at);
                    depth = depth.min(at);
                    let b = popped.swap_remove(0);
                    for &t in &b.twins {
                        out[t].end = n;
                    }
                    if let Some(o) = b.owner {
                        out[o].end = n;
                        closed_owner = Some((o, b.kind, b.container));
                    }
                }
            }
            // The outermost block opened on this line and still open (a
            // body taken from a line before already has its owner).
            let declared = opens.is_some();
            // In a row with header lines (Kotlin), any parameter list handing
            // over to a body still to come (`) : Base,` then `Other {`).
            let from_params = !declared && (line.trim_end().ends_with(')') || (self.header_continued.is_some() && closed_owner.is_some()));
            let what = opens.or_else(|| closed_owner.map(|(o, kind, container)| (kind, container, Some(o))));
            // A header line's parameter list closed on it hands over to a
            // body it opens (`private constructor(x: X) {`).
            if stack.len() > depth && (!inherited || header_take) {
                if let Some((kind, container, owner)) = what {
                    let b = &mut stack[depth];
                    b.kind = kind;
                    b.container = container;
                    b.owner = owner;
                }
            } else if stack.len() <= depth && self.allman && !line.trim_end().ends_with(';') {
                // Nothing left open: the body may start on a later line (a
                // declaration's, or a parameter list's closed on this line).
                if let Some(w) = what.filter(|_| declared || from_params) {
                    let from = w.2.map_or(indent(line), |o| indent(lines[out[o].line as usize - 1]));
                    pending = Some(Pending { what: w, indent: from, directive: false, twins });
                }
            }
        }
        // A block never closed (a truncated file) leaves its owner ending
        // at the last line.
        let last = lines.len() as u32;
        for b in &stack {
            for &o in b.owner.iter().chain(&b.twins) {
                out[o].end = last.max(out[o].line);
            }
        }
        if let Some(re) = &self.continued_by {
            let lines: Vec<&str> = clean.lines().collect();
            // The first definition of a line is the line's item; the others
            // (an inline object type's members) end with it.
            for i in 0..out.len() {
                // In a row whose bodies may open on a later line (C#), a
                // definition whose body closed ends with it.
                if self.allman
                    && out[i].end > out[i].line
                    && lines.get(out[i].end as usize - 1).is_some_and(|l| l.trim_end().ends_with('}'))
                {
                    continue;
                }
                if i == 0 || out[i - 1].line != out[i].line {
                    out[i].end = continued(&lines, &out[i], re);
                }
            }
        }
        self.first_only(out)
    }

    /// Drops the later definitions of a name already defined in the same
    /// container with a kind of [`crate::langs::ScanRow::once`].
    fn first_only(&self, defs: Vec<Def>) -> Vec<Def> {
        if self.once.is_empty() {
            return defs;
        }
        let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
        defs.into_iter().filter(|d| !self.once.contains(&d.kind) || seen.insert((d.container.clone(), d.name.clone()))).collect()
    }

    /// [`Scanner::scan_file`] for a row whose blocks are indentation
    /// ([`crate::langs::ScanRow::indent`], Python).
    ///
    /// Lines join into logical lines while a bracket is open or a line ends
    /// in `\`; rules apply to a logical line's first line. A logical line
    /// ending in `:` opens a block holding the lines indented deeper than
    /// its first line; the first logical line indented as deep or less
    /// closes it, and its owner ends on the block's last line that is not
    /// blank or a comment alone (a string's lines count: a docstring is a
    /// statement). A definition without a block ends with its logical line.
    fn scan_indented(&self, path: &str, text: &str) -> Vec<Def> {
        /// An open block.
        struct IBlock {
            /// The indent of the line that opened it.
            indent: usize,
            /// Its kind ([`crate::langs::SAME_SCOPE`]: the scope around it).
            kind: &'static str,
            /// The name its definitions take as container.
            container: String,
            /// The definition it belongs to.
            owner: Option<usize>,
            /// Its last line with content, 1-based.
            last: u32,
        }
        let (clean, starts) = self.blank(text);
        let lines: Vec<&str> = clean.lines().collect();
        let raw: Vec<&str> = text.lines().collect();
        let mut file_container = if self.file_container { file_stem(path).to_string() } else { String::new() };
        let mut out: Vec<Def> = Vec::new();
        let mut stack: Vec<IBlock> = Vec::new();
        let indent = |l: &str| l.len() - l.trim_start().len();
        // Open brackets, across lines.
        let mut depth: i32 = 0;
        // The logical line in progress: its first line's indent, what it
        // opens if it ends in `:`, and the definitions it made.
        type Header = (usize, Option<(&'static str, String, Option<usize>)>, usize);
        let mut header: Option<Header> = None;
        let close = |stack: &mut Vec<IBlock>, out: &mut Vec<Def>, b: IBlock| {
            if let Some(o) = b.owner {
                out[o].end = b.last.max(out[o].line);
            }
            if let Some(parent) = stack.last_mut() {
                parent.last = parent.last.max(b.last);
            }
        };
        for (i, &line) in lines.iter().enumerate() {
            let n = i as u32 + 1;
            let r = raw.get(i).copied().unwrap_or("");
            let in_code = starts.get(i).copied().unwrap_or(true);
            // A comment alone, or a blank line, is no content.
            let content = !r.trim().is_empty() && !(in_code && line.trim().is_empty() && r.trim_start().starts_with('#'));
            if !content {
                continue;
            }
            if header.is_none() && in_code {
                // A new logical line: it closes the blocks indented as deep
                // or deeper, then the rules of the scope it is in apply.
                let at = indent(line);
                while stack.last().is_some_and(|b| b.indent >= at) {
                    let b = stack.pop().expect("a block");
                    close(&mut stack, &mut out, b);
                }
                let scoped = stack.iter().rev().find(|b| b.kind != crate::langs::SAME_SCOPE);
                let scope = match scoped {
                    None => Some(""),
                    Some(b) if !b.kind.is_empty() => Some(b.kind),
                    Some(_) => None,
                };
                let outer = scoped.map_or(file_container.as_str(), |b| b.container.as_str()).to_string();
                let first = out.len();
                let opens = match scope {
                    Some(scope) => self.rules_on(line, n, scope, &outer, &mut file_container, &mut out),
                    None => None,
                };
                // A block no rule named is opaque; a scope-less one keeps
                // the container around it.
                let opens =
                    opens.map(
                        |(kind, container, owner)| {
                            if kind == crate::langs::SAME_SCOPE {
                                (kind, outer.clone(), None)
                            } else {
                                (kind, container, owner)
                            }
                        },
                    );
                header = Some((at, opens, first));
            }
            for b in &mut stack {
                b.last = n;
            }
            for c in line.bytes() {
                match c {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => depth = (depth - 1).max(0),
                    _ => {}
                }
            }
            let Some((at, opens, first)) = header.take() else { continue };
            // The logical line's definitions run to its last line so far.
            for d in &mut out[first..] {
                d.end = n;
            }
            let t = line.trim_end();
            if depth > 0 || t.ends_with('\\') {
                header = Some((at, opens, first));
                continue;
            }
            if t.ends_with(':') {
                let (kind, container, owner) = opens.unwrap_or(("", String::new(), None));
                stack.push(IBlock { indent: at, kind, container, owner, last: n });
            }
        }
        while let Some(b) = stack.pop() {
            close(&mut stack, &mut out, b);
        }
        out
    }

    /// Applies the rules of `scope` to one (blanked) line: the first that
    /// matches, then the [`Does::Each`] rules when it defined something.
    ///
    /// # Returns
    ///
    /// What the line opens: the block's kind, container and owner (an index
    /// into `out`), if a rule says.
    fn rules_on(
        &self,
        line: &str,
        n: u32,
        scope: &str,
        outer: &str,
        file_container: &mut String,
        out: &mut Vec<Def>,
    ) -> Option<(&'static str, String, Option<usize>)> {
        let outer = outer.to_string();
        let mut opens: Option<(&'static str, String, Option<usize>)> = None;
        for (rule, _, re) in self.rules.iter().filter(|(_, scopes, _)| scopes.contains(&scope)) {
            let Some(c) = re.captures(line) else { continue };
            match rule.does {
                Does::Container => {
                    *file_container = c.name("name").map_or("", |m| m.as_str()).to_string();
                }
                Does::Open => {
                    // A named block (a C# namespace) names its members' container.
                    let named = c.name("name").map_or_else(|| outer.clone(), |m| m.as_str().to_string());
                    opens = Some((rule.opens, named, None));
                }
                Does::DefOwner(kind) => {
                    if outer.is_empty() {
                        break;
                    }
                    // A pattern naming the definition matches only the
                    // block's own name (a constructor, not a call).
                    if c.name("name").is_some_and(|m| m.as_str() != outer) {
                        continue;
                    }
                    let at = line.len() - line.trim_start().len();
                    out.push(Def { name: outer.clone(), kind, container: outer.clone(), line: n, col: at as u32, end: n });
                    opens = Some((rule.opens, outer.clone(), Some(out.len() - 1)));
                }
                Does::Def(kind) | Does::Member(kind) => {
                    let container = c.name("container").map(|m| m.as_str().to_string()).unwrap_or_else(|| outer.clone());
                    let first = out.len();
                    if let Some(m) = c.name("name") {
                        let mut at = m.start();
                        for part in m.as_str().split([',', '=']) {
                            let lead = part.len() - part.trim_start().len();
                            let name = part.trim();
                            if !name.is_empty() && name != "_" && !self.not_names.contains(&name) {
                                out.push(Def {
                                    name: name.to_string(),
                                    kind,
                                    container: container.clone(),
                                    line: n,
                                    col: (at + lead) as u32,
                                    end: n,
                                });
                            }
                            at += part.len() + 1;
                        }
                    }
                    let owner = (out.len() > first).then_some(first);
                    // A member's block keeps the enclosing container.
                    let named = match owner {
                        Some(o) if !matches!(rule.does, Does::Member(_)) => out[o].name.clone(),
                        _ => outer.clone(),
                    };
                    opens = Some((rule.opens, named, owner));
                }
                Does::Each(_) => continue,
            }
            break;
        }
        // Rules that define every match on the line, beside the
        // rule that matched first, when that one defined something
        // (a declaration's line, not an expression's).
        let defined = out.last().is_some_and(|d| d.line == n);
        // Their container: the scope the line's definition opens (a Kotlin
        // class whose constructor properties are on its line), else the
        // line's own.
        let each_in = match &opens {
            Some((kind, named, Some(_))) if !kind.is_empty() => named.clone(),
            _ => outer.clone(),
        };
        for (rule, _, re) in
            self.rules.iter().filter(|(r, scopes, _)| defined && matches!(r.does, Does::Each(_)) && scopes.contains(&scope))
        {
            let Does::Each(kind) = rule.does else { continue };
            for c in re.captures_iter(line) {
                let Some(m) = c.name("name") else { continue };
                if self.not_names.contains(&m.as_str()) || out.iter().rev().take_while(|d| d.line == n).any(|d| d.name == m.as_str()) {
                    continue;
                }
                out.push(Def { name: m.as_str().to_string(), kind, container: each_in.clone(), line: n, col: m.start() as u32, end: n });
            }
        }
        opens
    }
}

/// The last line of a definition that runs on past its own line or block
/// (a TypeScript type alias written over several lines): while the next
/// code line is indented deeper than the definition's line, or starts as
/// the row's [`crate::langs::ScanRow::continued_by`] says (`|`, `&`, `?`,
/// `:`, `.`), or a bracket a continuation line opened is still open; a
/// line ending in `;` ends it, and so does a definition whose last line
/// does.
fn continued(lines: &[&str], d: &Def, re: &Regex) -> u32 {
    let indent = |l: &str| l.len() - l.trim_start().len();
    let mut end = d.end as usize;
    let Some(own) = lines.get(d.line as usize - 1) else { return d.end };
    if lines.get(end - 1).is_some_and(|l| l.trim_end().ends_with(';')) {
        return d.end;
    }
    let base = indent(own);
    let mut depth = 0i32;
    let mut j = end;
    while j < lines.len() {
        let l = lines[j];
        j += 1;
        if l.trim().is_empty() {
            continue;
        }
        if depth <= 0 && indent(l) <= base && !re.is_match(l) {
            break;
        }
        for c in l.bytes() {
            match c {
                b'{' | b'(' | b'[' => depth += 1,
                b'}' | b')' | b']' => depth -= 1,
                _ => {}
            }
        }
        end = j;
        if depth <= 0 && l.trim_end().ends_with(';') {
            break;
        }
    }
    end as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn go() -> &'static Scanner {
        of(crate::langs::by_name("go").unwrap())
    }

    fn labels(text: &str) -> Vec<String> {
        go().scan(text)
            .iter()
            .map(|d| {
                format!(
                    "{} {} {}-{} @{}",
                    d.kind,
                    if d.container.is_empty() { d.name.clone() } else { format!("{}.{}", d.container, d.name) },
                    d.line,
                    d.end,
                    d.col
                )
            })
            .collect()
    }

    #[test]
    fn go_top_level_members_and_groups() {
        let src = "package shapes

import (
\t\"fmt\"
)

// Shape is a thing.
type Shape interface {
\tArea() float64
\tfmt.Stringer
}

type Square struct {
\tSide, Width float64 `json:\"side\"`
\t*Base
\tinner struct {
\t\tdepth int
\t}
}

func (s *Square) Area() float64 {
\tvar local = 1
\treturn s.Side * s.Side
}

func Total[T any](
\txs []T,
) float64 {
\treturn 0
}

const (
\tA = iota
\tB
\t_ = 3
)

var x, y = 1, 2

type (
\tID string
\tPair[K comparable] struct{ k K }
)
";
        assert_eq!(
            labels(src),
            [
                "interface shapes.Shape 8-11 @5",
                "method Shape.Area 9-9 @1",
                "struct shapes.Square 13-19 @5",
                "field Square.Side 14-14 @1",
                "field Square.Width 14-14 @7",
                "field Square.Base 15-15 @2",
                "field Square.inner 16-18 @1",
                "field inner.depth 17-17 @2",
                "method Square.Area 21-24 @17",
                "fn shapes.Total 26-30 @5",
                "const shapes.A 33-33 @1",
                "const shapes.B 34-34 @1",
                "var shapes.x 38-38 @4",
                "var shapes.y 38-38 @7",
                "type shapes.ID 41-41 @1",
                "struct shapes.Pair 42-42 @1",
            ]
        );
    }

    #[test]
    fn strings_and_comments_hide_braces_and_keywords() {
        let src = "package p\n\nvar s = `\nfunc Fake() {\n`\n\n/* func Gone() {\n*/\nfunc Real() { _ = '{'; _ = \"}\" }\n";
        assert_eq!(labels(src), ["var p.s 3-3 @4", "fn p.Real 9-9 @5"]);
        assert_eq!(go().code_lines(src), [true, true, true, false, false, true, true, false, true, true]);
        let (clean, _) = go().blank("x := \"a\\\"{\" // }\n");
        assert_eq!(clean, format!("x :={}\n", " ".repeat(12)));
    }

    #[test]
    fn a_truncated_file_ends_its_open_items_at_the_last_line() {
        assert_eq!(labels("package p\nfunc F() {\n\tx := 1\n"), ["fn p.F 2-3 @5"]);
    }

    fn ts_labels(path: &str, text: &str) -> Vec<String> {
        of(crate::langs::by_name("typescript").unwrap())
            .scan_file(path, text)
            .iter()
            .map(|d| format!("{} {}.{} {}-{}", d.kind, d.container, d.name, d.line, d.end))
            .collect()
    }

    #[test]
    fn typescript_declarations_members_and_object_types() {
        let src = r#"import {x} from "./x";

/** A shape. */
export interface Shape {
  readonly area(): number;
  name?: string;
  get size(): number;
  [key: string]: unknown;
}

export abstract class Square implements Shape {
  static count = 0;
  #secret: string;
  private readonly opts: {
    depth: number;
  };
  constructor(public side: number) {
    const local = 1;
  }
  get size(): number {
    return this.side;
  }
  async *items<T>(): AsyncIterable<T> {}
}

export type Options = Merge<Base, {
  url: string;
}>;
export type Pair = Merge<A, {left: string; right?: number}>;
type Union =
  | 'a'
  | 'b';

export const enum Color {
  Red = 1,
  Green,
}

export function total(shapes: Shape[]): number {
  let sum = 0;
  return sum;
}

export const handler = async (event: Event) => {
  return event;
};

declare module "got" {
  function extra(): void;
}
"#;
        assert_eq!(
            ts_labels("src/shapes.ts", src),
            [
                "interface shapes.Shape 4-9",
                "method Shape.area 5-5",
                "property Shape.name 6-6",
                "method Shape.size 7-7",
                "class shapes.Square 11-24",
                "property Square.count 12-12",
                "property Square.#secret 13-13",
                "property Square.opts 14-16",
                "property opts.depth 15-15",
                "ctor Square.Square 17-19",
                "method Square.size 20-22",
                "method Square.items 23-23",
                "type shapes.Options 26-28",
                "property Options.url 27-27",
                "type shapes.Pair 29-29",
                "property shapes.left 29-29",
                "property shapes.right 29-29",
                "type shapes.Union 30-32",
                "enum shapes.Color 34-37",
                "variant Color.Red 35-35",
                "variant Color.Green 36-36",
                "fn shapes.total 39-42",
                "var shapes.handler 44-46",
                "fn shapes.extra 49-49",
            ]
        );
    }

    #[test]
    fn commonjs_prototype_methods_and_comma_first_declarators() {
        let src = "var a = require('a')\n  , b = require('b');\n\nfunction View(name) {\n  this.name = name\n    , x = 1;\n}\n\nView.prototype.render = function render(fn) {\n  return fn;\n};\nView.prototype.size == 2;\nvar list = [\n  1\n  , two\n];\n";
        let js = of(crate::langs::by_name("javascript").unwrap());
        let got: Vec<String> = js
            .scan_file("lib/view.js", src)
            .iter()
            .map(|d| format!("{} {}.{} {}-{}", d.kind, d.container, d.name, d.line, d.end))
            .collect();
        assert_eq!(got, ["var view.a 1-2", "var view.b 2-2", "fn view.View 4-7", "method View.render 9-11", "var view.list 13-16"]);
    }

    #[test]
    fn csharp_braces_on_their_own_lines_and_preprocessor_branches() {
        let src = r##"using System;

namespace Demo.Core
{
    [Serializable]
    public sealed class Logger : ILogger, IDisposable
    {
        const string
            A = "a",
            B = "b{";
        static readonly string Path = @"C:\x{" + """
            raw } text
            """;
        public const int Max = 10;
        ILogger _a = null!, _b = null!;
        public event EventHandler? Changed;

        static Logger()
        {
        }

        public Logger(int level)
            : base()
        {
            Level = level;
        }

        public int Level { get; private set; }

        public string Name => $"{Level}";

        public T Get<T>(string key)
            where T : class, new()
        {
            return new T();
        }

        IEnumerator IEnumerable.GetEnumerator() => throw new NotSupportedException();

#if NET8_0
        public void Write(ReadOnlySpan<char> text)
#else
        public void Write(string text)
#endif
        {
            Console.Write(text);
        }

        void Flush(
            bool hard,
            int timeout)
#if FEATURE_X
            => Console.Out.Flush()
#endif
        ;

        public enum Level2
        {
            Low = 1,
            [Obsolete] High,
        }

        public delegate void Handler(object sender);

        public record struct Point(int X, int Y);
    }
}
"##;
        let cs = of(crate::langs::by_name("csharp").unwrap());
        let got: Vec<String> =
            cs.scan_file("Logger.cs", src).iter().map(|d| format!("{} {}.{} {}-{}", d.kind, d.container, d.name, d.line, d.end)).collect();
        assert_eq!(
            got,
            [
                "class Core.Logger 6-66",
                "field Logger.Path 11-13",
                "const Logger.Max 14-14",
                "field Logger._a 15-15",
                "field Logger._b 15-15",
                "event Logger.Changed 16-16",
                "ctor Logger.Logger 18-20",
                "ctor Logger.Logger 22-26",
                "property Logger.Level 28-28",
                "property Logger.Name 30-30",
                "method Logger.Get 32-36",
                "method Logger.GetEnumerator 38-38",
                "method Logger.Write 41-47",
                "method Logger.Write 43-47",
                "method Logger.Flush 49-55",
                "enum Logger.Level2 57-61",
                "field Level2.Low 59-59",
                "field Level2.High 60-60",
                "type Logger.Handler 63-63",
                "struct Logger.Point 65-65",
            ]
        );
    }

    #[test]
    fn a_declaration_file_is_named_without_its_double_extension() {
        assert_eq!(ts_labels("types/index.d.ts", "export declare function f(): void;\n"), ["fn index.f 1-1"]);
        assert_eq!(file_stem(r"a\b\main.mjs"), "main");
        assert_eq!(file_stem(".eslintrc"), ".eslintrc");
    }

    fn py_labels(path: &str, text: &str) -> Vec<String> {
        of(crate::langs::by_name("python").unwrap())
            .scan_file(path, text)
            .iter()
            .map(|d| format!("{} {}.{} {}-{}", d.kind, d.container, d.name, d.line, d.end))
            .collect()
    }

    #[test]
    fn python_indentation_blocks_members_and_locals() {
        let src = r#""""A module."""
import os

UNIT = 1.0
A = B = 0
X, Y = 1, 2
TABLE: dict[
    str, int
] = {}

if os.name == "nt":
    def native():
        return 1
else:
    def native():
        return 2


class Shape(Base):
    """A shape."""

    sides: int = 0
    label = "shape"

    def __init__(self, name: str) -> None:
        self.name = name
        local = 1
        if name:
            self.tagged: bool = True

    @property
    def title(self) -> str:
        def inner():
            return "def fake():"
        return self.name

# a comment at the left edge
    def area(
        self,
        scale: float,
    ) -> float:
        self.name = "again"
        return 0.0

    async def grow(self): ...

    class Meta:
        ordering = "name"


def outer(x):
    """Only a docstring
    over two lines."""


def factory():
    class Local:
        pass

    def helper():
        pass
    value = \
        2
    return Local


with open(__file__) as handle:
    DATA = handle.read()
"#;
        // Both branches of an `if` define; an attribute assigned again is
        // defined where it is first assigned; a function nested in a method
        // is the class's (as SCIP stores it), one nested in a function is
        // that function's; a comment at the left edge closes no block.
        assert_eq!(
            py_labels("pkg/shapes.py", src),
            [
                "var shapes.UNIT 4-4",
                "var shapes.A 5-5",
                "var shapes.B 5-5",
                "var shapes.X 6-6",
                "var shapes.Y 6-6",
                "var shapes.TABLE 7-9",
                "fn shapes.native 12-13",
                "fn shapes.native 15-16",
                "class shapes.Shape 19-48",
                "field Shape.sides 22-22",
                "field Shape.label 23-23",
                "ctor Shape.Shape 25-29",
                "field Shape.name 26-26",
                "field Shape.tagged 29-29",
                "method Shape.title 32-35",
                "method Shape.inner 33-34",
                "method Shape.area 38-43",
                "method Shape.grow 45-45",
                "class Shape.Meta 47-48",
                "field Meta.ordering 48-48",
                "fn shapes.outer 51-53",
                "fn shapes.factory 56-64",
                "class factory.Local 57-58",
                "fn factory.helper 60-61",
                "var shapes.handle 67-67",
                "var shapes.DATA 68-68",
            ]
        );
    }
}

#[cfg(test)]
mod jvm_tests {
    use super::of;

    fn labels(lang: &str, path: &str, text: &str) -> Vec<String> {
        of(crate::langs::by_name(lang).unwrap())
            .scan_file(path, text)
            .iter()
            .map(|d| format!("{} {}.{} {}-{}", d.kind, d.container, d.name, d.line, d.end))
            .collect()
    }

    /// Bodies after a `throws` or `implements` line, a `throws` clause
    /// ending a bodiless method, fields with several declarators, local
    /// classes, anonymous classes (in a value on the next line, in a field,
    /// in an enum constant), records and annotation types.
    #[test]
    fn java_declarations_members_and_anonymous_classes() {
        let src = r#"package demo.shapes;

import java.util.List;

/** A shape. */
@Deprecated
public abstract class Shape<T extends Comparable<T>>
    implements Comparable<Shape<T>>, java.io.Serializable {
  private static final long serialVersionUID = 1L;
  protected int sides, corners;
  String label = "{", note = "x";

  public Shape(int sides) {
    this.sides = sides;
  }

  @Override
  public abstract int compareTo(Shape<T> other);

  public <R> List<R> map(
      java.util.function.Function<T, R> f)
      throws java.io.IOException {
    class Local {
      int x;
    }
    Runnable r =
        new Runnable() {
          @Override
          public void run() {}
        };
    return null;
  }

  static final Comparable<String> NAME = new Comparable<String>() {
    @Override
    public int compareTo(String o) {
      return 0;
    }
  };

  abstract void draw(int scale)
      throws Exception;

  enum Kind {
    ROUND("r") {
      @Override
      String code() {
        return "o";
      }
    },
    FLAT("f");

    private final String tag;

    Kind(String tag) {
      this.tag = tag;
    }

    String code() {
      return tag;
    }
  }

  public interface Visitor {
    void visit(Shape<?> s);
  }

  @interface Marker {
    String value() default "";
  }

  record Point(int x, int y) {}
}
"#;
        assert_eq!(
            labels("java", "src/demo/shapes/Shape.java", src),
            [
                "class shapes.Shape 7-73",
                "field Shape.serialVersionUID 9-9",
                "field Shape.sides 10-10",
                "field Shape.corners 10-10",
                "field Shape.label 11-11",
                "field Shape.note 11-11",
                "ctor Shape.Shape 13-15",
                "method Shape.compareTo 18-18",
                "method Shape.map 20-32",
                "class map.Local 23-25",
                "field Local.x 24-24",
                "method map.run 29-29",
                "field Shape.NAME 34-39",
                "method NAME.compareTo 36-38",
                "method Shape.draw 41-42",
                "enum Shape.Kind 44-62",
                "field Kind.ROUND 45-50",
                "method ROUND.code 47-49",
                "field Kind.FLAT 51-51",
                "field Kind.tag 53-53",
                "ctor Kind.Kind 55-57",
                "method Kind.code 59-61",
                "interface Shape.Visitor 64-66",
                "method Visitor.visit 65-65",
                "interface Shape.Marker 68-70",
                "method Marker.value 69-69",
                "class Shape.Point 72-72",
            ]
        );
    }

    /// A primary constructor on its own line, its properties, supertypes on
    /// later lines, template expressions holding strings and braces,
    /// companion and object-expression members, enum entries with bodies,
    /// extension functions.
    #[test]
    fn kotlin_headers_templates_and_objects() {
        let src = r#"@file:JvmName("Shapes")

package demo.shapes

import kotlin.math.PI

const val UNIT = 1.0

public class Circle
private constructor(
  public val radius: Double,
  private val label: String = "c",
) :
  Shape,
  Comparable<Circle> {
  override val area: Double
    get() = PI * radius * radius

  public constructor(r: Int) : this(r.toDouble())

  override fun compareTo(other: Circle): Int = radius.compareTo(other.radius)

  fun describe(): String {
    val local = "${radius} and ${label.let { "x}" }}"
    return local
  }

  companion object {
    fun unit(): Circle = Circle(1)
  }
}

data class Point(val x: Int, val y: Int)

interface Shape {
  val area: Double
}

enum class Color(val rgb: Int) {
  RED(0xff0000) {
    override fun hex() = "f00"
  },
  GREEN(0x00ff00);

  open fun hex(): String = rgb.toString(16)
}

object Registry {
  private val visitor = object : Visitor {
    override fun visit(s: Shape) {}
  }
}

fun <T> List<T>.second(): T = this[1]

typealias Shapes = List<Shape>
"#;
        assert_eq!(
            labels("kotlin", "src/demo/shapes/Shapes.kt", src),
            [
                "property shapes.UNIT 7-7",
                "class shapes.Circle 9-31",
                "property Circle.radius 11-11",
                "property Circle.label 12-12",
                "property Circle.area 16-16",
                "ctor Circle.Circle 19-19",
                "method Circle.compareTo 21-21",
                "method Circle.describe 23-26",
                "method Circle.unit 29-29",
                "class shapes.Point 33-33",
                "property Point.x 33-33",
                "property Point.y 33-33",
                "interface shapes.Shape 35-37",
                "property Shape.area 36-36",
                "enum shapes.Color 39-46",
                "property Color.rgb 39-39",
                "variant Color.RED 40-42",
                "method RED.hex 41-41",
                "variant Color.GREEN 43-43",
                "method Color.hex 45-45",
                "object shapes.Registry 48-52",
                "property Registry.visitor 49-51",
                "method visitor.visit 50-50",
                "fn shapes.second 54-54",
                "type shapes.Shapes 56-56",
            ]
        );
    }
}
