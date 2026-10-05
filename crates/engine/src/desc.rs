//! Descriptions of sounds and controls: what evaluating code produces.
//!
//! A `Sound` is a cheap, immutable *description*; only `instantiate` turns it
//! into live, stateful audio nodes, once per voice. `Control` is the same for
//! control signals (envelopes, modulations, arithmetic on them).
//!
//! Built-in nodes (`Node`) have params (see `spec`). A param is *free* until
//! it's set: a free control param is a hole (`Control::Hole`) named after the
//! param, holding its default; a free param of another type (a table name, a
//! duration) is marked free. Setting a param (see `eval`) fills every free
//! param of that name in a whole description, so composed sounds still have
//! the free params of their parts. `?name` makes a new hole, which is how
//! params get linked: `x:note(?note + 12)` fills the `note` holes in `x` with
//! a control that has a new, outer `?note`.
//!
//! An effect (`lowpass`) is a sound with an open `Input`; applying it to a
//! sound (`sound * lowpass`) fills the input.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use crate::control::{
    self, Combine, ControlNode, EnvelopePlayer, Glide, Map, ModulationPlayer, NotePath, Param,
    RandomPlayer, Range, ScaleDegree,
};
use crate::envelope::Envelope;
use crate::filter::{self, Filter};
use crate::fx::{Drive, Echo, EchoParams, Pan, Spread};
use crate::lang::{hz_to_note, note_name, note_to_hz_text};
use crate::modulation::Modulation;
use crate::nodes::{
    Add, Delay, Fit, Gain, Limit, Multiply, Node as AudioNode, Repeat, SampleData, Sampler, Seq,
    Slice,
};
use crate::noise::{self, Noise};
use crate::pattern::Pattern;
use crate::reverb::{Impulse, Reverb};
use crate::sidechain::{Bus, Duck};
use crate::spec::{Def, Kind, ParamDoc, Role, Spec, Ty, param_doc, trim};
use crate::wavetable::{Oscillator, Wavetable};

/// Fade-out applied where `fit` cuts a sound off. A few ms is enough to remove the
/// click without audibly softening a transient.
pub const FIT_FADE_SECONDS: f64 = 0.003;

/// `limit`'s lookahead and release.
const LIMIT_LOOKAHEAD_SECONDS: f64 = 0.005;
const LIMIT_RELEASE_SECONDS: f64 = 0.1;

/// The value of a node's param.
#[derive(Clone)]
pub enum Arg {
    Control(Control),
    Num(f64),
    Str(String),
    Seconds(f64),
    /// A sound, as a reverb's space.
    Sound(Box<Sound>),
    /// Not set (see the param's doc for what that means).
    Unset,
}

impl Arg {
    /// The value as it would be written.
    fn text(&self, ty: Ty) -> String {
        match self {
            Arg::Control(c) => c.describe(ty),
            Arg::Num(n) if *n == f64::INFINITY => "inf".into(),
            Arg::Num(n) => trim(*n),
            Arg::Str(s) => format!("\"{s}\""),
            Arg::Seconds(s) => Def::Seconds(*s).text(),
            Arg::Sound(_) => "a sound".into(),
            Arg::Unset => "unset".into(),
        }
    }
}

/// What a node's params refer to, loaded when they're set (see
/// `Evaluator::resolve`), so nothing is loaded while voices are built.
#[derive(Clone)]
pub enum Resolved {
    None,
    Sample(Arc<SampleData>),
    Table(Arc<Wavetable>),
    Impulse(Arc<Impulse>),
    Bus(Arc<Bus>),
    Envelope(Arc<Envelope>),
    Modulation(Arc<Modulation>),
    Noise(noise::Color),
    Seed(u64),
}

/// A built-in node with its params.
#[derive(Clone)]
pub struct Node {
    pub spec: &'static Spec,
    /// One per `spec.params`.
    pub args: Vec<Arg>,
    /// Whether each non-control param is still free. (A control param is
    /// free where it's a hole.)
    pub free: Vec<bool>,
    pub resolved: Resolved,
    /// An effect's input.
    pub input: Option<Box<Sound>>,
    /// Seconds into a pattern that this voice starts: free-running controls
    /// pick up there.
    pub since: f64,
    /// The pitch a glide slides in from.
    pub from: Option<f64>,
}

