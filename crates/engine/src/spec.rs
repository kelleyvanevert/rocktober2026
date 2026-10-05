//! The built-in nodes and functions: their params, defaults and
//! documentation. The evaluator builds nodes from these specs, and the editor
//! shows them as documentation.
//!
//! Every param of every node has a name and a default (except a few that
//! name what the node is made of, like a sample's file), and stays a free
//! param until it's set with `:name(value)`. A param's name means the same
//! thing on every node that has it (`note`, `mix`, `retrig`, ...), since
//! setting a name sets it everywhere in a composed sound.

/// What kind of thing a node is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// Makes sound.
    Source,
    /// Changes the sound it's applied to: `sound * lowpass`.
    Effect,
    /// A signal that shapes params or amplitudes: `sound * envelope("pluck")`.
    Control,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Source => "source",
            Role::Effect => "effect",
            Role::Control => "control",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Wavetable,
    Noise,
    Sample,
    Lowpass,
    Highpass,
    Bandpass,
    Pan,
    Spread,
    Drive,
    Echo,
    Pingpong,
    Reverb,
    Duck,
    Limit,
    Envelope,
    Mod,
    Random,
    Glide,
}

/// What a param accepts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ty {
    /// A number, a pitch or a control: it can change while it plays, and be
    /// linked to other params (`?note + 12`).
    Control,
    /// A control that's a pitch (shown as a note name).
    Pitch,
    /// A pitch used as a filter frequency: a plain number above 140 is
    /// surely meant in Hz, and is refused.
    Cutoff,
    /// A plain number, fixed when the code runs.
    Num,
    Str,
    /// A duration (or a length in beats, at the tempo when it's set).
    Seconds,
    /// A whole number of times, or `inf`.
    Count,
    /// A switch: 0 or 1.
    Flag,
    /// A reverb space: a preset's name, or a sound to use as the impulse
    /// response.
    Space,
}

impl Ty {
    /// Whether values of this type are controls (and so can be linked).
    pub fn is_control(self) -> bool {
        matches!(self, Ty::Control | Ty::Pitch | Ty::Cutoff)
    }

    /// What it expects, for an error message.
    pub fn expected(self) -> &'static str {
        match self {
            Ty::Control => "a number or a control",
            Ty::Pitch => "a pitch (like c3, 440hz or ?note + 12)",
            Ty::Cutoff => "a frequency (like 800hz, c6 or ?note + 24)",
            Ty::Num => "a number",
            Ty::Str => "a string",
            Ty::Seconds => "a duration (like 300ms or 0.5b)",
            Ty::Count => "a whole number or inf",
            Ty::Flag => "0 or 1",
            Ty::Space => "a space (like \"hall\") or a sound",
        }
    }
}

/// A param's default.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Def {
    /// There's none: it has to be given.
    Required,
    /// Not set, which means something particular (see the param's doc).
    Unset,
    Num(f64),
    /// A note number.
    Note(f64),
    Hz(f64),
    Str(&'static str),
    Beats(f64),
    Seconds(f64),
    Flag(bool),
}

impl Def {
    /// The default as it would be written.
    pub fn text(self) -> String {
        match self {
            Def::Required => "required".into(),
            Def::Unset => "unset".into(),
            Def::Num(n) if n == f64::INFINITY => "inf".into(),
            Def::Num(n) => trim(n),
            Def::Note(n) => crate::lang::note_name(n),
            Def::Hz(hz) if hz >= 1000.0 => format!("{}khz", trim(hz / 1000.0)),
            Def::Hz(hz) => format!("{}hz", trim(hz)),
            Def::Str(s) => format!("\"{s}\""),
            Def::Beats(b) => format!("{}b", trim(b)),
            Def::Seconds(s) if s < 1.0 => format!("{}ms", trim(s * 1000.0)),
            Def::Seconds(s) => format!("{}s", trim(s)),
            Def::Flag(b) => (b as u8).to_string(),
        }
    }
}

/// A number without needless decimals: `0.3`, `4`, `0.891`.
pub fn trim(n: f64) -> String {
    let s = format!("{:.3}", n);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.into() }
}

#[derive(Clone, Copy)]
pub struct ParamSpec {
    pub name: &'static str,
    pub ty: Ty,
    pub default: Def,
    pub doc: &'static str,
}

