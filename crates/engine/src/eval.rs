//! Turns parsed expressions into commands for the audio thread (application thread).
//!
//! Evaluation produces `Sound` values: cheap, immutable *descriptions* of sounds.
//! Only `play` turns a description into a live, stateful node graph. Keeping those
//! two apart means a description can be stored, reused or played many times at
//! once, each play getting its own fresh playback state.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::engine::Command;
use crate::lang::{Error, Expr, Spanned};
use crate::nodes::{Add, Delay, Fit, Gain, Limit, Node, Repeat, SampleData, Sampler, Seq, Slice};
use crate::reverb::{self, Impulse, MAX_IR_SECONDS, Reverb};
use crate::sample;

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
    Gain(f64, Box<Sound>),
    Limit(f64, Box<Sound>),
    /// Impulse response and wet/dry mix.
    Reverb(Box<Sound>, Arc<Impulse>, f64),
}

impl Sound {
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
            Sound::Gain(amount, child) => {
                Box::new(Gain::new(*amount as f32, child.instantiate(sample_rate)))
            }
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
        }
    }
}

enum Value {
    Sound(Sound),
    Str(String),
    Num(f64),
    Duration(f64),
    Nothing,
}

impl Value {
    fn type_name(&self) -> &'static str {
        match self {
            Value::Sound(_) => "a sound",
            Value::Str(_) => "a string",
            Value::Num(_) => "a number",
            Value::Duration(_) => "a duration",
            Value::Nothing => "nothing",
        }
    }
}

/// A decoded part of a file, as (start, end) seconds in `f64::to_bits` form so it
/// can be a map key.
type Window = (u64, Option<u64>);

pub struct Evaluator {
    sample_rate: u32,
    sample_dirs: Vec<PathBuf>,
    /// Decoded samples, so each file is only loaded once. Holding an `Arc` here
    /// also guarantees the last reference to a sample is never dropped on the
    /// audio thread.
    cache: HashMap<(PathBuf, Window), Arc<SampleData>>,
    /// Prepared preset impulse responses, by name.
    spaces: HashMap<String, Arc<Impulse>>,
}

impl Evaluator {
    pub fn new(sample_rate: u32, sample_dirs: Vec<PathBuf>) -> Self {
        Self {
            sample_rate,
            sample_dirs,
            cache: HashMap::new(),
            spaces: HashMap::new(),
        }
    }

    /// Evaluate a program, returning the commands it wants sent to the audio thread.
    /// Nothing is sent if any statement fails, so a typo never half-plays a line.
    pub fn run(&mut self, program: &[Spanned]) -> Result<Vec<Command>, Error> {
        let mut commands = Vec::new();
        for stmt in program {
            self.eval(stmt, &mut commands)?;
        }
        Ok(commands)
    }

    fn eval(&mut self, e: &Spanned, commands: &mut Vec<Command>) -> Result<Value, Error> {
        let fail = |msg: String| Err(Error { pos: e.pos, msg });
        let (name, args) = match &e.expr {
            Expr::Str(s) => return Ok(Value::Str(s.clone())),
            Expr::Num(n) => return Ok(Value::Num(*n)),
            Expr::Duration(d) => return Ok(Value::Duration(*d)),
            Expr::Call { name, args } => (name.as_str(), args),
        };

        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            values.push(self.eval(arg, commands)?);
        }

        let (min_args, max_args) = match name {
            "stop" => (0, 0),
            "play" => (1, 1),
            "sample" => (1, 3),
            "fit" | "repeat" | "gain" | "delay" => (2, 2),
            "slice" => (3, 3),
            "limit" => (1, 2),
            "reverb" => (2, 3),
            "add" | "seq" => (0, usize::MAX),
            _ => return fail(format!("unknown function '{name}'")),
        };
        if values.len() < min_args || values.len() > max_args {
            let expected = match (min_args, max_args) {
                (a, b) if a == b => format!("{a}"),
                (a, b) => format!("{a} or {b}"),
            };
            return fail(format!(
                "{name} takes {expected} argument(s), got {}",
                values.len()
            ));
        }

        let wrong = |v: &Value, want: &str, n: usize| {
            Err(Error {
                pos: args[n].pos,
                msg: format!("{name}: expected {want}, got {}", v.type_name()),
            })
        };
        let sound = |s: Sound| Ok(Value::Sound(s));

