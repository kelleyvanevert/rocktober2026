// The automaton language: lexer + recursive-descent parser.
//
//   program := (stmt | ";")*
//   stmt    := ("step" | "move") DIR [expr]
//            | "note" expr (("dur" | "channel") expr)*   -- each option at most once
//            | "every" expr block
//            | "pattern" STRING block           -- "x" = run, "-"/"." = rest; " " and "|" ignored
//            | "if" expr block ["else" (block | if-stmt)]
//            | "spawn" ["spread"] block         -- spread: one in each of the 8 neighbours
//            | "morph" block                   -- give the others on this cell this code
//            | "once" block                    -- only on the automaton's first frame
//            | "kill"                          -- remove the others on this cell
//            | "value" expr
//   block   := "{" program "}"
//   expr    := sum [(">=" | "<=" | ">" | "<" | "==" | "!=") sum]
//   sum     := product (("+" | "-") product)*
//   product := unary (("*" | "/" | "%") unary)*
//   unary   := "-" unary | NUMBER | NOTE | VAR | call | "(" expr ")"
//   call    := FUNCTION ["(" arg ("," arg)* ")"]  -- see functions.ts
//   arg     := expr | STRING
//   VAR     := "hit" | "hitv" | "value" | "frame"
//
// Comments start with "--". Note names use Ableton's convention: c3 = 60.

import { FUNCTIONS } from "./functions";

export type Var = "hit" | "hitv" | "value" | "frame";
const VARS: string[] = ["hit", "hitv", "value", "frame"] satisfies Var[];

export type Dir = "left" | "right" | "up" | "down";
export type BinOp = "+" | "-" | "*" | "/" | "%" | ">=" | "<=" | ">" | "<" | "==" | "!=";

export type Expr =
  | { kind: "num"; value: number }
  | { kind: "var"; name: Var }
  | { kind: "neg"; arg: Expr }
  | { kind: "call"; name: string; args: Arg[] }
  | { kind: "bin"; op: BinOp; left: Expr; right: Expr };

/** Function argument: strings only appear here, and only where a function expects one. */
export type Arg = Expr | { kind: "str"; value: string };

export type Stmt =
  | { kind: "step"; dir: Dir; count: Expr | null }
  | { kind: "note"; note: Expr; dur: Expr | null; channel: Expr | null }
  | { kind: "every"; n: Expr; body: Program }
  | { kind: "pattern"; steps: boolean[]; body: Program }
  | { kind: "if"; cond: Expr; then: Program; else: Program | null }
  | { kind: "spawn"; body: Program; source: string; spread: boolean }
  | { kind: "morph"; body: Program; source: string }
  | { kind: "value"; value: Expr }
  | { kind: "once"; body: Program }
  | { kind: "kill" };

export type Program = Stmt[];

export class ParseError extends Error {}

const DIRS = ["left", "right", "up", "down"];
const COMPARE = [">=", "<=", ">", "<", "==", "!="];
const PITCH: Record<string, number> = { c: 0, d: 2, e: 4, f: 5, g: 7, a: 9, b: 11 };