impl Node {
    /// A node with every param at its default; `bpm` turns default lengths
    /// in beats into seconds. (Not resolved yet.)
    pub fn new(spec: &'static Spec, bpm: f64) -> Node {
        let mut args = Vec::new();
        let mut free = Vec::new();
        for param in spec.params {
            let preset = spec.presets.iter().find(|(name, _)| *name == param.name);
            let default = preset.map_or(param.default, |(_, d)| *d);
            let value = def_arg(default, bpm);
            let arg = match (param.ty.is_control(), value) {
                (true, Arg::Num(n)) if preset.is_some() => Arg::Control(Control::Constant(n)),
                (true, value) => Arg::Control(Control::Hole {
                    name: param.name.to_string(),
                    default: match value {
                        Arg::Num(n) => Some(Box::new(Control::Constant(n))),
                        _ => None,
                    },
                }),
                (false, value) => value,
            };
            args.push(arg);
            free.push(preset.is_none());
        }
        Node {
            spec,
            args,
            free,
            resolved: Resolved::None,
            input: (spec.role == Role::Effect).then(|| Box::new(Sound::Input)),
            since: 0.0,
            from: None,
        }
    }

    pub fn kind(&self) -> Kind {
        self.spec.kind
    }

    fn index(&self, name: &str) -> usize {
        self.spec
            .param(name)
            .unwrap_or_else(|| panic!("{} has no {name}", self.spec.name))
            .0
    }

    pub fn arg(&self, name: &str) -> &Arg {
        &self.args[self.index(name)]
    }

    fn control(&self, name: &str) -> &Control {
        match self.arg(name) {
            Arg::Control(c) => c,
            _ => unreachable!("{name} isn't a control"),
        }
    }

    fn param(&self, name: &str, sample_rate: u32) -> Param {
        self.control(name).param(sample_rate)
    }

    pub fn num(&self, name: &str) -> f64 {
        match self.arg(name) {
            Arg::Num(n) => *n,
            _ => unreachable!("{name} isn't a number"),
        }
    }

    pub fn str(&self, name: &str) -> &str {
        match self.arg(name) {
            Arg::Str(s) => s,
            _ => unreachable!("{name} isn't a string"),
        }
    }

    pub fn seconds(&self, name: &str) -> Option<f64> {
        match self.arg(name) {
            Arg::Seconds(s) => Some(*s),
            _ => None,
        }
    }

    fn flag(&self, name: &str) -> bool {
        self.num(name) != 0.0
    }

    /// How many times a modulation plays.
    fn times(&self) -> usize {
        let n = self.num("repeat");
        if n == f64::INFINITY {
            usize::MAX
        } else {
            n as usize
        }
    }

    /// The same node with its input and control params rewritten.
    pub fn try_map<M: Mapper + ?Sized>(&self, m: &mut M) -> Result<Node, M::Error> {
        let mut node = self.clone();
        for arg in &mut node.args {
            if let Arg::Control(control) = arg {
                *control = m.control(control)?;
            }
        }
        if let Some(input) = &self.input {
            node.input = Some(Box::new(m.sound(input)?));
        }
        Ok(node)
    }

    fn controls(&self) -> impl Iterator<Item = &Control> {
        self.args.iter().filter_map(|a| match a {
            Arg::Control(c) => Some(c),
            _ => None,
        })
    }

    /// The node's free params that aren't holes, as docs.
    fn free_params(&self, out: &mut Params) {
        for (i, param) in self.spec.params.iter().enumerate() {
            if self.free[i] && !param.ty.is_control() {
                out.add(param.name, self.args[i].text(param.ty));
            }
        }
    }

    fn instantiate_control(&self, sample_rate: u32) -> Box<dyn ControlNode> {
        let rate = sample_rate as f64;
        match (self.kind(), &self.resolved) {
            (Kind::Envelope, Resolved::Envelope(env)) => {
                let gate = self
                    .seconds("gate")
                    .map(|seconds| (seconds * rate).round() as usize);
                Box::new(EnvelopePlayer::new(env.clone(), gate, sample_rate))
            }
            (Kind::Mod, Resolved::Modulation(data)) => Box::new(ModulationPlayer::new(
                data.clone(),
                self.times(),
                (self.since * rate).round() as usize,
                sample_rate,
            )),
            (Kind::Random, Resolved::Seed(seed)) => {
                let period = self.seconds("every").unwrap_or(1.0);
                Box::new(RandomPlayer::new(*seed, period * rate, self.since * rate))
            }
            (Kind::Glide, _) => Box::new(Glide::new(
                self.control("target").instantiate(sample_rate),
                (self.seconds("dur").unwrap_or(0.0) * rate) as f32,
                self.from.map(|f| f as f32),
            )),
            (kind, _) => unreachable!("{kind:?} isn't a resolved control"),
        }
    }

