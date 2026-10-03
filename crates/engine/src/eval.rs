//! Turns parsed expressions into commands for the audio thread (application thread).
//!
//! Evaluation produces `Sound` values: cheap, immutable *descriptions* of sounds.
//! Only `play` turns a description into a live, stateful node graph. Keeping those
//! two apart means a description can be stored, reused or played many times at
//! once, each play getting its own fresh playback state. `Control` is the same for
//! control signals (envelopes, modulations).
//!
//! Every value has one nominal `Type`, and a few convert implicitly where that's
//! lossless and obvious: a number is a constant control, and an envelope is a
//! control that's never released. Functions are declared as signatures (see
//! `builtins`); a name can have several, and a call runs the first whose
//! parameter types the arguments fit. Errors are generated from the
//! signatures, so they say what was expected where.
//!
//! A control can be a hole (`?pos`), to be filled in later with `.with(pos:
//! ...)`. Filling holes makes a new description, so one instrument can be
//! filled in several ways; nothing is instantiated until `play`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use crate::control::{self, Combine, ControlNode, EnvelopePlayer, ModulationPlayer, Param};
use crate::engine::Command;
use crate::envelope::Envelope;
use crate::lang::{Error, Expr, Spanned};
use crate::modulation::Modulation;
use crate::nodes::{
    Add, Delay, Fit, Gain, Limit, Multiply, Node, Repeat, SampleData, Sampler, Seq, Slice,
};
use crate::resource::{self, ResourceKind, Resources};
use crate::reverb::{self, Impulse, MAX_IR_SECONDS, Reverb};
use crate::sample;
use crate::wavetable::{self, Oscillator, Wavetable};

/// Fade-out applied where `fit` cuts a sound off. A few ms is enough to remove the
/// click without audibly softening a transient.
const FIT_FADE_SECONDS: f64 = 0.003;

/// `limit` defaults: -1 dBFS ceiling, leaving a little room for the resampling
/// and DACs that come after us.
const LIMIT_CEILING: f64 = 0.891;
const LIMIT_LOOKAHEAD_SECONDS: f64 = 0.005;
const LIMIT_RELEASE_SECONDS: f64 = 0.1;

/// Default `reverb` wet/dry mix.
const REVERB_MIX: f64 = 0.3;

#[derive(Clone)]
pub enum Sound {
    Sample(Arc<SampleData>),
    Fit(Box<Sound>, f64),
    Delay(Box<Sound>, f64),
    /// Start and end, in seconds.
    Slice(Box<Sound>, f64, f64),
    Repeat(Box<Sound>, usize),
    Add(Vec<Sound>),
    Seq(Vec<Sound>),
    Gain(Control, Box<Sound>),
    /// Two sounds multiplied (ring modulation).
    Multiply(Box<Sound>, Box<Sound>),
    Limit(f64, Box<Sound>),
    /// Impulse response and wet/dry mix.
    Reverb(Box<Sound>, Arc<Impulse>, f64),
    Wavetable {
        table: Arc<Wavetable>,
        position: Control,
        warp: Control,
        /// A MIDI note number.
        pitch: Control,
    },
}

impl Sound {
    /// The same sound with every control replaced by `f(control)`.
    fn map_controls(&self, f: &mut dyn FnMut(&Control) -> Control) -> Sound {
        let mut m = |s: &Sound| Box::new(s.map_controls(f));
        match self {
            Sound::Sample(data) => Sound::Sample(data.clone()),
            Sound::Fit(child, seconds) => Sound::Fit(m(child), *seconds),
            Sound::Delay(child, seconds) => Sound::Delay(m(child), *seconds),
            Sound::Slice(child, start, end) => Sound::Slice(m(child), *start, *end),
            Sound::Repeat(child, times) => Sound::Repeat(m(child), *times),
            Sound::Add(children) => {
                Sound::Add(children.iter().map(|c| c.map_controls(f)).collect())
            }
            Sound::Seq(children) => {
                Sound::Seq(children.iter().map(|c| c.map_controls(f)).collect())
            }
            Sound::Gain(amount, child) => {
                let amount = f(amount);
                Sound::Gain(amount, Box::new(child.map_controls(f)))
            }
            Sound::Multiply(a, b) => {
                let a = m(a);
                Sound::Multiply(a, Box::new(b.map_controls(f)))
            }
            Sound::Limit(ceiling, child) => Sound::Limit(*ceiling, m(child)),
            Sound::Reverb(child, impulse, mix) => Sound::Reverb(m(child), impulse.clone(), *mix),
            Sound::Wavetable {
                table,
                position,
                warp,
                pitch,
            } => Sound::Wavetable {
                table: table.clone(),
                position: f(position),
                warp: f(warp),
                pitch: f(pitch),
            },
        }
    }

    /// Every hole in the sound, and whether it has a default.
    fn holes(&self) -> Vec<(String, bool)> {
        let mut holes = Vec::new();
        self.map_controls(&mut |c| {
            c.holes(&mut holes);
            c.clone()
        });
        holes
    }

