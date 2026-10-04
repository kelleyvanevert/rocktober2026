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
use std::sync::{Arc, LazyLock};

use crate::bundle::Bundle;
use crate::clock::{Clock, Grid};
use crate::control::{
    self, Combine, ControlNode, EnvelopePlayer, Map, ModulationPlayer, NotePath, Param,
    RandomPlayer, Range,
};
use crate::envelope::Envelope;
use crate::filter::{self, Filter};
use crate::fx::{Drive, Echo, EchoParams, Pan, Spread};
use crate::lang::{Error, Expr, Spanned, hz_to_note};
use crate::modulation::Modulation;
use crate::nodes::{
    Add, Delay, Fit, Gain, Limit, Multiply, Node, Repeat, SampleData, Sampler, Seq, Slice,
};
use crate::noise::{self, Noise};
use crate::pattern::Pattern;
use crate::resource::ResourceKind;
use crate::reverb::{self, Impulse, MAX_IR_SECONDS, Reverb};
use crate::sample;
use crate::sidechain::{Bus, Duck};
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

/// `spread` default amount.
const SPREAD: f64 = 0.5;
/// `drive` default: 12 dB into the saturation.
const DRIVE: f64 = 4.0;
/// `echo` defaults: feedback, wet/dry mix, and the band the repeats are
/// filtered to.
const ECHO_FEEDBACK: f64 = 0.5;
const ECHO_MIX: f64 = 0.35;
const ECHO_LOW_HZ: f64 = 80.0;
const ECHO_HIGH_HZ: f64 = 10_000.0;
/// `duck` defaults: how far down, and how long it takes to come back up.
const DUCK_AMOUNT: f64 = 0.8;
const DUCK_RELEASE_SECONDS: f64 = 0.15;
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
            Expr::Let { value, .. } | Expr::Named { value, .. } => walk(value, out),
            Expr::Hole {
                default: Some(d), ..
            } => walk(d, out),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for stmt in program {
        walk(stmt, &mut out);
    }
    out
}

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
    Noise(noise::Color),
    Filter {
        kind: filter::Kind,
        /// A pitch.
        cutoff: Control,
        resonance: Control,
        child: Box<Sound>,
    },
    Pan(Control, Box<Sound>),
    Spread(Control, Box<Sound>),
    /// Saturation; the control is the gain into it.
    Drive(Control, Box<Sound>),
    Echo {
        child: Box<Sound>,
        /// Seconds.
        time: f64,
        feedback: Control,
        mix: Control,
        /// The band the repeats are filtered to, as pitches.
        low: Control,
        high: Control,
        pingpong: bool,
    },
    /// Turned down while what plays on the bus sounds.
    Duck {
        child: Box<Sound>,
        bus: Arc<Bus>,
        amount: Control,
        /// Seconds.
        release: f64,
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
            Sound::Noise(color) => Sound::Noise(*color),
            Sound::Filter {
                kind,
                cutoff,
                resonance,
                child,
            } => Sound::Filter {
                kind: *kind,
                cutoff: f(cutoff),
                resonance: f(resonance),
                child: Box::new(child.map_controls(f)),
            },
            Sound::Pan(position, child) => {
                let position = f(position);
                Sound::Pan(position, Box::new(child.map_controls(f)))
            }
            Sound::Spread(amount, child) => {
                let amount = f(amount);
                Sound::Spread(amount, Box::new(child.map_controls(f)))
            }
            Sound::Drive(drive, child) => {
                let drive = f(drive);
                Sound::Drive(drive, Box::new(child.map_controls(f)))
            }
            Sound::Echo {
                child,
                time,
                feedback,
                mix,
                low,
                high,
                pingpong,
            } => Sound::Echo {
                feedback: f(feedback),
                mix: f(mix),
                low: f(low),
                high: f(high),
                child: Box::new(child.map_controls(f)),
                time: *time,
                pingpong: *pingpong,
            },
            Sound::Duck {
                child,
                bus,
                amount,
                release,
            } => Sound::Duck {
                amount: f(amount),
                child: Box::new(child.map_controls(f)),
                bus: bus.clone(),
                release: *release,
            },
        }
    }

    /// The voice for one note of a pattern: `?note` filled in (with a
    /// pitch, or a gliding `NotePath`), envelopes released after `gate`
    /// seconds (`None`: never), and free-running modulations picked up where
    /// they are `since` seconds into the pattern. A sound that would go on
    /// forever and has no envelope to end it is cut off at the gate.
    pub fn for_note(&self, note: Option<Control>, gate: Option<f64>, since: f64) -> Sound {
        let mut values = HashMap::new();
        if let Some(note) = note {
            values.insert("note".to_string(), note);
        }
        let mut has_envelope = false;
        let voice = self.map_controls(&mut |c| {
            has_envelope |= c.has_envelope();
            c.fill(&values).for_note(gate, since)
        });
        match gate {
            Some(gate) if !has_envelope && voice.is_endless() => Sound::Fit(Box::new(voice), gate),
            _ => voice,
        }
    }

    /// Whether the sound never ends by itself (ignoring controls that might
    /// end it).
    fn is_endless(&self) -> bool {
        match self {
            Sound::Sample(_) | Sound::Fit(..) | Sound::Slice(..) => false,
            Sound::Wavetable { .. } | Sound::Noise(_) => true,
            Sound::Repeat(child, times) => *times == usize::MAX || child.is_endless(),
            Sound::Add(children) | Sound::Seq(children) => children.iter().any(Sound::is_endless),
            Sound::Multiply(a, b) => a.is_endless() && b.is_endless(),
            Sound::Delay(child, _)
            | Sound::Gain(_, child)
            | Sound::Limit(_, child)
            | Sound::Reverb(child, ..)
            | Sound::Filter { child, .. }
            | Sound::Pan(_, child)
            | Sound::Spread(_, child)
            | Sound::Drive(_, child)
            | Sound::Echo { child, .. }
            | Sound::Duck { child, .. } => child.is_endless(),
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
            Sound::Noise(color) => Box::new(Noise::new(*color, noise::next_seed())),
            Sound::Filter {
                kind,
                cutoff,
                resonance,
                child,
            } => Box::new(Filter::new(
                child.instantiate(sample_rate),
                *kind,
                cutoff.param(sample_rate),
                resonance.param(sample_rate),
                sample_rate,
            )),
            Sound::Pan(position, child) => Box::new(Pan::new(
                child.instantiate(sample_rate),
                position.param(sample_rate),
            )),
            Sound::Drive(drive, child) => Box::new(Drive::new(
                child.instantiate(sample_rate),
                drive.param(sample_rate),
            )),
            Sound::Spread(amount, child) => Box::new(Spread::new(
                child.instantiate(sample_rate),
                amount.param(sample_rate),
                sample_rate,
            )),
            Sound::Echo {
                child,
                time,
                feedback,
                mix,
                low,
                high,
                pingpong,
            } => Box::new(Echo::new(
                child.instantiate(sample_rate),
                frames(*time),
                EchoParams {
                    feedback: feedback.param(sample_rate),
                    mix: mix.param(sample_rate),
                    low: low.param(sample_rate),
                    high: high.param(sample_rate),
                    pingpong: *pingpong,
                },
                sample_rate,
            )),
            Sound::Duck {
                child,
                bus,
                amount,
                release,
            } => Box::new(Duck::new(
                child.instantiate(sample_rate),
                bus.clone(),
                amount.param(sample_rate),
                *release as f32,
                sample_rate,
            )),
        }
    }
}

