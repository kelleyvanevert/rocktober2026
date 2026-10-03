//! A tiny language: nested function calls over strings, numbers and durations.
//!
//! ```text
//! play(repeat(fit(sample("kick.mp3"), 500ms), 4))   -- comment
//! ```
//!
//! `x.f(a, b)` is shorthand for `f(x, a, b)`, and `x.f` for `f(x)`, so the two
//! styles can be mixed freely:
//!
//! ```text
//! sample("kick.mp3").fit(500ms).repeat(4).play
//! ```
//!
//! `a * b` is shorthand for `mul(a, b)` and `a + b` for `add(a, b)`. Method
//! calls bind tightest, then `*`, then `+`; parentheses group:
//!
//! ```text
//! (sample("kick.mp3") * envelope("pluck").gate(100ms)).play
//! ```
//!
//! The parser knows nothing about what `play` or `fit` mean; it just builds a tree.
//! Giving the tree meaning is `eval`'s job.

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
    LParen,
    RParen,
    Comma,
    Dot,
    Star,
    Plus,
}

/// Whether a '.' or '-' at `i` begins (or continues) a number rather than being
/// punctuation.
fn starts_number(bytes: &[u8], i: usize) -> bool {
    matches!(bytes[i], b'.' | b'-')
        && bytes
            .get(i + 1)
            .is_some_and(|b| b.is_ascii_digit() || *b == b'.')
        && !(bytes[i] == b'-' && bytes.get(i + 1) == Some(&b'-'))
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
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let start = i;
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'(' | b')' | b',' | b'.' | b'*' | b'+' if !starts_number(bytes, i) => {
                let tok = match c {
                    b'(' => Token::LParen,
                    b')' => Token::RParen,
                    b',' => Token::Comma,
                    b'*' => Token::Star,
                    b'+' => Token::Plus,
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
                while i < bytes.len()
                    && (bytes[i].is_ascii_digit() || (bytes[i] == b'.' && starts_number(bytes, i)))
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
                    // Decibels are just a way to write an amplitude factor.
                    "db" => Token::Num(10f64.powf(value / 20.0)),
                    unit => return err(unit_start, format!("unknown unit '{unit}'")),
                };
                tokens.push((tok, start));
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let tok = match &src[start..i] {
                    "inf" => Token::Num(f64::INFINITY),
                    name => Token::Ident(name.to_string()),
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

struct Parser {
    tokens: Vec<(Token, usize)>,
    i: usize,
    end: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.i).map(|(t, _)| t)
    }

    fn pos(&self) -> usize {
        self.tokens.get(self.i).map_or(self.end, |(_, p)| *p)
    }

    fn expect(&mut self, tok: Token, what: &str) -> Result<(), Error> {
        if self.peek() == Some(&tok) {
            self.i += 1;
            Ok(())
        } else {
            err(self.pos(), format!("expected {what}"))
        }
    }

    /// Terms joined by `+`, left to right.
    fn expr(&mut self) -> Result<Spanned, Error> {
        self.binary(Token::Plus, "add", Self::term)
    }

    /// Method chains joined by `*`, left to right.
    fn term(&mut self) -> Result<Spanned, Error> {
        self.binary(Token::Star, "mul", Self::chain)
    }

    /// `operand (op operand)*`, as nested calls of `function`.
    fn binary(
        &mut self,
        op: Token,
        function: &str,
        operand: fn(&mut Self) -> Result<Spanned, Error>,
    ) -> Result<Spanned, Error> {
        let mut expr = operand(self)?;
        while self.peek() == Some(&op) {
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

    /// A primary expression followed by any number of `.method` calls.
    fn chain(&mut self) -> Result<Spanned, Error> {
        let mut expr = self.primary()?;
        while self.peek() == Some(&Token::Dot) {
            self.i += 1;
            let pos = self.pos();
            let Some(Token::Ident(name)) = self.peek().cloned() else {
                return err(pos, "expected a name after '.'");
            };
            self.i += 1;
            let mut args = vec![expr];
            if self.peek() == Some(&Token::LParen) {
                args.extend(self.args()?);
            }
            expr = Spanned {
                expr: Expr::Call { name, args },
                pos,
            };
        }
        Ok(expr)
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
            Token::LParen => {
                let inner = self.expr()?;
                self.expect(Token::RParen, "')'")?;
                return Ok(inner);
            }
            Token::Ident(name) => {
                if self.peek() != Some(&Token::LParen) {
                    return err(self.pos(), format!("expected '(' after '{name}'"));
                }
                let args = self.args()?;
                Expr::Call { name, args }
            }
            _ => return err(pos, "expected an expression"),
        };
        Ok(Spanned { expr, pos })
    }

    /// `( a, b, c )`, allowing a trailing comma.
    fn args(&mut self) -> Result<Vec<Spanned>, Error> {
        self.expect(Token::LParen, "'('")?;
        let mut args = Vec::new();
        while self.peek() != Some(&Token::RParen) {
            args.push(self.expr()?);
            match self.peek() {
                Some(Token::Comma) => self.i += 1,
                Some(Token::RParen) => {}
                _ => return err(self.pos(), "expected ',' or ')'"),
            }
        }
        self.i += 1;
        Ok(args)
    }
}

/// Parse a sequence of top-level expressions (statements).
pub fn parse(src: &str) -> Result<Vec<Spanned>, Error> {
    let mut parser = Parser {
        tokens: lex(src)?,
        i: 0,
        end: src.len(),
    };
    let mut program = Vec::new();
    while parser.peek().is_some() {
        program.push(parser.expr()?);
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
    fn inf_is_a_number() {
        assert_eq!(desugar("x().repeat(inf)"), "repeat(x(), inf)");
    }

    #[test]
    fn reports_position() {
        let e = parse("play(sample(\"a\" 3))").unwrap_err();
        assert_eq!(e.pos, 16);
    }
}
