// The sequencer language: a deliberately tiny subset of webmidi's.
//
//   program := stmt*
//   stmt    := "every" INT block                    -- every INT sixteenths
//            | "at" POS block                       -- when the position matches
//            | "note" EXPR (("dur" | "vel" | "ch") EXPR)*  -- each option at most once
//   block   := "{" program "}"
//   POS     := field ["." field ["." field]]        -- bar.beat.sixteenth, 0-based
//   field   := term ("|" term)*                     -- matches if any term does
//   term    := INT | [INT] VAR [("+" | "-") INT]    -- 3, x, 4x, 2y+1, 2n-1
//   EXPR    := sum, with sum := product (("+" | "-") product)*, product := unary ("*" unary)*,
//              unary := "-" unary | INT | NOTE | "r" INT    -- rN: random from 0 to N-1
//   NOTE    := c3 | c#3 | db3                       -- Ableton's convention: c3 = 60
//
// Expressions are evaluated each time a note plays, so `vel 1+r127` differs per note. They can be
// one word (`c3+r12`) or have spaces around the operators (`c3 + r12`).
//
// Positions count from 0 (Ableton's 1.1.1 is 0.0.0). Like CSS's nth-child, a variable stands for
// any whole number from 0 up, so `4x` matches bars 0, 4, 8, ... A variable used in more than one
// field must have the same value in each: `x.x.0` matches 1.1.0 and 2.2.0 but not 1.2.0.
// Missing fields match anything. Comments start with "--".

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Every { n: u32, body: Program },
    At { pattern: Pattern, body: Program },
    Note { note: Expr, dur: Expr, vel: Expr, ch: Expr },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Num(i64),
    /// A random number from 0 to n - 1, picked anew every time.
    Random(i64),
    Neg(Box<Expr>),
    Bin(Op, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Add,
    Sub,
    Mul,
}

impl Expr {
    pub fn eval(&self, rng: &mut Rng) -> i64 {
        match self {
            Expr::Num(n) => *n,
            Expr::Random(n) => rng.below(*n),
            Expr::Neg(e) => e.eval(rng).saturating_neg(),
            Expr::Bin(op, a, b) => {
                let (a, b) = (a.eval(rng), b.eval(rng));
                match op {
                    Op::Add => a.saturating_add(b),
                    Op::Sub => a.saturating_sub(b),
                    Op::Mul => a.saturating_mul(b),
                }
            }
        }
    }

    /// The value, if it's the same every time (no random parts).
    fn constant(&self) -> Option<i64> {
        (!self.is_random()).then(|| self.eval(&mut Rng::new(1)))
    }

    fn is_random(&self) -> bool {
        match self {
            Expr::Num(_) => false,
            Expr::Random(_) => true,
            Expr::Neg(e) => e.is_random(),
            Expr::Bin(_, a, b) => a.is_random() || b.is_random(),
        }
    }
}

/// A small, fast random number generator (xorshift64*) that's fine to use on the audio thread.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(if seed == 0 { 0x9e37_79b9_7f4a_7c15 } else { seed })
    }

    /// From 0 to n - 1, for n >= 1.
    pub fn below(&mut self, n: i64) -> i64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        let r = x.wrapping_mul(0x2545_f491_4f6c_dd1d);
        ((r as u128 * n.max(1) as u128) >> 64) as i64
    }
}

pub type Program = Vec<Stmt>;

/// bar.beat.sixteenth; a field with no terms matches anything.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern(pub [Vec<Term>; 3]);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Term {
    Const(i64),
    /// a * var + b, for any var >= 0. `a` is at least 1.
    Linear { a: i64, var: char, b: i64 },
}

/// Where we are in the song: one step is one sixteenth note.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pos {
    /// Sixteenths since the start of the song.
    pub step: i64,
    /// All 0-based: Ableton's 1.1.1 is 0.0.0.
    pub bar: i64,
    pub beat: u32,
    pub sixteenth: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteOut {
    pub note: u8,
    pub vel: u8,
    /// 1-based MIDI channel.
    pub ch: u8,
    /// In sixteenths.
    pub dur: u32,
}

#[derive(Debug, PartialEq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