const fn p(name: &'static str, ty: Ty, default: Def, doc: &'static str) -> ParamSpec {
    ParamSpec {
        name,
        ty,
        default,
        doc,
    }
}

pub struct Spec {
    pub name: &'static str,
    pub kind: Kind,
    pub role: Role,
    pub summary: &'static str,
    pub params: &'static [ParamSpec],
    /// Params this name fixes (`sine` is a wavetable with table "basic" at
    /// pos 0): they aren't free.
    pub presets: &'static [(&'static str, Def)],
    pub examples: &'static [&'static str],
}

impl Spec {
    pub fn param(&self, name: &str) -> Option<(usize, &'static ParamSpec)> {
        self.params.iter().enumerate().find(|(_, p)| p.name == name)
    }

    /// The params that can be set: all but the presets.
    pub fn free_params(&self) -> impl Iterator<Item = &'static ParamSpec> + '_ {
        self.params
            .iter()
            .filter(|p| !self.presets.iter().any(|(name, _)| *name == p.name))
    }
}

const WAVETABLE: &[ParamSpec] = &[
    p(
        "table",
        Ty::Str,
        Def::Str("basic"),
        "basic (sine → triangle → saw → square), sine-square, sine-saw, bright, pulse, or a wav in wavetables/",
    ),
    p(
        "pos",
        Ty::Control,
        Def::Num(0.0),
        "0 to 1: where in the table",
    ),
    p(
        "warp",
        Ty::Control,
        Def::Num(0.0),
        "0 to 1: phase distortion, bunching the cycle up",
    ),
    p(
        "note",
        Ty::Pitch,
        Def::Note(48.0),
        "the pitch: c3, 440hz, or ?note + 12. A pattern plays its notes here",
    ),
];

const FILTER_RES: ParamSpec = p("res", Ty::Control, Def::Num(0.0), "resonance, 0 to 1");

const ECHO: &[ParamSpec] = &[
    p("time", Ty::Seconds, Def::Beats(0.75), "the delay time"),
    p(
        "feedback",
        Ty::Control,
        Def::Num(0.5),
        "0 to 1: how much of each repeat repeats",
    ),
    p(
        "mix",
        Ty::Control,
        Def::Num(0.35),
        "0 (dry) to 1 (only the effect)",
    ),
    p(
        "low",
        Ty::Cutoff,
        Def::Hz(80.0),
        "the repeats lose what's below this",
    ),
    p(
        "high",
        Ty::Cutoff,
        Def::Hz(10_000.0),
        "the repeats lose what's above this",
    ),
];

const CLOCK: [ParamSpec; 2] = [
    p(
        "retrig",
        Ty::Flag,
        Def::Flag(false),
        "1: start over on every note",
    ),
    p(
        "latch",
        Ty::Flag,
        Def::Flag(false),
        "1: take its value when a note starts and hold it",
    ),
];

const fn wave(
    name: &'static str,
    presets: &'static [(&'static str, Def)],
    summary: &'static str,
) -> Spec {
    Spec {
        name,
        kind: Kind::Wavetable,
        role: Role::Source,
        summary,
        params: WAVETABLE,
        presets,
        examples: &[],
    }
}

const fn filter(
    name: &'static str,
    kind: Kind,
    params: &'static [ParamSpec],
    summary: &'static str,
) -> Spec {
    Spec {
        name,
        kind,
        role: Role::Effect,
        summary,
        params,
        presets: &[],
        examples: &[],
    }
}

const LOWPASS: &[ParamSpec] = &[
    p(
        "freq",
        Ty::Cutoff,
        Def::Hz(2000.0),
        "the cutoff: 800hz, c6, ?note + 24",
    ),
    FILTER_RES,
];
const HIGHPASS: &[ParamSpec] = &[
    p(
        "freq",
        Ty::Cutoff,
        Def::Hz(200.0),
        "the cutoff: 800hz, c6, ?note + 24",
    ),
    FILTER_RES,
];
const BANDPASS: &[ParamSpec] = &[
    p(
        "freq",
        Ty::Cutoff,
        Def::Hz(1000.0),
        "the center: 800hz, c6, ?note + 24",
    ),
    FILTER_RES,
];

