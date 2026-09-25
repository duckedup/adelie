//! A hand-rolled regex engine for `ScalarFunc::RegexpMatch` (adelie-1st.1, D0004: no crate).
//! Parses to an AST, compiles to a Thompson NFA program, and matches with a Pike VM: an
//! explicit thread list per input position, no recursion, no backtracking.

use crate::exec::kernels::boolean::BoolBuilder;
use crate::exec::{Column, ExecError};
use crate::types::{DataType, Value};

const MAX_DEPTH: usize = 128;
const MAX_REPEAT: u32 = 1000;
/// Nested counted repeats multiply (`((a{1000}){1000})`), so the compiled size is capped too.
const MAX_PROGRAM: u64 = 100_000;

/// A compiled pattern. `PartialEq` compares only the source text (contract C1), not the
/// compiled program.
#[derive(Debug, Clone)]
pub struct Regex {
    pattern: String,
    prog: Vec<Inst>,
    classes: Vec<CharClass>,
    entry: usize,
}

impl PartialEq for Regex {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern
    }
}

impl Regex {
    /// Parses and compiles `pattern`. `ExecError::Invalid` names any unsupported construct
    /// (backrefs, lookaround, lazy quantifiers, named classes) or a parse failure.
    pub fn compile(pattern: &str) -> Result<Regex, ExecError> {
        let ast = Parser::parse(pattern)?;
        if program_size(&ast) > MAX_PROGRAM {
            return Err(ExecError::Invalid(format!(
                "regex compiles to more than {MAX_PROGRAM} instructions"
            )));
        }
        let mut prog = Vec::new();
        let mut classes = Vec::new();
        let frag = compile_ast(&mut prog, &mut classes, &ast);
        let match_idx = prog.len();
        prog.push(Inst::Match);
        patch(&mut prog, &frag.out, match_idx);
        // Wraps the pattern so it can start matching at any position: try the pattern here,
        // or skip one char and retry at the next position (search, not anchored match).
        let any_idx = prog.len();
        prog.push(Inst::Any(NONE));
        let split_idx = prog.len();
        prog.push(Inst::Split(frag.start, any_idx));
        prog[any_idx] = Inst::Any(split_idx);
        Ok(Regex {
            pattern: pattern.to_string(),
            prog,
            classes,
            entry: split_idx,
        })
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// An unanchored search: true iff the pattern matches anywhere in `s`. O(chars × program
    /// size), one Pike-VM pass with no backtracking.
    pub(crate) fn is_match(&self, s: &str) -> bool {
        let chars: Vec<char> = s.chars().collect();
        let len = chars.len();
        let mut clist = Vec::new();
        let mut seen = vec![false; self.prog.len()];
        if add_thread(&self.prog, &mut clist, &mut seen, self.entry, 0, len) {
            return true;
        }
        for (pos, &c) in chars.iter().enumerate() {
            let mut nlist = Vec::new();
            let mut nseen = vec![false; self.prog.len()];
            let mut matched = false;
            for &pc in &clist {
                let (consumes, next) = match self.prog[pc] {
                    Inst::Char(want, next) => (want == c, next),
                    Inst::Any(next) => (true, next),
                    Inst::Class(idx, next) => (class_match(&self.classes[idx], c), next),
                    other => {
                        unreachable!("only consuming instructions reach the thread list: {other:?}")
                    }
                };
                if consumes && add_thread(&self.prog, &mut nlist, &mut nseen, next, pos + 1, len) {
                    matched = true;
                }
            }
            if matched {
                return true;
            }
            clist = nlist;
        }
        false
    }
}

/// The BOOL result of `RegexpMatch`, NULL where `col` is NULL. `col` must be STRING (`func_type`
/// checks this before `eval_func` ever calls here).
pub(crate) fn eval_match(col: &Column, re: &Regex) -> Result<Column, ExecError> {
    if col.data_type() != &DataType::String {
        return Err(ExecError::Plan(format!(
            "regexp_match operand must be STRING, found {}",
            col.data_type()
        )));
    }
    let mut out = BoolBuilder::new();
    for i in 0..col.len() {
        if col.is_null(i) {
            out.push(None);
            continue;
        }
        let Value::String(s) = col.get(i) else {
            unreachable!("checked STRING above")
        };
        out.push(Some(re.is_match(&s)));
    }
    Ok(out.finish())
}

// ---- NFA program ----------------------------------------------------------------------

const NONE: usize = usize::MAX;

#[derive(Debug, Clone, Copy)]
enum Inst {
    Char(char, usize),
    Any(usize),
    Class(usize, usize),
    Split(usize, usize),
    Jmp(usize),
    Start(usize),
    End(usize),
    Match,
}

/// A dangling "next" (or `Split`'s second arm) that `patch` fills in once the following
/// instruction's address is known.
#[derive(Clone, Copy)]
enum Out {
    Next(usize),
    Split2(usize),
}

struct Frag {
    start: usize,
    out: Vec<Out>,
}

fn patch(prog: &mut [Inst], outs: &[Out], target: usize) {
    for &o in outs {
        match o {
            Out::Next(i) => {
                prog[i] = match prog[i] {
                    Inst::Char(c, _) => Inst::Char(c, target),
                    Inst::Any(_) => Inst::Any(target),
                    Inst::Class(cl, _) => Inst::Class(cl, target),
                    Inst::Jmp(_) => Inst::Jmp(target),
                    Inst::Start(_) => Inst::Start(target),
                    Inst::End(_) => Inst::End(target),
                    other => other,
                };
            }
            Out::Split2(i) => {
                if let Inst::Split(a, _) = prog[i] {
                    prog[i] = Inst::Split(a, target);
                }
            }
        }
    }
}

/// Thompson construction, one arm per `Ast` node. Recursion here is bounded by the parser's
/// own 128-deep nesting cap, not by input length (the VM that runs the result never recurses).
fn compile_ast(prog: &mut Vec<Inst>, classes: &mut Vec<CharClass>, ast: &Ast) -> Frag {
    match ast {
        Ast::Empty => {
            let idx = prog.len();
            prog.push(Inst::Jmp(NONE));
            Frag {
                start: idx,
                out: vec![Out::Next(idx)],
            }
        }
        Ast::Char(c) => leaf(prog, Inst::Char(*c, NONE)),
        Ast::Any => leaf(prog, Inst::Any(NONE)),
        Ast::Class(cc) => {
            let idx = classes.len();
            classes.push(cc.clone());
            leaf(prog, Inst::Class(idx, NONE))
        }
        Ast::StartAnchor => leaf(prog, Inst::Start(NONE)),
        Ast::EndAnchor => leaf(prog, Inst::End(NONE)),
        Ast::Concat(parts) => {
            let mut iter = parts.iter();
            let first = iter.next().expect("non-empty by construction");
            let mut frag = compile_ast(prog, classes, first);
            for part in iter {
                let next = compile_ast(prog, classes, part);
                patch(prog, &frag.out, next.start);
                frag = Frag {
                    start: frag.start,
                    out: next.out,
                };
            }
            frag
        }
        Ast::Alt(branches) => {
            let mut iter = branches.iter().rev();
            let first = iter.next().expect("non-empty by construction");
            let mut acc = compile_ast(prog, classes, first);
            for b in iter {
                let f = compile_ast(prog, classes, b);
                let idx = prog.len();
                prog.push(Inst::Split(f.start, acc.start));
                let mut out = f.out;
                out.extend(acc.out);
                acc = Frag { start: idx, out };
            }
            acc
        }
        Ast::Star(inner) => {
            let f = compile_ast(prog, classes, inner);
            let idx = prog.len();
            prog.push(Inst::Split(f.start, NONE));
            patch(prog, &f.out, idx);
            Frag {
                start: idx,
                out: vec![Out::Split2(idx)],
            }
        }
        Ast::Plus(inner) => {
            let f = compile_ast(prog, classes, inner);
            let idx = prog.len();
            prog.push(Inst::Split(f.start, NONE));
            patch(prog, &f.out, idx);
            Frag {
                start: f.start,
                out: vec![Out::Split2(idx)],
            }
        }
        Ast::Question(inner) => {
            let f = compile_ast(prog, classes, inner);
            let idx = prog.len();
            prog.push(Inst::Split(f.start, NONE));
            let mut out = f.out;
            out.push(Out::Split2(idx));
            Frag { start: idx, out }
        }
        Ast::Repeat(inner, m, n) => compile_ast(prog, classes, &expand_repeat(inner, *m, *n)),
    }
}

/// An upper bound on `compile_ast`'s instruction count, saturating, checked before the
/// expansion of `{m,n}` clones anything.
fn program_size(ast: &Ast) -> u64 {
    match ast {
        Ast::Empty
        | Ast::Char(_)
        | Ast::Any
        | Ast::Class(_)
        | Ast::StartAnchor
        | Ast::EndAnchor => 1,
        Ast::Concat(parts) => parts
            .iter()
            .fold(0u64, |a, p| a.saturating_add(program_size(p))),
        Ast::Alt(parts) => parts.iter().fold(0u64, |a, p| {
            a.saturating_add(program_size(p)).saturating_add(1)
        }),
        Ast::Star(inner) | Ast::Plus(inner) | Ast::Question(inner) => {
            program_size(inner).saturating_add(1)
        }
        Ast::Repeat(inner, m, n) => {
            let copies = u64::from(n.unwrap_or(*m).max(*m)).saturating_add(1);
            program_size(inner).saturating_add(1).saturating_mul(copies)
        }
    }
}

fn leaf(prog: &mut Vec<Inst>, inst: Inst) -> Frag {
    let idx = prog.len();
    prog.push(inst);
    Frag {
        start: idx,
        out: vec![Out::Next(idx)],
    }
}

/// `{m}`/`{m,}`/`{m,n}` as `m` mandatory copies plus either a trailing `*` (unbounded) or
/// `n - m` trailing `?`s (bounded): no dedicated counting instruction needed.
fn expand_repeat(inner: &Ast, m: u32, n: Option<u32>) -> Ast {
    let mut parts: Vec<Ast> = (0..m).map(|_| inner.clone()).collect();
    match n {
        None => parts.push(Ast::Star(Box::new(inner.clone()))),
        Some(n) => parts.extend((m..n).map(|_| Ast::Question(Box::new(inner.clone())))),
    }
    match parts.len() {
        0 => Ast::Empty,
        1 => parts.pop().expect("checked len == 1"),
        _ => Ast::Concat(parts),
    }
}

/// Follows epsilon transitions (`Split`/`Jmp`/anchors) with an explicit stack — never
/// recursion — and returns true the moment a `Match` is reached from `pc`.
fn add_thread(
    prog: &[Inst],
    list: &mut Vec<usize>,
    seen: &mut [bool],
    pc: usize,
    pos: usize,
    len: usize,
) -> bool {
    let mut stack = vec![pc];
    while let Some(pc) = stack.pop() {
        if seen[pc] {
            continue;
        }
        seen[pc] = true;
        match prog[pc] {
            Inst::Jmp(n) => stack.push(n),
            Inst::Split(a, b) => {
                stack.push(b);
                stack.push(a);
            }
            Inst::Start(n) => {
                if pos == 0 {
                    stack.push(n);
                }
            }
            Inst::End(n) => {
                if pos == len {
                    stack.push(n);
                }
            }
            Inst::Match => return true,
            Inst::Char(..) | Inst::Any(..) | Inst::Class(..) => list.push(pc),
        }
    }
    false
}

// ---- character classes -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum ClassItem {
    Range(char, char),
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
}