// --- Lexer ---
//
// Tokens are braces and "words": anything else up to whitespace or a brace. Words are only
// interpreted by the parser, which knows whether it expects a keyword, a number, a note or a
// position. That keeps `4x.2y+1.0` and `c#-1` single tokens without a fancy lexer.

#[derive(Debug, Clone, Copy)]
struct Token<'a> {
    text: &'a str,
    line: usize,
}

fn lex(src: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let line_no = i + 1;
        let code = line.split_once("--").map_or(line, |(code, _comment)| code);
        let mut start = None;
        for (j, c) in code.char_indices() {
            let breaks = c.is_whitespace() || c == '{' || c == '}';
            if breaks {
                if let Some(s) = start.take() {
                    tokens.push(Token { text: &code[s..j], line: line_no });
                }
                if !c.is_whitespace() {
                    tokens.push(Token { text: &code[j..j + 1], line: line_no });
                }
            } else if start.is_none() {
                start = Some(j);
            }
        }
        if let Some(s) = start {
            tokens.push(Token { text: &code[s..], line: line_no });
        }
    }
    tokens
}

// --- Parser ---

pub fn parse(src: &str) -> Result<Program, ParseError> {
    let tokens = lex(src);
    let mut p = Parser { tokens: &tokens, i: 0 };
    let program = p.program()?;
    match p.peek() {
        Some(t) => Err(p.error_at(t, format!("unexpected '{}'", t.text))),
        None => Ok(program),
    }
}

struct Parser<'a> {
    tokens: &'a [Token<'a>],
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<Token<'a>> {
        self.tokens.get(self.i).copied()
    }

    fn next(&mut self, what: &str) -> Result<Token<'a>, ParseError> {
        let t = self.peek().ok_or_else(|| ParseError {
            line: self.tokens.last().map_or(1, |t| t.line),
            message: format!("expected {what}, got the end of the code"),
        })?;
        self.i += 1;
        Ok(t)
    }

    fn error_at(&self, t: Token, message: String) -> ParseError {
        ParseError { line: t.line, message }
    }

    /// Statements until the end of the code or a closing brace (left for the caller).
    fn program(&mut self) -> Result<Program, ParseError> {
        let mut program = Vec::new();
        while let Some(t) = self.peek() {
            if t.text == "}" {
                break;
            }
            program.push(self.stmt()?);
        }
        Ok(program)
    }

    fn stmt(&mut self) -> Result<Stmt, ParseError> {
        let t = self.next("a statement")?;
        match t.text {
            "every" => {
                let n = self.next("a number of sixteenths after 'every'")?;
                match n.text.parse::<u32>() {
                    Ok(0) => Err(self.error_at(n, "'every 0' never happens".into())),
                    Ok(n) => Ok(Stmt::Every { n, body: self.block()? }),
                    Err(_) if n.text.contains('.') => Err(self.error_at(
                        n,
                        format!("'every' takes a number of sixteenths; for positions use 'at {}'", n.text),
                    )),
                    Err(_) => Err(self.error_at(n, format!("'every' takes a number, not '{}'", n.text))),
                }
            }
            "at" => {
                let p = self.next("a position after 'at'")?;
                let pattern = pattern(p.text).map_err(|m| self.error_at(p, m))?;
                Ok(Stmt::At { pattern, body: self.block()? })
            }
            "note" => {
                let (note, n) = self.expr("a note after 'note'")?;
                if note.constant().is_some_and(|n| !(0..=127).contains(&n)) {
                    return Err(self.error_at(n, "notes go from c-2 (0) to g8 (127)".into()));
                }
                let (mut dur, mut vel, mut ch) = (None, None, None);
                while let Some(opt) = self.peek() {
                    let (slot, max) = match opt.text {
                        "dur" => (&mut dur, u32::MAX as i64),
                        "vel" => (&mut vel, 127),
                        "ch" => (&mut ch, 16),
                        _ => break,
                    };
                    self.i += 1;
                    if slot.is_some() {
                        return Err(self.error_at(opt, format!("'{}' given twice", opt.text)));
                    }
                    let (value, v) = self.expr(&format!("a number after '{}'", opt.text))?;
                    if value.constant().is_some_and(|n| !(1..=max).contains(&n)) {
                        return Err(self.error_at(v, format!("'{}' needs a number from 1 to {max}", opt.text)));
                    }
                    *slot = Some(value);
                }
                Ok(Stmt::Note {
                    note,
                    dur: dur.unwrap_or(Expr::Num(1)),
                    vel: vel.unwrap_or(Expr::Num(100)),
                    ch: ch.unwrap_or(Expr::Num(1)),
                })
            }
            "{" | "}" => Err(self.error_at(t, format!("unexpected '{}'", t.text))),
            other => Err(self.error_at(t, format!("unknown statement '{other}'"))),
        }
    }

    /// An expression: one word like `1+r127`, or several with spaces around the operators. Also
    /// returns its first token, for error messages.
    fn expr(&mut self, what: &str) -> Result<(Expr, Token<'a>), ParseError> {
        const OPS: [char; 3] = ['+', '-', '*'];
        let first = self.next(what)?;
        let mut text = first.text.to_string();
        while let Some(t) = self.peek() {
            if t.text == "{" || t.text == "}" || !(text.ends_with(OPS) || t.text.starts_with(OPS)) {
                break;
            }
            text.push_str(t.text);
            self.i += 1;
        }
        let expr = parse_expr(&text).map_err(|m| self.error_at(first, m))?;
        Ok((expr, first))
    }

    fn block(&mut self) -> Result<Program, ParseError> {
        let open = self.next("'{'")?;
        if open.text != "{" {
            return Err(self.error_at(open, format!("expected '{{', got '{}'", open.text)));
        }
        let body = self.program()?;
        self.next("'}'")?; // program() only stops at '}' or the end
        Ok(body)
    }
}