pub static SPECS: &[Spec] = &[
    Spec {
        name: "wavetable",
        kind: Kind::Wavetable,
        role: Role::Source,
        summary: "An oscillator reading through a table of single-cycle waves. With its defaults it's a sine.",
        params: WAVETABLE,
        presets: &[],
        examples: &[
            "wavetable:table(\"sine-saw\"):pos(0.6):note(g2)",
            "wavetable:pos(mod(\"sweep\"):repeat(inf))",
        ],
    },
    wave(
        "sine",
        &[("table", Def::Str("basic")), ("pos", Def::Num(0.0))],
        "A sine wave: a wavetable, table \"basic\" at pos 0.",
    ),
    wave(
        "triangle",
        &[("table", Def::Str("basic")), ("pos", Def::Num(1.0 / 3.0))],
        "A triangle wave: a wavetable, table \"basic\" at pos 0.33.",
    ),
    wave(
        "saw",
        &[("table", Def::Str("basic")), ("pos", Def::Num(2.0 / 3.0))],
        "A saw wave: a wavetable, table \"basic\" at pos 0.67.",
    ),
    wave(
        "square",
        &[("table", Def::Str("basic")), ("pos", Def::Num(1.0))],
        "A square wave: a wavetable, table \"basic\" at pos 1.",
    ),
    Spec {
        name: "noise",
        kind: Kind::Noise,
        role: Role::Source,
        summary: "Noise, forever. Mono, like the oscillators.",
        params: &[p(
            "color",
            Ty::Str,
            Def::Str("white"),
            "white (flat), pink (even per octave), brown (a rumble), blue or violet (hiss)",
        )],
        presets: &[],
        examples: &["noise(\"pink\") * bandpass:freq(2khz):res(0.6)"],
    },
    Spec {
        name: "sample",
        kind: Kind::Sample,
        role: Role::Source,
        summary: "A sample from the .rock file's samples/. Put the cursor on it to see (or add) it.",
        params: &[
            p(
                "file",
                Ty::Str,
                Def::Required,
                "its name, like \"kick.mp3\"",
            ),
            p(
                "start",
                Ty::Seconds,
                Def::Seconds(0.0),
                "where it starts in the file (like 0:11:188)",
            ),
            p(
                "end",
                Ty::Seconds,
                Def::Unset,
                "where it ends in the file (unset: at the end)",
            ),
        ],
        presets: &[],
        examples: &[
            "sample(\"kick.mp3\")",
            "sample(\"talk.wav\", 0:11:188, 0:12:625)",
        ],
    },
    filter(
        "lowpass",
        Kind::Lowpass,
        LOWPASS,
        "A lowpass filter (12 dB/octave): lets the lows through.",
    ),
    filter(
        "highpass",
        Kind::Highpass,
        HIGHPASS,
        "A highpass filter (12 dB/octave): lets the highs through.",
    ),
    filter(
        "bandpass",
        Kind::Bandpass,
        BANDPASS,
        "A bandpass filter: lets a band around freq through.",
    ),
    Spec {
        name: "pan",
        kind: Kind::Pan,
        role: Role::Effect,
        summary: "Places the sound between the speakers.",
        params: &[p(
            "side",
            Ty::Control,
            Def::Num(0.0),
            "-1 (left) to 1 (right)",
        )],
        presets: &[],
        examples: &["sound * pan:side(-0.5)"],
    },
    Spec {
        name: "spread",
        kind: Kind::Spread,
        role: Role::Effect,
        summary: "A stereo chorus: makes a sound wide.",
        params: &[p("width", Ty::Control, Def::Num(0.5), "0 to 1")],
        presets: &[],
        examples: &["pad * spread:width(0.8)"],
    },
    Spec {
        name: "drive",
        kind: Kind::Drive,
        role: Role::Effect,
        summary: "Saturation: rounds off the peaks, adding overtones (what makes a bass audible on small speakers).",
        params: &[p(
            "power",
            Ty::Control,
            Def::Num(4.0),
            "the gain into the curve (4 is 12db)",
        )],
        presets: &[],
        examples: &["bass * drive:power(6)"],
    },
    Spec {
        name: "echo",
        kind: Kind::Echo,
        role: Role::Effect,
        summary: "A stereo delay with feedback; the repeats thin out and darken.",
        params: ECHO,
        presets: &[],
        examples: &["blip * echo:time(0.75b):feedback(0.6)"],
    },
    Spec {
        name: "pingpong",
        kind: Kind::Pingpong,
        role: Role::Effect,
        summary: "An echo whose repeats bounce between left and right.",
        params: ECHO,
        presets: &[],
        examples: &["blip * pingpong:time(0.75b):low(300hz):high(3khz)"],
    },
    Spec {
        name: "reverb",
        kind: Kind::Reverb,
        role: Role::Effect,
        summary: "Puts the sound in a space.",
        params: &[
            p(
                "space",
                Ty::Space,
                Def::Str("hall"),
                "a preset (small_room, dark_room, plate, hall, cathedral, ...) or a sound to use as the space",
            ),
            p(
                "mix",
                Ty::Num,
                Def::Num(0.3),
                "0 (dry) to 1 (only the effect)",
            ),
        ],
        presets: &[],
        examples: &[
            "kick * reverb(\"plate\"):mix(0.2)",
            "kick * reverb(sample(\"room.wav\"))",
        ],
    },
    Spec {
        name: "duck",
        kind: Kind::Duck,
        role: Role::Effect,
        summary: "Turns the sound down while what plays in another slot sounds (sidechaining).",
        params: &[
            p("key", Ty::Str, Def::Str("kick"), "the slot to duck under"),
            p("depth", Ty::Control, Def::Num(0.8), "0 to 1: how far down"),
            p(
                "release",
                Ty::Seconds,
                Def::Seconds(0.15),
                "how long it takes to come back up",
            ),
        ],
        presets: &[],
        examples: &["pad * duck(\"kick\"):depth(0.9)"],
    },
    Spec {
        name: "limit",
        kind: Kind::Limit,
        role: Role::Effect,
        summary: "A lookahead limiter: keeps the peaks below the ceiling.",
        params: &[p(
            "ceiling",
            Ty::Num,
            Def::Num(0.891),
            "a factor (or decibels, like -1db)",
        )],
        presets: &[],
        examples: &["mix * limit"],
    },
    Spec {
        name: "envelope",
        kind: Kind::Envelope,
        role: Role::Control,
        summary: "An ADSR envelope from the .rock file's envelopes/. Put the cursor on it to edit it.",
        params: &[
            p("env", Ty::Str, Def::Required, "its name, like \"pluck\""),
            p(
                "gate",
                Ty::Seconds,
                Def::Unset,
                "released after this long (unset: at the end of each note, or never)",
            ),
        ],
        presets: &[],
        examples: &[
            "sound * envelope(\"pluck\")",
            "envelope(\"hit\"):gate(60ms)",
        ],
    },
    Spec {
        name: "mod",
        kind: Kind::Mod,
        role: Role::Control,
        summary: "A modulation (0 to 1 over time) from the .rock file's modulations/. Put the cursor on it to edit it. It runs on through a pattern's notes.",
        params: &[
            p("mod", Ty::Str, Def::Required, "its name, like \"sweep\""),
            p(
                "repeat",
                Ty::Count,
                Def::Num(1.0),
                "how often it plays (inf loops it, like an LFO); then it holds",
            ),
            CLOCK[0],
            CLOCK[1],
        ],
        presets: &[],
        examples: &[
            "mod(\"swoop\"):retrig * 9 - 1",
            "wavetable:pos(mod(\"lfo\"):repeat(inf))",
        ],
    },
    Spec {
        name: "random",
        kind: Kind::Random,
        role: Role::Control,
        summary: "A new random value (0 to 1) every so often, held in between. Like a modulation, it runs on through a pattern's notes.",
        params: &[
            p(
                "every",
                Ty::Seconds,
                Def::Beats(1.0),
                "how often it changes",
            ),
            CLOCK[0],
            CLOCK[1],
        ],
        presets: &[],
        examples: &["random(0.25b):latch.range(-0.5, 6.5).scale(\"minor\", f4)"],
    },
    Spec {
        name: "glide",
        kind: Kind::Glide,
        role: Role::Control,
        summary: "Slides to where its target goes. On a pattern's ?note, each note slides from the one before.",
        params: &[
            p(
                "target",
                Ty::Control,
                Def::Required,
                "what it follows, like ?note",
            ),
            p(
                "dur",
                Ty::Seconds,
                Def::Seconds(0.1),
                "how long a slide takes",
            ),
            p(
                "legato",
                Ty::Flag,
                Def::Flag(false),
                "1: a pattern's notes up to a rest are one voice, sliding along (like a mono synth)",
            ),
        ],
        presets: &[],
        examples: &["lead:note(glide(?note):dur(300ms))"],
    },
];

