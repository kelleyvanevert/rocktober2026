//! Turns parsed expressions into descriptions of sounds, and actions for the
//! session (application thread).
//!
//! Evaluation produces `Sound` and `Control` values (see `desc`): cheap,
//! immutable *descriptions*. Only `play` turns a description into a live,
//! stateful node graph. Keeping those two apart means a description can be
//! stored, reused or played many times at once.
//!
//! A built-in node's name gives a node with all its params free, at their
//! defaults (see `spec`): `wavetable`. Arguments fill its params in order:
//! `wavetable("sine-saw", 0.6)`. `x:name(value)` sets every free param called
//! `name` in `x`, however deep in it, and they're no longer free. A value
//! with a `?name` in it links them to a new free param (`:note(?note + 12)`).
//! So composing sounds keeps their params open, and they can be set (or
//! linked) afterwards, all at once.
//!
//! Every value has one nominal `Type`, and a few convert implicitly where that's
//! lossless and obvious: a number or a pitch is a constant control, and a
//! list of sounds is their mix. Functions are declared as signatures (see
//! `builtins`); a name can have several, and a call runs the first whose
//! parameter types the arguments fit. Errors are generated from the
//! signatures, so they say what was expected where.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use crate::bundle::Bundle;
use crate::clock::{Clock, Grid};
use crate::control;
use crate::desc::{Arg, Mapper, Node, Op, Resolved};
pub use crate::desc::{Control, Sound};
use crate::envelope::Envelope;
use crate::lang::{Error, Expr, Spanned};
use crate::modulation::Modulation;
use crate::nodes::SampleData;
use crate::noise;
use crate::pattern::Pattern;
use crate::resource::ResourceKind;
use crate::reverb::{self, Impulse, MAX_IR_SECONDS};
use crate::sample;
use crate::sidechain::Bus;
use crate::spec::{self, Def, Doc, Role, Spec, Ty, param_doc};
use crate::wavetable::{self, Wavetable};

/// The highest plain number taken as a cutoff pitch: above this it's surely
/// meant as Hz.
const MAX_CUTOFF_NOTE: f64 = 140.0;

/// What running code asks the session to do. Timing is the session's
/// business: it starts things right away, or on the next point of a grid.
pub enum Action {
    /// Play a sound. With a slot name, it replaces what's playing in that slot.
    Play {
        sound: Sound,
        slot: Option<String>,
        at: Option<Grid>,
    },
    /// Play a pattern of notes with an instrument (see `Sound::for_note`).
    Pattern {
        pattern: Arc<Pattern>,
        instrument: Sound,
        slot: Option<String>,
        at: Option<Grid>,
    },
    Bpm(f64),
    StopAll,
}