#[derive(Debug, Clone, PartialEq)]
struct CharClass {
    items: Vec<ClassItem>,
    negate: bool,
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn item_match(item: &ClassItem, c: char) -> bool {
    match item {
        ClassItem::Range(lo, hi) => *lo <= c && c <= *hi,
        ClassItem::Digit => c.is_ascii_digit(),
        ClassItem::NotDigit => !c.is_ascii_digit(),
        ClassItem::Word => is_word_char(c),
        ClassItem::NotWord => !is_word_char(c),
        ClassItem::Space => c.is_whitespace(),
        ClassItem::NotSpace => !c.is_whitespace(),
    }
}

fn class_match(cc: &CharClass, c: char) -> bool {
    cc.items.iter().any(|it| item_match(it, c)) != cc.negate
}

// ---- parsing ------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Ast {
    Empty,
    Char(char),
    Any,
    Class(CharClass),
    Concat(Vec<Ast>),
    Alt(Vec<Ast>),
    Star(Box<Ast>),
    Plus(Box<Ast>),
    Question(Box<Ast>),
    Repeat(Box<Ast>, u32, Option<u32>),
    StartAnchor,
    EndAnchor,
}

enum ClassAtom {
    Ch(char),
    Item(ClassItem),
}

struct Parser<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn parse(pattern: &'a str) -> Result<Ast, ExecError> {
        let mut p = Parser {
            chars: pattern.chars().peekable(),
            depth: 0,
        };
        let ast = p.parse_alt()?;
        if let Some(c) = p.chars.next() {
            return Err(ExecError::Invalid(format!("unexpected '{c}' in regex")));
        }
        Ok(ast)
    }

    fn parse_alt(&mut self) -> Result<Ast, ExecError> {
        let mut branches = vec![self.parse_concat()?];
        while self.chars.peek() == Some(&'|') {
            self.chars.next();
            branches.push(self.parse_concat()?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().expect("checked len == 1")
        } else {
            Ast::Alt(branches)
        })
    }

    fn parse_concat(&mut self) -> Result<Ast, ExecError> {
        let mut parts = Vec::new();
        while !matches!(self.chars.peek(), None | Some('|') | Some(')')) {
            parts.push(self.parse_repeat()?);
        }
        Ok(match parts.len() {
            0 => Ast::Empty,
            1 => parts.pop().expect("checked len == 1"),
            _ => Ast::Concat(parts),
        })
    }

    fn parse_repeat(&mut self) -> Result<Ast, ExecError> {
        let atom = self.parse_atom()?;
        match self.chars.peek() {
            Some('*') => {
                self.chars.next();
                self.reject_lazy()?;
                Ok(Ast::Star(Box::new(atom)))
            }
            Some('+') => {
                self.chars.next();
                self.reject_lazy()?;
                Ok(Ast::Plus(Box::new(atom)))
            }
            Some('?') => {
                self.chars.next();
                self.reject_lazy()?;
                Ok(Ast::Question(Box::new(atom)))
            }
            Some('{') => self.parse_bound(atom),
            _ => Ok(atom),
        }
    }

    /// Lazy quantifiers (`*?`, `+?`, `??`, `{m,n}?`) are unsupported (SPEC scope).
    fn reject_lazy(&mut self) -> Result<(), ExecError> {
        if self.chars.peek() == Some(&'?') {
            return Err(ExecError::Invalid(
                "lazy quantifiers are not supported".into(),
            ));
        }
        Ok(())
    }

    fn parse_number(&mut self) -> Result<u32, ExecError> {
        let mut s = String::new();
        while let Some(&c) = self.chars.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            s.push(c);
            self.chars.next();
        }
        s.parse::<u32>()
            .map_err(|_| ExecError::Invalid("expected a number in a repetition bound".into()))
    }

    fn parse_bound(&mut self, atom: Ast) -> Result<Ast, ExecError> {
        self.chars.next(); // '{'
        let m = self.parse_number()?;
        let n = if self.chars.peek() == Some(&',') {
            self.chars.next();
            if self.chars.peek() == Some(&'}') {
                None
            } else {
                Some(self.parse_number()?)
            }
        } else {
            Some(m)
        };
        if self.chars.next() != Some('}') {
            return Err(ExecError::Invalid("unterminated repetition bound".into()));
        }
        if m > MAX_REPEAT || n.is_some_and(|n| n > MAX_REPEAT) {
            return Err(ExecError::Invalid(format!(
                "repetition bound exceeds {MAX_REPEAT}"
            )));
        }
        if let Some(n) = n
            && n < m
        {
            return Err(ExecError::Invalid(
                "repetition bound {m,n} has n < m".into(),
            ));
        }
        self.reject_lazy()?;
        Ok(Ast::Repeat(Box::new(atom), m, n))
    }

    fn parse_atom(&mut self) -> Result<Ast, ExecError> {
        match self.chars.next() {
            None => Err(ExecError::Invalid("unexpected end of regex".into())),
            Some('.') => Ok(Ast::Any),
            Some('^') => Ok(Ast::StartAnchor),
            Some('$') => Ok(Ast::EndAnchor),
            Some('(') => self.parse_group(),
            Some('[') => self.parse_class(),
            Some('\\') => self.parse_escape(),
            Some(')') => Err(ExecError::Invalid("unmatched ')' in regex".into())),
            Some(c @ ('*' | '+' | '?')) => {
                Err(ExecError::Invalid(format!("'{c}' has nothing to repeat")))
            }
            Some(c) => Ok(Ast::Char(c)),
        }
    }

    fn parse_group(&mut self) -> Result<Ast, ExecError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ExecError::Invalid(format!(
                "regex nesting exceeds depth {MAX_DEPTH}"
            )));
        }
        if self.chars.peek() == Some(&'?') {
            let mut probe = self.chars.clone();
            probe.next();
            if probe.peek() == Some(&':') {
                self.chars.next();
                self.chars.next();
            } else {
                return Err(ExecError::Invalid(
                    "unsupported group construct '(?...)' (lookaround is not supported)".into(),
                ));
            }
        }
        let inner = self.parse_alt()?;
        if self.chars.next() != Some(')') {
            return Err(ExecError::Invalid("unterminated group".into()));
        }
        self.depth -= 1;
        Ok(inner)
    }

    fn parse_escape(&mut self) -> Result<Ast, ExecError> {
        match self.escape_char()? {
            ClassAtom::Ch(c) => Ok(Ast::Char(c)),
            ClassAtom::Item(item) => Ok(Ast::Class(CharClass {
                items: vec![item],
                negate: false,
            })),
        }
    }

    /// The escapes shared by top-level atoms and bracket-class members: the literal escapes
    /// plus `\d \w \s \D \W \S`. Anything else (backrefs, `\b`, …) is `Invalid`.
    fn escape_char(&mut self) -> Result<ClassAtom, ExecError> {
        match self.chars.next() {
            None => Err(ExecError::Invalid("trailing '\\' in regex".into())),
            Some(c) => match c {
                '.' | '\\' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '*' | '+' | '?' | '^'
                | '$' | '/' | '-' => Ok(ClassAtom::Ch(c)),
                'd' => Ok(ClassAtom::Item(ClassItem::Digit)),
                'D' => Ok(ClassAtom::Item(ClassItem::NotDigit)),
                'w' => Ok(ClassAtom::Item(ClassItem::Word)),
                'W' => Ok(ClassAtom::Item(ClassItem::NotWord)),
                's' => Ok(ClassAtom::Item(ClassItem::Space)),
                'S' => Ok(ClassAtom::Item(ClassItem::NotSpace)),
                other => Err(ExecError::Invalid(format!(
                    "unsupported escape '\\{other}'"
                ))),
            },
        }
    }

    fn parse_class(&mut self) -> Result<Ast, ExecError> {
        let negate = self.chars.peek() == Some(&'^');
        if negate {
            self.chars.next();
        }
        let mut items = Vec::new();
        let mut first = true;
        loop {
            match self.chars.peek() {
                None => return Err(ExecError::Invalid("unterminated character class".into())),
                Some(']') if !first => {
                    self.chars.next();
                    break;
                }
                _ => {}
            }
            first = false;
            self.parse_class_member(&mut items)?;
        }
        if items.is_empty() {
            return Err(ExecError::Invalid("empty character class".into()));
        }
        Ok(Ast::Class(CharClass { items, negate }))
    }

    fn parse_class_char(&mut self) -> Result<ClassAtom, ExecError> {
        match self.chars.next() {
            None => Err(ExecError::Invalid("unterminated character class".into())),
            Some('\\') => self.escape_char(),
            Some('[') => Err(ExecError::Invalid("nested '[' in character class".into())),
            Some(c) => Ok(ClassAtom::Ch(c)),
        }
    }

    /// One class member: a shorthand class, a lone char, or a `lo-hi` range (only when both
    /// ends are literal chars, in order).
    fn parse_class_member(&mut self, items: &mut Vec<ClassItem>) -> Result<(), ExecError> {
        let lo = match self.parse_class_char()? {
            ClassAtom::Item(it) => {
                items.push(it);
                return Ok(());
            }
            ClassAtom::Ch(c) => c,
        };
        let is_range = self.chars.peek() == Some(&'-') && {
            let mut probe = self.chars.clone();
            probe.next();
            !matches!(probe.peek(), None | Some(']'))
        };
        if !is_range {
            items.push(ClassItem::Range(lo, lo));
            return Ok(());
        }
        self.chars.next(); // '-'
        let hi = match self.parse_class_char()? {
            ClassAtom::Ch(c) => c,
            ClassAtom::Item(_) => {
                return Err(ExecError::Invalid(
                    "character class range cannot end with a shorthand class".into(),
                ));
            }
        };
        if hi < lo {
            return Err(ExecError::Invalid(
                "character class range is out of order".into(),
            ));
        }
        items.push(ClassItem::Range(lo, hi));
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn nested_counted_repeats_are_an_error_not_an_allocation() {
        let err = Regex::compile("((a{1000}){1000}){1000}").unwrap_err();
        assert!(
            matches!(err, ExecError::Invalid(ref m) if m.contains("instructions")),
            "{err:?}"
        );
        assert!(Regex::compile("(a{1000}){40}").is_ok());
    }

    fn m(pattern: &str, input: &str) -> bool {
        Regex::compile(pattern).unwrap().is_match(input)
    }

    #[test]
    fn literal_alternation_and_anchors() {
        let cases: &[(&str, &str, bool)] = &[
            ("cat", "concatenate", true),
            ("^cat", "concatenate", false),
            ("^cat", "catalog", true),
            ("dog$", "hot dog", true),
            ("dog$", "dogged", false),
            ("cat|dog", "I have a dog", true),
            ("cat|dog", "I have a fish", false),
        ];
        for (pattern, input, want) in cases {
            assert_eq!(m(pattern, input), *want, "{pattern:?} vs {input:?}");
        }
    }

    #[test]
    fn classes_and_bounds() {
        let cases: &[(&str, &str, bool)] = &[
            ("[a-c]+", "abcabc", true),
            ("[^a-c]+", "abc", false),
            ("[^a-c]", "xyz", true),
            (r"\d{3}-\d{4}", "call 555-1234 now", true),
            (r"\d{3}-\d{4}", "call 55-1234 now", false),
            ("a{2,4}b", "aaab", true),
            ("a{2,4}b", "ab", false),
            ("colou?r", "color", true),
            ("colou?r", "colour", true),
        ];
        for (pattern, input, want) in cases {
            assert_eq!(m(pattern, input), *want, "{pattern:?} vs {input:?}");
        }
    }

    #[test]
    fn unicode_scalar_values_match() {
        assert!(m("h.llo", "héllo"));
        assert!(m("[é]", "café"));
    }

    #[test]
    fn groups_are_non_capturing_and_identical() {
        assert!(m("(?:ab)+c", "ababc"));
        assert!(m("(ab)+c", "ababc"));
    }

    #[test]
    fn backreference_is_invalid() {
        let err = Regex::compile(r"(a)\1").unwrap_err();
        assert!(matches!(err, ExecError::Invalid(_)));
    }

    #[test]
    fn lookahead_is_invalid() {
        let err = Regex::compile("a(?=b)").unwrap_err();
        assert!(matches!(err, ExecError::Invalid(_)));
    }

    #[test]
    fn lazy_quantifier_is_invalid() {
        let err = Regex::compile("a*?").unwrap_err();
        assert!(matches!(err, ExecError::Invalid(_)));
    }

    #[test]
    fn depth_cap_is_enforced() {
        let pattern = format!("{}a{}", "(".repeat(130), ")".repeat(130));
        let err = Regex::compile(&pattern).unwrap_err();
        assert!(matches!(err, ExecError::Invalid(_)));
    }

    #[test]
    fn repeat_bound_above_1000_is_invalid() {
        let err = Regex::compile("a{1001}").unwrap_err();
        assert!(matches!(err, ExecError::Invalid(_)));
    }

    /// Falsify: a backtracking engine spends exponential time on these classic pathological
    /// patterns. The NFA simulation stays linear and returns `false` for all three.
    #[test]
    #[cfg_attr(miri, ignore)] // 10k-char NFA scan, no UB surface beyond the small tests
    fn no_catastrophic_backtracking() {
        let long_a = "a".repeat(10_000);
        for pattern in ["(a*)*b", "(a|a)*b", "(a|aa)*c"] {
            let re = Regex::compile(pattern).unwrap();
            assert!(!re.is_match(&long_a), "{pattern} should not match");
        }
    }

    #[test]
    fn regex_partial_eq_compares_pattern_text_only() {
        assert_eq!(Regex::compile("a+").unwrap(), Regex::compile("a+").unwrap());
        assert_ne!(Regex::compile("a+").unwrap(), Regex::compile("a*").unwrap());
    }
}