    pub fn instantiate(&self, sample_rate: u32) -> Box<dyn Node> {
        let frames = |seconds: f64| (seconds * sample_rate as f64).round() as usize;
        match self {
            Sound::Sample(data) => Box::new(Sampler::new(data.clone(), sample_rate)),
            Sound::Fit(child, seconds) => Box::new(Fit::new(
                child.instantiate(sample_rate),
                frames(*seconds),
                frames(FIT_FADE_SECONDS),
            )),
            Sound::Slice(child, start, end) => Box::new(Slice::new(
                child.instantiate(sample_rate),
                frames(*start),
                frames(*end) - frames(*start),
                frames(FIT_FADE_SECONDS),
            )),
            Sound::Delay(child, seconds) => {
                Box::new(Delay::new(child.instantiate(sample_rate), frames(*seconds)))
            }
            Sound::Repeat(child, times) => {
                Box::new(Repeat::new(child.instantiate(sample_rate), *times))
            }
            Sound::Add(children) => Box::new(Add::new(
                children
                    .iter()
                    .map(|c| c.instantiate(sample_rate))
                    .collect(),
            )),
            Sound::Seq(children) => Box::new(Seq::new(
                children
                    .iter()
                    .map(|c| c.instantiate(sample_rate))
                    .collect(),
            )),
            Sound::Gain(amount, child) => Box::new(Gain::new(
                amount.param(sample_rate),
                child.instantiate(sample_rate),
            )),
            Sound::Multiply(a, b) => Box::new(Multiply::new(
                a.instantiate(sample_rate),
                b.instantiate(sample_rate),
            )),
            Sound::Reverb(child, impulse, mix) => Box::new(Reverb::new(
                child.instantiate(sample_rate),
                impulse.clone(),
                *mix as f32,
            )),
            Sound::Limit(ceiling, child) => Box::new(Limit::new(
                child.instantiate(sample_rate),
                *ceiling as f32,
                frames(LIMIT_LOOKAHEAD_SECONDS),
                (LIMIT_RELEASE_SECONDS * sample_rate as f64) as f32,
            )),
            Sound::Wavetable {
                table,
                position,
                warp,
                pitch,
            } => Box::new(Oscillator::new(
                table.clone(),
                position.param(sample_rate),
                warp.param(sample_rate),
                pitch.param(sample_rate),
                sample_rate,
            )),
        }
    }
}

/// A description of a control signal, the counterpart of `Sound`.
#[derive(Clone)]
pub enum Control {
    Constant(f64),
    /// Played this many times, then held at its last value.
    Modulation(Arc<Modulation>, usize),
    /// Released after the gate (in seconds), if it has one.
    Envelope(Arc<Envelope>, Option<f64>),
    Mul(Box<Control>, Box<Control>),
    Add(Box<Control>, Box<Control>),
    /// `?name`, filled in by `with`. Unfilled, it's its default (or 0, but
    /// `play` refuses holes without one).
    Hole {
        name: String,
        default: Option<Box<Control>>,
    },
}

impl Control {
    fn holes(&self, out: &mut Vec<(String, bool)>) {
        match self {
            Control::Hole { name, default } => out.push((name.clone(), default.is_some())),
            Control::Mul(a, b) | Control::Add(a, b) => {
                a.holes(out);
                b.holes(out);
            }
            _ => {}
        }
    }

    /// The same control with the holes in `values` filled.
    fn fill(&self, values: &HashMap<String, Control>) -> Control {
        match self {
            Control::Hole { name, .. } if values.contains_key(name) => values[name].clone(),
            Control::Mul(a, b) => Control::Mul(Box::new(a.fill(values)), Box::new(b.fill(values))),
            Control::Add(a, b) => Control::Add(Box::new(a.fill(values)), Box::new(b.fill(values))),
            other => other.clone(),
        }
    }

    pub fn instantiate(&self, sample_rate: u32) -> Box<dyn ControlNode> {
        match self {
            Control::Constant(v) => Box::new(control::Constant(*v as f32)),
            Control::Modulation(data, times) => {
                Box::new(ModulationPlayer::new(data.clone(), *times, sample_rate))
            }
            Control::Envelope(env, gate) => {
                let gate = gate.map(|seconds| (seconds * sample_rate as f64).round() as usize);
                Box::new(EnvelopePlayer::new(env.clone(), gate, sample_rate))
            }
            Control::Mul(a, b) => Box::new(Combine::new(
                a.instantiate(sample_rate),
                b.instantiate(sample_rate),
                |a, b| a * b,
            )),
            Control::Add(a, b) => Box::new(Combine::new(
                a.instantiate(sample_rate),
                b.instantiate(sample_rate),
                |a, b| a + b,
            )),
            Control::Hole { default, .. } => match default {
                Some(default) => default.instantiate(sample_rate),
                None => Box::new(control::Constant(0.0)),
            },
        }
    }