pub fn spec(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.name == name)
}

/// The doc of the first param anywhere called `name`.
pub fn param_doc(name: &str) -> Option<&'static ParamSpec> {
    SPECS.iter().find_map(|s| s.param(name).map(|(_, p)| p))
}

/// A built-in function (not a node).
pub struct FnDoc {
    pub name: &'static str,
    pub usage: &'static [&'static str],
    pub summary: &'static str,
}

pub static FUNCTIONS: &[FnDoc] = &[
    FnDoc {
        name: "play",
        usage: &[
            "sound.play()",
            "sound.play(\"slot\")",
            "sound.play(at 4b)",
            "pattern.play(sound, \"slot\", at 5b + 2)",
        ],
        summary: "Plays a sound (or a pattern of notes with a sound): right away, or on the next point of a grid. A named slot replaces what played there.",
    },
    FnDoc {
        name: "notes",
        usage: &["notes(\"c2 e2 _ g2 . x\", 0.25b)"],
        summary: "A pattern: notes, x (a hit without a pitch), . or ~ (a rest), _ (hold the note). Each note sets the sound's free ?note.",
    },
    FnDoc {
        name: "fit",
        usage: &["sound.fit(500ms)"],
        summary: "Cuts the sound off (with a short fade), or pads it with silence, to a length.",
    },
    FnDoc {
        name: "slice",
        usage: &["sound.slice(0:00:100, 0:00:300)"],
        summary: "The part of a sound between two times.",
    },
    FnDoc {
        name: "delay",
        usage: &["sound.delay(1b)"],
        summary: "Starts the sound later.",
    },
    FnDoc {
        name: "repeat",
        usage: &["sound.repeat(4)", "sound.repeat(inf)", "pattern.repeat(2)"],
        summary: "Plays a sound (or a pattern) a number of times in a row.",
    },
    FnDoc {
        name: "add",
        usage: &["add(a, b, c)", "a + b"],
        summary: "Mixes sounds, as long as the longest; or adds controls or numbers.",
    },
    FnDoc {
        name: "seq",
        usage: &["seq(a, b, c)"],
        summary: "Plays sounds one after another.",
    },
    FnDoc {
        name: "bpm",
        usage: &["120.bpm"],
        summary: "Sets the tempo, right away.",
    },
    FnDoc {
        name: "stop",
        usage: &["stop()"],
        summary: "Fades out everything, and stops all patterns.",
    },
    FnDoc {
        name: "at",
        usage: &["at 4b", "at 5b + 2"],
        summary: "A grid to start on: every 4 beats, counted from beat 0 (plus an offset in beats).",
    },
    FnDoc {
        name: "range",
        usage: &["control.range(lo, hi)", "random(1b).range(a3, c4)"],
        summary: "Maps 0..1 onto lo..hi. Between two pitches it picks whole notes.",
    },
    FnDoc {
        name: "round",
        usage: &["control.round"],
        summary: "Rounds to whole numbers.",
    },
    FnDoc {
        name: "scale",
        usage: &["degree.scale(\"minor\", f4)"],
        summary: "Turns a scale degree (0 is the root, -1 the note below) into a pitch.",
    },
    FnDoc {
        name: "map",
        usage: &["[0, 3, 7].map(n => sine:note(?note + n))"],
        summary: "A list with a function applied to each item. A list of sounds plays as a mix, and its params are set all at once.",
    },
];