    fn instantiate_sound(&self, sample_rate: u32) -> Box<dyn AudioNode> {
        let frames = |seconds: f64| (seconds * sample_rate as f64).round() as usize;
        let input = || match &self.input {
            Some(input) => input.instantiate(sample_rate),
            None => unreachable!("an effect without an input"),
        };
        let param = |name| self.param(name, sample_rate);
        let filter = |kind| {
            Box::new(Filter::new(
                input(),
                kind,
                param("freq"),
                param("res"),
                sample_rate,
            ))
        };
        let echo = |pingpong| {
            Box::new(Echo::new(
                input(),
                frames(self.seconds("time").unwrap_or(0.5)),
                EchoParams {
                    feedback: param("feedback"),
                    mix: param("mix"),
                    low: param("low"),
                    high: param("high"),
                    pingpong,
                },
                sample_rate,
            ))
        };
        match (self.kind(), &self.resolved) {
            (Kind::Wavetable, Resolved::Table(table)) => Box::new(Oscillator::new(
                table.clone(),
                param("pos"),
                param("warp"),
                param("note"),
                sample_rate,
            )),
            (Kind::Noise, Resolved::Noise(color)) => {
                Box::new(Noise::new(*color, noise::next_seed()))
            }
            (Kind::Sample, Resolved::Sample(data)) => {
                Box::new(Sampler::new(data.clone(), sample_rate))
            }
            (Kind::Lowpass, _) => filter(filter::Kind::Lowpass),
            (Kind::Highpass, _) => filter(filter::Kind::Highpass),
            (Kind::Bandpass, _) => filter(filter::Kind::Bandpass),
            (Kind::Pan, _) => Box::new(Pan::new(input(), param("side"))),
            (Kind::Spread, _) => Box::new(Spread::new(input(), param("width"), sample_rate)),
            (Kind::Drive, _) => Box::new(Drive::new(input(), param("power"))),
            (Kind::Echo, _) => echo(false),
            (Kind::Pingpong, _) => echo(true),
            (Kind::Reverb, Resolved::Impulse(impulse)) => Box::new(Reverb::new(
                input(),
                impulse.clone(),
                self.num("mix") as f32,
            )),
            (Kind::Duck, Resolved::Bus(bus)) => Box::new(Duck::new(
                input(),
                bus.clone(),
                param("depth"),
                self.seconds("release").unwrap_or(0.15) as f32,
                sample_rate,
            )),
            (Kind::Limit, _) => Box::new(Limit::new(
                input(),
                self.num("ceiling") as f32,
                frames(LIMIT_LOOKAHEAD_SECONDS),
                (LIMIT_RELEASE_SECONDS * sample_rate as f64) as f32,
            )),
            (kind, _) => unreachable!("{kind:?} isn't a resolved sound"),
        }
    }
}

/// Rewrites sounds and controls. By default it goes on into their children,
/// so an implementation only says what it changes.
pub trait Mapper {
    type Error;

    fn sound(&mut self, s: &Sound) -> Result<Sound, Self::Error> {
        s.try_map(self)
    }

    fn control(&mut self, c: &Control) -> Result<Control, Self::Error> {
        c.try_map(self)
    }
}

/// Rewrites every control with a function (which goes into their insides,
/// or not, as it likes), going down through the sounds.
struct Controls<F>(F);

impl<F: FnMut(&Control) -> Control> Mapper for Controls<F> {
    type Error = Infallible;

    fn control(&mut self, c: &Control) -> Result<Control, Infallible> {
        Ok((self.0)(c))
    }
}

/// Fills an effect's input.
struct Apply<'a>(&'a Sound);

