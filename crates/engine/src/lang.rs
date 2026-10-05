//! A tiny language of nodes, their params, and operators.
//!
//! ```text
//! let wub = wavetable:table("sine-saw"):note(?note + 12) * lowpass:freq(c6) * 0.5
//! wub:note(g2).play("wub", at 4b)                          -- comment
//! ```
//!
//! - A bare name like `wavetable` is a node with all its params at their
//!   defaults. `wavetable("sine-saw", 0.6)` fills its params in order.
//! - `x:name(value)` sets every free param called `name` in `x` (`x:name`
//!   alone is `x:name(1)`, for switches like `:retrig`).
//! - `?name` is a new free param (`?name = 0.2` one with a default).
//! - `x.f(a, b)` is shorthand for `f(x, a, b)`, and `x.f` for `f(x)`. These
//!   are for operations, not nodes: `.play`, `.fit(500ms)`, `.repeat(4)`, ...
//! - `a + b`, `a - b`, `a * b`, `a / b` are `add`, `sub`, `mul` and `div`,
//!   with the usual precedence; `-x` is `mul(-1, x)`. Method calls and
//!   `:params` bind tightest.
//! - `[a, b, c]` is a list, and `n => n * 2` a function of one parameter.
//!
//! Notes are written `c2`, `f#3`, `eb4` (`c4` is middle C, MIDI note 60; Ableton
//! calls it C3). A frequency like `800hz` or `2khz` is just another way to write
//! a pitch, the way `-6db` is another way to write an amount. `at 4b` is a grid
//! to start things on (the next multiple of 4 beats); it binds like a method
//! call, so `at 5b + 2` is `add(at(5b), 2)`.
//!
//! `let name = ...` names a value, and a bare `name` refers to it. `fn
//! name(a, b) = ...` defines a function: one expression, with its parameters
//! bound to the arguments of each call (so `x.name(b)` works too).
//!
//! A name followed by `(` on the same line is a call; on the next line it's a
//! name followed by a parenthesized expression (the next statement).
//!
//! The parser knows nothing about what `play` or `wavetable` mean; it just
//! builds a tree. Giving the tree meaning is `eval`'s job.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Call {
        name: String,
        args: Vec<Spanned>,
    },
    Str(String),
    Num(f64),
    /// In seconds.
    Duration(f64),
    /// A length in beats: `0.25b`, `1bar` (4 beats).
    Beats(f64),
    /// A MIDI note number.
    Pitch(f64),
    /// A name given with `let`, or a node with its defaults (`wavetable`).
    Var(String),
    /// `let name = value`, only as a statement.
    Let {
        name: String,
        value: Box<Spanned>,
    },
    /// `fn name(params) = body`, only as a statement.
    Fn {
        name: String,
        params: Vec<String>,
        body: Box<Spanned>,
    },
    /// `?name` or `?name = default`.
    Hole {
        name: String,
        default: Option<Box<Spanned>>,
    },
    /// `target:name(value)`. Its position is the name's.
    Set {
        target: Box<Spanned>,
        name: String,
        value: Box<Spanned>,
    },
    /// `[a, b, c]`.
    List(Vec<Spanned>),
    /// `param => body`.
    Lambda {
        param: String,
        body: Box<Spanned>,
    },
}

/// An expression plus its byte offset in the source, for error messages.
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned {
    pub expr: Expr,
    pub pos: usize,
}

