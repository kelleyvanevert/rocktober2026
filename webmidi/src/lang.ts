// The automaton language: lexer + recursive-descent parser.
//
//   program := (stmt | ";")*
//   stmt    := ("step" | "move") DIR [expr]
//            | "note" expr (("dur" | "channel") expr)*   -- each option at most once
//            | "every" expr block
//            | "if" expr block ["else" (block | if-stmt)]
//            | "spawn" block
//            | "once" block                    -- only on the automaton's first frame
//            | "value" expr
//   block   := "{" program "}"
//   expr    := sum [(">=" | "<=" | ">" | "<" | "==" | "!=") sum]
//   sum     := product (("+" | "-") product)*
//   product := unary (("*" | "/" | "%") unary)*
//   unary   := "-" unary | NUMBER | NOTE | VAR | rand | "(" expr ")"
//   rand    := "rand" ["(" expr "," expr ")"]   -- random int, both ends inclusive; default 0..8
//   VAR     := "hit" | "hitv" | "value" | "frame"
//
// Comments start with "--". Note names use Ableton's convention: c3 = 60.

export type Var = "hit" | "hitv" | "value" | "frame";
const VARS: string[] = ["hit", "hitv", "value", "frame"] satisfies Var[];

export type Dir = "left" | "right" | "up" | "down";
export type BinOp = "+" | "-" | "*" | "/" | "%" | ">=" | "<=" | ">" | "<" | "==" | "!=";

export type Expr =
  | { kind: "num"; value: number }
  | { kind: "var"; name: Var }
  | { kind: "neg"; arg: Expr }
  | { kind: "rand"; min: Expr; max: Expr }
  | { kind: "bin"; op: BinOp; left: Expr; right: Expr };

export type Stmt =
  | { kind: "step"; dir: Dir; count: Expr | null }
  | { kind: "note"; note: Expr; dur: Expr | null; channel: Expr | null }
  | { kind: "every"; n: Expr; body: Program }
  | { kind: "if"; cond: Expr; then: Program; else: Program | null }
  | { kind: "spawn"; body: Program; source: string }
  | { kind: "value"; value: Expr }
  | { kind: "once"; body: Program };

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

type Token = { type: "num" | "note" | "ident" | "op" | "eof"; text: string; pos: number };

const TOKEN =
  /(\s+|--[^\n]*)|(\d+(?:\.\d+)?)|([a-gA-G](?:#|b)?-?\d+)(?![\w#])|([a-zA-Z_]\w*)|(>=|<=|==|!=|[-+*/%(){}<>;,])/y;

function lex(src: string): Token[] {
  const tokens: Token[] = [];
  TOKEN.lastIndex = 0;
  while (TOKEN.lastIndex < src.length) {
    const pos = TOKEN.lastIndex;
    const m = TOKEN.exec(src);
    if (!m) throw new ParseError(`${where(src, pos)}: unexpected '${src[pos]}'`);
    if (m[1]) continue;
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
      case "if":
        return ifStmt();
      case "value":
        return { kind: "value", value: expr() };
      case "once":
        return { kind: "once", body: block() };
      case "spawn": {
        const open = peek().pos;
        const body = block();
        const close = tokens[i - 1].pos;
        return { kind: "spawn", body, source: dedent(src.slice(open + 1, close)) };
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
    return t.type === "num" || t.type === "note" || t.text === "(" || t.text === "rand" || VARS.includes(t.text);
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
    if (tok.text === "rand") {
      if (!accept("(")) return { kind: "rand", min: { kind: "num", value: 0 }, max: { kind: "num", value: 8 } };
      const min = expr();
      expect(",");
      const max = expr();
      expect(")");
      return { kind: "rand", min, max };
    }
    if (/^[a-g](#|b)?$/i.test(tok.text)) fail(`note '${tok.text}' needs an octave, e.g. ${tok.text}3`, tok);
    if (tok.text === "(") {
      const e = expr();
      expect(")");
      return e;
    }
    return fail(`expected a value but found '${tok.text}'`, tok);
  }

  const result = program("");
  if (peek().type !== "eof") fail(`unexpected '${peek().text}'`);
  return result;
}