pub fn function_doc(name: &str) -> Option<&'static FnDoc> {
    FUNCTIONS.iter().find(|f| f.name == name)
}

/// Documentation for the editor: of a built-in, or of a value named with
/// `let` (then its params are the ones still free in it).
#[derive(Clone, Debug, PartialEq)]
pub struct Doc {
    pub title: String,
    /// "source", "effect", "control", "function", "a sound", ...
    pub kind: String,
    pub summary: String,
    pub params: Vec<ParamDoc>,
    pub examples: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParamDoc {
    pub name: String,
    pub default: String,
    pub doc: String,
}

impl Doc {
    pub fn of_spec(spec: &Spec) -> Doc {
        Doc {
            title: spec.name.to_string(),
            kind: spec.role.name().to_string(),
            summary: spec.summary.to_string(),
            params: spec
                .free_params()
                .map(|p| ParamDoc {
                    name: p.name.to_string(),
                    default: p.default.text(),
                    doc: p.doc.to_string(),
                })
                .collect(),
            examples: spec.examples.iter().map(|e| e.to_string()).collect(),
        }
    }

    pub fn of_function(f: &FnDoc) -> Doc {
        Doc {
            title: f.name.to_string(),
            kind: "function".to_string(),
            summary: f.summary.to_string(),
            params: Vec::new(),
            examples: f.usage.iter().map(|e| e.to_string()).collect(),
        }
    }