impl Mapper for Apply<'_> {
    type Error = Infallible;

    fn sound(&mut self, s: &Sound) -> Result<Sound, Infallible> {
        match s {
            Sound::Input => Ok(self.0.clone()),
            s => s.try_map(self),
        }
    }

    fn control(&mut self, c: &Control) -> Result<Control, Infallible> {
        Ok(c.clone())
    }
}

/// A default as a param value.
fn def_arg(default: Def, bpm: f64) -> Arg {
    match default {
        Def::Required | Def::Unset => Arg::Unset,
        Def::Num(n) | Def::Note(n) => Arg::Num(n),
        Def::Hz(hz) => Arg::Num(hz_to_note(hz)),
        Def::Str(s) => Arg::Str(s.to_string()),
        Def::Beats(b) => Arg::Seconds(b * 60.0 / bpm),
        Def::Seconds(s) => Arg::Seconds(s),
        Def::Flag(b) => Arg::Num(b as u8 as f64),
    }
}

#[derive(Clone)]
pub enum Sound {
    /// Where an effect's input goes, until it's applied to a sound.
    Input,
    Node(Box<Node>),
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
}

impl Sound {
    /// The same sound with its child sounds and its own controls rewritten
    /// (one level down: `m` decides whether to go on).
    pub fn try_map<M: Mapper + ?Sized>(&self, m: &mut M) -> Result<Sound, M::Error> {
        let b = |m: &mut M, x: &Sound| m.sound(x).map(Box::new);
        Ok(match self {
            Sound::Input => Sound::Input,
            Sound::Node(node) => Sound::Node(Box::new(node.try_map(m)?)),
            Sound::Fit(x, seconds) => Sound::Fit(b(m, x)?, *seconds),
            Sound::Delay(x, seconds) => Sound::Delay(b(m, x)?, *seconds),
            Sound::Slice(x, start, end) => Sound::Slice(b(m, x)?, *start, *end),
            Sound::Repeat(x, times) => Sound::Repeat(b(m, x)?, *times),
            Sound::Add(xs) => Sound::Add(xs.iter().map(|x| m.sound(x)).collect::<Result<_, _>>()?),
            Sound::Seq(xs) => Sound::Seq(xs.iter().map(|x| m.sound(x)).collect::<Result<_, _>>()?),
            Sound::Gain(amount, x) => Sound::Gain(m.control(amount)?, b(m, x)?),
            Sound::Multiply(x, y) => Sound::Multiply(b(m, x)?, b(m, y)?),
        })
    }

    /// Every control in the sound replaced by `f(control)`.
    pub fn map_controls(&self, f: impl FnMut(&Control) -> Control) -> Sound {
        let Ok(sound) = self.try_map(&mut Controls(f));
        sound
    }

    /// The child sounds (and an effect's input).
    fn children(&self) -> Vec<&Sound> {
        match self {
            Sound::Input => vec![],
            Sound::Node(node) => node.input.iter().map(|b| &**b).collect(),
            Sound::Fit(x, _)
            | Sound::Delay(x, _)
            | Sound::Slice(x, ..)
            | Sound::Repeat(x, _)
            | Sound::Gain(_, x) => vec![x],
            Sound::Add(xs) | Sound::Seq(xs) => xs.iter().collect(),
            Sound::Multiply(x, y) => vec![x, y],
        }
    }

    /// Every control in the sound, at the top (not their insides).
    fn controls(&self) -> Vec<&Control> {
        let mut out = Vec::new();
        self.each(&mut |s| match s {
            Sound::Node(node) => out.extend(node.controls()),
            Sound::Gain(c, _) => out.push(c),
            _ => {}
        });
        out
    }

    /// Call `f` on this sound and every sound in it.
    fn each<'a>(&'a self, f: &mut dyn FnMut(&'a Sound)) {
        f(self);
        for child in self.children() {
            child.each(f);
        }
    }

    /// The sound's own nodes (not its controls').
    fn sound_nodes(&self) -> Vec<&Node> {
        let mut out = Vec::new();
        self.each(&mut |s| {
            if let Sound::Node(node) = s {
                out.push(&**node);
            }
        });
        out
    }

    /// Every node in the sound, its controls' nodes included.
    pub fn nodes(&self) -> Vec<&Node> {
        let mut out = self.sound_nodes();
        for c in self.controls() {
            c.nodes(&mut out);
        }
        out
    }