        match (name, values.as_slice()) {
            ("sample", [Value::Str(path), rest @ ..]) => {
                let start = match rest.first() {
                    None => 0.0,
                    Some(Value::Duration(d)) => *d,
                    Some(v) => return wrong(v, "a start time (like 0:11:188)", 1),
                };
                let end = match rest.get(1) {
                    None => None,
                    Some(Value::Duration(d)) if *d > start => Some(*d),
                    Some(Value::Duration(_)) => {
                        return wrong(&rest[1], "an end after the start", 2);
                    }
                    Some(v) => return wrong(v, "an end time (like 0:12:625)", 2),
                };
                match self.load(path, start, end) {
                    Ok(data) => sound(Sound::Sample(data)),
                    Err(msg) => fail(msg),
                }
            }
            ("sample", [v, ..]) => wrong(v, "a file name", 0),

            ("fit", [Value::Sound(s), Value::Duration(d)]) => {
                sound(Sound::Fit(Box::new(s.clone()), *d))
            }
            ("fit", [Value::Sound(_), v]) => wrong(v, "a duration (like 500ms)", 1),
            ("fit", [v, _]) => wrong(v, "a sound", 0),

            (
                "slice",
                [
                    Value::Sound(s),
                    Value::Duration(start),
                    Value::Duration(end),
                ],
            ) => {
                if end <= start {
                    return wrong(&values[2], "an end after the start", 2);
                }
                sound(Sound::Slice(Box::new(s.clone()), *start, *end))
            }
            ("slice", [Value::Sound(_), Value::Duration(_), v]) => {
                wrong(v, "an end time (like 0:12:625)", 2)
            }
            ("slice", [Value::Sound(_), v, _]) => wrong(v, "a start time (like 0:11:188)", 1),
            ("slice", [v, ..]) => wrong(v, "a sound", 0),

            // `repeat(inf)` repeats forever (well, usize::MAX times).
            ("delay", [Value::Sound(s), Value::Duration(d)]) => {
                sound(Sound::Delay(Box::new(s.clone()), *d))
            }
            ("delay", [Value::Sound(_), v]) => wrong(v, "a duration (like 12s)", 1),
            ("delay", [v, _]) => wrong(v, "a sound", 0),

            ("repeat", [Value::Sound(s), Value::Num(n)])
                if *n == f64::INFINITY || (*n >= 0.0 && n.fract() == 0.0) =>
            {
                let times = if n.is_infinite() {
                    usize::MAX
                } else {
                    *n as usize
                };
                sound(Sound::Repeat(Box::new(s.clone()), times))
            }
            ("repeat", [Value::Sound(_), v]) => wrong(v, "a whole number or inf", 1),
            ("repeat", [v, _]) => wrong(v, "a sound", 0),

            ("add" | "seq", _) => {
                let mut children = Vec::with_capacity(values.len());
                for (n, v) in values.iter().enumerate() {
                    match v {
                        Value::Sound(s) => children.push(s.clone()),
                        v => return wrong(v, "a sound", n),
                    }
                }
                sound(match name {
                    "add" => Sound::Add(children),
                    _ => Sound::Seq(children),
                })
            }

            ("gain", [Value::Sound(s), Value::Num(amount)]) => {
                sound(Sound::Gain(*amount, Box::new(s.clone())))
            }
            ("gain", [Value::Sound(_), v]) => wrong(v, "an amount (like 0.5 or -6db)", 1),
            ("gain", [v, _]) => wrong(v, "a sound", 0),

            ("limit", [Value::Sound(s)]) => sound(Sound::Limit(LIMIT_CEILING, Box::new(s.clone()))),
            ("limit", [Value::Sound(s), Value::Num(ceiling)]) if *ceiling > 0.0 => {
                sound(Sound::Limit(*ceiling, Box::new(s.clone())))
            }
            ("limit", [Value::Sound(_), v]) => wrong(v, "a positive ceiling (like 0.9 or -1db)", 1),
            ("limit", [v, ..]) => wrong(v, "a sound", 0),

            ("reverb", [Value::Sound(s), space, rest @ ..]) => {
                let mix = match rest.first() {
                    None => REVERB_MIX,
                    Some(Value::Num(m)) if (0.0..=1.0).contains(m) => *m,
                    Some(v) => return wrong(v, "a mix between 0 and 1", 2),
                };
                let impulse = match space {
                    Value::Str(name) => match self.space(name) {
                        Some(impulse) => impulse,
                        None => {
                            let names = reverb::preset_names().join(", ");
                            return Err(Error {
                                pos: args[1].pos,
                                msg: format!(
                                    "reverb: unknown space \"{name}\" (try {names}, or a sound)"
                                ),
                            });
                        }
                    },
                    Value::Sound(ir) => Arc::new(Impulse::new(self.render(ir, MAX_IR_SECONDS))),
                    v => return wrong(v, "a space name or a sound", 1),
                };
                sound(Sound::Reverb(Box::new(s.clone()), impulse, mix))
            }
            ("reverb", [v, ..]) => wrong(v, "a sound", 0),

            ("play", [Value::Sound(s)]) => {
                commands.push(Command::Play(s.instantiate(self.sample_rate)));
                Ok(Value::Nothing)
            }
            ("play", [v]) => wrong(v, "a sound", 0),

            ("stop", []) => {
                commands.push(Command::StopAll);
                Ok(Value::Nothing)
            }
            _ => unreachable!(),
        }
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
        let path = self
            .sample_dirs
            .iter()
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
            .ok_or_else(|| format!("sample '{name}' not found in {:?}", self.sample_dirs))?;
        let key = (path, (start.to_bits(), end.map(f64::to_bits)));
        if let Some(data) = self.cache.get(&key) {
            return Ok(data.clone());
        }
        let data = Arc::new(sample::load_range(&key.0, start, end)?);
        self.cache.insert(key, data.clone());
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::parse;
    use crate::nodes::Frame;

    fn evaluator() -> Evaluator {
        let samples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples");
        Evaluator::new(48_000, vec![samples])
    }

    /// Evaluate `src` (which must `play` exactly one sound) and render it.
    fn render(src: &str) -> Vec<Frame> {
        let mut commands = evaluator().run(&parse(src).unwrap()).unwrap();
        let Some(Command::Play(mut node)) = commands.pop() else {
            panic!("nothing played")
        };
        let mut all = Vec::new();
        let mut buf = vec![[0.0; 2]; 512];
        loop {
            let n = node.process(&mut buf);
            all.extend_from_slice(&buf[..n]);
            if n < buf.len() {
                return all;
            }
        }
    }

    fn peak(frames: &[Frame]) -> f32 {
        frames.iter().flatten().fold(0.0, |m, s| m.max(s.abs()))
    }

    fn error(src: &str) -> String {
        match evaluator().run(&parse(src).unwrap()) {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.msg,
        }
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
            "gain: expected an amount (like 0.5 or -6db), got a sound"
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
        assert_eq!(a, b);
    }
}