    /// A param name (after a `:`): which nodes have it, and what it does.
    pub fn of_param(name: &str) -> Option<Doc> {
        let having: Vec<&Spec> = SPECS
            .iter()
            .filter(|s| s.free_params().any(|p| p.name == name))
            .collect();
        let first = having.first()?.param(name)?.1;
        let names: Vec<&str> = having.iter().map(|s| s.name).collect();
        Some(Doc {
            title: format!(":{name}"),
            kind: "param".to_string(),
            summary: format!("A param of {}.", names.join(", ")),
            params: having
                .iter()
                .map(|s| {
                    let p = s.param(name).unwrap().1;
                    ParamDoc {
                        name: s.name.to_string(),
                        default: p.default.text(),
                        doc: p.doc.to_string(),
                    }
                })
                .collect(),
            examples: vec![format!("x:{name}(...)  sets every free {name} in x")]
                .into_iter()
                .chain(
                    (first.ty.is_control())
                        .then(|| format!("x:{name}(?{name} + ...)  links them to a new ?{name}")),
                )
                .collect(),
        })
    }

    /// A built-in, by name.
    pub fn builtin(name: &str) -> Option<Doc> {
        if let Some(spec) = spec(name) {
            return Some(Doc::of_spec(spec));
        }
        function_doc(name).map(Doc::of_function)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_read_like_code() {
        assert_eq!(Def::Note(48.0).text(), "c3");
        assert_eq!(Def::Hz(10_000.0).text(), "10khz");
        assert_eq!(Def::Hz(80.0).text(), "80hz");
        assert_eq!(Def::Seconds(0.15).text(), "150ms");
        assert_eq!(Def::Beats(0.75).text(), "0.75b");
        assert_eq!(Def::Num(0.891).text(), "0.891");
        assert_eq!(Def::Num(f64::INFINITY).text(), "inf");
    }

    #[test]
    fn names_are_unique_and_params_mean_one_thing() {
        for (i, a) in SPECS.iter().enumerate() {
            assert!(
                SPECS[i + 1..].iter().all(|b| b.name != a.name),
                "{}",
                a.name
            );
            for (name, _) in a.presets {
                assert!(a.param(name).is_some(), "{}: {name}", a.name);
            }
        }
        // A param name has one type everywhere it's used, except `mix`
        // (a control on echoes, a number on reverbs).
        for a in SPECS {
            for pa in a.params {
                for b in SPECS {
                    if let Some((_, pb)) = b.param(pa.name)
                        && pa.name != "mix"
                    {
                        assert_eq!(pa.ty.is_control(), pb.ty.is_control(), "{}", pa.name);
                    }
                }
            }
        }
    }

    #[test]
    fn docs() {
        let doc = Doc::builtin("wavetable").unwrap();
        assert_eq!(doc.kind, "source");
        let names: Vec<&str> = doc.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["table", "pos", "warp", "note"]);
        assert_eq!(Doc::builtin("sine").unwrap().params.len(), 2);
        assert_eq!(Doc::builtin("play").unwrap().kind, "function");
        let mix = Doc::of_param("mix").unwrap();
        assert_eq!(mix.params.len(), 3);
        assert!(Doc::of_param("nope").is_none());
    }
}