    /// As a parameter of an audio node: constants stay plain numbers.
    fn param(&self, sample_rate: u32) -> Param {
        match self {
            Control::Constant(v) => Param::Const(*v as f32),
            Control::Hole {
                default: Some(default),
                ..
            } => default.param(sample_rate),
            other => Param::signal(other.instantiate(sample_rate)),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Type {
    Sound,
    Control,
    Envelope,
    Number,
    Duration,
    Pitch,
    String,
    /// `name: value`, for `with`.
    Binding,
    Nothing,
}

impl Type {
    fn name(self) -> &'static str {
        match self {
            Type::Sound => "a sound",
            Type::Control => "a control",
            Type::Envelope => "an envelope",
            Type::Number => "a number",
            Type::Duration => "a duration",
            Type::Pitch => "a pitch",
            Type::String => "a string",
            Type::Binding => "a binding (like pos: 0.2)",
            Type::Nothing => "nothing",
        }
    }
}

#[derive(Clone)]
enum Value {
    Sound(Sound),
    Control(Control),
    Envelope(Arc<Envelope>),
    Str(String),
    Num(f64),
    Duration(f64),
    /// A MIDI note number.
    Pitch(f64),
    Binding(String, Box<Value>),
    Nothing,
}

impl Value {
    fn ty(&self) -> Type {
        match self {
            Value::Sound(_) => Type::Sound,
            Value::Control(_) => Type::Control,
            Value::Envelope(_) => Type::Envelope,
            Value::Str(_) => Type::String,
            Value::Num(_) => Type::Number,
            Value::Duration(_) => Type::Duration,
            Value::Pitch(_) => Type::Pitch,
            Value::Binding(..) => Type::Binding,
            Value::Nothing => Type::Nothing,
        }
    }

    /// Whether this value can be passed where `ty` is expected.
    fn fits(&self, ty: Type) -> bool {
        self.ty() == ty
            || (ty == Type::Control
                && matches!(self, Value::Num(_) | Value::Pitch(_) | Value::Envelope(_)))
    }

    // Accessors for arguments whose type a signature has already checked.

    fn sound(&self) -> Sound {
        match self {
            Value::Sound(s) => s.clone(),
            _ => unreachable!("not a sound"),
        }
    }

    fn control(&self) -> Control {
        match self {
            Value::Control(c) => c.clone(),
            Value::Num(n) | Value::Pitch(n) => Control::Constant(*n),
            Value::Envelope(env) => Control::Envelope(env.clone(), None),
            _ => unreachable!("not a control"),
        }
    }

    fn envelope(&self) -> Arc<Envelope> {
        match self {
            Value::Envelope(env) => env.clone(),
            _ => unreachable!("not an envelope"),
        }
    }

    fn num(&self) -> f64 {
        match self {
            Value::Num(n) => *n,
            _ => unreachable!("not a number"),
        }
    }

    fn duration(&self) -> f64 {
        match self {
            Value::Duration(d) => *d,
            _ => unreachable!("not a duration"),
        }
    }

    fn str(&self) -> &str {
        match self {
            Value::Str(s) => s,
            _ => unreachable!("not a string"),
        }
    }
}

/// A parameter in a signature: its type, and what to call it in an error.
#[derive(Clone, Copy)]
struct P(Type, &'static str);

const SOUND: P = P(Type::Sound, "a sound");
const CONTROL: P = P(Type::Control, "a control");
const NUMBER: P = P(Type::Number, "a number");
const NAME: P = P(Type::String, "a file name");

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
    commands: &'a mut Vec<Command>,
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
        self.fail(
            i,
            format!("expected {want}, got {}", self.args[i].ty().name()),
        )
    }

    fn sound(&self, i: usize) -> Sound {
        self.args[i].sound()
    }

    fn control(&self, i: usize) -> Control {
        self.args[i].control()
    }
}

fn sound(s: Sound) -> Result<Value, Error> {
    Ok(Value::Sound(s))
}

fn control(c: Control) -> Result<Value, Error> {
    Ok(Value::Control(c))
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

/// Every built-in function. Where a name has several signatures, the first
/// one that fits wins, so exact types come before ones that need converting.
fn builtins() -> Vec<Builtin> {
    const START: P = P(Type::Duration, "a start time (like 0:11:188)");
    const END: P = P(Type::Duration, "an end time (like 0:12:625)");
    const AMOUNT: P = P(Type::Control, "an amount (like 0.5, -6db or an envelope)");

    const BINDING: P = P(Type::Binding, "a binding (like pos: 0.2)");

    vec![
        builtin("play", &[SOUND], 1, |ev, c| {
            let unfilled: Vec<String> = c
                .sound(0)
                .holes()
                .into_iter()
                .filter(|(_, default)| !default)
                .map(|(name, _)| name)
                .collect();
            if let Some(name) = unfilled.first() {
                return Err(Error {
                    pos: c.pos,
                    msg: format!("play: ?{name} has no value (fill it with .with({name}: ...))"),
                });
            }
            let node = c.sound(0).instantiate(ev.sample_rate);
            c.commands.push(Command::Play(node));
            Ok(Value::Nothing)
        }),
        builtin("stop", &[], 0, |_, c| {
            c.commands.push(Command::StopAll);
            Ok(Value::Nothing)
        }),
        builtin("sample", &[NAME, START, END], 1, |ev, c| {
            let start = c.args.get(1).map_or(0.0, Value::duration);
            let end = c.args.get(2).map(Value::duration);
            if end.is_some_and(|end| end <= start) {
                return Err(c.wrong(2, "an end after the start"));
            }
            match ev.load(c.args[0].str(), start, end) {
                Ok(data) => sound(Sound::Sample(data)),
                Err(msg) => Err(Error { pos: c.pos, msg }),
            }
        }),
        builtin(
            "envelope",
            &[P(Type::String, "an envelope name")],
            1,
            |ev, c| match ev.load_envelope(c.args[0].str()) {
                Ok(env) => Ok(Value::Envelope(Arc::new(env))),
                Err(msg) => Err(c.fail(0, msg)),
            },
        ),
        builtin(
            "modulation",
            &[P(Type::String, "a modulation name")],
            1,
            |ev, c| match ev.load_modulation(c.args[0].str()) {
                Ok(m) => control(Control::Modulation(Arc::new(m), 1)),
                Err(msg) => Err(c.fail(0, msg)),
            },
        ),
        builtin(
            "wavetable",
            &[
                P(Type::String, "a wavetable name"),
                P(Type::Control, "a position (0 to 1)"),
                P(Type::Control, "a warp amount (0 to 1)"),
                P(Type::Control, "a pitch (like c2)"),
            ],
            1,
            |ev, c| {
                let table = ev
                    .wavetable(c.args[0].str())
                    .map_err(|msg| c.fail(0, msg))?;
                let arg = |i: usize, default| {
                    c.args
                        .get(i)
                        .map_or(Control::Constant(default), Value::control)
                };
                sound(Sound::Wavetable {
                    table,
                    position: arg(1, 0.0),
                    warp: arg(2, 0.0),
                    // Middle C.
                    pitch: arg(3, 60.0),
                })
            },
        ),
        Builtin {
            rest: Some(BINDING),
            ..builtin("with", &[SOUND], 1, |_, c| {
                let values = fill_values(c, &c.sound(0).holes())?;
                sound(
                    c.sound(0)
                        .map_controls(&mut |control| control.fill(&values)),
                )
            })
        },
        Builtin {
            rest: Some(BINDING),
            ..builtin(
                "with",
                &[P(Type::Control, "a sound or a control")],
                1,
                |_, c| {
                    let control = c.control(0);
                    let mut holes = Vec::new();
                    control.holes(&mut holes);
                    let values = fill_values(c, &holes)?;
                    Ok(Value::Control(control.fill(&values)))
                },
            )
        },
        builtin(
            "gate",
            &[
                P(Type::Envelope, "an envelope"),
                P(Type::Duration, "a duration (like 500ms)"),
            ],
            2,
            |_, c| {
                let env = c.args[0].envelope();
                control(Control::Envelope(env, Some(c.args[1].duration())))
            },
        ),
        builtin(
            "fit",
            &[SOUND, P(Type::Duration, "a duration (like 500ms)")],
            2,
            |_, c| sound(Sound::Fit(Box::new(c.sound(0)), c.args[1].duration())),
        ),
        builtin("slice", &[SOUND, START, END], 3, |_, c| {
            let (start, end) = (c.args[1].duration(), c.args[2].duration());
            if end <= start {
                return Err(c.wrong(2, "an end after the start"));
            }
            sound(Sound::Slice(Box::new(c.sound(0)), start, end))
        }),
        builtin(
            "delay",
            &[SOUND, P(Type::Duration, "a duration (like 12s)")],
            2,
            |_, c| sound(Sound::Delay(Box::new(c.sound(0)), c.args[1].duration())),
        ),
        // `repeat(inf)` repeats forever (well, usize::MAX times).
        builtin("repeat", &[SOUND, NUMBER], 2, |_, c| {
            let times = repeat_count(c)?;
            sound(Sound::Repeat(Box::new(c.sound(0)), times))
        }),
        builtin(
            "repeat",
            &[P(Type::Control, "a sound or a modulation"), NUMBER],
            2,
            |_, c| {
                let times = repeat_count(c)?;
                match c.control(0) {
                    Control::Modulation(m, n) => {
                        control(Control::Modulation(m, n.saturating_mul(times)))
                    }
                    _ => Err(c.wrong(0, "a sound or a modulation")),
                }
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
            control(Control::Add(Box::new(c.control(0)), Box::new(c.control(1))))
        }),
        Builtin {
            rest: Some(SOUND),
            ..builtin("add", &[], 0, |_, c| {
                sound(Sound::Add(c.args.iter().map(Value::sound).collect()))
            })
        },
        // `a * b`
        builtin("mul", &[NUMBER, NUMBER], 2, |_, c| {
            Ok(Value::Num(c.args[0].num() * c.args[1].num()))
        }),
        builtin("mul", &[SOUND, SOUND], 2, |_, c| {
            sound(Sound::Multiply(Box::new(c.sound(0)), Box::new(c.sound(1))))
        }),
        builtin("mul", &[SOUND, CONTROL], 2, |_, c| {
            sound(Sound::Gain(c.control(1), Box::new(c.sound(0))))
        }),
        builtin("mul", &[CONTROL, SOUND], 2, |_, c| {
            sound(Sound::Gain(c.control(0), Box::new(c.sound(1))))
        }),
        builtin("mul", &[CONTROL, CONTROL], 2, |_, c| {
            control(Control::Mul(Box::new(c.control(0)), Box::new(c.control(1))))
        }),
        builtin("gain", &[SOUND, AMOUNT], 2, |_, c| {
            sound(Sound::Gain(c.control(1), Box::new(c.sound(0))))
        }),
        builtin(
            "limit",
            &[
                SOUND,
                P(Type::Number, "a positive ceiling (like 0.9 or -1db)"),
            ],
            1,
            |_, c| {
                let ceiling = c.args.get(1).map_or(LIMIT_CEILING, Value::num);
                if ceiling <= 0.0 {
                    return Err(c.wrong(1, "a positive ceiling (like 0.9 or -1db)"));
                }
                sound(Sound::Limit(ceiling, Box::new(c.sound(0))))
            },
        ),
        builtin(
            "reverb",
            &[
                SOUND,
                P(Type::String, "a space name"),
                P(Type::Number, "a mix between 0 and 1"),
            ],
            2,
            |ev, c| {
                let name = c.args[1].str();
                let Some(impulse) = ev.space(name) else {
                    let names = reverb::preset_names().join(", ");
                    return Err(c.fail(
                        1,
                        format!("unknown space \"{name}\" (try {names}, or a sound)"),
                    ));
                };
                reverb_with(c, impulse)
            },
        ),
        builtin(
            "reverb",
            &[SOUND, SOUND, P(Type::Number, "a mix between 0 and 1")],
            2,
            |ev, c| {
                let impulse = Arc::new(Impulse::new(ev.render(&c.sound(1), MAX_IR_SECONDS)));
                reverb_with(c, impulse)
            },
        ),
    ]
}

/// The values `with` fills holes with, checked against the holes there are.
fn fill_values(c: &Call, holes: &[(String, bool)]) -> Result<HashMap<String, Control>, Error> {
    let mut values = HashMap::new();
    for (i, arg) in c.args.iter().enumerate().skip(1) {
        let Value::Binding(name, value) = arg else {
            unreachable!("not a binding")
        };
        if !holes.iter().any(|(hole, _)| hole == name) {
            let mut names: Vec<String> = holes.iter().map(|(h, _)| format!("?{h}")).collect();
            names.dedup();
            let there = match names.as_slice() {
                [] => "there are no holes".to_string(),
                names => format!("there's {}", names.join(", ")),
            };
            return Err(c.fail(i, format!("there's no ?{name} to fill ({there})")));
        }
        if !value.fits(Type::Control) {
            return Err(c.fail(
                i,
                format!(
                    "{name}: expected a control, number or pitch, got {}",
                    value.ty().name()
                ),
            ));
        }
        values.insert(name.clone(), value.control());
    }
    Ok(values)
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

fn reverb_with(c: &Call, impulse: Arc<Impulse>) -> Result<Value, Error> {
    let mix = match c.args.get(2) {
        None => REVERB_MIX,
        Some(Value::Num(m)) if (0.0..=1.0).contains(m) => *m,
        Some(_) => return Err(c.wrong(2, "a mix between 0 and 1")),
    };
    sound(Sound::Reverb(Box::new(c.sound(0)), impulse, mix))
}

/// A decoded part of a file, as (start, end) seconds in `f64::to_bits` form so it
/// can be a map key.
type Window = (u64, Option<u64>);

pub struct Evaluator {
    sample_rate: u32,
    resources: Resources,
    /// Decoded samples, so each file is only loaded once. Holding an `Arc` here
    /// also guarantees the last reference to a sample is never dropped on the
    /// audio thread.
    cache: HashMap<(PathBuf, Window), Arc<SampleData>>,
    /// Prepared preset impulse responses, by name.
    spaces: HashMap<String, Arc<Impulse>>,
    /// Wavetables by name (built-in) or path and modification time (files).
    /// Like samples, they're kept so they're never freed on the audio thread.
    wavetables: HashMap<String, Arc<Wavetable>>,
    /// Values named with `let`. They live as long as the evaluator, so a block
    /// can use what an earlier one defined.
    vars: HashMap<String, Value>,
}

impl Evaluator {
    pub fn new(sample_rate: u32, resources: Resources) -> Self {
        Self {
            sample_rate,
            resources,
            cache: HashMap::new(),
            spaces: HashMap::new(),
            wavetables: HashMap::new(),
            vars: HashMap::new(),
        }
    }

    pub fn resources(&self) -> &Resources {
        &self.resources
    }

    /// Evaluate a program, returning the commands it wants sent to the audio thread.
    /// Nothing is sent if any statement fails, so a typo never half-plays a line.
    /// (The same goes for `let`s: they only take effect if everything succeeds.)
    pub fn run(&mut self, program: &[Spanned]) -> Result<Vec<Command>, Error> {
        let vars = self.vars.clone();
        let mut commands = Vec::new();
        for stmt in program {
            if let Err(e) = self.eval(stmt, &mut commands) {
                self.vars = vars;
                return Err(e);
            }
        }
        Ok(commands)
    }

    fn eval(&mut self, e: &Spanned, commands: &mut Vec<Command>) -> Result<Value, Error> {
        let fail = |msg: String| Err(Error { pos: e.pos, msg });
        let (name, args) = match &e.expr {
            Expr::Str(s) => return Ok(Value::Str(s.clone())),
            Expr::Num(n) => return Ok(Value::Num(*n)),
            Expr::Duration(d) => return Ok(Value::Duration(*d)),
            Expr::Pitch(note) => return Ok(Value::Pitch(*note)),
            Expr::Var(name) => {
                return match self.vars.get(name) {
                    Some(value) => Ok(value.clone()),
                    None if BUILTINS.iter().any(|b| b.name == name) => {
                        fail(format!("'{name}' is a function: call it with {name}(...)"))
                    }
                    None => fail(format!("unknown name '{name}'")),
                };
            }
            Expr::Let { name, value } => {
                let value = self.eval(value, commands)?;
                self.vars.insert(name.clone(), value);
                return Ok(Value::Nothing);
            }
            Expr::Hole { name, default } => {
                let default = match default {
                    None => None,
                    Some(d) => match self.eval(d, commands)? {
                        v if v.fits(Type::Control) => Some(Box::new(v.control())),
                        v => {
                            return Err(Error {
                                pos: d.pos,
                                msg: format!(
                                    "?{name}: expected a default like 0.2 or c2, got {}",
                                    v.ty().name()
                                ),
                            });
                        }
                    },
                };
                let name = name.clone();
                return Ok(Value::Control(Control::Hole { name, default }));
            }
            Expr::Named { name, value } => {
                let value = self.eval(value, commands)?;
                return Ok(Value::Binding(name.clone(), Box::new(value)));
            }
            Expr::Call { name, args } => (name.as_str(), args),
        };

        let candidates: Vec<&Builtin> = BUILTINS.iter().filter(|b| b.name == name).collect();
        if candidates.is_empty() {
            return fail(format!("unknown function '{name}'"));
        }

        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            values.push(self.eval(arg, commands)?);
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
                    values[at].ty().name()
                ),
            });
        };