/// A grid written out literally, like `at 4b` or `at 5b + 2`.
fn literal_grid(e: &Expr) -> Option<Grid> {
    let Expr::Call { name, args } = e else {
        return None;
    };
    match (name.as_str(), args.as_slice()) {
        ("at", [every]) => match every.expr {
            Expr::Beats(b) if b > 0.0 => Some(Grid::new(b)),
            _ => None,
        },
        ("add", [grid, offset]) => {
            let grid = literal_grid(&grid.expr)?;
            match offset.expr {
                Expr::Beats(b) | Expr::Num(b) => Some(Grid {
                    offset: grid.offset + b,
                    ..grid
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The slots a program plays into by name, like `.play("drums")` or
/// `.play(lead, "lead", at 1bar)`, and the grid they start on (if it's
/// written out), found by reading the code rather than running it: a call of
/// `play` with a string among its arguments after the first.
pub fn named_slots(program: &[Spanned]) -> Vec<(String, Option<Grid>)> {
    fn walk(e: &Spanned, out: &mut Vec<(String, Option<Grid>)>) {
        match &e.expr {
            Expr::Call { name, args } => {
                if name == "play"
                    && let Some(slot) = args.iter().skip(1).find_map(|a| match &a.expr {
                        Expr::Str(slot) => Some(slot),
                        _ => None,
                    })
                    && !out.iter().any(|(s, _)| s == slot)
                {
                    let grid = args.iter().skip(1).find_map(|a| literal_grid(&a.expr));
                    out.push((slot.clone(), grid));
                }
                for arg in args {
                    walk(arg, out);
                }
            }
            Expr::Let { value, .. } => walk(value, out),
            Expr::Hole {
                default: Some(d), ..
            } => walk(d, out),
            Expr::Set { target, value, .. } => {
                walk(target, out);
                walk(value, out);
            }
            Expr::List(items) => items.iter().for_each(|i| walk(i, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for stmt in program {
        walk(stmt, &mut out);
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Type {
    Sound,
    Control,
    Number,
    Duration,
    Beats,
    Pitch,
    Pattern,
    String,
    /// `at 4b`: where things may start.
    Grid,
    Function,
    List,
    Nothing,
}

#[derive(Clone)]
enum Value {
    Sound(Sound),
    Control(Control),
    Str(String),
    Num(f64),
    Duration(f64),
    Beats(f64),
    /// A MIDI note number.
    Pitch(f64),
    Pattern(Arc<Pattern>),
    Grid(Grid),
    Function(Arc<Function>),
    List(Vec<Value>),
    Nothing,
}

/// A function defined with `fn name(params) = body`, or `param => body`.
pub struct Function {
    name: String,
    params: Vec<String>,
    body: Spanned,
}

/// How deep user functions may call each other (or themselves).
const MAX_CALL_DEPTH: usize = 64;
const TOO_DEEP: &str = "too many calls inside calls (does a function call itself?)";

impl Value {
    fn ty(&self) -> Type {
        match self {
            Value::Sound(_) => Type::Sound,
            Value::Control(_) => Type::Control,
            Value::Str(_) => Type::String,
            Value::Num(_) => Type::Number,
            Value::Duration(_) => Type::Duration,
            Value::Beats(_) => Type::Beats,
            Value::Pattern(_) => Type::Pattern,
            Value::Pitch(_) => Type::Pitch,
            Value::Grid(_) => Type::Grid,
            Value::Function(_) => Type::Function,
            Value::List(_) => Type::List,
            Value::Nothing => Type::Nothing,
        }
    }

    /// What it is, for an error message.
    fn name(&self) -> &'static str {
        match self {
            Value::Sound(s) if s.has_input() => "an effect",
            Value::Sound(_) => "a sound",
            Value::Control(_) => "a control",
            Value::Str(_) => "a string",
            Value::Num(_) => "a number",
            Value::Duration(_) => "a duration",
            Value::Beats(_) => "a length in beats",
            Value::Pattern(_) => "a pattern",
            Value::Pitch(_) => "a pitch",
            Value::Grid(_) => "a start grid (like at 4b)",
            Value::Function(_) => "a function",
            Value::List(_) => "a list",
            Value::Nothing => "nothing",
        }
    }

    /// Whether this value can be passed where `ty` is expected.
    fn fits(&self, ty: Type) -> bool {
        self.ty() == ty
            || match (ty, self) {
                (Type::Duration, Value::Beats(_)) => true,
                (Type::Control, Value::Num(_) | Value::Pitch(_)) => true,
                (Type::Sound, Value::List(items)) => items.iter().all(|i| i.fits(Type::Sound)),
                _ => false,
            }
    }

    // Accessors for arguments whose type a signature has already checked.

    fn sound(&self) -> Sound {
        match self {
            Value::Sound(s) => s.clone(),
            Value::List(items) => Sound::Add(items.iter().map(Value::sound).collect()),
            _ => unreachable!("not a sound"),
        }
    }

    fn control(&self) -> Control {
        match self {
            Value::Control(c) => c.clone(),
            Value::Num(n) | Value::Pitch(n) => Control::Constant(*n),
            _ => unreachable!("not a control"),
        }
    }

    fn num(&self) -> f64 {
        match self {
            Value::Num(n) => *n,
            _ => unreachable!("not a number"),
        }
    }

    fn pattern(&self) -> Arc<Pattern> {
        match self {
            Value::Pattern(p) => p.clone(),
            _ => unreachable!("not a pattern"),
        }
    }

    fn str(&self) -> &str {
        match self {
            Value::Str(s) => s,
            _ => unreachable!("not a string"),
        }
    }

    /// Its free params, as docs.
    fn params(&self) -> Vec<spec::ParamDoc> {
        match self {
            Value::Sound(s) => s.free_params(),
            Value::Control(c) => c.params(),
            Value::List(items) => {
                let mut all: Vec<spec::ParamDoc> = Vec::new();
                for p in items.iter().flat_map(Value::params) {
                    if !all.iter().any(|q| q.name == p.name) {
                        all.push(p);
                    }
                }
                all
            }
            _ => Vec::new(),
        }
    }

    /// Holes without a default take a same-named hole's default.
    fn share_defaults(self) -> Value {
        match self {
            Value::Sound(s) => Value::Sound(s.share_defaults()),
            Value::Control(c) => Value::Control(c.share_defaults()),
            Value::List(items) => {
                Value::List(items.into_iter().map(Value::share_defaults).collect())
            }
            other => other,
        }
    }
}

/// A parameter in a signature: its type, and what to call it in an error.
#[derive(Clone, Copy)]
struct P(Type, &'static str);

const SOUND: P = P(Type::Sound, "a sound");
const CONTROL: P = P(Type::Control, "a control");
const NUMBER: P = P(Type::Number, "a number");
const SLOT: P = P(Type::String, "a slot name (like \"drums\")");
const GRID: P = P(Type::Grid, "a start grid (like at 4b)");
const PATTERN: P = P(Type::Pattern, "a pattern or a sound");

/// A built-in function signature.
struct Builtin {
    name: &'static str,
    params: &'static [P],
    /// How many of `params` must be given; the rest are optional.
    required: usize,
    /// Any number of further arguments like this.
    rest: Option<P>,
    run: fn(&mut Evaluator, &mut Call) -> Result<Value, Error>,
}

impl Builtin {
    fn takes(&self, n: usize) -> bool {
        n >= self.required && (n <= self.params.len() || self.rest.is_some())
    }

    fn param(&self, i: usize) -> P {
        self.params.get(i).copied().or(self.rest).unwrap()
    }

    /// The first argument that doesn't fit, if any.
    fn mismatch(&self, args: &[Value]) -> Option<usize> {
        args.iter()
            .enumerate()
            .position(|(i, v)| !v.fits(self.param(i).0))
    }

    fn most_args(&self) -> usize {
        if self.rest.is_some() {
            usize::MAX
        } else {
            self.params.len()
        }
    }
}

/// The arguments of a call being run.
struct Call<'a> {
    name: &'static str,
    args: Vec<Value>,
    /// Each argument's position in the source.
    positions: Vec<usize>,
    /// The call's own position.
    pos: usize,
    /// The tempo, for turning beats into seconds.
    bpm: f64,
    actions: &'a mut Vec<Action>,
}

impl Call<'_> {
    /// An error about argument `i`.
    fn fail(&self, i: usize, msg: impl std::fmt::Display) -> Error {
        Error {
            pos: self.positions[i],
            msg: format!("{}: {msg}", self.name),
        }
    }

    /// An error about argument `i`, saying what was wanted instead.
    fn wrong(&self, i: usize, want: &str) -> Error {
        self.fail(i, format!("expected {want}, got {}", self.args[i].name()))
    }

    fn sound(&self, i: usize) -> Sound {
        self.args[i].sound()
    }

    fn control(&self, i: usize) -> Control {
        self.args[i].control()
    }

    /// Argument `i` in seconds: a duration, or beats at the current tempo.
    fn duration(&self, i: usize) -> f64 {
        seconds(&self.args[i], self.bpm).expect("not a duration")
    }

    /// The slot name and start grid among the arguments from `i` on.
    fn slot_and_grid(&self, i: usize) -> (Option<String>, Option<Grid>) {
        let mut found = (None, None);
        for arg in self.args.iter().skip(i) {
            match arg {
                Value::Str(s) => found.0 = Some(s.clone()),
                Value::Grid(g) => found.1 = Some(*g),
                _ => unreachable!("not a slot or grid"),
            }
        }
        found
    }
}

/// A duration or a length in beats, in seconds.
fn seconds(value: &Value, bpm: f64) -> Option<f64> {
    match value {
        Value::Duration(d) => Some(*d),
        Value::Beats(b) => Some(b * 60.0 / bpm),
        _ => None,
    }
}

/// A sound ready to play: no open input, and every free param has a value.
fn playable(c: &Call, i: usize, except: &str) -> Result<Sound, Error> {
    let sound = c.sound(i).share_defaults();
    if sound.has_input() {
        let name = sound
            .nodes()
            .iter()
            .find(|n| n.spec.role == Role::Effect)
            .map_or("it", |n| n.spec.name);
        return Err(c.fail(
            i,
            format!("this is an effect: apply it to a sound, like sound * {name}"),
        ));
    }
    if let Some((name, _)) = sound
        .holes()
        .into_iter()
        .find(|(name, default)| !default && name != except)
    {
        return Err(Error {
            pos: c.pos,
            msg: format!("play: ?{name} has no value (set it with :{name}(...))"),
        });
    }
    Ok(sound)
}

fn play_sound(_: &mut Evaluator, c: &mut Call) -> Result<Value, Error> {
    let sound = playable(c, 0, "")?;
    let (slot, at) = c.slot_and_grid(1);
    c.actions.push(Action::Play { sound, slot, at });
    Ok(Value::Nothing)
}

fn play_pattern(_: &mut Evaluator, c: &mut Call) -> Result<Value, Error> {
    let pattern = c.args[0].pattern();
    let instrument = playable(c, 1, "note")?;
    let holes = instrument.holes();
    let note = holes.iter().find(|(name, _)| name == "note");
    if pattern.has_notes() && note.is_none() {
        return Err(c.fail(
            1,
            "the sound has no free note for the pattern's notes (use x for hits without one)",
        ));
    }
    if pattern.has_unpitched_hits() && note.is_some_and(|(_, default)| !default) {
        return Err(c.fail(
            1,
            "the pattern has hits without a note (x), but ?note has no default",
        ));
    }
    let (slot, at) = c.slot_and_grid(2);
    c.actions.push(Action::Pattern {
        pattern,
        instrument,
        slot,
        at,
    });
    Ok(Value::Nothing)
}

fn grid_offset(_: &mut Evaluator, c: &mut Call) -> Result<Value, Error> {
    let Value::Grid(grid) = c.args[0] else {
        unreachable!()
    };
    let (Value::Beats(offset) | Value::Num(offset)) = c.args[1] else {
        unreachable!()
    };
    Ok(Value::Grid(Grid {
        offset: grid.offset + offset,
        ..grid
    }))
}

fn sound(s: Sound) -> Result<Value, Error> {
    Ok(Value::Sound(s))
}

fn control(c: Control) -> Result<Value, Error> {
    Ok(Value::Control(c))
}

/// Two controls combined, folded right away if both are constants.
fn op(op: Op, a: Control, b: Control) -> Result<Value, Error> {
    match (&a, &b) {
        (Control::Constant(a), Control::Constant(b)) => {
            control(Control::Constant(op.apply(*a, *b)))
        }
        _ => control(Control::Op(op, Box::new(a), Box::new(b))),
    }
}

fn builtin(
    name: &'static str,
    params: &'static [P],
    required: usize,
    run: fn(&mut Evaluator, &mut Call) -> Result<Value, Error>,
) -> Builtin {
    Builtin {
        name,
        params,
        required,
        rest: None,
        run,
    }
}

static BUILTINS: LazyLock<Vec<Builtin>> = LazyLock::new(builtins);

/// Every built-in function (nodes aren't functions: see `spec`). Where a name
/// has several signatures, the first one that fits wins, so exact types come
/// before ones that need converting.
fn builtins() -> Vec<Builtin> {
    const START: P = P(Type::Duration, "a start time (like 0:11:188)");
    const END: P = P(Type::Duration, "an end time (like 0:12:625)");

    vec![
        // Starts right away, or on the next point of the grid. A named slot
        // replaces what played in it.
        builtin("play", &[SOUND, SLOT, GRID], 1, play_sound),
        builtin("play", &[SOUND, GRID], 2, play_sound),
        builtin("play", &[PATTERN, SOUND, SLOT, GRID], 2, play_pattern),
        builtin("play", &[PATTERN, SOUND, GRID], 3, play_pattern),
        builtin(
            "at",
            &[P(Type::Beats, "a grid size in beats (like 4b)")],
            1,
            |_, c| {
                let Value::Beats(every) = c.args[0] else {
                    unreachable!()
                };
                if every <= 0.0 {
                    return Err(c.wrong(0, "a grid size above 0 beats"));
                }
                Ok(Value::Grid(Grid::new(every)))
            },
        ),
        // `at 5b + 2`: an offset into the grid, in beats.
        builtin(
            "add",
            &[GRID, P(Type::Beats, "an offset in beats (like 2b)")],
            2,
            grid_offset,
        ),
        builtin(
            "add",
            &[GRID, P(Type::Number, "an offset in beats (like 2b)")],
            2,
            grid_offset,
        ),
        builtin("stop", &[], 0, |_, c| {
            c.actions.push(Action::StopAll);
            Ok(Value::Nothing)
        }),
        builtin(
            "bpm",
            &[P(Type::Number, "a tempo (like 120)")],
            1,
            |ev, c| {
                let bpm = c.args[0].num();
                if !(20.0..=999.0).contains(&bpm) {
                    return Err(c.wrong(0, "a tempo between 20 and 999"));
                }
                ev.bpm = bpm;
                c.actions.push(Action::Bpm(bpm));
                Ok(Value::Nothing)
            },
        ),
        builtin(
            "notes",
            &[
                P(Type::String, "steps (like \"c2 e2 _ x .\")"),
                P(Type::Beats, "a step length in beats (like 0.25b)"),
            ],
            2,
            |_, c| {
                let Value::Beats(step) = c.args[1] else {
                    unreachable!()
                };
                if step <= 0.0 {
                    return Err(c.wrong(1, "a step length above 0"));
                }
                match Pattern::parse(c.args[0].str(), step) {
                    Ok(pattern) => Ok(Value::Pattern(Arc::new(pattern))),
                    Err(msg) => Err(c.fail(0, msg)),
                }
            },
        ),
        builtin(
            "fit",
            &[SOUND, P(Type::Duration, "a duration (like 500ms)")],
            2,
            |_, c| sound(Sound::Fit(Box::new(c.sound(0)), c.duration(1))),
        ),
        builtin("slice", &[SOUND, START, END], 3, |_, c| {
            let (start, end) = (c.duration(1), c.duration(2));
            if end <= start {
                return Err(c.wrong(2, "an end after the start"));
            }
            sound(Sound::Slice(Box::new(c.sound(0)), start, end))
        }),
        builtin(
            "delay",
            &[SOUND, P(Type::Duration, "a duration (like 12s)")],
            2,
            |_, c| sound(Sound::Delay(Box::new(c.sound(0)), c.duration(1))),
        ),
        // `repeat(inf)` repeats forever (well, usize::MAX times).
        builtin("repeat", &[SOUND, NUMBER], 2, |_, c| {
            let times = repeat_count(c)?;
            sound(Sound::Repeat(Box::new(c.sound(0)), times))
        }),
        builtin(
            "repeat",
            &[P(Type::Pattern, "a pattern"), NUMBER],
            2,
            |_, c| {
                let times = repeat_count(c)?;
                let mut pattern = (*c.args[0].pattern()).clone();
                pattern.times = times;
                Ok(Value::Pattern(Arc::new(pattern)))
            },
        ),
        Builtin {
            rest: Some(SOUND),
            ..builtin("seq", &[], 0, |_, c| {
                sound(Sound::Seq(c.args.iter().map(Value::sound).collect()))
            })
        },
        // `a + b`
        builtin("add", &[NUMBER, NUMBER], 2, |_, c| {
            Ok(Value::Num(c.args[0].num() + c.args[1].num()))
        }),
        builtin("add", &[CONTROL, CONTROL], 2, |_, c| {
            op(Op::Add, c.control(0), c.control(1))
        }),
        Builtin {
            rest: Some(SOUND),
            ..builtin("add", &[], 0, |_, c| {
                sound(Sound::Add(c.args.iter().map(Value::sound).collect()))
            })
        },
        // `a - b`
        builtin("sub", &[NUMBER, NUMBER], 2, |_, c| {
            Ok(Value::Num(c.args[0].num() - c.args[1].num()))
        }),
        builtin("sub", &[CONTROL, CONTROL], 2, |_, c| {
            op(Op::Sub, c.control(0), c.control(1))
        }),
        // `a * b`: a sound times an effect applies the effect.
        builtin("mul", &[NUMBER, NUMBER], 2, |_, c| {
            Ok(Value::Num(c.args[0].num() * c.args[1].num()))
        }),
        builtin("mul", &[SOUND, SOUND], 2, |_, c| {
            let (a, b) = (c.sound(0), c.sound(1));
            if b.has_input() {
                sound(b.apply(&a))
            } else {
                sound(Sound::Multiply(Box::new(a), Box::new(b)))
            }
        }),
        builtin("mul", &[SOUND, CONTROL], 2, |_, c| {
            sound(Sound::Gain(c.control(1), Box::new(c.sound(0))))
        }),
        builtin("mul", &[CONTROL, SOUND], 2, |_, c| {
            sound(Sound::Gain(c.control(0), Box::new(c.sound(1))))
        }),
        builtin("mul", &[CONTROL, CONTROL], 2, |_, c| {
            op(Op::Mul, c.control(0), c.control(1))
        }),
        // `a / b`
        builtin("div", &[NUMBER, NUMBER], 2, |_, c| {
            Ok(Value::Num(Op::Div.apply(c.args[0].num(), c.args[1].num())))
        }),
        builtin("div", &[CONTROL, CONTROL], 2, |_, c| {
            op(Op::Div, c.control(0), c.control(1))
        }),
        // Whole notes only between two pitches: `range(a3, c4)` is a3, a#3,
        // b3 or c4.
        builtin(
            "range",
            &[
                CONTROL,
                P(Type::Control, "a lowest value"),
                P(Type::Control, "a highest value"),
            ],
            3,
            |_, c| {
                let whole = matches!((&c.args[1], &c.args[2]), (Value::Pitch(_), Value::Pitch(_)));
                control(Control::Range {
                    x: Box::new(c.control(0)),
                    lo: Box::new(c.control(1)),
                    hi: Box::new(c.control(2)),
                    whole,
                })
            },
        ),
        builtin("round", &[CONTROL], 1, |_, c| {
            control(Control::Round(Box::new(c.control(0))))
        }),
        // A scale degree (0 is the root, 7 the root an octave up in a
        // 7-note scale, -1 the note below) to a pitch:
        // `random(1b):latch.range(-0.5, 6.5).scale("minor", f4)`.
        builtin(
            "scale",
            &[
                CONTROL,
                P(Type::String, "a scale (like \"minor\")"),
                P(Type::Control, "a root (like f4)"),
            ],
            3,
            |_, c| {
                let name = c.args[1].str();
                let Some(steps) = control::scale(name) else {
                    let names: Vec<_> = control::SCALES.iter().map(|(n, _)| *n).collect();
                    return Err(c.fail(
                        1,
                        format!("unknown scale \"{name}\" (try {})", names.join(", ")),
                    ));
                };
                let degree = Control::Scale(Box::new(c.control(0)), steps);
                op(Op::Add, c.control(2), degree)
            },
        ),
        builtin(
            "map",
            &[
                P(Type::List, "a list"),
                P(Type::Function, "a function (like n => n * 2)"),
            ],
            2,
            |ev, c| {
                let (Value::List(items), Value::Function(f)) = (&c.args[0], &c.args[1]) else {
                    unreachable!()
                };
                let (items, f) = (items.clone(), f.clone());
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(ev.call_values(&f, vec![item], c.positions[1], c.actions)?);
                }
                Ok(Value::List(out))
            },
        ),
    ]
}

/// Hints for names that were something else before params: they're now
/// effects, or params.
fn renamed(name: &str) -> Option<&'static str> {
    Some(match name {
        "with" => "set params with :name(value), like x:note(c3)",
        "gain" => "multiply instead: sound * 0.5 or sound * -6db",
        "gate" => "it's a param now: envelope(\"pluck\"):gate(100ms)",
        "retrig" | "latch" => "it's a param now: mod(\"sweep\"):retrig",
        "free" => "free-running is the default: leave out :retrig and :latch",
        "glide" => "glide is on the note now: sound:note(glide(?note):dur(100ms))",
        "modulation" => "it's called mod now: mod(\"sweep\")",
        _ => return None,
    })
}

/// The values `x:name(value)` can be set to, by type.
fn coerce(value: &Value, ty: Ty, bpm: f64) -> Result<Arg, String> {
    let wrong = || Err(format!("expected {}, got {}", ty.expected(), value.name()));
    Ok(match (ty, value) {
        (Ty::Cutoff, Value::Num(n)) if *n > MAX_CUTOFF_NOTE => {
            return Err(format!(
                "a frequency is a pitch: {n} would be note {n} (did you mean {n}hz?)"
            ));
        }
        (Ty::Control | Ty::Pitch | Ty::Cutoff, v) if v.fits(Type::Control) => {
            Arg::Control(v.control())
        }
        (Ty::Num, Value::Num(n)) => Arg::Num(*n),
        (Ty::Str | Ty::Space, Value::Str(s)) => Arg::Str(s.clone()),
        (Ty::Space, Value::Sound(s)) if !s.has_input() => Arg::Sound(Box::new(s.clone())),
        (Ty::Seconds, v) => match seconds(v, bpm) {
            Some(s) if s >= 0.0 => Arg::Seconds(s),
            _ => return wrong(),
        },
        (Ty::Count, Value::Num(n)) if *n == f64::INFINITY || (*n >= 0.0 && n.fract() == 0.0) => {
            Arg::Num(*n)
        }
        (Ty::Flag, Value::Num(n)) => Arg::Num((*n != 0.0) as u8 as f64),
        _ => return wrong(),
    })
}

/// A control filling the hole `name`: a hole in it of the same name and
/// without a default gets the filled hole's default, so `:note(?note + 12)`
/// keeps note's default.
fn inherit(c: Control, name: &str, default: Option<&Control>) -> Control {
    let Some(default) = default else {
        return c;
    };
    struct Inherit<'a>(&'a str, &'a Control);
    impl Mapper for Inherit<'_> {
        type Error = std::convert::Infallible;
        fn control(&mut self, c: &Control) -> Result<Control, Self::Error> {
            match c {
                Control::Hole {
                    name,
                    default: None,
                } if name == self.0 => Ok(Control::Hole {
                    name: name.clone(),
                    default: Some(Box::new(self.1.clone())),
                }),
                Control::Hole { .. } => Ok(c.clone()),
                c => c.try_map(self),
            }
        }
    }
    let Ok(c) = Inherit(name, default).control(&c);
    c
}

/// Sets every free param called `name` to `value`, counting them.
struct Assign<'a> {
    ev: &'a mut Evaluator,
    name: &'a str,
    value: &'a Value,
    hits: usize,
}

impl Assign<'_> {
    fn value(&mut self, v: &Value) -> Result<Value, String> {
        Ok(match v {
            Value::Sound(s) => Value::Sound(self.sound(s)?),
            Value::Control(c) => Value::Control(self.control(c)?),
            Value::List(items) => Value::List(
                items
                    .iter()
                    .map(|i| self.value(i))
                    .collect::<Result<_, _>>()?,
            ),
            other => other.clone(),
        })
    }

    fn node(&mut self, n: &Node) -> Result<Node, String> {
        let mut node = n.try_map(self)?;
        let mut changed = false;
        for (i, param) in node.spec.params.iter().enumerate() {
            if param.ty.is_control() || !node.free[i] || param.name != self.name {
                continue;
            }
            node.args[i] = coerce(self.value, param.ty, self.ev.bpm)?;
            node.free[i] = false;
            self.hits += 1;
            changed = true;
        }
        if changed {
            self.ev.resolve(&mut node)?;
        }
        Ok(node)
    }
}

impl Mapper for Assign<'_> {
    type Error = String;

    fn sound(&mut self, s: &Sound) -> Result<Sound, String> {
        match s {
            Sound::Node(node) => Ok(Sound::Node(Box::new(self.node(node)?))),
            s => s.try_map(self),
        }
    }

    fn control(&mut self, c: &Control) -> Result<Control, String> {
        match c {
            Control::Hole { name, default } if name == self.name => {
                let ty = param_doc(name)
                    .map(|p| p.ty)
                    .filter(|ty| ty.is_control())
                    .unwrap_or(Ty::Control);
                let Arg::Control(value) = coerce(self.value, ty, self.ev.bpm)? else {
                    unreachable!("a control type gives a control")
                };
                self.hits += 1;
                Ok(inherit(value, name, default.as_deref()))
            }
            Control::Node(node) => Ok(Control::Node(Box::new(self.node(node)?))),
            c => c.try_map(self),
        }
    }
}

fn repeat_count(c: &Call) -> Result<usize, Error> {
    let n = c.args[1].num();
    if n == f64::INFINITY {
        Ok(usize::MAX)
    } else if n >= 0.0 && n.fract() == 0.0 {
        Ok(n as usize)
    } else {
        Err(c.wrong(1, "a whole number or inf"))
    }
}

/// A decoded part of a file, as (start, end) seconds in `f64::to_bits` form so it
/// can be a map key.
type Window = (u64, Option<u64>);

pub struct Evaluator {
    sample_rate: u32,
    /// The resources in the `.rock` file.
    bundle: Bundle,
    /// Decoded samples, by where they're from (see `Evaluator::load`), so each
    /// is only decoded once. Holding an `Arc` here also guarantees the last
    /// reference to a sample is never dropped on the audio thread.
    cache: HashMap<(String, Window), Arc<SampleData>>,
    /// Prepared preset impulse responses, by name.
    spaces: HashMap<String, Arc<Impulse>>,
    /// Wavetables by name (built-in) or name and version (in the bundle). Like
    /// samples, they're kept so they're never freed on the audio thread.
    wavetables: HashMap<String, Arc<Wavetable>>,
    /// The tempo, for beats in durations.
    bpm: f64,
    /// Values named with `let`. They live as long as the evaluator, so a block
    /// can use what an earlier one defined.
    vars: HashMap<String, Value>,
    /// The buses `duck` listens to, by slot name.
    buses: HashMap<String, Arc<Bus>>,
    /// How many user functions are being called inside each other.
    depth: usize,
}

impl Evaluator {
    pub fn new(sample_rate: u32, bundle: Bundle) -> Self {
        Self {
            sample_rate,
            bundle,
            cache: HashMap::new(),
            spaces: HashMap::new(),
            wavetables: HashMap::new(),
            bpm: Clock::DEFAULT_BPM,
            vars: HashMap::new(),
            buses: HashMap::new(),
            depth: 0,
        }
    }

    /// The buses `duck` listens to, by slot name: the session has the engine
    /// fill them from those slots' voices.
    pub fn buses(&self) -> impl Iterator<Item = (&String, &Arc<Bus>)> {
        self.buses.iter()
    }

    fn bus(&mut self, slot: &str) -> Arc<Bus> {
        self.buses.entry(slot.to_string()).or_default().clone()
    }

    pub fn bundle(&self) -> &Bundle {
        &self.bundle
    }

    /// Documentation for a name: a value named with `let` (its free params),
    /// or a built-in.
    pub fn describe(&self, name: &str) -> Option<Doc> {
        let Some(value) = self.vars.get(name) else {
            return Doc::builtin(name);
        };
        let kind = match value {
            Value::Function(f) => format!("fn {}({})", name, f.params.join(", ")),
            v => v
                .name()
                .trim_start_matches("a ")
                .trim_start_matches("an ")
                .to_string(),
        };
        let params = value.params();
        let summary = match value {
            Value::Sound(s) if s.has_input() => {
                format!("An effect: apply it with sound * {name}.")
            }
            Value::Pattern(p) => format!(
                "{} steps of {}b; play it with {name}.play(sound).",
                p.steps.len(),
                spec::trim(p.step)
            ),
            Value::Num(n) => spec::trim(*n),
            Value::Str(s) => format!("\"{s}\""),
            _ if params.is_empty() => "Its params are all set.".to_string(),
            _ => format!("Its free params, set with {name}:param(value):"),
        };
        Some(Doc {
            title: name.to_string(),
            kind,
            summary,
            params,
            examples: Vec::new(),
        })
    }

    /// Evaluate a program, returning the commands it wants sent to the audio thread.
    /// Nothing is sent if any statement fails, so a typo never half-plays a line.
    /// (The same goes for `let`s: they only take effect if everything succeeds.)
    pub fn run(&mut self, program: &[Spanned]) -> Result<Vec<Action>, Error> {
        let (vars, bpm) = (self.vars.clone(), self.bpm);
        let mut actions = Vec::new();
        for stmt in program {
            if let Err(e) = self.eval(stmt, &mut actions) {
                self.vars = vars;
                self.bpm = bpm;
                return Err(e);
            }
        }
        Ok(actions)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn eval(&mut self, e: &Spanned, actions: &mut Vec<Action>) -> Result<Value, Error> {
        let fail = |msg: String| Err(Error { pos: e.pos, msg });
        let (name, args) = match &e.expr {
            Expr::Str(s) => return Ok(Value::Str(s.clone())),
            Expr::Num(n) => return Ok(Value::Num(*n)),
            Expr::Duration(d) => return Ok(Value::Duration(*d)),
            Expr::Beats(b) => return Ok(Value::Beats(*b)),
            Expr::Pitch(note) => return Ok(Value::Pitch(*note)),
            Expr::Var(name) => {
                if let Some(value) = self.vars.get(name) {
                    return Ok(value.clone());
                }
                if let Some(spec) = spec::spec(name) {
                    return self.node(spec, Vec::new(), &[], e.pos);
                }
                return match renamed(name) {
                    _ if BUILTINS.iter().any(|b| b.name == name) => {
                        fail(format!("'{name}' is a function: call it with {name}(...)"))
                    }
                    Some(hint) => fail(format!("unknown name '{name}': {hint}")),
                    None => fail(format!("unknown name '{name}'")),
                };
            }
            Expr::Let { name, value } => {
                let value = self.eval(value, actions)?;
                self.vars.insert(name.clone(), value);
                return Ok(Value::Nothing);
            }
            Expr::Fn { name, params, body } => {
                if BUILTINS.iter().any(|b| b.name == name) || spec::spec(name).is_some() {
                    return fail(format!("'{name}' is already a built-in"));
                }
                let function = Function {
                    name: name.clone(),
                    params: params.clone(),
                    body: (**body).clone(),
                };
                self.vars
                    .insert(name.clone(), Value::Function(Arc::new(function)));
                return Ok(Value::Nothing);
            }
            Expr::Lambda { param, body } => {
                return Ok(Value::Function(Arc::new(Function {
                    name: format!("{param} =>"),
                    params: vec![param.clone()],
                    body: (**body).clone(),
                })));
            }
            Expr::Hole { name, default } => {
                let default = match default {
                    None => None,
                    Some(d) => match self.eval(d, actions)? {
                        v if v.fits(Type::Control) => Some(Box::new(v.control())),
                        v => {
                            return Err(Error {
                                pos: d.pos,
                                msg: format!(
                                    "?{name}: expected a default like 0.2 or c2, got {}",
                                    v.name()
                                ),
                            });
                        }
                    },
                };
                let name = name.clone();
                return Ok(Value::Control(Control::Hole { name, default }));
            }
            Expr::Set {
                target,
                name,
                value,
            } => {
                let target = self.eval(target, actions)?;
                let v = self.eval(value, actions)?;
                return self
                    .set(&target, name, &v)
                    .map_err(|msg| Error { pos: e.pos, msg });
            }
            Expr::List(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.eval(item, actions)?);
                }
                return Ok(Value::List(values));
            }
            Expr::Call { name, args } => (name.as_str(), args),
        };

        if let Some(spec) = spec::spec(name) {
            let mut values = Vec::with_capacity(args.len());
            for arg in args {
                values.push(self.eval(arg, actions)?);
            }
            let positions: Vec<usize> = args.iter().map(|a| a.pos).collect();
            return self.node(spec, values, &positions, e.pos);
        }

        let candidates: Vec<&Builtin> = BUILTINS.iter().filter(|b| b.name == name).collect();
        if candidates.is_empty() {
            return match self.vars.get(name) {
                Some(Value::Function(f)) => {
                    let f = f.clone();
                    let mut values = Vec::with_capacity(args.len());
                    for arg in args {
                        values.push(self.eval(arg, actions)?);
                    }
                    self.call_values(&f, values, e.pos, actions)
                }
                Some(v) => fail(format!("'{name}' is {}, not a function", v.name())),
                None => match renamed(name) {
                    Some(hint) => fail(format!("unknown function '{name}': {hint}")),
                    None => fail(format!("unknown function '{name}'")),
                },
            };
        }

        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            values.push(self.eval(arg, actions)?);
        }

        let n = values.len();
        let fitting: Vec<&Builtin> = candidates.iter().copied().filter(|b| b.takes(n)).collect();
        if fitting.is_empty() {
            let least = candidates.iter().map(|b| b.required).min().unwrap();
            let most = candidates.iter().map(|b| b.most_args()).max().unwrap();
            let expected = match (least, most) {
                (a, b) if a == b => format!("{a}"),
                (a, b) if b == a + 1 => format!("{a} or {b}"),
                (a, usize::MAX) => format!("at least {a}"),
                (a, b) => format!("{a} to {b}"),
            };
            return fail(format!("{name} takes {expected} argument(s), got {n}"));
        }

        let Some(found) = fitting.iter().find(|b| b.mismatch(&values).is_none()) else {
            // Blame the argument that the closest signatures got stuck on.
            let stuck = |b: &Builtin| b.mismatch(&values).unwrap();
            let at = fitting.iter().map(|b| stuck(b)).max().unwrap();
            let mut wanted: Vec<&str> = Vec::new();
            for b in fitting.iter().filter(|b| stuck(b) == at) {
                let hint = b.param(at).1;
                if !wanted.contains(&hint) {
                    wanted.push(hint);
                }
            }
            return Err(Error {
                pos: args[at].pos,
                msg: format!(
                    "{name}: expected {}, got {}",
                    wanted.join(" or "),
                    values[at].name()
                ),
            });
        };

        let mut call = Call {
            name: found.name,
            args: values,
            positions: args.iter().map(|a| a.pos).collect(),
            pos: e.pos,
            bpm: self.bpm,
            actions,
        };
        (found.run)(self, &mut call)
    }

    /// A built-in node, with `args` filling its free params in order.
    fn node(
        &mut self,
        spec: &'static Spec,
        args: Vec<Value>,
        positions: &[usize],
        pos: usize,
    ) -> Result<Value, Error> {
        let name = spec.name;
        let mut node = Node::new(spec, self.bpm);
        let free: Vec<usize> = (0..spec.params.len()).filter(|i| node.free[*i]).collect();
        if args.len() > free.len() {
            let names: Vec<&str> = free.iter().map(|i| spec.params[*i].name).collect();
            return Err(Error {
                pos,
                msg: format!(
                    "{name} takes at most {} value(s) ({}), got {}",
                    free.len(),
                    names.join(", "),
                    args.len()
                ),
            });
        }
        for (k, value) in args.iter().enumerate() {
            let at = |msg: String| Error {
                pos: positions[k],
                msg,
            };
            if k == 0
                && spec.role == Role::Effect
                && spec.params[0].ty != Ty::Space
                && value.fits(Type::Sound)
            {
                return Err(at(format!(
                    "{name} is an effect now: apply it to a sound, like sound * {name}:{}(...)",
                    spec.params[0].name
                )));
            }
            let i = free[k];
            let param = &spec.params[i];
            let arg = coerce(value, param.ty, self.bpm)
                .map_err(|msg| at(format!("{name}: {}: {msg}", param.name)))?;
            node.args[i] = match (arg, &node.args[i]) {
                (Arg::Control(c), Arg::Control(Control::Hole { default, .. })) => {
                    Arg::Control(inherit(c, param.name, default.as_deref()))
                }
                (arg, _) => arg,
            };
            node.free[i] = false;
        }
        if let Some(param) = spec
            .params
            .iter()
            .enumerate()
            .find(|(i, p)| p.default == Def::Required && node.free[*i])
            .map(|(_, p)| p)
        {
            let example = spec.examples.first().copied().unwrap_or_default();
            return Err(Error {
                pos,
                msg: format!("{name} needs its {}, like {example}", param.name),
            });
        }
        self.resolve(&mut node).map_err(|msg| Error {
            pos: positions.first().copied().unwrap_or(pos),
            msg: format!("{name}: {msg}"),
        })?;
        Ok(match spec.role {
            Role::Control => Value::Control(Control::Node(Box::new(node))),
            Role::Source | Role::Effect => Value::Sound(Sound::Node(Box::new(node))),
        })
    }

    /// Load what a node's params refer to (a sample, a table, ...), and check
    /// the values that are only wrong together.
    fn resolve(&mut self, node: &mut Node) -> Result<(), String> {
        node.resolved = match node.kind() {
            spec::Kind::Wavetable => Resolved::Table(self.wavetable(node.str("table"))?),
            spec::Kind::Noise => {
                let name = node.str("color");
                Resolved::Noise(noise::Color::from_name(name).ok_or_else(|| {
                    format!(
                        "unknown color \"{name}\" (try {})",
                        noise::Color::NAMES.join(", ")
                    )
                })?)
            }
            spec::Kind::Sample => {
                let start = node.seconds("start").unwrap_or(0.0);
                let end = node.seconds("end");
                if end.is_some_and(|end| end <= start) {
                    return Err("the end has to be after the start".into());
                }
                Resolved::Sample(self.load(node.str("file"), start, end)?)
            }
            spec::Kind::Reverb => {
                let mix = node.num("mix");
                if !(0.0..=1.0).contains(&mix) {
                    return Err("mix: expected a mix between 0 and 1".into());
                }
                Resolved::Impulse(match node.arg("space") {
                    Arg::Str(name) => self.space(name).ok_or_else(|| {
                        let names = reverb::preset_names().join(", ");
                        format!("unknown space \"{name}\" (try {names}, or a sound)")
                    })?,
                    Arg::Sound(space) => Arc::new(Impulse::new(self.render(space, MAX_IR_SECONDS))),
                    _ => unreachable!("a space is a name or a sound"),
                })
            }
            spec::Kind::Duck => Resolved::Bus(self.bus(node.str("key"))),
            spec::Kind::Envelope => {
                Resolved::Envelope(Arc::new(self.load_envelope(node.str("env"))?))
            }
            spec::Kind::Mod => {
                Resolved::Modulation(Arc::new(self.load_modulation(node.str("mod"))?))
            }
            spec::Kind::Random => {
                if node.seconds("every").is_none_or(|s| s <= 0.0) {
                    return Err("every: expected a period above 0".into());
                }
                match node.resolved {
                    Resolved::Seed(seed) => Resolved::Seed(seed),
                    _ => Resolved::Seed(noise::next_seed()),
                }
            }
            spec::Kind::Echo | spec::Kind::Pingpong => {
                if node.seconds("time").is_none_or(|s| s <= 0.0) {
                    return Err("time: expected a delay time above 0".into());
                }
                Resolved::None
            }
            spec::Kind::Limit => {
                if node.num("ceiling") <= 0.0 {
                    return Err("ceiling: expected a positive ceiling (like 0.9 or -1db)".into());
                }
                Resolved::None
            }
            _ => Resolved::None,
        };
        Ok(())
    }

    /// `target:name(value)`.
    fn set(&mut self, target: &Value, name: &str, value: &Value) -> Result<Value, String> {
        if !matches!(target, Value::Sound(_) | Value::Control(_) | Value::List(_)) {
            return Err(format!(":{name}: {} has no params", target.name()));
        }
        let mut assign = Assign {
            ev: self,
            name,
            value,
            hits: 0,
        };
        let result = assign.value(target).map_err(|e| {
            if e.starts_with(&format!("{name}:")) {
                e
            } else {
                format!("{name}: {e}")
            }
        })?;
        if assign.hits == 0 {
            let free: Vec<String> = target.params().into_iter().map(|p| p.name).collect();
            let there = if free.is_empty() {
                "it has no free params left".to_string()
            } else {
                format!("free: {}", free.join(", "))
            };
            return Err(format!("there's no free {name} to set ({there})"));
        }
        Ok(result.share_defaults())
    }

    /// Call a user function (or a lambda) with evaluated arguments: its
    /// parameters are bound to them while its body is evaluated. Names in the
    /// body are looked up when it's called, so it sees the `let`s and
    /// functions there are by then.
    ///
    /// The body was parsed from whichever code defined the function, so its
    /// positions mean nothing in the code being run: errors from inside it
    /// are reported at the call, with the function's name in front.
    fn call_values(
        &mut self,
        f: &Function,
        values: Vec<Value>,
        pos: usize,
        actions: &mut Vec<Action>,
    ) -> Result<Value, Error> {
        if values.len() != f.params.len() {
            return Err(Error {
                pos,
                msg: format!(
                    "{} takes {} argument(s), got {}",
                    f.name,
                    f.params.len(),
                    values.len()
                ),
            });
        }
        if self.depth >= MAX_CALL_DEPTH {
            return Err(Error {
                pos,
                msg: TOO_DEEP.to_string(),
            });
        }
        let shadowed: Vec<Option<Value>> = f
            .params
            .iter()
            .zip(values)
            .map(|(param, value)| self.vars.insert(param.clone(), value))
            .collect();
        self.depth += 1;
        let result = self.eval(&f.body, actions);
        self.depth -= 1;
        for (param, old) in f.params.iter().zip(shadowed) {
            match old {
                Some(value) => self.vars.insert(param.clone(), value),
                None => self.vars.remove(param),
            };
        }
        result.map_err(|e| {
            // Runaway recursion gets one name in front (the outermost), not
            // one per call.
            let msg = if e.msg == TOO_DEEP && self.depth > 0 {
                e.msg
            } else {
                format!("{}: {}", f.name, e.msg)
            };
            Error { pos, msg }
        })
    }

    /// A preset space's impulse response, synthesized on first use.
    fn space(&mut self, name: &str) -> Option<Arc<Impulse>> {
        if let Some(impulse) = self.spaces.get(name) {
            return Some(impulse.clone());
        }
        let impulse = Arc::new(Impulse::new(reverb::preset(name, self.sample_rate)?));
        self.spaces.insert(name.to_string(), impulse.clone());
        Some(impulse)
    }

    /// Render a sound to a buffer, here on the application thread (e.g. to use
    /// it as an impulse response). Stops after `max_seconds`, fading out.
    fn render(&self, sound: &Sound, max_seconds: f64) -> Vec<crate::nodes::Frame> {
        let max = (max_seconds * self.sample_rate as f64) as usize;
        let mut node = sound.share_defaults().instantiate(self.sample_rate);
        let mut frames = vec![[0.0; 2]; max];
        let n = node.process(&mut frames);
        frames.truncate(n);
        if n == max {
            let fade = (self.sample_rate as usize / 100).min(n);
            for (i, f) in frames[n - fade..].iter_mut().enumerate() {
                let gain = 1.0 - i as f32 / fade as f32;
                f[0] *= gain;
                f[1] *= gain;
            }
        }
        frames
    }

    fn load(
        &mut self,
        name: &str,
        start: f64,
        end: Option<f64>,
    ) -> Result<Arc<SampleData>, String> {
        let (id, source) = sample_source(&self.bundle, name).ok_or_else(|| {
            format!("\"{name}\" isn't in this file yet: put the cursor on it to add it")
        })?;
        let key = (id, (start.to_bits(), end.map(f64::to_bits)));
        if let Some(data) = self.cache.get(&key) {
            return Ok(data.clone());
        }
        let data = Arc::new(sample::load_range(&source, start, end)?);
        self.cache.insert(key, data.clone());
        Ok(data)
    }

    /// An envelope or modulation file's contents. (These are read fresh every
    /// time, so edits apply the next time the code runs.)
    fn resource_data(&self, kind: ResourceKind, name: &str) -> Result<Arc<[u8]>, String> {
        match self.bundle.get(kind, name) {
            Some(entry) => Ok(entry.data),
            None => Err(format!(
                "\"{name}\" doesn't exist yet ({}): put the cursor on it to create it",
                kind.bundle_path(name)
            )),
        }
    }

    /// A built-in wavetable, or one from the bundle's `wavetables/`.
    fn wavetable(&mut self, name: &str) -> Result<Arc<Wavetable>, String> {
        let entry = self.bundle.get(ResourceKind::Wavetable, name);
        let key = match &entry {
            Some(entry) => format!("{name}#{}", entry.version),
            None => name.to_string(),
        };
        if let Some(table) = self.wavetables.get(&key) {
            return Ok(table.clone());
        }
        let table = match entry {
            Some(entry) => Wavetable::load(&sample::Source::Memory {
                name: ResourceKind::Wavetable.file_name(name),
                data: entry.data,
            })?,
            None => Wavetable::builtin(name).ok_or_else(|| {
                format!(
                    "unknown wavetable \"{name}\" (try {}, or add {}/{})",
                    wavetable::BUILTIN_NAMES.join(", "),
                    ResourceKind::Wavetable.dir(),
                    ResourceKind::Wavetable.file_name(name)
                )
            })?,
        };
        let table = Arc::new(table);
        self.wavetables.insert(key, table.clone());
        Ok(table)
    }

    fn load_envelope(&self, name: &str) -> Result<Envelope, String> {
        Envelope::from_json(name, &self.resource_data(ResourceKind::Envelope, name)?)
    }

    fn load_modulation(&self, name: &str) -> Result<Modulation, String> {
        Modulation::from_json(name, &self.resource_data(ResourceKind::Modulation, name)?)
    }
}

/// Where `sample(name)` reads from, if it's in the bundle, and an id for
/// caching what was decoded, which changes when the sample does.
pub fn sample_source(bundle: &Bundle, name: &str) -> Option<(String, sample::Source)> {
    let entry = bundle.get(ResourceKind::Sample, name)?;
    let id = format!("{name}#{}", entry.version);
    let source = sample::Source::Memory {
        name: name.to_string(),
        data: entry.data,
    };
    Some((id, source))
}

#[cfg(test)]
mod tests;