/// How a modulation in an instrument follows the notes of a pattern.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ModClock {
    /// Runs on through the notes, from the start of the pattern.
    Free,
    /// Starts over with every note.
    Retrig,
    /// Its value at the start of each note, held for the note.
    Latch,
}

/// A description of a control signal, the counterpart of `Sound`.
#[derive(Clone)]
pub enum Control {
    Constant(f64),
    /// Played `times` times, then held at its last value, starting `start`
    /// seconds in.
    Modulation {
        data: Arc<Modulation>,
        times: usize,
        clock: ModClock,
        start: f64,
    },
    /// A new random value every `period` seconds, starting `start` seconds in.
    Random {
        period: f64,
        seed: u64,
        clock: ModClock,
        start: f64,
    },
    /// Released after the gate (in seconds), if it has one.
    Envelope(Arc<Envelope>, Option<f64>),
    Mul(Box<Control>, Box<Control>),
    Add(Box<Control>, Box<Control>),
    /// The first mapped from 0..1 onto the other two (whole numbers only, if
    /// `whole`).
    Range {
        x: Box<Control>,
        lo: Box<Control>,
        hi: Box<Control>,
        whole: bool,
    },
    Round(Box<Control>),
    /// The pitch of a gliding phrase of a pattern (see `NotePath`): from step
    /// `start`, with steps and glide in seconds.
    NotePath {
        pattern: Arc<Pattern>,
        start: usize,
        step: f64,
        glide: f64,
    },
    /// `?name`, filled in by `with`. Unfilled, it's its default (or 0, but
    /// `play` refuses holes without one).
    Hole {
        name: String,
        default: Option<Box<Control>>,
    },
}

impl Control {
    /// The controls this one is made of.
    fn children(&self) -> Vec<&Control> {
        match self {
            Control::Mul(a, b) | Control::Add(a, b) => vec![a, b],
            Control::Range { x, lo, hi, .. } => vec![x, lo, hi],
            Control::Round(x) => vec![x],
            Control::Hole {
                default: Some(d), ..
            } => vec![d],
            _ => vec![],
        }
    }

    /// The same control with each of its children replaced by `f(child)`.
    fn map_children(&self, f: &mut dyn FnMut(&Control) -> Control) -> Control {
        let mut m = |c: &Control| Box::new(f(c));
        match self {
            Control::Mul(a, b) => Control::Mul(m(a), m(b)),
            Control::Add(a, b) => Control::Add(m(a), m(b)),
            Control::Range { x, lo, hi, whole } => Control::Range {
                x: m(x),
                lo: m(lo),
                hi: m(hi),
                whole: *whole,
            },
            Control::Round(x) => Control::Round(m(x)),
            Control::Hole {
                name,
                default: Some(d),
            } => Control::Hole {
                name: name.clone(),
                default: Some(m(d)),
            },
            other => other.clone(),
        }
    }

    fn holes(&self, out: &mut Vec<(String, bool)>) {
        match self {
            // A default's own holes aren't the instrument's to fill.
            Control::Hole { name, default } => out.push((name.clone(), default.is_some())),
            other => other.children().iter().for_each(|c| c.holes(out)),
        }
    }

    fn has_envelope(&self) -> bool {
        matches!(self, Control::Envelope(..)) || self.children().iter().any(|c| c.has_envelope())
    }

    /// See `Sound::for_note`.
    fn for_note(&self, gate: Option<f64>, since: f64) -> Control {
        match self {
            Control::Envelope(env, None) => Control::Envelope(env.clone(), gate),
            Control::Modulation {
                data,
                times,
                clock,
                start,
            } => match clock {
                ModClock::Retrig => self.clone(),
                ModClock::Free => Control::Modulation {
                    data: data.clone(),
                    times: *times,
                    clock: *clock,
                    start: start + since,
                },
                ModClock::Latch => Control::Constant(data.value_after(start + since, *times)),
            },
            Control::Random {
                period,
                seed,
                clock,
                start,
            } => match clock {
                // Starting over with the same values every note would be the
                // same note every time: a new stream per note instead.
                ModClock::Retrig => Control::Random {
                    period: *period,
                    seed: noise::mix(seed ^ since.to_bits()),
                    clock: *clock,
                    start: *start,
                },
                ModClock::Free => Control::Random {
                    period: *period,
                    seed: *seed,
                    clock: *clock,
                    start: start + since,
                },
                ModClock::Latch => Control::Constant(noise::random_at(
                    *seed,
                    ((start + since) / period + 1e-9).floor() as i64,
                )),
            },
            other => other.map_children(&mut |c| c.for_note(gate, since)),
        }
    }