#[derive(Debug)]
pub struct Error {
    pub pos: usize,
    pub msg: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

fn err<T>(pos: usize, msg: impl Into<String>) -> Result<T, Error> {
    Err(Error {
        pos,
        msg: msg.into(),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Token {
    Ident(String),
    Str(String),
    Num(f64),
    Duration(f64),
    Beats(f64),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Star,
    Slash,
    Plus,
    Minus,
    Pitch(f64),
    Let,
    Fn,
    At,
    Question,
    Colon,
    Equals,
    /// `=>`
    Arrow,
}

impl Token {
    /// Whether an expression can end with this token: then a `-` after it is
    /// a minus, not the sign of a number (`9 - 1`, `9 -1`, `f(x) -1`).
    fn ends_operand(&self) -> bool {
        matches!(
            self,
            Token::Ident(_)
                | Token::Str(_)
                | Token::Num(_)
                | Token::Duration(_)
                | Token::Beats(_)
                | Token::Pitch(_)
                | Token::RParen
                | Token::RBracket
        )
    }
}

/// The MIDI note number of a note name like `c4` (60), `f#3` or `eb4`.
pub fn note(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    let (letter, rest) = bytes.split_first()?;
    let semitone = match letter {
        b'c' => 0,
        b'd' => 2,
        b'e' => 4,
        b'f' => 5,
        b'g' => 7,
        b'a' => 9,
        b'b' => 11,
        _ => return None,
    };
    let (accidental, rest) = match rest {
        [b'#', rest @ ..] => (1, rest),
        [b'b', rest @ ..] => (-1, rest),
        rest => (0, rest),
    };
    let [octave @ b'0'..=b'9'] = rest else {
        return None;
    };
    Some((12 * (*octave as i32 - b'0' as i32 + 1) + semitone + accidental) as f64)
}

/// A note number written as a note name if it's a whole one (`c3`, `f#4`),
/// or else as a number.
pub fn note_name(note: f64) -> String {
    const NAMES: [&str; 12] = [
        "c", "c#", "d", "eb", "e", "f", "f#", "g", "ab", "a", "bb", "b",
    ];
    if note.fract() == 0.0 && (12.0..=131.0).contains(&note) {
        let n = note as i32;
        format!("{}{}", NAMES[(n % 12) as usize], n / 12 - 1)
    } else {
        format!("{note:.2}")
    }
}

/// The (fractional) MIDI note number of a frequency.
pub fn hz_to_note(hz: f64) -> f64 {
    69.0 + 12.0 * (hz / 440.0).log2()
}

/// A note number as a frequency, rounded for showing: `800hz`, `2.4khz`.
pub fn note_to_hz_text(note: f64) -> String {
    let hz = 440.0 * 2f64.powf((note - 69.0) / 12.0);
    if hz >= 1000.0 {
        format!("{}khz", (hz / 100.0).round() / 10.0)
    } else {
        format!("{}hz", hz.round())
    }
}

/// Whether a '.' at `i` begins (or continues) a number rather than being
/// punctuation.
fn dot_starts_number(bytes: &[u8], i: usize) -> bool {
    bytes[i] == b'.' && bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit())
}

/// Whether a '-' at `i` is the sign of a number, given the token before it.
fn minus_starts_number(bytes: &[u8], i: usize, previous: Option<&Token>) -> bool {
    bytes[i] == b'-'
        && bytes
            .get(i + 1)
            .is_some_and(|b| b.is_ascii_digit() || *b == b'.')
        && !previous.is_some_and(Token::ends_operand)
}

/// A time position: `m:ss` (seconds may have decimals) or `m:ss:mmm`, e.g.
/// `0:11:188` is 11.188 seconds.
fn parse_time(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.split(':').collect();
    let int = |p: &str| p.parse::<u64>().ok().map(|v| v as f64);
    match parts.as_slice() {
        [m, s] => Some(int(m)? * 60.0 + s.parse::<f64>().ok().filter(|s| *s >= 0.0)?),
        [m, s, ms] => Some(int(m)? * 60.0 + int(s)? + int(ms)? / 1000.0),
        _ => None,
    }
}

pub(crate) fn lex(src: &str) -> Result<Vec<(Token, usize)>, Error> {
    let bytes = src.as_bytes();
    let mut tokens: Vec<(Token, usize)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let start = i;
        let previous = tokens.last().map(|(t, _)| t);
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'=' if bytes.get(i + 1) == Some(&b'>') => {
                tokens.push((Token::Arrow, start));
                i += 2;
            }
            b'(' | b')' | b'[' | b']' | b',' | b'.' | b'*' | b'/' | b'+' | b'-' | b'?' | b':'
            | b'='
                if !dot_starts_number(bytes, i) && !minus_starts_number(bytes, i, previous) =>
            {
                let tok = match c {
                    b'(' => Token::LParen,
                    b')' => Token::RParen,
                    b'[' => Token::LBracket,
                    b']' => Token::RBracket,
                    b',' => Token::Comma,
                    b'*' => Token::Star,
                    b'/' => Token::Slash,
                    b'+' => Token::Plus,
                    b'-' => Token::Minus,
                    b'?' => Token::Question,
                    b':' => Token::Colon,
                    b'=' => Token::Equals,
                    _ => Token::Dot,
                };
                tokens.push((tok, start));
                i += 1;
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += 1;
                }
                if i == bytes.len() {
                    return err(start, "unterminated string");
                }
                tokens.push((Token::Str(src[start + 1..i].to_string()), start));
                i += 1;
            }
            b'0'..=b'9' | b'.' | b'-' => {
                if c == b'-' {
                    i += 1;
                }
                // A '.' only continues a number if a digit follows, so `4.repeat`
                // lexes as `4` `.` `repeat`.
                while i < bytes.len() && (bytes[i].is_ascii_digit() || dot_starts_number(bytes, i))
                {
                    i += 1;
                }
                if bytes.get(i) == Some(&b':') && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    while i < bytes.len()
                        && (bytes[i].is_ascii_digit() || matches!(bytes[i], b':' | b'.'))
                    {
                        i += 1;
                    }
                    let seconds = parse_time(&src[start..i]).ok_or_else(|| Error {
                        pos: start,
                        msg: format!("bad time '{}' (use m:ss or m:ss:mmm)", &src[start..i]),
                    })?;
                    tokens.push((Token::Duration(seconds), start));
                    continue;
                }
                let value: f64 = match src[start..i].parse() {
                    Ok(v) => v,
                    Err(_) => return err(start, format!("bad number '{}'", &src[start..i])),
                };
                let unit_start = i;
                while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let tok = match &src[unit_start..i] {
                    "" => Token::Num(value),
                    "ms" => Token::Duration(value / 1000.0),
                    "s" => Token::Duration(value),
                    "b" => Token::Beats(value),
                    "bar" | "bars" => Token::Beats(value * 4.0),
                    // Decibels are just a way to write an amplitude factor.
                    "db" => Token::Num(10f64.powf(value / 20.0)),
                    // And a frequency is just a way to write a pitch.
                    "hz" | "khz" => {
                        let hz = if &src[unit_start..i] == "khz" {
                            value * 1000.0
                        } else {
                            value
                        };
                        if hz <= 0.0 {
                            return err(start, "a frequency has to be above 0hz");
                        }
                        Token::Pitch(hz_to_note(hz))
                    }
                    unit => return err(unit_start, format!("unknown unit '{unit}'")),
                };
                tokens.push((tok, start));
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                // A sharp note, like `f#3`.
                if i == start + 1
                    && bytes.get(i) == Some(&b'#')
                    && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)
                {
                    i += 2;
                }
                let tok = match &src[start..i] {
                    "inf" => Token::Num(f64::INFINITY),
                    "let" => Token::Let,
                    "fn" => Token::Fn,
                    "at" => Token::At,
                    name => match note(name) {
                        Some(note) => Token::Pitch(note),
                        None => Token::Ident(name.to_string()),
                    },
                };
                tokens.push((tok, start));
            }
            _ => {
                let ch = src[i..].chars().next().unwrap();
                return err(start, format!("unexpected character '{ch}'"));
            }
        }
    }
    Ok(tokens)
}