/// "4x.2y+1.0" -> bar is a multiple of 4, beat is odd, sixteenth is 0.
fn pattern(text: &str) -> Result<Pattern, String> {
    let fields: Vec<&str> = text.split('.').collect();
    if fields.len() > 3 {
        return Err(format!("'{text}' has more than 3 parts (bar.beat.sixteenth)"));
    }
    let mut pattern = Pattern(Default::default());
    for (slot, field) in pattern.0.iter_mut().zip(&fields) {
        for t in field.split('|') {
            slot.push(term(t).ok_or_else(|| {
                format!("'{t}' in '{text}' should be a number or like x, 4x, 2y+1 (no spaces)")
            })?);
        }
    }
    Ok(pattern)
}

/// "3", "x", "4x", "2y+1", "2n-1"
fn term(text: &str) -> Option<Term> {
    let digits = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let (a, rest) = text.split_at(digits);
    let mut chars = rest.chars();
    let Some(var) = chars.next() else {
        return Some(Term::Const(a.parse().ok()?));
    };
    if !var.is_ascii_lowercase() {
        return None;
    }
    let a = if a.is_empty() { 1 } else { a.parse().ok().filter(|a| *a > 0)? };
    let rest = chars.as_str();
    let b = if rest.is_empty() {
        0
    } else if let Some(b) = rest.strip_prefix('+') {
        b.parse::<u32>().ok()? as i64
    } else {
        -(rest.strip_prefix('-')?.parse::<u32>().ok()? as i64)
    };
    Some(Term::Linear { a, var, b })
}

fn parse_expr(text: &str) -> Result<Expr, String> {
    let mut p = ExprParser { text, i: 0 };
    let expr = p.sum()?;
    match p.peek() {
        Some(_) => Err(format!("unexpected '{}' in '{text}'", &text[p.i..])),
        None => Ok(expr),
    }
}

/// Parses expressions within a single (joined) word.
struct ExprParser<'a> {
    text: &'a str,
    i: usize,
}

