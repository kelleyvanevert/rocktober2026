//! Turns parsed expressions into commands for the audio thread (application thread).
//!
//! Evaluation produces `Sound` values: cheap, immutable *descriptions* of sounds.
//! Only `play` turns a description into a live, stateful node graph. Keeping those
//! two apart means a description can be stored, reused or played many times at
//! once, each play getting its own fresh playback state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::engine::Command;
use crate::lang::{Error, Expr, Spanned};
use crate::nodes::{Fit, Node, Repeat, SampleData, Sampler};
use crate::sample;

/// Fade-out applied where `fit` cuts a sound off. A few ms is enough to remove the
/// click without audibly softening a transient.
const FIT_FADE_SECONDS: f64 = 0.003;

#[derive(Clone)]
pub enum Sound {
    Sample(Arc<SampleData>),
    Fit(Box<Sound>, f64),
    Repeat(Box<Sound>, usize),
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
            Sound::Repeat(child, times) => {
                Box::new(Repeat::new(child.instantiate(sample_rate), *times))
            }
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

pub struct Evaluator {
    sample_rate: u32,
    sample_dirs: Vec<PathBuf>,
    /// Decoded samples, so each file is only loaded once. Holding an `Arc` here
    /// also guarantees the last reference to a sample is never dropped on the
    /// audio thread.
    cache: HashMap<PathBuf, Arc<SampleData>>,
}

impl Evaluator {
    pub fn new(sample_rate: u32, sample_dirs: Vec<PathBuf>) -> Self {
        Self {
            sample_rate,
            sample_dirs,
            cache: HashMap::new(),
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

        let expected = match name {
            "sample" | "play" => 1,
            "fit" | "repeat" => 2,
            "stop" => 0,
            _ => return fail(format!("unknown function '{name}'")),
        };
        if values.len() != expected {
            return fail(format!(
                "{name} takes {expected} argument(s), got {}",
                values.len()
            ));
        }

        let mut values = values.into_iter();
        let mut next = || values.next().unwrap();
        let wrong = |v: &Value, want: &str, n: usize| {
            Err(Error {
                pos: args[n].pos,
                msg: format!("{name}: expected {want}, got {}", v.type_name()),
            })
        };

        match name {
            "sample" => match next() {
                Value::Str(path) => match self.load(&path) {
                    Ok(data) => Ok(Value::Sound(Sound::Sample(data))),
                    Err(msg) => fail(msg),
                },
                v => wrong(&v, "a file name", 0),
            },
            "fit" => match (next(), next()) {
                (Value::Sound(s), Value::Duration(d)) => {
                    Ok(Value::Sound(Sound::Fit(Box::new(s), d)))
                }
                (Value::Sound(_), v) => wrong(&v, "a duration (like 500ms)", 1),
                (v, _) => wrong(&v, "a sound", 0),
            },
            "repeat" => match (next(), next()) {
                (Value::Sound(s), Value::Num(n)) if n >= 0.0 && n.fract() == 0.0 => {
                    Ok(Value::Sound(Sound::Repeat(Box::new(s), n as usize)))
                }
                (Value::Sound(_), v) => wrong(&v, "a whole number", 1),
                (v, _) => wrong(&v, "a sound", 0),
            },
            "play" => match next() {
                Value::Sound(s) => {
                    commands.push(Command::Play(s.instantiate(self.sample_rate)));
                    Ok(Value::Nothing)
                }
                v => wrong(&v, "a sound", 0),
            },
            "stop" => {
                commands.push(Command::StopAll);
                Ok(Value::Nothing)
            }
            _ => unreachable!(),
        }
    }

    fn load(&mut self, name: &str) -> Result<Arc<SampleData>, String> {
        let path = self
            .sample_dirs
            .iter()
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
            .ok_or_else(|| format!("sample '{name}' not found in {:?}", self.sample_dirs))?;
        if let Some(data) = self.cache.get(&path) {
            return Ok(data.clone());
        }
        let data = Arc::new(sample::load(Path::new(&path))?);
        self.cache.insert(path, data.clone());
        Ok(data)
    }
}