    /// Whether it's an effect: a sound with an open input.
    pub fn has_input(&self) -> bool {
        matches!(self, Sound::Input) || self.children().iter().any(|c| c.has_input())
    }

    /// This effect applied to `input`.
    pub fn apply(&self, input: &Sound) -> Sound {
        let Ok(sound) = Apply(input).sound(self);
        sound
    }

    /// Every hole in the sound, and whether it has a default.
    pub fn holes(&self) -> Vec<(String, bool)> {
        let mut holes = Vec::new();
        for c in self.controls() {
            c.holes(&mut holes);
        }
        holes
    }

    /// The holes called `name` filled with `value`.
    pub fn fill(&self, name: &str, value: &Control) -> Sound {
        self.map_controls(|c| c.fill(name, value))
    }

    /// Holes without a default take the default of another hole of the same
    /// name, if there is one: a sound's params are known by name, so they
    /// share it.
    pub fn share_defaults(&self) -> Sound {
        let defaults = shared_defaults(self.controls());
        self.map_controls(|c| c.with_defaults(&defaults))
    }

    /// The free params, as docs, merged by name.
    pub fn free_params(&self) -> Vec<ParamDoc> {
        let mut params = Params::default();
        for node in self.sound_nodes() {
            node.free_params(&mut params);
        }
        for c in self.controls() {
            c.free_params(&mut params);
        }
        params.done()
    }

    /// Whether the sound never ends by itself (ignoring controls that might
    /// end it).
    pub fn is_endless(&self) -> bool {
        match self {
            Sound::Input => false,
            Sound::Node(node) => match node.kind() {
                Kind::Wavetable | Kind::Noise => true,
                Kind::Sample => false,
                _ => node.input.as_ref().is_some_and(|i| i.is_endless()),
            },
            Sound::Fit(..) | Sound::Slice(..) => false,
            Sound::Repeat(child, times) => *times == usize::MAX || child.is_endless(),
            Sound::Add(children) | Sound::Seq(children) => children.iter().any(Sound::is_endless),
            Sound::Multiply(a, b) => a.is_endless() && b.is_endless(),
            Sound::Delay(child, _) | Sound::Gain(_, child) => child.is_endless(),
        }
    }

    /// Whether a pattern plays it legato: it has a glide with `:legato`.
    pub fn is_legato(&self) -> bool {
        self.nodes()
            .iter()
            .any(|n| n.kind() == Kind::Glide && n.flag("legato"))
    }

    /// The voice for one note of a pattern: `?note` filled in (with a pitch,
    /// or a `NotePath` for a legato phrase), envelopes released after `gate`
    /// seconds (`None`: never), free-running controls picked up where they
    /// are `since` seconds into the pattern, and glides sliding in `from` the
    /// note before. A sound that would go on forever and has no envelope to
    /// end it is cut off at the gate.
    pub fn for_note(
        &self,
        note: Option<Control>,
        gate: Option<f64>,
        since: f64,
        from: Option<f64>,
    ) -> Sound {
        let mut has_envelope = false;
        let voice = self.map_controls(|c| {
            let c = match &note {
                Some(note) => c.fill("note", note),
                None => c.clone(),
            };
            has_envelope |= c.has_envelope();
            c.for_note(gate, since, from)
        });
        match gate {
            Some(gate) if !has_envelope && voice.is_endless() => Sound::Fit(Box::new(voice), gate),
            _ => voice,
        }
    }

