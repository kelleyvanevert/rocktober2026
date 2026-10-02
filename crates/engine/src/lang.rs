//! A tiny language: nested function calls over strings, numbers and durations.
//!
//! ```text
//! play(repeat(fit(sample("kick.mp3"), 500ms), 4))   -- comment
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
enum Token {
    Ident(String),
    Str(String),
    Num(f64),
    Duration(f64),
    LParen,
    RParen,
    Comma,
}

fn lex(src: &str) -> Result<Vec<(Token, usize)>, Error> {
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
            b'(' | b')' | b',' => {
                let tok = match c {
                    b'(' => Token::LParen,
                    b')' => Token::RParen,
                    _ => Token::Comma,
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
            b'0'..=b'9' | b'.' => {
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
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
                    unit => return err(unit_start, format!("unknown unit '{unit}'")),
                };
                tokens.push((tok, start));
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                tokens.push((Token::Ident(src[start..i].to_string()), start));
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

    fn expr(&mut self) -> Result<Spanned, Error> {
        let pos = self.pos();
        let Some((tok, _)) = self.tokens.get(self.i).cloned() else {
            return err(pos, "expected an expression");
        };
        self.i += 1;
        let expr = match tok {
            Token::Str(s) => Expr::Str(s),
            Token::Num(n) => Expr::Num(n),
            Token::Duration(d) => Expr::Duration(d),
            Token::Ident(name) => {
                self.expect(Token::LParen, &format!("'(' after '{name}'"))?;
                let mut args = Vec::new();
                if self.peek() != Some(&Token::RParen) {
                    args.push(self.expr()?);
                    while self.peek() == Some(&Token::Comma) {
                        self.i += 1;
                        args.push(self.expr()?);
                    }
                }
                self.expect(Token::RParen, "',' or ')'")?;
                Expr::Call { name, args }
            }
            _ => return err(pos, "expected an expression"),
        };
        Ok(Spanned { expr, pos })
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
    fn reports_position() {
        let e = parse("play(sample(\"a\" 3))").unwrap_err();
        assert_eq!(e.pos, 16);
    }
}