struct Parser<'a> {
    src: &'a str,
    tokens: Vec<(Token, usize)>,
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.i).map(|(t, _)| t)
    }

    fn pos(&self) -> usize {
        self.tokens.get(self.i).map_or(self.src.len(), |(_, p)| *p)
    }

    fn expect(&mut self, tok: Token, what: &str) -> Result<(), Error> {
        if self.peek() == Some(&tok) {
            self.i += 1;
            Ok(())
        } else {
            err(self.pos(), format!("expected {what}"))
        }
    }

    /// A name, for `what` (like "after 'let'").
    fn name(&mut self, what: &str) -> Result<String, Error> {
        let Some(Token::Ident(name)) = self.peek().cloned() else {
            return err(self.pos(), format!("expected a name {what}"));
        };
        self.i += 1;
        Ok(name)
    }

    /// Whether the next token is a `(` on the same line as the source up to
    /// `from`: that makes the name before it a call.
    fn call_paren(&self, from: usize) -> bool {
        self.peek() == Some(&Token::LParen) && !self.src[from..self.pos()].contains('\n')
    }

    /// `let name = value`, `fn name(params) = body`, or an expression.
    fn statement(&mut self) -> Result<Spanned, Error> {
        if self.peek() == Some(&Token::Fn) {
            return self.function();
        }
        if self.peek() != Some(&Token::Let) {
            return self.expr();
        }
        let pos = self.pos();
        self.i += 1;
        let name = self.name("after 'let'")?;
        self.expect(Token::Equals, "'='")?;
        let value = Box::new(self.expr()?);
        Ok(Spanned {
            expr: Expr::Let { name, value },
            pos,
        })
    }

    /// `fn name(a, b) = body`. The parameters may end with a comma, like
    /// arguments.
    fn function(&mut self) -> Result<Spanned, Error> {
        let pos = self.pos();
        self.i += 1;
        let name = self.name("after 'fn'")?;
        self.expect(Token::LParen, "'(' and the parameters")?;
        let mut params = Vec::new();
        while self.peek() != Some(&Token::RParen) {
            let param = self.name("for a parameter")?;
            if params.contains(&param) {
                return err(
                    self.tokens[self.i - 1].1,
                    format!("'{param}' is a parameter twice"),
                );
            }
            params.push(param);
            match self.peek() {
                Some(Token::Comma) => self.i += 1,
                Some(Token::RParen) => {}
                _ => return err(self.pos(), "expected ',' or ')'"),
            }
        }
        self.i += 1;
        self.expect(Token::Equals, "'='")?;
        let body = Box::new(self.expr()?);
        Ok(Spanned {
            expr: Expr::Fn { name, params, body },
            pos,
        })
    }

    /// Terms joined by `+` and `-`, left to right.
    fn expr(&mut self) -> Result<Spanned, Error> {
        self.binary(&[(Token::Plus, "add"), (Token::Minus, "sub")], Self::term)
    }

    /// Factors joined by `*` and `/`, left to right.
    fn term(&mut self) -> Result<Spanned, Error> {
        self.binary(&[(Token::Star, "mul"), (Token::Slash, "div")], Self::unary)
    }

    /// `operand (op operand)*`, as nested calls of the ops' functions.
    fn binary(
        &mut self,
        ops: &[(Token, &str)],
        operand: fn(&mut Self) -> Result<Spanned, Error>,
    ) -> Result<Spanned, Error> {
        let mut expr = operand(self)?;
        while let Some((_, function)) = ops.iter().find(|(op, _)| self.peek() == Some(op)) {
            let pos = self.pos();
            self.i += 1;
            let rhs = operand(self)?;
            expr = Spanned {
                expr: Expr::Call {
                    name: function.to_string(),
                    args: vec![expr, rhs],
                },
                pos,
            };
        }
        Ok(expr)
    }

    /// `-x`, as `mul(-1, x)`, or a chain.
    fn unary(&mut self) -> Result<Spanned, Error> {
        if self.peek() != Some(&Token::Minus) {
            return self.chain();
        }
        let pos = self.pos();
        self.i += 1;
        let operand = self.unary()?;
        Ok(Spanned {
            expr: Expr::Call {
                name: "mul".to_string(),
                args: vec![
                    Spanned {
                        expr: Expr::Num(-1.0),
                        pos,
                    },
                    operand,
                ],
            },
            pos,
        })
    }

    /// A primary expression followed by any number of `.method` calls and
    /// `:param` settings.
    fn chain(&mut self) -> Result<Spanned, Error> {
        let mut expr = self.primary()?;
        loop {
            match self.peek() {
                Some(Token::Dot) => {
                    self.i += 1;
                    let pos = self.pos();
                    let name = self.name("after '.'")?;
                    let mut args = vec![expr];
                    if self.call_paren(pos) {
                        args.extend(self.args()?);
                    }
                    expr = Spanned {
                        expr: Expr::Call { name, args },
                        pos,
                    };
                }
                Some(Token::Colon) => {
                    self.i += 1;
                    let pos = self.pos();
                    let name = self.name("after ':' (a param, like :note(c3))")?;
                    let value = if self.call_paren(pos) {
                        self.i += 1;
                        let value = self.expr()?;
                        if self.peek() == Some(&Token::Comma) {
                            return err(self.pos(), format!(":{name} takes one value"));
                        }
                        self.expect(Token::RParen, "')'")?;
                        value
                    } else {
                        // `:retrig` is `:retrig(1)`.
                        Spanned {
                            expr: Expr::Num(1.0),
                            pos,
                        }
                    };
                    expr = Spanned {
                        expr: Expr::Set {
                            target: Box::new(expr),
                            name,
                            value: Box::new(value),
                        },
                        pos,
                    };
                }
                _ => return Ok(expr),
            }
        }
    }

    fn primary(&mut self) -> Result<Spanned, Error> {
        let pos = self.pos();
        let Some((tok, _)) = self.tokens.get(self.i).cloned() else {
            return err(pos, "expected an expression");
        };
        self.i += 1;
        let expr = match tok {
            Token::Str(s) => Expr::Str(s),
            Token::Num(n) => Expr::Num(n),
            Token::Duration(d) => Expr::Duration(d),
            Token::Beats(b) => Expr::Beats(b),
            Token::Pitch(note) => Expr::Pitch(note),
            Token::Question => {
                let name = self.name("after '?'")?;
                let default = if self.peek() == Some(&Token::Equals) {
                    self.i += 1;
                    Some(Box::new(self.expr()?))
                } else {
                    None
                };
                Expr::Hole { name, default }
            }
            // `at 4b`: a grid, as `at(4b)`. It takes a method chain, so it binds
            // tighter than the operators.
            Token::At => Expr::Call {
                name: "at".to_string(),
                args: vec![self.chain()?],
            },
            Token::LParen => {
                let inner = self.expr()?;
                self.expect(Token::RParen, "')'")?;
                return Ok(inner);
            }
            Token::LBracket => Expr::List(self.list(Token::RBracket, "']'")?),
            Token::Ident(name) => {
                if self.peek() == Some(&Token::Arrow) {
                    self.i += 1;
                    let body = Box::new(self.expr()?);
                    Expr::Lambda { param: name, body }
                } else if self.call_paren(pos + name.len()) {
                    let args = self.args()?;
                    Expr::Call { name, args }
                } else {
                    Expr::Var(name)
                }
            }
            _ => return err(pos, "expected an expression"),
        };
        Ok(Spanned { expr, pos })
    }

    /// `( a, b, c )`, allowing a trailing comma.
    fn args(&mut self) -> Result<Vec<Spanned>, Error> {
        self.expect(Token::LParen, "'('")?;
        self.list(Token::RParen, "')'")
    }

    /// Expressions separated by commas up to `close` (the opening bracket
    /// has been read), allowing a trailing comma.
    fn list(&mut self, close: Token, what: &str) -> Result<Vec<Spanned>, Error> {
        let mut items = Vec::new();
        while self.peek() != Some(&close) {
            items.push(self.expr()?);
            match self.peek() {
                Some(Token::Comma) => self.i += 1,
                Some(t) if *t == close => {}
                _ => return err(self.pos(), format!("expected ',' or {what}")),
            }
        }
        self.i += 1;
        Ok(items)
    }
}