    pub fn instantiate(&self, sample_rate: u32) -> Box<dyn AudioNode> {
        let frames = |seconds: f64| (seconds * sample_rate as f64).round() as usize;
        let all = |children: &[Sound]| {
            children
                .iter()
                .map(|c| c.instantiate(sample_rate))
                .collect()
        };
        match self {
            // Never played (effects without an input are refused), but
            // silence is the honest rendering.
            Sound::Input => Box::new(Add::new(Vec::new())),
            Sound::Node(node) => node.instantiate_sound(sample_rate),
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
            Sound::Add(children) => Box::new(Add::new(all(children))),
            Sound::Seq(children) => Box::new(Seq::new(all(children))),
            Sound::Gain(amount, child) => Box::new(Gain::new(
                amount.param(sample_rate),
                child.instantiate(sample_rate),
            )),
            Sound::Multiply(a, b) => Box::new(Multiply::new(
                a.instantiate(sample_rate),
                b.instantiate(sample_rate),
            )),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
}

impl Op {
    fn symbol(self) -> &'static str {
        match self {
            Op::Add => "+",
            Op::Sub => "-",
            Op::Mul => "*",
            Op::Div => "/",
        }
    }

    pub fn apply(self, a: f64, b: f64) -> f64 {
        match self {
            Op::Add => a + b,
            Op::Sub => a - b,
            Op::Mul => a * b,
            Op::Div if b == 0.0 => 0.0,
            Op::Div => a / b,
        }
    }

    fn function(self) -> fn(f32, f32) -> f32 {
        match self {
            Op::Add => |a, b| a + b,
            Op::Sub => |a, b| a - b,
            Op::Mul => |a, b| a * b,
            Op::Div => |a, b| if b == 0.0 { 0.0 } else { a / b },
        }
    }
}

/// A description of a control signal, the counterpart of `Sound`.
#[derive(Clone)]
pub enum Control {
    Constant(f64),
    /// An envelope, modulation, random or glide.
    Node(Box<Node>),
    Op(Op, Box<Control>, Box<Control>),
    /// The first mapped from 0..1 onto the other two (whole numbers only, if
    /// `whole`).
    Range {
        x: Box<Control>,
        lo: Box<Control>,
        hi: Box<Control>,
        whole: bool,
    },
    Round(Box<Control>),
    /// Scale degrees (rounded) to semitones above a scale's root.
    Scale(Box<Control>, &'static [u8]),
    /// The notes of a legato phrase of a pattern, from step `start` (steps
    /// in seconds), changing on each note.
    NotePath {
        pattern: Arc<Pattern>,
        start: usize,
        step: f64,
    },
    /// A free param, until it's filled. Unfilled, it's its default (`play`
    /// refuses holes without one).
    Hole {
        name: String,
        default: Option<Box<Control>>,
    },
}

impl Control {
    /// The same control with its children rewritten (one level down: `m`
    /// decides whether to go on).
    pub fn try_map<M: Mapper + ?Sized>(&self, m: &mut M) -> Result<Control, M::Error> {
        let mut f = |c: &Control| m.control(c).map(Box::new);
        let m = &mut f;
        Ok(match self {
            Control::Op(op, a, b) => {
                let a = m(a)?;
                Control::Op(*op, a, m(b)?)
            }
            Control::Range { x, lo, hi, whole } => Control::Range {
                x: m(x)?,
                lo: m(lo)?,
                hi: m(hi)?,
                whole: *whole,
            },
            Control::Round(x) => Control::Round(m(x)?),
            Control::Scale(x, steps) => Control::Scale(m(x)?, steps),
            Control::Hole {
                name,
                default: Some(d),
            } => Control::Hole {
                name: name.clone(),
                default: Some(m(d)?),
            },
            Control::Node(node) => Control::Node(Box::new(node.try_map(&mut Children(m))?)),
            other => other.clone(),
        })
    }

    /// The same control with its children replaced by `f(child)`.
    fn map(&self, f: impl FnMut(&Control) -> Control) -> Control {
        let Ok(control) = self.try_map(&mut Controls(f));
        control
    }

    /// The controls this one is made of.
    fn children(&self) -> Vec<&Control> {
        match self {
            Control::Op(_, a, b) => vec![a, b],
            Control::Range { x, lo, hi, .. } => vec![x, lo, hi],
            Control::Round(x) | Control::Scale(x, _) => vec![x],
            Control::Hole {
                default: Some(d), ..
            } => vec![d],
            Control::Node(node) => node.controls().collect(),
            _ => vec![],
        }
    }

    fn nodes<'a>(&'a self, out: &mut Vec<&'a Node>) {
        if let Control::Node(node) = self {
            out.push(node);
        }
        for c in self.children() {
            c.nodes(out);
        }
    }

    pub fn holes(&self, out: &mut Vec<(String, bool)>) {
        match self {
            // A default's own holes aren't the sound's to fill.
            Control::Hole { name, default } => out.push((name.clone(), default.is_some())),
            other => other.children().iter().for_each(|c| c.holes(out)),
        }
    }