/** "c3" -> 60, "c#3" -> 61, "db3" -> 61, "c-2" -> 0 */
export function noteNumber(name: string): number | null {
  const m = /^([a-g])(#|b)?(-?\d+)$/i.exec(name);
  if (!m) return null;
  const accidental = m[2] === "#" ? 1 : m[2] === "b" ? -1 : 0;
  return (Number(m[3]) + 2) * 12 + PITCH[m[1].toLowerCase()] + accidental;
}

// --- Lexer ---

type Token = { type: "num" | "note" | "ident" | "op" | "str" | "eof"; text: string; pos: number };

const TOKEN =
  /(\s+|--[^\n]*)|(\d+(?:\.\d+)?)|([a-gA-G](?:#|b)?-?\d+)(?![\w#])|([a-zA-Z_]\w*)|(>=|<=|==|!=|[-+*/%(){}<>;,])|"([^"\n]*)"/y;

function lex(src: string): Token[] {
  const tokens: Token[] = [];
  TOKEN.lastIndex = 0;
  while (TOKEN.lastIndex < src.length) {
    const pos = TOKEN.lastIndex;
    const m = TOKEN.exec(src);
    if (!m && src[pos] === '"') throw new ParseError(`${where(src, pos)}: missing closing '"'`);
    if (!m) throw new ParseError(`${where(src, pos)}: unexpected '${src[pos]}'`);
    if (m[1]) continue;
    if (m[6] !== undefined) {
      tokens.push({ type: "str", text: m[6], pos });
      continue;
    }
    const type = m[2] ? "num" : m[3] ? "note" : m[4] ? "ident" : "op";
    tokens.push({ type, text: m[0], pos });
  }
  tokens.push({ type: "eof", text: "end of code", pos: src.length });
  return tokens;
}

/** Strip blank edge lines and common indentation, e.g. from the inside of a block. */
function dedent(text: string): string {
  const lines = text.split("\n").map((l) => l.trimEnd());
  while (lines.length && !lines[0].trim()) lines.shift();
  while (lines.length && !lines[lines.length - 1].trim()) lines.pop();
  const indent = Math.min(...lines.filter((l) => l.trim()).map((l) => l.length - l.trimStart().length));
  return lines.map((l) => l.slice(indent)).join("\n");
}

function where(src: string, pos: number) {
  return `line ${src.slice(0, pos).split("\n").length}`;
}

// --- Parser ---

export function parse(src: string): Program {
  const tokens = lex(src);
  let i = 0;

  const peek = () => tokens[i];
  const next = () => tokens[i++];
  const fail = (msg: string, tok = peek()): never => {
    throw new ParseError(`${where(src, tok.pos)}: ${msg}`);
  };
  const accept = (text: string) => (peek().text === text && peek().type !== "num" ? (i++, true) : false);
  const expect = (text: string) => {
    if (!accept(text)) fail(`expected '${text}' but found '${peek().text}'`);
  };

  function program(end: string): Program {
    const stmts: Program = [];
    while (peek().type !== "eof" && peek().text !== end) {
      if (!accept(";")) stmts.push(stmt()); // ";" optionally separates statements
    }
    return stmts;
  }

  function block(): Program {
    expect("{");
    const body = program("}");
    expect("}");
    return body;
  }

  function stmt(): Stmt {
    const tok = next();
    switch (tok.type === "ident" ? tok.text : "") {
      case "step":
      case "move": {
        const dir = next();
        if (!DIRS.includes(dir.text)) fail(`expected a direction (${DIRS.join(", ")})`, dir);
        return { kind: "step", dir: dir.text as Dir, count: startsExpr() ? expr() : null };
      }
      case "note": {
        const note = expr();
        const opts: Record<"dur" | "channel", Expr | null> = { dur: null, channel: null };
        for (let opt; (opt = peek().text) === "dur" || opt === "channel"; ) {
          if (opts[opt]) fail(`'${opt}' given twice`);
          next();
          opts[opt] = expr();
        }
        return { kind: "note", note, ...opts };
      }
      case "every":
        return { kind: "every", n: expr(), body: block() };
      case "pattern": {
        const str = next();
        if (str.type !== "str") fail(`expected a quoted pattern, e.g. pattern "x---x---" { … }`, str);
        const steps: boolean[] = [];
        for (const c of str.text) {
          if (c === " " || c === "|") continue;
          if (c !== "x" && c !== "X" && c !== "-" && c !== ".") {
            fail(`pattern can only contain x (hit) and - or . (rest), not '${c}'`, str);
          }
          steps.push(c === "x" || c === "X");
        }
        if (!steps.length) fail("pattern is empty", str);
        return { kind: "pattern", steps, body: block() };
      }
      case "if":
        return ifStmt();
      case "value":
        return { kind: "value", value: expr() };
      case "once":
        return { kind: "once", body: block() };
      case "kill":
        return { kind: "kill" };
      case "spawn":
      case "morph": {
        const spread = tok.text === "spawn" && accept("spread");
        const open = peek().pos;
        const body = block();
        const close = tokens[i - 1].pos;
        const source = dedent(src.slice(open + 1, close));
        return tok.text === "spawn" ? { kind: "spawn", body, source, spread } : { kind: "morph", body, source };
      }
      default:
        return fail(`unknown command '${tok.text}'`, tok);
    }
  }

  function ifStmt(): Stmt {
    const cond = expr();
    const then = block();
    if (!accept("else")) return { kind: "if", cond, then, else: null };
    if (accept("if")) return { kind: "if", cond, then, else: [ifStmt()] };
    return { kind: "if", cond, then, else: block() };
  }

  // A trailing count is optional, so only treat unambiguous tokens as its start.
  function startsExpr() {
    const t = peek();
    return t.type === "num" || t.type === "note" || t.text === "(" || VARS.includes(t.text) || t.text in FUNCTIONS;
  }

  function expr(): Expr {
    const left = sum();
    const op = peek().text;
    if (peek().type === "op" && COMPARE.includes(op)) {
      next();
      return { kind: "bin", op: op as BinOp, left, right: sum() };
    }
    return left;
  }

  function sum(): Expr {
    let left = product();
    while (peek().text === "+" || peek().text === "-") {
      const op = next().text as BinOp;
      left = { kind: "bin", op, left, right: product() };
    }
    return left;
  }

  function product(): Expr {
    let left = unary();
    while (["*", "/", "%"].includes(peek().text)) {
      const op = next().text as BinOp;
      left = { kind: "bin", op, left, right: unary() };
    }
    return left;
  }

  function unary(): Expr {
    const tok = next();
    if (tok.text === "-" && tok.type === "op") return { kind: "neg", arg: unary() };
    if (tok.type === "num") return { kind: "num", value: Number(tok.text) };
    if (tok.type === "note") return { kind: "num", value: noteNumber(tok.text)! };
    if (VARS.includes(tok.text)) return { kind: "var", name: tok.text as Var };
    if (tok.type === "ident" && tok.text in FUNCTIONS) return call(tok);
    if (/^[a-g](#|b)?$/i.test(tok.text)) fail(`note '${tok.text}' needs an octave, e.g. ${tok.text}3`, tok);
    if (tok.text === "(") {
      const e = expr();
      expect(")");
      return e;
    }
    if (tok.type === "str") fail(`text "${tok.text}" can only be a function argument, like scale("ionian", …)`, tok);
    return fail(`expected a value but found '${tok.text}'`, tok);
  }

  function call(tok: Token): Expr {
    const name = tok.text;
    const { params, bare } = FUNCTIONS[name];
    const usage = `${name}(${params.map((p) => p.name).join(", ")})`;

    if (peek().text !== "(") {
      if (!bare) fail(`${name} needs arguments: ${usage}`, tok);
      return { kind: "call", name, args: bare!.map((value) => ({ kind: "num", value })) };
    }
    next();

    const args: Arg[] = [];
    if (peek().text !== ")") {
      do {
        const param = params[args.length];
        if (!param) fail(`too many arguments, expected ${usage}`);
        if (param.type === "string") {
          const str = next();
          const value = str.text.toLowerCase();
          if (str.type !== "str") fail(`expected a quoted ${param.name} for ${usage}, e.g. "${param.choices[0]}"`, str);
          if (!param.choices.includes(value)) fail(`unknown ${name} ${param.name} "${str.text}"`, str);
          args.push({ kind: "str", value });
        } else {
          args.push(expr());
        }
      } while (accept(","));
    }
    expect(")");
    if (args.length !== params.length) fail(`expected ${usage}`, tok);
    return { kind: "call", name, args };
  }

  const result = program("");
  if (peek().type !== "eof") fail(`unexpected '${peek().text}'`);
  return result;
}

// --- Commenting out (⌘/ on a cell) ---

/** True if every non-blank line is a comment, i.e. the code does nothing. */
export function isCommentedOut(code: string): boolean {
  return code.split("\n").every((line) => !line.trim() || line.trimStart().startsWith("--"));
}

export function commentOut(code: string): string {
  return code
    .split("\n")
    .map((line) => (line.trim() ? `-- ${line}` : line))
    .join("\n");
}

/** Removes one level of "--" per line, so comments that were already there survive. */
export function uncomment(code: string): string {
  return code
    .split("\n")
    .map((line) => line.replace(/^(\s*)-- ?/, "$1"))
    .join("\n");
}