impl ExprParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.i).copied()
    }

    fn sum(&mut self) -> Result<Expr, String> {
        let mut left = self.product()?;
        while let Some(c @ (b'+' | b'-')) = self.peek() {
            self.i += 1;
            let op = if c == b'+' { Op::Add } else { Op::Sub };
            left = Expr::Bin(op, Box::new(left), Box::new(self.product()?));
        }
        Ok(left)
    }

    fn product(&mut self) -> Result<Expr, String> {
        let mut left = self.unary()?;
        while self.peek() == Some(b'*') {
            self.i += 1;
            left = Expr::Bin(Op::Mul, Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if self.peek() == Some(b'-') {
            self.i += 1;
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        let start = self.i;
        let skip_digits = |p: &mut Self| {
            let from = p.i;
            while p.peek().is_some_and(|c| c.is_ascii_digit()) {
                p.i += 1;
            }
            p.i > from
        };
        match self.peek() {
            Some(c) if c.is_ascii_digit() => {
                skip_digits(self);
                let n = self.text[start..self.i].parse().map_err(|_| "number too big".to_string())?;
                Ok(Expr::Num(n))
            }
            Some(b'r') => {
                self.i += 1;
                if !skip_digits(self) {
                    return Err(format!("'r' needs a number, like r8 (in '{}')", self.text));
                }
                match self.text[start + 1..self.i].parse::<u32>() {
                    Ok(n) if n > 0 => Ok(Expr::Random(n as i64)),
                    _ => Err(format!("'{}' should be r1 or more", &self.text[start..self.i])),
                }
            }
            Some(b'a'..=b'g' | b'A'..=b'G') => {
                // A note name: letter, optional # or b, octave (which may be negative).
                self.i += 1;
                if let Some(b'#' | b'b') = self.peek() {
                    self.i += 1;
                }
                let bytes = self.text.as_bytes();
                if self.peek() == Some(b'-') && bytes.get(self.i + 1).is_some_and(|c| c.is_ascii_digit()) {
                    self.i += 1;
                }
                let name = |p: &Self| &p.text[start..p.i];
                if !skip_digits(self) {
                    return Err(format!("'{}' needs an octave, like c3", name(self)));
                }
                let n = note_number(name(self)).ok_or_else(|| format!("'{}' is not a note", name(self)))?;
                Ok(Expr::Num(n as i64))
            }
            _ => Err(format!("expected a number, note or rN in '{}'", self.text)),
        }
    }
}

const PITCH: [(char, i32); 7] = [('c', 0), ('d', 2), ('e', 4), ('f', 5), ('g', 7), ('a', 9), ('b', 11)];

/// "c3" -> 60, "c#3" -> 61, "db3" -> 61, "c-2" -> 0, "64" -> 64
pub fn note_number(text: &str) -> Option<u8> {
    if let Ok(n) = text.parse::<u8>() {
        return (n <= 127).then_some(n);
    }
    let mut chars = text.chars();
    let letter = chars.next()?.to_ascii_lowercase();
    let pitch = PITCH.iter().find(|(c, _)| *c == letter)?.1;
    let rest = chars.as_str();
    let (accidental, octave) = match rest.strip_prefix('#') {
        Some(octave) => (1, octave),
        None => match rest.strip_prefix('b') {
            Some(octave) => (-1, octave),
            None => (0, rest),
        },
    };
    let n = (octave.parse::<i32>().ok()? + 2) * 12 + pitch + accidental;
    u8::try_from(n).ok().filter(|n| *n <= 127)
}

// --- Interpreter ---

/// Run the program for one step, calling `emit` for each note it plays. Doesn't allocate, so
/// it's fine to call from the audio thread.
pub fn run(program: &[Stmt], pos: &Pos, rng: &mut Rng, emit: &mut impl FnMut(NoteOut)) {
    for stmt in program {
        match stmt {
            Stmt::Every { n, body } => {
                if pos.step.rem_euclid(*n as i64) == 0 {
                    run(body, pos, rng, emit);
                }
            }
            Stmt::At { pattern, body } => {
                let values = [pos.bar, pos.beat as i64, pos.sixteenth as i64];
                if matches(&pattern.0, &values, Bindings::default()) {
                    run(body, pos, rng, emit);
                }
            }
            Stmt::Note { note, dur, vel, ch } => {
                let (note, vel) = (note.eval(rng), vel.eval(rng));
                let (dur, ch) = (dur.eval(rng), ch.eval(rng));
                // Velocity 0 or less means "don't play" (in MIDI it's a note-off), which makes
                // `vel r2*100` a coin flip. Notes that don't exist aren't played either.
                if (0..=127).contains(&note) && vel > 0 {
                    emit(NoteOut {
                        note: note as u8,
                        vel: vel.min(127) as u8,
                        ch: ch.clamp(1, 16) as u8,
                        dur: dur.clamp(1, u32::MAX as i64) as u32,
                    });
                }
            }
        }
    }
}

/// Variable values chosen so far. Each field binds at most one, so three slots will do.
#[derive(Default, Clone, Copy)]
struct Bindings {
    vars: [(char, i64); 3],
    len: usize,
}

impl Bindings {
    fn get(&self, var: char) -> Option<i64> {
        self.vars[..self.len].iter().find(|(v, _)| *v == var).map(|(_, k)| *k)
    }

    fn with(mut self, var: char, k: i64) -> Self {
        self.vars[self.len] = (var, k);
        self.len += 1;
        self
    }
}

/// Whether some choice of terms and variable values makes every field match its value.
fn matches(fields: &[Vec<Term>], values: &[i64], bindings: Bindings) -> bool {
    let (Some((terms, fields)), Some((&value, values))) = (fields.split_first(), values.split_first()) else {
        return true;
    };
    if terms.is_empty() {
        return matches(fields, values, bindings);
    }
    terms.iter().any(|term| match *term {
        Term::Const(c) => c == value && matches(fields, values, bindings),
        Term::Linear { a, var, b } => {
            // value = a * k + b for a whole k >= 0
            let d = value - b;
            if d < 0 || d % a != 0 {
                return false;
            }
            let k = d / a;
            match bindings.get(var) {
                Some(bound) => bound == k && matches(fields, values, bindings),
                None => matches(fields, values, bindings.with(var, k)),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos_4_4(step: i64) -> Pos {
        Pos {
            step,
            bar: step / 16,
            beat: (step % 16 / 4) as u32,
            sixteenth: (step % 4) as u32,
        }
    }

    fn steps_with_notes(src: &str, steps: i64) -> Vec<i64> {
        let program = parse(src).unwrap();
        let mut out = Vec::new();
        for step in 0..steps {
            run(&program, &pos_4_4(step), &mut Rng::new(1), &mut |_| out.push(step));
        }
        out
    }

    /// Whether `at <pattern>` fires at bar.beat.sixteenth.
    fn at(pattern: &str, bar: i64, beat: u32, sixteenth: u32) -> bool {
        let program = parse(&format!("at {pattern} {{ note c3 }}")).unwrap();
        let mut hit = false;
        run(&program, &Pos { step: 0, bar, beat, sixteenth }, &mut Rng::new(1), &mut |_| hit = true);
        hit
    }

    #[test]
    fn notes() {
        assert_eq!(note_number("c3"), Some(60));
        assert_eq!(note_number("C#3"), Some(61));
        assert_eq!(note_number("db3"), Some(61));
        assert_eq!(note_number("c-2"), Some(0));
        assert_eq!(note_number("g8"), Some(127));
        assert_eq!(note_number("a8"), None);
        assert_eq!(note_number("42"), Some(42));
        assert_eq!(note_number("h3"), None);
    }

    #[test]
    fn every() {
        assert_eq!(steps_with_notes("every 4 { note c3 }", 12), [0, 4, 8]);
    }

    #[test]
    fn at_constants_and_wildcards() {
        assert_eq!(steps_with_notes("at x.0.0 { note c3 }", 48), [0, 16, 32]);
        assert_eq!(steps_with_notes("at 1 { note c3 }", 48), (16..32).collect::<Vec<_>>());
        assert_eq!(steps_with_notes("at x.y.2 { note c3 }", 16), [2, 6, 10, 14]);
        assert_eq!(steps_with_notes("at x.1|3.0 { note c3 }", 16), [4, 12]);
    }

    #[test]
    fn at_shared_variables() {
        assert!(at("x.x.1", 1, 1, 1));
        assert!(at("x.x.1", 2, 2, 1));
        assert!(!at("x.x.1", 1, 2, 1));
        assert!(at("x.y.1", 1, 2, 1));

        // 4x.x.0: bar 4k with beat k
        assert!(at("4x.x.0", 0, 0, 0));
        assert!(at("4x.x.0", 4, 1, 0));
        assert!(!at("4x.x.0", 4, 0, 0));
        assert!(!at("4x.x.0", 2, 0, 0));
        // 4x.y.0: any beat in bars 0, 4, 8, ...
        assert!(at("4x.y.0", 8, 3, 0));
    }

    #[test]
    fn at_linear_terms() {
        assert!(at("x.2y+1.0", 7, 1, 0));
        assert!(at("x.2y+1.0", 7, 3, 0));
        assert!(!at("x.2y+1.0", 7, 2, 0));
        assert!(at("2n-1", 3, 0, 0));
        assert!(!at("2n-1", 2, 0, 0));
        assert!(at("x+2", 2, 0, 0));
        assert!(!at("x+2", 1, 0, 0));
        // Alternatives can bind differently: bar 2 matches via 4x+2, bar 4 via 4x.
        assert!(at("4x|4x+2.x", 2, 0, 0));
        assert!(at("4x|4x+2.x", 4, 1, 0));
        assert!(!at("4x|4x+2.x", 4, 0, 0));
    }

    #[test]
    fn nesting_and_options() {
        let program = parse("every 8 { at x.0 { note c3 dur 2 vel 90 ch 3 } }").unwrap();
        let mut out = Vec::new();
        run(&program, &pos_4_4(16), &mut Rng::new(1), &mut |n| out.push(n));
        assert_eq!(out, [NoteOut { note: 60, vel: 90, ch: 3, dur: 2 }]);
    }

    /// Play a single `note ...` statement many times.
    fn notes_from(stmt: &str, times: usize) -> Vec<NoteOut> {
        let program = parse(stmt).unwrap();
        let mut rng = Rng::new(42);
        let mut out = Vec::new();
        for _ in 0..times {
            run(&program, &pos_4_4(0), &mut rng, &mut |n| out.push(n));
        }
        out
    }

    #[test]
    fn expressions() {
        let one = |stmt: &str| notes_from(stmt, 1)[0];
        assert_eq!(one("note c3+7").note, 67);
        assert_eq!(one("note c3 + 7").note, 67);
        assert_eq!(one("note c3 -12").note, 48);
        assert_eq!(one("note c-1+2*3").note, 18); // * before +
        assert_eq!(one("note 60 vel 2*-3+100").vel, 94);
        assert_eq!(one("note db3-c3").note, 1);
    }

    #[test]
    fn random_numbers() {
        let notes = notes_from("note c3+r4 vel 1+r127 dur 1+r2", 2000);
        let mut seen = [false; 4];
        for n in &notes {
            assert!((60..64).contains(&n.note));
            assert!((1..=127).contains(&n.vel));
            assert!((1..=2).contains(&n.dur));
            seen[n.note as usize - 60] = true;
        }
        assert_eq!(seen, [true; 4]);
        // r2*100: half the notes get velocity 0, which means they don't play.
        let played = notes_from("note c3 vel r2*100", 2000).len();
        assert!((900..1100).contains(&played), "{played}");
        // Notes that come out of range aren't played.
        assert!(notes_from("note g8+r1+1", 10).is_empty());
    }

    #[test]
    fn patterns_parse() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/patterns");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if let Err(e) = parse(&std::fs::read_to_string(&path).unwrap()) {
                panic!("{}: {e}", path.display());
            }
        }
    }

    #[test]
    fn comments_and_errors() {
        assert_eq!(parse("-- nothing\n\n").unwrap(), vec![]);
        assert_eq!(parse("every 4 {\n  note h3\n}").unwrap_err().line, 2);
        assert!(parse("every 4 { note c3").is_err());
        assert!(parse("every 0 { }").is_err());
        assert!(parse("every x.1.0 { }").unwrap_err().message.contains("at x.1.0"));
        assert!(parse("at x.Y.1 { }").is_err());
        assert!(parse("at 0x.1 { }").is_err());
        assert!(parse("at 2y+ { }").is_err());
        assert!(parse("at x.x.x.x { }").is_err());
        assert!(parse("note c3 vel 200").is_err());
        assert!(parse("note c3 dur 2 dur 3").is_err());
        assert!(parse("note 200").is_err());
        assert!(parse("note c3+100").is_err());
        assert!(parse("note c3 vel 0").is_err());
        assert!(parse("note c3+r0").is_err());
        assert!(parse("note c3+r").is_err());
        assert!(parse("note c3+").is_err());
        assert!(parse("note c3 vel 100+r200").is_ok()); // clamped when played
        assert!(parse("}").is_err());
    }
}