    pub fn has_envelope(&self) -> bool {
        matches!(self, Control::Node(n) if n.kind() == Kind::Envelope)
            || self.children().iter().any(|c| c.has_envelope())
    }

    /// The holes called `name` filled with `value`.
    pub fn fill(&self, name: &str, value: &Control) -> Control {
        match self {
            Control::Hole { name: n, .. } if n == name => value.clone(),
            Control::Hole { .. } => self.clone(),
            other => other.map(|c| c.fill(name, value)),
        }
    }

    /// Holes without defaults given the defaults in `defaults`, by name.
    fn with_defaults(&self, defaults: &HashMap<String, Control>) -> Control {
        match self {
            Control::Hole {
                name,
                default: None,
            } => Control::Hole {
                name: name.clone(),
                default: defaults.get(name).cloned().map(Box::new),
            },
            Control::Hole { .. } => self.clone(),
            other => other.map(|c| c.with_defaults(defaults)),
        }
    }

    /// See `Sound::share_defaults`.
    pub fn share_defaults(&self) -> Control {
        self.with_defaults(&shared_defaults([self]))
    }

    fn free_params(&self, out: &mut Params) {
        match self {
            Control::Hole { name, default } => {
                let ty = param_doc(name).map_or(Ty::Control, |p| p.ty);
                let default = default
                    .as_ref()
                    .map_or("none".to_string(), |d| d.describe(ty));
                out.add(name, default);
            }
            Control::Node(node) => {
                node.free_params(out);
                node.controls().for_each(|c| c.free_params(out));
            }
            other => other.children().iter().for_each(|c| c.free_params(out)),
        }
    }

    /// The free params, as docs, merged by name.
    pub fn params(&self) -> Vec<ParamDoc> {
        let mut params = Params::default();
        self.free_params(&mut params);
        params.done()
    }

    /// See `Sound::for_note`.
    fn for_note(&self, gate: Option<f64>, since: f64, from: Option<f64>) -> Control {
        let Control::Node(node) = self else {
            return self.map(|c| c.for_note(gate, since, from));
        };
        let Ok(mut node) = node.try_map(&mut Controls(|c: &Control| c.for_note(gate, since, from)));
        let start = node.since + since;
        match node.kind() {
            Kind::Envelope => {
                let i = node.index("gate");
                if let (true, Some(gate)) = (node.free[i], gate) {
                    node.args[i] = Arg::Seconds(gate);
                }
            }
            Kind::Mod if node.flag("latch") => {
                if let Resolved::Modulation(data) = &node.resolved {
                    return Control::Constant(data.value_after(start, node.times()));
                }
            }
            Kind::Mod if !node.flag("retrig") => node.since = start,
            Kind::Random => {
                let period = node.seconds("every").unwrap_or(1.0);
                let Resolved::Seed(seed) = node.resolved else {
                    unreachable!("an unresolved random")
                };
                if node.flag("latch") {
                    let k = (start / period + 1e-9).floor() as i64;
                    return Control::Constant(noise::random_at(seed, k));
                } else if node.flag("retrig") {
                    // Starting over with the same values every note would be
                    // the same note every time: a new stream per note instead.
                    node.resolved = Resolved::Seed(noise::mix(seed ^ since.to_bits()));
                } else {
                    node.since = start;
                }
            }
            Kind::Glide if !node.flag("legato") => node.from = from,
            _ => {}
        }
        Control::Node(Box::new(node))
    }

    pub fn instantiate(&self, sample_rate: u32) -> Box<dyn ControlNode> {
        let i = |c: &Control| c.instantiate(sample_rate);
        match self {
            Control::Constant(v) => Box::new(control::Constant(*v as f32)),
            Control::Node(node) => node.instantiate_control(sample_rate),
            Control::Op(op, a, b) => Box::new(Combine::new(i(a), i(b), op.function())),
            Control::Range { x, lo, hi, whole } => Box::new(Range::new(i(x), i(lo), i(hi), *whole)),
            Control::Round(x) => Box::new(Map::new(i(x), f32::round)),
            Control::Scale(x, steps) => Box::new(ScaleDegree::new(i(x), steps)),
            Control::NotePath {
                pattern,
                start,
                step,
            } => Box::new(NotePath::new(
                pattern.clone(),
                *start,
                step * sample_rate as f64,
                0.0,
            )),
            Control::Hole { default, .. } => match default {
                Some(default) => i(default),
                None => Box::new(control::Constant(0.0)),
            },
        }
    }