/// Parse a sequence of top-level expressions (statements).
pub fn parse(src: &str) -> Result<Vec<Spanned>, Error> {
    let mut parser = Parser {
        src,
        tokens: lex(src)?,
        i: 0,
    };
    let mut program = Vec::new();
    while parser.peek().is_some() {
        program.push(parser.statement()?);
    }
    Ok(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_calls() {
        let prog = parse(r#"play(repeat(fit(sample("kick.mp3"), 500ms), 4)) -- hi"#).unwrap();
        assert_eq!(prog.len(), 1);
        let Expr::Call { name, args } = &prog[0].expr else {
            panic!()
        };
        assert_eq!(name, "play");
        let Expr::Call { name, args } = &args[0].expr else {
            panic!()
        };
        assert_eq!(name, "repeat");
        assert_eq!(args[1].expr, Expr::Num(4.0));
        let Expr::Call { args, .. } = &args[0].expr else {
            panic!()
        };
        assert_eq!(args[1].expr, Expr::Duration(0.5));
    }

    #[test]
    fn lexes_negative_numbers_and_decibels() {
        let prog = parse("f(-0.5, -6db, 0db)").unwrap();
        let Expr::Call { args, .. } = &prog[0].expr else {
            panic!()
        };
        let nums: Vec<f64> = args
            .iter()
            .map(|a| match a.expr {
                Expr::Num(n) => (n * 1000.0).round() / 1000.0,
                _ => panic!(),
            })
            .collect();
        assert_eq!(nums, [-0.5, 0.501, 1.0]);
    }

    /// Parse one expression and print it back in plain call syntax.
    fn desugar(src: &str) -> String {
        fn show(e: &Expr) -> String {
            match e {
                Expr::Call { name, args } => {
                    let args: Vec<String> = args.iter().map(|a| show(&a.expr)).collect();
                    format!("{name}({})", args.join(", "))
                }
                Expr::Str(s) => format!("{s:?}"),
                Expr::Num(n) => n.to_string(),
                Expr::Duration(d) => format!("{d}s"),
                Expr::Beats(b) => format!("{b}b"),
                Expr::Pitch(note) => format!("note{note}"),
                Expr::Var(name) => name.clone(),
                Expr::Let { name, value } => format!("let {name} = {}", show(&value.expr)),
                Expr::Fn { name, params, body } => {
                    format!("fn {name}({}) = {}", params.join(", "), show(&body.expr))
                }
                Expr::Hole { name, default } => match default {
                    Some(d) => format!("?{name}={}", show(&d.expr)),
                    None => format!("?{name}"),
                },
                Expr::Set {
                    target,
                    name,
                    value,
                } => format!("{}:{name}({})", show(&target.expr), show(&value.expr)),
                Expr::List(items) => {
                    let items: Vec<String> = items.iter().map(|a| show(&a.expr)).collect();
                    format!("[{}]", items.join(", "))
                }
                Expr::Lambda { param, body } => format!("({param} => {})", show(&body.expr)),
            }
        }
        let prog = parse(src).unwrap();
        assert_eq!(prog.len(), 1);
        show(&prog[0].expr)
    }

    #[test]
    fn method_calls_desugar_to_calls() {
        assert_eq!(
            desugar(r#"sample("k").fit(500ms).repeat(4).play"#),
            r#"play(repeat(fit(sample("k"), 0.5s), 4))"#
        );
        assert_eq!(desugar("x.play()"), "play(x)");
        assert_eq!(desugar("4.repeat"), "repeat(4)");
        assert_eq!(desugar("x(.5).y(-.5)"), "y(x(0.5), -0.5)");
    }

    #[test]
    fn trailing_commas() {
        assert_eq!(desugar("add(\n  a(),\n  b(),\n)"), "add(a(), b())");
        assert_eq!(desugar("add(a(),)"), "add(a())");
        assert!(parse("add(,)").is_err());
    }

    #[test]
    fn method_error_points_at_method_name() {
        let prog = parse("a().fit").unwrap();
        assert_eq!(prog[0].pos, 4);
        assert_eq!(parse("a().(").unwrap_err().pos, 4);
    }

    #[test]
    fn lexes_times() {
        let times: Vec<f64> = ["0:11:188", "1:02", "0:30.5", "10:00:005"]
            .iter()
            .map(|t| match parse(&format!("f({t})")).unwrap()[0].expr {
                Expr::Call { ref args, .. } => match args[0].expr {
                    Expr::Duration(d) => (d * 1e6).round() / 1e6,
                    _ => panic!(),
                },
                _ => panic!(),
            })
            .collect();
        assert_eq!(times, [11.188, 62.0, 30.5, 600.005]);
        assert!(parse("f(1:2:3:4)").is_err());
        assert!(parse("f(-1:00)").is_err());
    }

    #[test]
    fn operators_desugar_with_precedence() {
        assert_eq!(
            desugar("a() * b() + c() * d()"),
            "add(mul(a(), b()), mul(c(), d()))"
        );
        assert_eq!(desugar("a() * b() * c()"), "mul(mul(a(), b()), c())");
        assert_eq!(desugar("a() * b().f(1)"), "mul(a(), f(b(), 1))");
        assert_eq!(desugar("(a() * b()).f"), "f(mul(a(), b()))");
        assert_eq!(desugar("2*-0.5+1"), "add(mul(2, -0.5), 1)");
        assert_eq!(desugar("f((1 + 2), 3)"), "f(add(1, 2), 3)");
        assert!(parse("a() *").is_err());
        assert!(parse("(a()").is_err());
        assert_eq!(parse("a() * * b()").unwrap_err().pos, 6);
    }

    #[test]
    fn minus_and_divide() {
        assert_eq!(desugar("x * 9 - 1"), "sub(mul(x, 9), 1)");
        assert_eq!(desugar("x * 9 -1"), "sub(mul(x, 9), 1)");
        assert_eq!(desugar("f(x) -1"), "sub(f(x), 1)");
        assert_eq!(desugar("x - -1"), "sub(x, -1)");
        assert_eq!(desugar("a - b - c"), "sub(sub(a, b), c)");
        assert_eq!(desugar("0 / n * 2"), "mul(div(0, n), 2)");
        assert_eq!(desugar("-x * 2"), "mul(mul(-1, x), 2)");
        assert_eq!(desugar("f(-x)"), "f(mul(-1, x))");
        assert_eq!(desugar("[1, -2]"), "[1, -2]");
        assert_eq!(desugar("x -- comment"), "x");
    }

    #[test]
    fn params_are_set_with_colons() {
        assert_eq!(
            desugar("wavetable:pos(0.3):note(g4)"),
            "wavetable:pos(0.3):note(note67)"
        );
        assert_eq!(
            desugar("(a + b:table(\"saw\")):note(?note + swoop)"),
            "add(a, b:table(\"saw\")):note(add(?note, swoop))"
        );
        assert_eq!(
            desugar("node * lowpass:freq(?note + 24)"),
            "mul(node, lowpass:freq(add(?note, 24)))"
        );
        assert_eq!(
            desugar("mod(\"swoop\"):retrig * 9 - 1"),
            "sub(mul(mod(\"swoop\"):retrig(1), 9), 1)"
        );
        assert_eq!(
            desugar("x:note(glide(?note):dur(300ms)).play()"),
            "play(x:note(glide(?note):dur(0.3s)))"
        );
        assert_eq!(
            parse("x:pos(1, 2)").unwrap_err().msg,
            ":pos takes one value"
        );
        assert!(parse("x:(1)").is_err());
        // Times still lex as times.
        assert_eq!(desugar("f(0:01)"), "f(1s)");
    }

    #[test]
    fn lists_and_lambdas() {
        assert_eq!(
            desugar("[0, 2, 3].map(n => w:note(?note + n * 110)):note(c3)"),
            "map([0, 2, 3], (n => w:note(add(?note, mul(n, 110))))):note(note48)"
        );
        assert_eq!(desugar("[]"), "[]");
        assert_eq!(desugar("[a,]"), "[a]");
        assert!(parse("[a b]").is_err());
    }

    #[test]
    fn calls_need_their_paren_on_the_same_line() {
        let prog = parse("let a = b\n(c + d).play").unwrap();
        assert_eq!(prog.len(), 2);
        assert_eq!(desugar("f(\n  1,\n)"), "f(1)");
        assert_eq!(desugar("x.f\n"), "f(x)");
    }

    #[test]
    fn beats() {
        assert_eq!(
            desugar("f(0.25b, 1bar, 2bars, 120.bpm)"),
            "f(0.25b, 4b, 8b, bpm(120))"
        );
    }

    #[test]
    fn notes() {
        assert_eq!(
            desugar("f(c4, a4, c#4, db4, b3, bb3, c0, g9)"),
            "f(note60, note69, note61, note61, note59, note58, note12, note127)"
        );
        // Not notes: other letters, longer names, two-digit octaves.
        assert_eq!(desugar("f(h2, c10, cc2, b)"), "f(h2, c10, cc2, b)");
        assert_eq!(note_name(48.0), "c3");
        assert_eq!(note_name(61.0), "c#4");
        assert_eq!(note_name(61.5), "61.50");
    }

    #[test]
    fn lets_vars_and_holes() {
        assert_eq!(
            desugar(
                "let lead = wavetable(\"basic\", ?pos = 0.2, ?warp, ?note) * envelope(\"pluck\")"
            ),
            "let lead = mul(wavetable(\"basic\", ?pos=0.2, ?warp, ?note), envelope(\"pluck\"))"
        );
        assert_eq!(desugar("lead"), "lead");
        assert_eq!(
            parse("let = 3").unwrap_err().msg,
            "expected a name after 'let'"
        );
        assert_eq!(parse("let x 3").unwrap_err().msg, "expected '='");
        assert_eq!(parse("f(?)").unwrap_err().msg, "expected a name after '?'");
        let prog = parse("let a = 1\nlet b = 2\nf(a, b)").unwrap();
        assert_eq!(prog.len(), 3);
    }

    #[test]
    fn frequencies_are_pitches() {
        assert_eq!(desugar("f(440hz, 0.44khz)"), "f(note69, note69)");
        let Expr::Call { args, .. } = &parse("f(880hz)").unwrap()[0].expr else {
            panic!()
        };
        assert!(matches!(args[0].expr, Expr::Pitch(n) if (n - 81.0).abs() < 1e-9));
        assert_eq!(
            parse("f(0hz)").unwrap_err().msg,
            "a frequency has to be above 0hz"
        );
        assert_eq!(note_to_hz_text(69.0), "440hz");
        assert_eq!(note_to_hz_text(hz_to_note(2400.0)), "2.4khz");
    }

    #[test]
    fn at_makes_a_grid() {
        assert_eq!(desugar("x.play(\"a\", at 4b)"), "play(x, \"a\", at(4b))");
        assert_eq!(desugar("x.play(at 5b + 2)"), "play(x, add(at(5b), 2))");
        assert_eq!(desugar("x.play(at 1bar)"), "play(x, at(4b))");
        assert_eq!(
            parse("x.play(at)").unwrap_err().msg,
            "expected an expression"
        );
    }

    #[test]
    fn functions() {
        assert_eq!(
            desugar("fn wide(x, amount) = x * spread:width(amount)"),
            "fn wide(x, amount) = mul(x, spread:width(amount))"
        );
        assert_eq!(desugar("fn f(\n  x,\n) = x"), "fn f(x) = x");
        assert_eq!(desugar("fn f() = g()"), "fn f() = g()");
        assert_eq!(
            parse("fn = 1").unwrap_err().msg,
            "expected a name after 'fn'"
        );
        assert_eq!(
            parse("fn f = 1").unwrap_err().msg,
            "expected '(' and the parameters"
        );
        assert_eq!(parse("fn f(x) x").unwrap_err().msg, "expected '='");
        assert_eq!(
            parse("fn f(1) = 1").unwrap_err().msg,
            "expected a name for a parameter"
        );
        assert_eq!(
            parse("fn f(x, x) = x").unwrap_err().msg,
            "'x' is a parameter twice"
        );
        // Only a statement, like let.
        assert!(parse("g(fn f() = 1)").is_err());
    }

    #[test]
    fn inf_is_a_number() {
        assert_eq!(desugar("x().repeat(inf)"), "repeat(x(), inf)");
    }

    #[test]
    fn reports_position() {
        let e = parse("play(sample(\"a\" 3))").unwrap_err();
        assert_eq!(e.pos, 16);
    }
}