    /// The same control with every modulation and random in it following
    /// `clock`, or `None` if there are none in it.
    fn with_clock(&self, clock: ModClock) -> Option<Control> {
        match self {
            Control::Modulation {
                data, times, start, ..
            } => Some(Control::Modulation {
                data: data.clone(),
                times: *times,
                clock,
                start: *start,
            }),
            Control::Random {
                period,
                seed,
                start,
                ..
            } => Some(Control::Random {
                period: *period,
                seed: *seed,
                clock,
                start: *start,
            }),
            other => {
                let mut changed = false;
                let control = other.map_children(&mut |c| match c.with_clock(clock) {
                    Some(c) => {
                        changed = true;
                        c
                    }
                    None => c.clone(),
                });
                changed.then_some(control)
            }
        }
    }

    /// The same control with the holes in `values` filled.
    fn fill(&self, values: &HashMap<String, Control>) -> Control {
        match self {
            Control::Hole { name, .. } if values.contains_key(name) => values[name].clone(),
            Control::Hole { .. } => self.clone(),
            other => other.map_children(&mut |c| c.fill(values)),
        }
    }

    pub fn instantiate(&self, sample_rate: u32) -> Box<dyn ControlNode> {
        let rate = sample_rate as f64;
        match self {
            Control::Constant(v) => Box::new(control::Constant(*v as f32)),
            Control::Modulation {
                data, times, start, ..
            } => {
                let start = (start * rate).round() as usize;
                Box::new(ModulationPlayer::new(
                    data.clone(),
                    *times,
                    start,
                    sample_rate,
                ))
            }
            Control::Random {
                period,
                seed,
                start,
                ..
            } => Box::new(RandomPlayer::new(*seed, period * rate, start * rate)),
            Control::Envelope(env, gate) => {
                let gate = gate.map(|seconds| (seconds * rate).round() as usize);
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
            Control::Range { x, lo, hi, whole } => Box::new(Range::new(
                x.instantiate(sample_rate),
                lo.instantiate(sample_rate),
                hi.instantiate(sample_rate),
                *whole,
            )),
            Control::Round(x) => Box::new(Map::new(x.instantiate(sample_rate), f32::round)),
            Control::NotePath {
                pattern,
                start,
                step,
                glide,
            } => Box::new(NotePath::new(
                pattern.clone(),
                *start,
                step * rate,
                glide * rate,
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
    Beats,
    Pitch,
    Pattern,
    String,
    /// `name: value`, for `with`.
    Binding,
    /// `at 4b`: where things may start.
    Grid,
    Function,
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
            Type::Beats => "a length in beats",
            Type::Pattern => "a pattern",
            Type::Pitch => "a pitch",
            Type::String => "a string",
            Type::Binding => "a binding (like pos: 0.2)",
            Type::Grid => "a start grid (like at 4b)",
            Type::Function => "a function",
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
    Beats(f64),
    /// A MIDI note number.
    Pitch(f64),
    Pattern(Arc<Pattern>),
    Binding(String, Box<Value>),
    Grid(Grid),
    Function(Arc<Function>),
    Nothing,
}

/// A function defined with `fn name(params) = body`.
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
            Value::Envelope(_) => Type::Envelope,
            Value::Str(_) => Type::String,
            Value::Num(_) => Type::Number,
            Value::Duration(_) => Type::Duration,
            Value::Beats(_) => Type::Beats,
            Value::Pattern(_) => Type::Pattern,
            Value::Pitch(_) => Type::Pitch,
            Value::Binding(..) => Type::Binding,
            Value::Grid(_) => Type::Grid,
            Value::Function(_) => Type::Function,
            Value::Nothing => Type::Nothing,
        }
    }

    /// Whether this value can be passed where `ty` is expected.
    fn fits(&self, ty: Type) -> bool {
        self.ty() == ty
            || (ty == Type::Duration && matches!(self, Value::Beats(_)))
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
}

/// A parameter in a signature: its type, and what to call it in an error.
#[derive(Clone, Copy)]
struct P(Type, &'static str);

const SOUND: P = P(Type::Sound, "a sound");
const CONTROL: P = P(Type::Control, "a control");
const NUMBER: P = P(Type::Number, "a number");
const NAME: P = P(Type::String, "a file name");
const SLOT: P = P(Type::String, "a slot name (like \"drums\")");
const GRID: P = P(Type::Grid, "a start grid (like at 4b)");
const PATTERN: P = P(Type::Pattern, "a pattern or a sound");
const MODULATION: P = P(Type::Control, "a modulation or a random");

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

    /// Argument `i` in seconds: a duration, or beats at the current tempo.
    fn duration(&self, i: usize) -> f64 {
        match &self.args[i] {
            Value::Duration(d) => *d,
            Value::Beats(b) => b * 60.0 / self.bpm,
            _ => unreachable!("not a duration"),
        }
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

    /// Argument `i` as a control, or `default` if it isn't there.
    fn control_or(&self, i: usize, default: f64) -> Control {
        self.args
            .get(i)
            .map_or(Control::Constant(default), Value::control)
    }

    /// Argument `i` as a cutoff pitch. A plain number too high to be a note
    /// was surely meant in Hz.
    fn cutoff(&self, i: usize) -> Result<Control, Error> {
        match self.args[i] {
            Value::Num(n) if n > MAX_CUTOFF_NOTE => Err(self.fail(
                i,
                format!("a cutoff is a pitch: {n} would be note {n} (did you mean {n}hz?)"),
            )),
            _ => Ok(self.control(i)),
        }
    }
}

/// Holes in `sound` that have no value, other than `except`.
fn unfilled(sound: &Sound, except: &str) -> Option<String> {
    sound
        .holes()
        .into_iter()
        .find(|(name, default)| !default && name != except)
        .map(|(name, _)| name)
}

fn no_value(c: &Call, name: &str) -> Error {
    Error {
        pos: c.pos,
        msg: format!("play: ?{name} has no value (fill it with .with({name}: ...))"),
    }
}

/// Switch the modulations in a control to another clock.
fn set_clock(c: &Call, clock: ModClock) -> Result<Value, Error> {
    match c.control(0).with_clock(clock) {
        Some(control) => Ok(Value::Control(control)),
        None => Err(c.wrong(0, "a modulation or a random")),
    }
}

fn play_sound(_: &mut Evaluator, c: &mut Call) -> Result<Value, Error> {
    if let Some(name) = unfilled(&c.sound(0), "") {
        return Err(no_value(c, &name));
    }
    let sound = c.sound(0);
    let (slot, at) = c.slot_and_grid(1);
    c.actions.push(Action::Play { sound, slot, at });
    Ok(Value::Nothing)
}

fn play_pattern(_: &mut Evaluator, c: &mut Call) -> Result<Value, Error> {
    let (pattern, instrument) = (c.args[0].pattern(), c.sound(1));
    if let Some(name) = unfilled(&instrument, "note") {
        return Err(no_value(c, &name));
    }
    let note = instrument
        .holes()
        .into_iter()
        .find(|(name, _)| name == "note");
    if pattern.has_notes() && note.is_none() {
        return Err(c.fail(
            1,
            "the sound has no ?note for the pattern's notes (use x for hits without one)",
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
        builtin("retrig", &[MODULATION], 1, |_, c| {
            set_clock(c, ModClock::Retrig)
        }),
        builtin("latch", &[MODULATION], 1, |_, c| {
            set_clock(c, ModClock::Latch)
        }),
        builtin("free", &[MODULATION], 1, |_, c| {
            set_clock(c, ModClock::Free)
        }),
        builtin("sample", &[NAME, START, END], 1, |ev, c| {
            let start = if c.args.len() > 1 { c.duration(1) } else { 0.0 };
            let end = (c.args.len() > 2).then(|| c.duration(2));
            if end.is_some_and(|end| end <= start) {
                return Err(c.wrong(2, "an end after the start"));
            }
            match ev.load(c.args[0].str(), start, end) {
                Ok(data) => sound(Sound::Sample(data)),
                Err(msg) => Err(c.fail(0, msg)),
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
                Ok(m) => control(Control::Modulation {
                    data: Arc::new(m),
                    times: 1,
                    clock: ModClock::Free,
                    start: 0.0,
                }),
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
                control(Control::Envelope(env, Some(c.duration(1))))
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
            &[
                P(Type::Control, "a sound, a modulation or a pattern"),
                NUMBER,
            ],
            2,
            |_, c| {
                let times = repeat_count(c)?;
                match c.control(0) {
                    Control::Modulation {
                        data,
                        times: n,
                        clock,
                        start,
                    } => control(Control::Modulation {
                        data,
                        times: n.saturating_mul(times),
                        clock,
                        start,
                    }),
                    _ => Err(c.wrong(0, "a sound, a modulation or a pattern")),
                }
            },
        ),
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
        builtin(
            "noise",
            &[P(
                Type::String,
                "a noise color (white, pink, brown, blue or violet)",
            )],
            0,
            |_, c| {
                let Some(name) = c.args.first().map(Value::str) else {
                    return sound(Sound::Noise(noise::Color::White));
                };
                match noise::Color::from_name(name) {
                    Some(color) => sound(Sound::Noise(color)),
                    None => Err(c.fail(
                        0,
                        format!(
                            "unknown color \"{name}\" (try {})",
                            noise::Color::NAMES.join(", ")
                        ),
                    )),
                }
            },
        ),
        builtin("lowpass", FILTER, 2, |_, c| {
            filter_with(c, filter::Kind::Lowpass)
        }),
        builtin("highpass", FILTER, 2, |_, c| {
            filter_with(c, filter::Kind::Highpass)
        }),
        builtin("bandpass", FILTER, 2, |_, c| {
            filter_with(c, filter::Kind::Bandpass)
        }),
        builtin(
            "pan",
            &[SOUND, P(Type::Control, "a position (-1 is left, 1 right)")],
            2,
            |_, c| sound(Sound::Pan(c.control(1), Box::new(c.sound(0)))),
        ),
        builtin(
            "spread",
            &[SOUND, P(Type::Control, "an amount (0 to 1)")],
            1,
            |_, c| sound(Sound::Spread(c.control_or(1, SPREAD), Box::new(c.sound(0)))),
        ),
        builtin(
            "drive",
            &[SOUND, P(Type::Control, "a drive (like 4 or 12db)")],
            1,
            |_, c| sound(Sound::Drive(c.control_or(1, DRIVE), Box::new(c.sound(0)))),
        ),
        builtin("echo", ECHO, 2, |_, c| echo_with(c, false)),
        builtin("pingpong", ECHO, 2, |_, c| echo_with(c, true)),
        builtin(
            "duck",
            &[
                SOUND,
                P(Type::String, "the slot to duck under (like \"kick\")"),
                P(Type::Control, "an amount (0 to 1)"),
                P(Type::Duration, "a release time (like 150ms)"),
            ],
            2,
            |ev, c| {
                let release = if c.args.len() > 3 {
                    c.duration(3)
                } else {
                    DUCK_RELEASE_SECONDS
                };
                sound(Sound::Duck {
                    child: Box::new(c.sound(0)),
                    bus: ev.bus(c.args[1].str()),
                    amount: c.control_or(2, DUCK_AMOUNT),
                    release,
                })
            },
        ),
        builtin(
            "random",
            &[P(Type::Duration, "how often it changes (like 1b)")],
            1,
            |_, c| {
                let period = c.duration(0);
                if period <= 0.0 {
                    return Err(c.wrong(0, "a period above 0"));
                }
                control(Control::Random {
                    period,
                    seed: noise::next_seed(),
                    clock: ModClock::Free,
                    start: 0.0,
                })
            },
        ),
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
        builtin(
            "glide",
            &[
                P(Type::Pattern, "a pattern"),
                P(Type::Duration, "a glide time (like 150ms)"),
            ],
            2,
            |_, c| {
                let mut pattern = (*c.args[0].pattern()).clone();
                pattern.glide = Some(c.duration(1).max(0.0));
                Ok(Value::Pattern(Arc::new(pattern)))
            },
        ),
    ]
}

const FILTER: &[P] = &[
    SOUND,
    P(Type::Control, "a cutoff (like 800hz or c6)"),
    P(Type::Control, "a resonance (0 to 1)"),
];

const ECHO: &[P] = &[
    SOUND,
    P(Type::Duration, "a delay time (like 0.75b)"),
    P(Type::Control, "a feedback amount (0 to 1)"),
    P(Type::Control, "a mix between 0 and 1"),
    P(Type::Control, "a low cut (like 200hz)"),
    P(Type::Control, "a high cut (like 4khz)"),
];

fn filter_with(c: &Call, kind: filter::Kind) -> Result<Value, Error> {
    sound(Sound::Filter {
        kind,
        cutoff: c.cutoff(1)?,
        resonance: c.control_or(2, 0.0),
        child: Box::new(c.sound(0)),
    })
}

fn echo_with(c: &Call, pingpong: bool) -> Result<Value, Error> {
    let time = c.duration(1);
    if time <= 0.0 {
        return Err(c.wrong(1, "a delay time above 0"));
    }
    let low = if c.args.len() > 4 {
        c.cutoff(4)?
    } else {
        Control::Constant(hz_to_note(ECHO_LOW_HZ))
    };
    let high = if c.args.len() > 5 {
        c.cutoff(5)?
    } else {
        Control::Constant(hz_to_note(ECHO_HIGH_HZ))
    };
    sound(Sound::Echo {
        child: Box::new(c.sound(0)),
        time,
        feedback: c.control_or(2, ECHO_FEEDBACK),
        mix: c.control_or(3, ECHO_MIX),
        low,
        high,
        pingpong,
    })
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
                return match self.vars.get(name) {
                    Some(value) => Ok(value.clone()),
                    None if BUILTINS.iter().any(|b| b.name == name) => {
                        fail(format!("'{name}' is a function: call it with {name}(...)"))
                    }
                    None => fail(format!("unknown name '{name}'")),
                };
            }
            Expr::Let { name, value } => {
                let value = self.eval(value, actions)?;
                self.vars.insert(name.clone(), value);
                return Ok(Value::Nothing);
            }
            Expr::Fn { name, params, body } => {
                if BUILTINS.iter().any(|b| b.name == name) {
                    return fail(format!("'{name}' is already a built-in function"));
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
                let value = self.eval(value, actions)?;
                return Ok(Value::Binding(name.clone(), Box::new(value)));
            }
            Expr::Call { name, args } => (name.as_str(), args),
        };

        let candidates: Vec<&Builtin> = BUILTINS.iter().filter(|b| b.name == name).collect();
        if candidates.is_empty() {
            return match self.vars.get(name) {
                Some(Value::Function(f)) => {
                    let f = f.clone();
                    self.call(&f, args, e.pos, actions)
                }
                Some(v) => fail(format!("'{name}' is {}, not a function", v.ty().name())),
                None => fail(format!("unknown function '{name}'")),
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
                    values[at].ty().name()
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

    /// Call a user function: its parameters are bound to the arguments while
    /// its body is evaluated. Names in the body are looked up when it's
    /// called, so it sees the `let`s and functions there are by then.
    ///
    /// The body was parsed from whichever code defined the function, so its
    /// positions mean nothing in the code being run: errors from inside it
    /// are reported at the call, with the function's name in front.
    fn call(
        &mut self,
        f: &Function,
        args: &[Spanned],
        pos: usize,
        actions: &mut Vec<Action>,
    ) -> Result<Value, Error> {
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            values.push(self.eval(arg, actions)?);
        }
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
mod tests {
    use super::*;
    use crate::envelope::Stage;
    use crate::lang::parse;
    use crate::modulation::Point;
    use crate::nodes::Frame;

    fn evaluator() -> Evaluator {
        Evaluator::new(48_000, Bundle::with_kick())
    }

    /// An evaluator whose `.rock` file has a `slow` envelope (1 s attack, 1 s
    /// release, straight lines), a `ramp` modulation (0 to 1 over 1 s), and a
    /// sample `ones.wav` (a second of 1.0).
    fn with_resources() -> Evaluator {
        let bundle = Bundle::default();
        let line = |time| Stage { time, curve: 0.0 };
        let slow = Envelope {
            attack: line(1.0),
            decay: line(0.0),
            sustain: 1.0,
            release: line(1.0),
        };
        bundle.put(ResourceKind::Envelope, "slow", slow.to_json());
        let ramp = Modulation {
            length: 1.0,
            points: vec![Point::new(0.0, 0.0), Point::new(1.0, 1.0)],
        };
        bundle.put(ResourceKind::Modulation, "ramp", ramp.to_json());
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut wav = Vec::new();
        let mut writer = hound::WavWriter::new(std::io::Cursor::new(&mut wav), spec).unwrap();
        for _ in 0..48_000 * 2 {
            writer.write_sample(1.0f32).unwrap();
        }
        writer.finalize().unwrap();
        bundle.put(ResourceKind::Sample, "ones.wav", wav);
        Evaluator::new(48_000, bundle)
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

    /// The sound `src` plays (it must play exactly one).
    fn played(evaluator: &mut Evaluator, src: &str) -> Sound {
        let mut actions = evaluator.run(&parse(src).unwrap()).unwrap();
        let Some(Action::Play { sound, .. }) = actions.pop() else {
            panic!("nothing played")
        };
        sound
    }

    fn render_with(evaluator: &mut Evaluator, src: &str) -> Vec<Frame> {
        let mut node = played(evaluator, src).instantiate(48_000);
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
            "repeat: expected a sound, a modulation or a pattern, got a number"
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
        let sound = played(
            &mut evaluator(),
            r#"sample("kick.mp3").fit(10ms).repeat(inf).play"#,
        );
        let mut node = sound.instantiate(48_000);
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
        let mut ev = with_resources();
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
    }

    #[test]
    fn repeated_modulation_is_an_lfo() {
        let mut ev = with_resources();
        let saw = render_with(
            &mut ev,
            &format!(r#"{ONES}.repeat(3).gain(modulation("ramp").repeat(2)).play"#),
        );
        assert!((saw[12_000][0] - 0.25).abs() < 1e-3);
        assert!((saw[48_000 + 12_000][0] - 0.25).abs() < 1e-3);
        // After two passes it holds its last value.
        assert!((saw[48_000 * 2 + 12_000][0] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn gated_envelope_ends_the_sound_after_its_release() {
        let mut ev = with_resources();
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
    fn named_slots_are_found_without_running() {
        let src = r#"
            let hat = sample("hat.wav")
            notes("x . x", 0.25b).play(hat, "hat")
            sample("a.wav").play("pad")
            sample("b.wav").play
            notes("c2", 1b).play(lead)
            sample("c.wav").play("pad")
            sample("d.wav").play(at 4b, "later")
            notes("x", 1b).play(hat, "fives", at 5b + 2)
        "#;
        let grid = |every, offset| Some(Grid { every, offset });
        assert_eq!(
            named_slots(&parse(src).unwrap()),
            [
                ("hat".to_string(), None),
                ("pad".to_string(), None),
                ("later".to_string(), grid(4.0, 0.0)),
                ("fives".to_string(), grid(5.0, 2.0)),
            ]
        );
    }

    #[test]
    fn beats_follow_the_tempo() {
        let mut ev = evaluator();
        let fit = r#"sample("kick.mp3").fit(1b).play"#;
        assert_eq!(render_with(&mut ev, fit).len(), 24_000);
        run(&mut ev, "60.bpm");
        assert_eq!(render_with(&mut ev, fit).len(), 48_000);
        assert_eq!(
            error_with(&mut ev, "10.bpm"),
            "bpm: expected a tempo between 20 and 999, got a number"
        );
    }

    #[test]
    fn patterns_need_a_fitting_instrument() {
        let mut ev = with_resources();
        let lead = r#"wavetable("basic", 0, 0, ?note)"#;
        assert_eq!(
            error_with(&mut ev, r#"notes("c2 q", 0.25b)"#),
            "notes: 'q' isn't a step (use notes like c2 or f#3, x, . or _)"
        );
        assert_eq!(
            error_with(&mut ev, r#"notes("c2", 250ms)"#),
            "notes: expected a step length in beats (like 0.25b), got a duration"
        );
        assert_eq!(
            error_with(&mut ev, r#"notes("c2 e2", 0.5b).play(sample("ones.wav"))"#),
            "play: the sound has no ?note for the pattern's notes (use x for hits without one)"
        );
        assert_eq!(
            error_with(&mut ev, &format!(r#"notes("x", 0.5b).play({lead})"#)),
            "play: the pattern has hits without a note (x), but ?note has no default"
        );
        assert_eq!(
            error_with(
                &mut ev,
                r#"notes("c2", 0.5b).play(wavetable("basic", ?pos, 0, ?note))"#
            ),
            "play: ?pos has no value (fill it with .with(pos: ...))"
        );
        let actions = ev
            .run(&parse(r#"notes("x . x x", 0.25b).play(sample("ones.wav"), "drums")"#).unwrap())
            .unwrap();
        let [Action::Pattern { pattern, slot, .. }] = actions.as_slice() else {
            panic!()
        };
        assert_eq!((pattern.steps.len(), slot.as_deref()), (4, Some("drums")));
    }

    #[test]
    fn voices_for_notes() {
        let mut ev = with_resources();
        run(
            &mut ev,
            r#"let lead = wavetable("basic", 0, 0, ?note) * envelope("slow")"#,
        );
        // Released at the gate (0.5 s), then a second of release.
        let voice = ev.vars["lead"]
            .sound()
            .for_note(Some(Control::Constant(69.0)), Some(0.5), 0.0);
        let frames = render_node(voice.instantiate(48_000).as_mut(), 48_000 * 10);
        assert_eq!(frames.len(), 72_000);

        // A ramp from 0 to 1 over a second, on a sound that never ends: cut off
        // at the gate. The note starts half a second into the pattern.
        let mut ramp = |clock: &str| {
            run(
                &mut ev,
                &format!(r#"let s = sample("ones.wav").repeat(inf) * modulation("ramp"){clock}"#),
            );
            let voice = ev.vars["s"].sound().for_note(None, Some(0.25), 0.5);
            let frames = render_node(voice.instantiate(48_000).as_mut(), 48_000 * 10);
            assert_eq!(frames.len(), 12_000);
            (frames[0][0], frames[6_000][0])
        };
        let close = |(a, b): (f32, f32), (x, y): (f32, f32)| {
            assert!((a - x).abs() < 1e-3 && (b - y).abs() < 1e-3, "{a}, {b}")
        };
        close(ramp(""), (0.5, 0.625));
        close(ramp(".free"), (0.5, 0.625));
        close(ramp(".retrig"), (0.0, 0.125));
        close(ramp(".latch"), (0.5, 0.5));
        assert_eq!(
            error_with(&mut ev, "0.5.latch"),
            "latch: expected a modulation or a random, got a number"
        );
    }

    #[test]
    fn resource_errors() {
        let mut ev = with_resources();
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
    }

    /// The one play action `src` produces: its slot and grid.
    fn play_timing(src: &str) -> (Option<String>, Option<Grid>) {
        match evaluator().run(&parse(src).unwrap()).unwrap().pop() {
            Some(Action::Play { slot, at, .. } | Action::Pattern { slot, at, .. }) => (slot, at),
            _ => panic!("nothing played"),
        }
    }

    #[test]
    fn plays_take_a_slot_and_a_grid() {
        let kick = r#"sample("kick.mp3")"#;
        let grid = |every, offset| Some(Grid { every, offset });
        assert_eq!(play_timing(&format!("{kick}.play")), (None, None));
        assert_eq!(play_timing(&format!("{kick}.play()")), (None, None));
        assert_eq!(
            play_timing(&format!(r#"{kick}.play("a")"#)),
            (Some("a".into()), None)
        );
        assert_eq!(
            play_timing(&format!("{kick}.play(at 4b)")),
            (None, grid(4.0, 0.0))
        );
        assert_eq!(
            play_timing(&format!(r#"{kick}.play("a", at 5b + 2)"#)),
            (Some("a".into()), grid(5.0, 2.0))
        );
        assert_eq!(
            play_timing(&format!(
                r#"notes("x", 1b).play({kick}, "d", at 1bar + 0.5b)"#
            )),
            (Some("d".into()), grid(4.0, 0.5))
        );
        assert_eq!(
            play_timing(&format!(r#"notes("x", 1b).play({kick}, at 2b)"#)),
            (None, grid(2.0, 0.0))
        );
        assert_eq!(
            error(&format!("{kick}.play(at 0b)")),
            "at: expected a grid size above 0 beats, got a length in beats"
        );
        assert_eq!(
            error(&format!("{kick}.play(at 4)")),
            "at: expected a grid size in beats (like 4b), got a number"
        );
        assert_eq!(
            error(&format!("{kick}.play(4)")),
            "play: expected a slot name (like \"drums\") or a start grid (like at 4b), got a number"
        );
    }

    fn rms(frames: &[Frame]) -> f32 {
        (frames.iter().map(|f| f[0] * f[0]).sum::<f32>() / frames.len() as f32).sqrt()
    }

    /// How bright a sound is: neighbouring samples' differences against the
    /// level.
    fn brightness(frames: &[Frame]) -> f32 {
        let diff: f32 = frames.windows(2).map(|w| (w[1][0] - w[0][0]).abs()).sum();
        diff / frames.iter().map(|f| f[0].abs()).sum::<f32>()
    }

    #[test]
    fn noise_and_filters() {
        let white = render(r#"noise().fit(1s).play"#);
        assert_eq!(white.len(), 48_000);
        assert!(rms(&white) > 0.1);
        let brown = render(r#"noise("brown").fit(1s).play"#);
        assert!(brightness(&brown) < brightness(&white) * 0.2);
        let low = render(r#"noise().lowpass(500hz).fit(1s).play"#);
        let high = render(r#"noise().highpass(5khz, 0.3).fit(1s).play"#);
        let band = render(r#"noise().bandpass(c6, 0.9).fit(1s).play"#);
        assert!(brightness(&low) < brightness(&white) * 0.3);
        assert!(brightness(&high) > brightness(&white));
        assert!(rms(&band) < rms(&white) * 0.3);
        // The cutoff can move: a lowpass opening up gets brighter.
        let mut ev = with_resources();
        let sweep = render_with(
            &mut ev,
            r#"noise().lowpass(30 + modulation("ramp") * 100).play"#,
        );
        let sweep = &sweep[..48_000];
        assert!(brightness(&sweep[40_000..]) > 3.0 * brightness(&sweep[..8_000]));
        assert!(
            error(r#"noise("green")"#)
                .starts_with("noise: unknown color \"green\" (try white, pink,")
        );
        assert_eq!(
            error(r#"noise().lowpass(800)"#),
            "lowpass: a cutoff is a pitch: 800 would be note 800 (did you mean 800hz?)"
        );
    }

    #[test]
    fn pan_spread_and_echo() {
        let side = |frames: &[Frame]| frames.iter().map(|f| (f[0] - f[1]).abs()).sum::<f32>();
        let tone = r#"wavetable("basic", 0, 0, a4).fit(500ms)"#;
        let left = render(&format!("{tone}.pan(-1).play"));
        assert!(left.iter().all(|f| f[1].abs() < 1e-6));
        assert_eq!(side(&render(&format!("{tone}.play"))), 0.0);
        assert!(side(&render(&format!("{tone}.spread.play"))) > 100.0);
        assert!(
            side(&render(&format!("{tone}.spread(0.2).play")))
                < side(&render(&format!("{tone}.spread(1).play")))
        );
        // Echoes: a 10 ms click every 250 ms (half a beat), dying away.
        let echoes = render(r#"sample("kick.mp3").fit(10ms).echo(0.5b, 0.5, 0.5).play"#);
        let peak = |at: usize| {
            echoes[at..at + 480]
                .iter()
                .fold(0f32, |m, f| m.max(f[0].abs()))
        };
        assert!(peak(12_000) > 0.05 && peak(24_000) > 0.02 && peak(24_000) < peak(12_000));
        assert!(peak(6_000) < 1e-3, "nothing in between");
        let pingpong = render(r#"sample("kick.mp3").fit(10ms).pingpong(100ms, 0.5, 1).play"#);
        assert!(pingpong[4_800..5_280].iter().all(|f| f[1].abs() < 1e-6));
        assert!(pingpong[9_600..10_080].iter().any(|f| f[1].abs() > 1e-3));
        assert_eq!(
            error(r#"sample("kick.mp3").echo(0ms)"#),
            "echo: expected a delay time above 0, got a duration"
        );
    }

    #[test]
    fn random_range_and_round() {
        let mut ev = evaluator();
        // A new note every 100 ms, a whole semitone from a3 to c4.
        let notes: Vec<f32> = {
            run(&mut ev, "let r = random(100ms).range(a3, c4)");
            let Value::Control(c) = &ev.vars["r"] else {
                panic!()
            };
            let mut node = c.instantiate(48_000);
            let mut buf = vec![0.0; 48_000];
            node.process(&mut buf);
            buf.iter().step_by(4_800).copied().collect()
        };
        assert!(
            notes.iter().all(|n| [57.0, 58.0, 59.0, 60.0].contains(n)),
            "{notes:?}"
        );
        assert!(notes.windows(2).any(|w| w[0] != w[1]));
        let values = |src: &str| {
            let mut ev = evaluator();
            run(&mut ev, &format!("let r = {src}"));
            let Value::Control(c) = &ev.vars["r"] else {
                panic!()
            };
            let mut buf = vec![0.0; 48_000];
            c.instantiate(48_000).process(&mut buf);
            buf
        };
        let continuous = values("random(10ms).range(-1, 1)");
        assert!(continuous.iter().all(|v| (-1.0..=1.0).contains(v)));
        assert!(continuous.iter().any(|v| v.fract() != 0.0));
        let rounded = values("random(10ms).range(0, 12).round");
        assert!(
            rounded
                .iter()
                .all(|v| v.fract() == 0.0 && (0.0..=12.0).contains(v))
        );
        // In a pattern, each note picks up the stream where it is; latched,
        // it holds that value for the whole note.
        let mut ev = with_resources();
        run(&mut ev, &format!("let s = {ONES}.gain(random(1ms).latch)"));
        let voice = |since| {
            let voice = ev.vars["s"].sound().for_note(None, Some(0.1), since);
            render_node(voice.instantiate(48_000).as_mut(), 48_000)
        };
        let a = voice(0.5);
        assert!(a.iter().all(|f| f[0] == a[0][0]), "held");
        assert_eq!(voice(0.5)[0], a[0], "the stream's value at 0.5 s");
        assert_ne!(voice(0.7)[0], a[0], "another note, another value");
        assert_eq!(
            error("random(0ms)"),
            "random: expected a period above 0, got a duration"
        );
    }

    #[test]
    fn patterns_glide() {
        let mut ev = evaluator();
        let actions = ev
            .run(
                &parse(
                    r#"notes("c4 e4", 0.5b).glide(100ms).play(wavetable("basic", 0, 0, ?note))"#,
                )
                .unwrap(),
            )
            .unwrap();
        let [Action::Pattern { pattern, .. }] = actions.as_slice() else {
            panic!()
        };
        assert_eq!(pattern.glide, Some(0.1));
        let glide_in_beats = ev
            .run(
                &parse(r#"notes("c4 e4", 0.5b).glide(0.5b).play(wavetable("basic", 0, 0, ?note))"#)
                    .unwrap(),
            )
            .unwrap();
        let [Action::Pattern { pattern, .. }] = glide_in_beats.as_slice() else {
            panic!()
        };
        assert_eq!(pattern.glide, Some(0.25));
    }

    #[test]
    fn functions() {
        let mut ev = evaluator();
        let kick = r#"sample("kick.mp3")"#;
        run(&mut ev, "fn louder(x, by) = x.gain(by)");
        // Called both ways.
        let plain = peak(&render_with(&mut ev, &format!("{kick}.play")));
        let twice = peak(&render_with(&mut ev, &format!("{kick}.louder(2).play")));
        assert!((twice - 2.0 * plain).abs() < 1e-4);
        let again = peak(&render_with(&mut ev, &format!("louder({kick}, 2).play")));
        assert_eq!(again, twice);

        // Holes in the body are holes in what it returns.
        run(
            &mut ev,
            "fn tone(pos) = wavetable(\"basic\", pos, 0, ?note = a4).fit(100ms)",
        );
        let a4 = render_with(&mut ev, "tone(0).play");
        let a5 = render_with(&mut ev, "tone(0).with(note: a5).play");
        assert!((frequency(&a4) - 440.0).abs() <= 10.0);
        assert!((frequency(&a5) - 880.0).abs() <= 10.0);

        // Parameters shadow lets for the call only.
        run(&mut ev, "let x = 3\nfn id(x) = x");
        run(&mut ev, "let y = id(4)");
        assert!(matches!(ev.vars["y"], Value::Num(4.0)));
        assert!(matches!(ev.vars["x"], Value::Num(3.0)));
        assert!(!ev.vars.contains_key("by"), "no parameter left behind");

        // Names in the body are looked up when it's called.
        run(
            &mut ev,
            "fn amount() = 0.5\nfn quieter(x) = x.gain(amount())",
        );
        run(&mut ev, "fn amount() = 0.25");
        let quarter = peak(&render_with(&mut ev, &format!("{kick}.quieter.play")));
        assert!((quarter - 0.25 * plain).abs() < 1e-4);
    }

    #[test]
    fn function_errors() {
        let mut ev = evaluator();
        run(&mut ev, "fn wide(x) = x.spread(\"lots\")");
        // At the call, not somewhere in the body's (other) code.
        let src = r#"sample("kick.mp3").wide.play"#;
        let e = ev.run(&parse(src).unwrap()).err().unwrap();
        assert_eq!(e.pos, 19);
        assert_eq!(
            e.msg,
            "wide: spread: expected an amount (0 to 1), got a string"
        );
        assert_eq!(
            error_with(&mut ev, r#"sample("kick.mp3").wide(1)"#),
            "wide takes 1 argument(s), got 2"
        );
        assert_eq!(
            error_with(&mut ev, "fn play(x) = x"),
            "'play' is already a built-in function"
        );
        run(&mut ev, "let n = 1");
        assert_eq!(
            error_with(&mut ev, "n(2)"),
            "'n' is a number, not a function"
        );
        run(&mut ev, "fn forever(x) = forever(x)");
        assert_eq!(
            error_with(&mut ev, "forever(1)"),
            "forever: too many calls inside calls (does a function call itself?)"
        );
        assert_eq!(
            error_with(&mut ev, "wide.play"),
            "play: expected a sound, got a function"
        );
        // A failed block leaves no function behind, like a failed let.
        assert!(ev.run(&parse("fn f() = 1\nnope()").unwrap()).is_err());
        assert_eq!(error_with(&mut ev, "f()"), "unknown function 'f'");
    }
}