    /// As a parameter of an audio node: constants stay plain numbers.
    pub fn param(&self, sample_rate: u32) -> Param {
        match self {
            Control::Constant(v) => Param::Const(*v as f32),
            Control::Hole {
                default: Some(default),
                ..
            } => default.param(sample_rate),
            other => Param::signal(other.instantiate(sample_rate)),
        }
    }

    /// The control as it would be written, roughly; a param of type `ty`
    /// shows constants as notes or frequencies.
    pub fn describe(&self, ty: Ty) -> String {
        match self {
            Control::Constant(v) => match ty {
                Ty::Pitch => note_name(*v),
                Ty::Cutoff => note_to_hz_text(*v),
                _ => trim(*v),
            },
            Control::Hole { name, .. } => format!("?{name}"),
            Control::Node(node) => match node.kind() {
                Kind::Envelope => format!("envelope(\"{}\")", node.str("env")),
                Kind::Mod => format!("mod(\"{}\")", node.str("mod")),
                Kind::Random => "random".to_string(),
                Kind::Glide => format!("glide({})", node.control("target").describe(ty)),
                kind => format!("{kind:?}").to_lowercase(),
            },
            Control::Op(op, a, b) => {
                let side = |c: &Control| match c {
                    Control::Op(inner, ..)
                        if matches!(op, Op::Mul | Op::Div)
                            && matches!(inner, Op::Add | Op::Sub) =>
                    {
                        format!("({})", c.describe(ty))
                    }
                    _ => c.describe(ty),
                };
                // Only the pitch side of `?note + 12` is a pitch.
                let b = match (op, &**b) {
                    (Op::Add | Op::Sub, Control::Constant(v)) => trim(*v),
                    _ => side(b),
                };
                format!("{} {} {b}", side(a), op.symbol())
            }
            Control::Range { x, lo, hi, .. } => format!(
                "{}.range({}, {})",
                x.describe(Ty::Control),
                lo.describe(ty),
                hi.describe(ty)
            ),
            Control::Round(x) => format!("{}.round", x.describe(ty)),
            Control::Scale(x, _) => format!("{}.scale(...)", x.describe(Ty::Control)),
            Control::NotePath { .. } => "the pattern's notes".to_string(),
        }
    }
}

/// A control node's params rewritten with a function that rewrites boxed
/// controls (see `Control::try_map`).
struct Children<'a, E>(&'a mut dyn FnMut(&Control) -> Result<Box<Control>, E>);

impl<E> Mapper for Children<'_, E> {
    type Error = E;

    fn control(&mut self, c: &Control) -> Result<Control, E> {
        (self.0)(c).map(|b| *b)
    }
}

/// The first default of each hole name among `controls`.
fn shared_defaults<'a>(
    controls: impl IntoIterator<Item = &'a Control>,
) -> HashMap<String, Control> {
    fn walk(c: &Control, out: &mut HashMap<String, Control>) {
        match c {
            Control::Hole {
                name,
                default: Some(d),
            } => {
                out.entry(name.clone()).or_insert_with(|| (**d).clone());
            }
            Control::Hole { .. } => {}
            other => other.children().into_iter().for_each(|c| walk(c, out)),
        }
    }
    let mut out = HashMap::new();
    for c in controls {
        walk(c, &mut out);
    }
    out
}

/// Free params collected for docs, merged by name.
#[derive(Default)]
struct Params(Vec<(String, String, usize)>);

impl Params {
    fn add(&mut self, name: &str, default: String) {
        match self.0.iter_mut().find(|(n, ..)| n == name) {
            Some((_, d, count)) => {
                *count += 1;
                if d == "none" {
                    *d = default;
                }
            }
            None => self.0.push((name.to_string(), default, 1)),
        }
    }

    fn done(self) -> Vec<ParamDoc> {
        self.0
            .into_iter()
            .map(|(name, default, count)| {
                let mut doc = param_doc(&name).map_or_else(
                    || "a param of its own (made with ?)".to_string(),
                    |p| p.doc.to_string(),
                );
                if count > 1 {
                    doc = format!("{count}×: {doc}");
                }
                ParamDoc { name, default, doc }
            })
            .collect()
    }
}