        let mut call = Call {
            name: found.name,
            args: values,
            positions: args.iter().map(|a| a.pos).collect(),
            pos: e.pos,
            commands,
        };
        (found.run)(self, &mut call)
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
        let mut node = sound.instantiate(self.sample_rate);
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
        let dirs = &self.resources.sample_dirs;
        let path = resource::find(dirs, name)
            .ok_or_else(|| format!("sample '{name}' not found in {dirs:?}"))?;
        let key = (path, (start.to_bits(), end.map(f64::to_bits)));
        if let Some(data) = self.cache.get(&key) {
            return Ok(data.clone());
        }
        let data = Arc::new(sample::load_range(&key.0, start, end)?);
        self.cache.insert(key, data.clone());
        Ok(data)
    }

    /// The path of an envelope or modulation file, if it exists. (These are
    /// read fresh every time, so edits apply the next time the code runs.)
    fn resource_path(&self, kind: ResourceKind, name: &str) -> Result<PathBuf, String> {
        let path = kind.path(&self.resources.root, name);
        if path.is_file() {
            Ok(path)
        } else {
            Err(format!(
                "\"{name}\" doesn't exist yet ({}/{}): put the cursor on it to create it",
                kind.dir(),
                kind.file_name(name)
            ))
        }
    }

    /// A built-in wavetable, or one from `wavetables/`.
    fn wavetable(&mut self, name: &str) -> Result<Arc<Wavetable>, String> {
        let path = ResourceKind::Wavetable.path(&self.resources.root, name);
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let key = match modified {
            Some(time) => format!("{}@{time:?}", path.display()),
            None => name.to_string(),
        };
        if let Some(table) = self.wavetables.get(&key) {
            return Ok(table.clone());
        }
        let table = match modified {
            Some(_) => Wavetable::load(&path)?,
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
        Envelope::load(&self.resource_path(ResourceKind::Envelope, name)?)
    }

    fn load_modulation(&self, name: &str) -> Result<Modulation, String> {
        Modulation::load(&self.resource_path(ResourceKind::Modulation, name)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Stage;
    use crate::lang::parse;
    use crate::modulation::Point;
    use crate::nodes::Frame;

    fn samples() -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples")
    }

    fn evaluator() -> Evaluator {
        Evaluator::new(
            48_000,
            Resources {
                root: samples(),
                sample_dirs: vec![samples()],
            },
        )
    }

    /// An evaluator whose code folder has a `slow` envelope (1 s attack, 1 s
    /// release, straight lines), a `ramp` modulation (0 to 1 over 1 s), and a
    /// sample `ones.wav` (a second of 1.0).
    fn with_resources() -> (Evaluator, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "rocktober-eval-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let line = |time| Stage { time, curve: 0.0 };
        Envelope {
            attack: line(1.0),
            decay: line(0.0),
            sustain: 1.0,
            release: line(1.0),
        }
        .save(&ResourceKind::Envelope.path(&root, "slow"))
        .unwrap();
        Modulation {
            length: 1.0,
            points: vec![Point::new(0.0, 0.0), Point::new(1.0, 1.0)],
        }
        .save(&ResourceKind::Modulation.path(&root, "ramp"))
        .unwrap();
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut wav = hound::WavWriter::create(root.join("ones.wav"), spec).unwrap();
        for _ in 0..48_000 * 2 {
            wav.write_sample(1.0f32).unwrap();
        }
        wav.finalize().unwrap();
        let evaluator = Evaluator::new(
            48_000,
            Resources {
                root: root.clone(),
                sample_dirs: vec![root.clone()],
            },
        );
        (evaluator, root)
    }

    /// Render a node to completion, or `max` frames.
    fn render_node(node: &mut dyn Node, max: usize) -> Vec<Frame> {
        let mut all = Vec::new();
        let mut buf = vec![[0.0; 2]; 512];
        while all.len() < max {
            let n = node.process(&mut buf);
            all.extend_from_slice(&buf[..n]);
            if n < buf.len() {
                break;
            }
        }
        all.truncate(max);
        all
    }

    fn render_with(evaluator: &mut Evaluator, src: &str) -> Vec<Frame> {
        let mut commands = evaluator.run(&parse(src).unwrap()).unwrap();
        let Some(Command::Play(mut node)) = commands.pop() else {
            panic!("nothing played")
        };
        render_node(node.as_mut(), 48_000 * 60)
    }

    /// Evaluate `src` (which must `play` exactly one sound) and render it.
    fn render(src: &str) -> Vec<Frame> {
        render_with(&mut evaluator(), src)
    }

    fn peak(frames: &[Frame]) -> f32 {
        frames.iter().flatten().fold(0.0, |m, s| m.max(s.abs()))
    }

    fn error_with(evaluator: &mut Evaluator, src: &str) -> String {
        match evaluator.run(&parse(src).unwrap()) {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.msg,
        }
    }

    fn error(src: &str) -> String {
        error_with(&mut evaluator(), src)
    }

    #[test]
    fn add_and_gain() {
        let kick = peak(&render(r#"play(sample("kick.mp3"))"#));
        let four = peak(&render(
            r#"play(add(sample("kick.mp3"), sample("kick.mp3"), sample("kick.mp3"), sample("kick.mp3")))"#,
        ));
        assert!((four - 4.0 * kick).abs() < 1e-4);
        let half = peak(&render(r#"sample("kick.mp3").gain(-6db).play"#));
        assert!((half - 0.501 * kick).abs() < 1e-3);
    }

    #[test]
    fn limit_tames_a_loud_mix() {
        let loud = r#"add(sample("kick.mp3"), sample("kick.mp3"), sample("kick.mp3"))"#;
        assert!(peak(&render(&format!("play({loud})"))) > 1.5);
        assert!(peak(&render(&format!("{loud}.limit.play"))) <= 0.892);
        assert!(peak(&render(&format!("{loud}.limit(-6db).play"))) <= 0.502);
    }

    #[test]
    fn argument_errors() {
        assert_eq!(
            error(r#"sample("kick.mp3").gain(sample("kick.mp3"))"#),
            "gain: expected an amount (like 0.5, -6db or an envelope), got a sound"
        );
        assert_eq!(
            error(r#"0.5.gain(sample("kick.mp3"))"#),
            "gain: expected a sound, got a number"
        );
        assert_eq!(
            error(r#"seq(sample("kick.mp3"), 3)"#),
            "seq: expected a sound, got a number"
        );
        assert_eq!(
            error("add().limit(0)"),
            "limit: expected a positive ceiling (like 0.9 or -1db), got a number"
        );
        assert_eq!(error("limit()"), "limit takes 1 or 2 argument(s), got 0");
        assert_eq!(error("sample()"), "sample takes 1 to 3 argument(s), got 0");
        assert_eq!(error("nope()"), "unknown function 'nope'");
        // Overloads: blamed on the argument the closest signatures got stuck on.
        assert_eq!(
            error(r#"sample("kick.mp3") * "loud""#),
            "mul: expected a sound or a control, got a string"
        );
        assert_eq!(
            error(r#"add(sample("kick.mp3"), 3)"#),
            "add: expected a sound, got a number"
        );
        assert_eq!(
            error(r#"sample("kick.mp3").reverb(2)"#),
            "reverb: expected a space name or a sound, got a number"
        );
        assert_eq!(
            error(r#"play(4.repeat(2))"#),
            "repeat: expected a sound or a modulation, got a number"
        );
    }

    #[test]
    fn seq_is_as_long_as_its_parts() {
        let frames =
            render(r#"seq(sample("kick.mp3").fit(100ms), sample("kick.mp3").fit(250ms),).play"#);
        assert_eq!(frames.len(), 4_800 + 12_000);
    }

    #[test]
    fn slice_cuts_a_window_out_of_a_sample() {
        let whole = render(r#"sample("kick.mp3").play"#);
        let slice = render(r#"sample("kick.mp3").slice(0:00:100, 0:00:300).play"#);
        assert_eq!(slice.len(), 9_600);
        // Away from the fades, it's exactly the original (resampled the same way).
        let diff = slice[200..9_400]
            .iter()
            .zip(&whole[4_800 + 200..])
            .map(|(a, b)| (a[0] - b[0]).abs())
            .fold(0f32, f32::max);
        assert!(diff < 1e-6, "max diff {diff}");
        assert_eq!(
            error(r#"sample("kick.mp3").slice(0:01, 0:00:500)"#),
            "slice: expected an end after the start, got a duration"
        );
    }

    #[test]
    fn sample_can_decode_just_a_window() {
        let whole = render(r#"sample("kick.mp3").play"#);
        let window = render(r#"sample("kick.mp3", 0:00:100, 0:00:300).play"#);
        assert!((9_600..=9_601).contains(&window.len()), "{}", window.len());
        // Same audio as slicing the whole file, away from the edge fades.
        for (a, b) in window[400..9_200].iter().zip(&whole[4_800 + 400..]) {
            assert!((a[0] - b[0]).abs() < 1e-6, "{a:?} vs {b:?}");
        }
        // Start only: runs to the end of the file.
        let tail = render(r#"sample("kick.mp3", 0:00:500).play"#);
        assert!(tail.len().abs_diff(whole.len() - 24_000) <= 1);
        assert_eq!(
            error(r#"sample("kick.mp3", 0:10)"#)
                .split(": ")
                .last()
                .unwrap(),
            "no audio in that range (is the file shorter?)"
        );
        assert_eq!(
            error(r#"sample("kick.mp3", 0:00:500, 0:00:100)"#),
            "sample: expected an end after the start, got a duration"
        );
    }

    #[test]
    fn repeat_inf_keeps_going() {
        let mut commands = evaluator()
            .run(&parse(r#"sample("kick.mp3").fit(10ms).repeat(inf).play"#).unwrap())
            .unwrap();
        let Some(Command::Play(mut node)) = commands.pop() else {
            panic!()
        };
        // Ten minutes' worth of 10ms kicks, in big blocks: still going.
        let mut buf = vec![[0.0; 2]; 48_000];
        for _ in 0..600 {
            assert_eq!(node.process(&mut buf), buf.len());
        }
        assert_eq!(
            error(r#"sample("kick.mp3").repeat(1.5)"#),
            "repeat: expected a whole number or inf, got a number"
        );
    }

    #[test]
    fn reverb_presets_and_custom_spaces() {
        let dry = render(r#"sample("kick.mp3").play"#);
        let hall = render(r#"sample("kick.mp3").reverb("hall").play"#);
        assert!(hall.len() > dry.len() + 48_000, "the hall rings on");
        let custom = render(r#"sample("kick.mp3").reverb(sample("kick.mp3").fit(100ms), 1).play"#);
        assert!(custom.len() > dry.len() && custom.len() < dry.len() + 48_000);
        assert!(
            error(r#"sample("kick.mp3").reverb("nowhere")"#)
                .starts_with("reverb: unknown space \"nowhere\" (try small_room, ")
        );
        assert_eq!(
            error(r#"sample("kick.mp3").reverb("hall", 2)"#),
            "reverb: expected a mix between 0 and 1, got a number"
        );
    }

    #[test]
    fn both_call_styles_are_equivalent() {
        let a = render(r#"play(repeat(fit(gain(sample("kick.mp3"), 0.5), 100ms), 3))"#);
        let b = render(r#"sample("kick.mp3").gain(0.5).fit(100ms).repeat(3).play"#);
        let c = render(r#"(sample("kick.mp3") * 0.5).fit(100ms).repeat(3).play"#);
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    /// One second of a constant 1.0 (see `with_resources`).
    const ONES: &str = r#"sample("ones.wav")"#;

    #[test]
    fn numbers_and_controls_combine() {
        let (mut ev, root) = with_resources();
        // Plain number arithmetic stays numbers.
        let half = render_with(&mut ev, &format!("({ONES} * (0.25 + 0.25)).play"));
        assert_eq!(half[100], [0.5, 0.5]);
        // A modulation shapes a sound: here a ramp from 0 to 1 over a second.
        let ramp = render_with(&mut ev, &format!(r#"({ONES} * modulation("ramp")).play"#));
        assert_eq!(ramp.len(), 48_000);
        assert!((ramp[24_000][0] - 0.5).abs() < 1e-3);
        // Controls combine: half the ramp, plus a quarter.
        let mixed = render_with(
            &mut ev,
            &format!(r#"({ONES} * (modulation("ramp") * 0.5 + 0.25)).play"#),
        );
        assert!((mixed[24_000][0] - 0.5).abs() < 1e-3);
        assert!((mixed[0][0] - 0.25).abs() < 1e-3);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repeated_modulation_is_an_lfo() {
        let (mut ev, root) = with_resources();
        let saw = render_with(
            &mut ev,
            &format!(r#"{ONES}.repeat(3).gain(modulation("ramp").repeat(2)).play"#),
        );
        assert!((saw[12_000][0] - 0.25).abs() < 1e-3);
        assert!((saw[48_000 + 12_000][0] - 0.25).abs() < 1e-3);
        // After two passes it holds its last value.
        assert!((saw[48_000 * 2 + 12_000][0] - 1.0).abs() < 1e-3);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gated_envelope_ends_the_sound_after_its_release() {
        let (mut ev, root) = with_resources();
        let note = render_with(
            &mut ev,
            &format!(r#"({ONES}.repeat(inf) * envelope("slow").gate(1500ms)).play"#),
        );
        // 1.5 s held, then 1 s of release, then it's over.
        assert_eq!(note.len(), 48_000 * 5 / 2);
        assert!(
            (note[24_000][0] - 0.5).abs() < 1e-3,
            "halfway up the attack"
        );
        assert!(
            (note[48_000 * 2][0] - 0.5).abs() < 1e-3,
            "halfway down the release"
        );
        // Without a gate it never releases: the sound lasts as long as it does.
        let held = render_with(&mut ev, &format!(r#"({ONES} * envelope("slow")).play"#));
        assert_eq!(held.len(), 48_000);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Upward zero crossings per second.
    fn frequency(frames: &[Frame]) -> f32 {
        let crossings = frames
            .windows(2)
            .filter(|w| w[0][0] < 0.0 && w[1][0] >= 0.0)
            .count();
        crossings as f32 * 48_000.0 / frames.len() as f32
    }

    fn run(ev: &mut Evaluator, src: &str) {
        ev.run(&parse(src).unwrap()).unwrap();
    }

    #[test]
    fn wavetables_play_notes() {
        let mut ev = evaluator();
        let a4 = render_with(&mut ev, r#"wavetable("basic", 0, 0, a4).fit(1s).play"#);
        assert_eq!(a4.len(), 48_000);
        assert!((frequency(&a4) - 440.0).abs() <= 1.0);
        // Pitches are note numbers, so adding 12 is an octave up.
        let a5 = render_with(&mut ev, r#"wavetable("basic", 0, 0, a4 + 12).fit(1s).play"#);
        assert!((frequency(&a5) - 880.0).abs() <= 1.0);
        // Defaults: the first frame, no warp, middle C.
        let c4 = render_with(&mut ev, r#"wavetable("sine-saw").fit(1s).play"#);
        assert!((frequency(&c4) - 261.6).abs() <= 1.0);
        assert!(
            error_with(&mut ev, r#"wavetable("nope")"#)
                .starts_with("wavetable: unknown wavetable \"nope\" (try basic, ")
        );
        assert_eq!(
            error_with(&mut ev, r#"wavetable("basic", sample("kick.mp3"))"#),
            "wavetable: expected a position (0 to 1), got a sound"
        );
    }

    #[test]
    fn holes_are_filled_by_with() {
        let mut ev = evaluator();
        run(
            &mut ev,
            r#"let lead = wavetable("basic", ?pos = 0.2, 0, ?note).fit(100ms)"#,
        );
        assert_eq!(
            error_with(&mut ev, "lead.play"),
            "play: ?note has no value (fill it with .with(note: ...))"
        );
        // One instrument, filled in two ways.
        let a4 = render_with(&mut ev, "lead.with(note: a4).play");
        let a5 = render_with(&mut ev, "lead.with(note: a5).play");
        assert_eq!(a4.len(), 4800);
        assert!((frequency(&a4) - 440.0).abs() <= 10.0);
        assert!((frequency(&a5) - 880.0).abs() <= 10.0);
        // Filling a hole with a default overrides it; filling in steps works.
        let brighter = render_with(&mut ev, "lead.with(pos: 1).with(note: a4).play");
        assert_ne!(brighter, a4);
        // A hole can be filled by anything that's a control.
        run(&mut ev, "let up = ?base + 12");
        let octave = render_with(
            &mut ev,
            r#"wavetable("basic", 0, 0, up.with(base: a4)).fit(100ms).play"#,
        );
        assert!((frequency(&octave) - 880.0).abs() <= 10.0);
        assert_eq!(
            error_with(&mut ev, "lead.with(nope: 1)"),
            "with: there's no ?nope to fill (there's ?pos, ?note)"
        );
        assert_eq!(
            error_with(&mut ev, r#"lead.with(note: sample("kick.mp3"))"#),
            "with: note: expected a control, number or pitch, got a sound"
        );
        assert_eq!(
            error_with(&mut ev, r#"wavetable("basic", ?pos = "x")"#),
            "?pos: expected a default like 0.2 or c2, got a string"
        );
    }

    #[test]
    fn lets_only_stick_when_the_block_succeeds() {
        let mut ev = evaluator();
        assert!(ev.run(&parse("let x = 1\nnope()").unwrap()).is_err());
        assert_eq!(error_with(&mut ev, "x.repeat(2)"), "unknown name 'x'");
        run(&mut ev, "let x = 1");
        run(&mut ev, "let y = x + 1");
        assert_eq!(
            error_with(&mut ev, "stop"),
            "'stop' is a function: call it with stop(...)"
        );
    }

    #[test]
    fn resource_errors() {
        let (mut ev, root) = with_resources();
        assert_eq!(
            error_with(&mut ev, r#"envelope("nope")"#),
            "envelope: \"nope\" doesn't exist yet (envelopes/nope.json): put the cursor on it to create it"
        );
        assert_eq!(
            error_with(&mut ev, r#"modulation("ramp").gate(1s)"#),
            "gate: expected an envelope, got a control"
        );
        assert_eq!(
            error_with(&mut ev, r#"play(modulation("ramp"))"#),
            "play: expected a sound, got a control"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
