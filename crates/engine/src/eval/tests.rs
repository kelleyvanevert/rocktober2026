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
    if let Some(kick) = Bundle::with_kick().get(ResourceKind::Sample, "kick.mp3") {
        bundle.put(ResourceKind::Sample, "kick.mp3", kick.data.to_vec());
    }
    Evaluator::new(48_000, bundle)
}

/// Render a node to completion, or `max` frames.
fn render_node(node: &mut dyn crate::nodes::Node, max: usize) -> Vec<Frame> {
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

fn run(ev: &mut Evaluator, src: &str) {
    if let Err(e) = ev.run(&parse(src).unwrap()) {
        panic!("{src}: {}", e.msg);
    }
}

/// Upward zero crossings per second.
fn frequency(frames: &[Frame]) -> f32 {
    let crossings = frames
        .windows(2)
        .filter(|w| w[0][0] < 0.0 && w[1][0] >= 0.0)
        .count();
    crossings as f32 * 48_000.0 / frames.len() as f32
}

/// The names of a value's free params, and their defaults.
fn free(ev: &Evaluator, name: &str) -> Vec<(String, String)> {
    ev.describe(name)
        .unwrap()
        .params
        .into_iter()
        .map(|p| (p.name, p.default))
        .collect()
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

#[test]
fn add_and_gain() {
    let kick = peak(&render(r#"sample("kick.mp3").play"#));
    let four = peak(&render(
        r#"add(sample("kick.mp3"), sample("kick.mp3"), sample("kick.mp3"), sample("kick.mp3")).play"#,
    ));
    assert!((four - 4.0 * kick).abs() < 1e-4);
    let half = peak(&render(r#"(sample("kick.mp3") * -6db).play"#));
    assert!((half - 0.501 * kick).abs() < 1e-3);
}

#[test]
fn limit_tames_a_loud_mix() {
    let loud = r#"add(sample("kick.mp3"), sample("kick.mp3"), sample("kick.mp3"))"#;
    assert!(peak(&render(&format!("play({loud})"))) > 1.5);
    assert!(peak(&render(&format!("({loud} * limit).play"))) <= 0.892);
    assert!(peak(&render(&format!("({loud} * limit:ceiling(-6db)).play"))) <= 0.502);
}

#[test]
fn argument_errors() {
    assert_eq!(
        error(r#"seq(sample("kick.mp3"), 3)"#),
        "seq: expected a sound, got a number"
    );
    assert_eq!(
        error("limit:ceiling(0)"),
        "ceiling: expected a positive ceiling (like 0.9 or -1db)"
    );
    assert_eq!(
        error("sample"),
        "sample needs its file, like sample(\"kick.mp3\")"
    );
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
        error(r#"reverb(2)"#),
        "reverb: space: expected a space (like \"hall\") or a sound, got a number"
    );
    assert_eq!(
        error(r#"play(4.repeat(2))"#),
        "repeat: expected a sound or a pattern, got a number"
    );
    assert_eq!(
        error("wavetable(\"basic\", 0, 0, c3, 1)"),
        "wavetable takes at most 4 value(s) (table, pos, warp, note), got 5"
    );
    assert_eq!(
        error("sine(c3, 0, 1)"),
        "sine takes at most 2 value(s) (warp, note), got 3"
    );
}

#[test]
fn old_style_calls_say_what_to_write_instead() {
    assert_eq!(
        error(r#"sample("kick.mp3").lowpass(800hz)"#),
        "lowpass is an effect now: apply it to a sound, like sound * lowpass:freq(...)"
    );
    assert_eq!(
        error(r#"sample("kick.mp3").gain(0.5)"#),
        "unknown function 'gain': multiply instead: sound * 0.5 or sound * -6db"
    );
    assert_eq!(
        error(r#"wavetable.with(c3)"#),
        "unknown function 'with': set params with :name(value), like x:note(c3)"
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
    // The window is made of params too.
    let set = render(r#"sample("kick.mp3"):start(0:00:100):end(0:00:300).play"#);
    assert_eq!(set, window);
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
        "sample: the end has to be after the start"
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
    let hall = render(r#"(sample("kick.mp3") * reverb).play"#);
    assert!(hall.len() > dry.len() + 48_000, "the hall rings on");
    let custom =
        render(r#"(sample("kick.mp3") * reverb(sample("kick.mp3").fit(100ms)):mix(1)).play"#);
    assert!(custom.len() > dry.len() && custom.len() < dry.len() + 48_000);
    assert!(
        error(r#"reverb("nowhere")"#)
            .starts_with("reverb: unknown space \"nowhere\" (try small_room, ")
    );
    assert_eq!(
        error(r#"reverb:mix(2)"#),
        "mix: expected a mix between 0 and 1"
    );
}

#[test]
fn both_call_styles_are_equivalent() {
    let a = render(r#"play(repeat(fit(mul(sample("kick.mp3"), 0.5), 100ms), 3))"#);
    let b = render(r#"(sample("kick.mp3") * 0.5).fit(100ms).repeat(3).play()"#);
    assert_eq!(a, b);
}

/// One second of a constant 1.0 (see `with_resources`).
const ONES: &str = r#"sample("ones.wav")"#;

#[test]
fn numbers_and_controls_combine() {
    let mut ev = with_resources();
    // Plain number arithmetic stays numbers.
    let half = render_with(&mut ev, &format!("({ONES} * (0.25 + 0.75 - 1 / 2)).play"));
    assert_eq!(half[100], [0.5, 0.5]);
    // A modulation shapes a sound: here a ramp from 0 to 1 over a second.
    let ramp = render_with(&mut ev, &format!(r#"({ONES} * mod("ramp")).play"#));
    assert_eq!(ramp.len(), 48_000);
    assert!((ramp[24_000][0] - 0.5).abs() < 1e-3);
    // Controls combine: half the ramp, plus a quarter, and minus and divide.
    let mixed = render_with(
        &mut ev,
        &format!(r#"({ONES} * (mod("ramp") * 0.5 + 0.25)).play"#),
    );
    assert!((mixed[24_000][0] - 0.5).abs() < 1e-3);
    assert!((mixed[0][0] - 0.25).abs() < 1e-3);
    let other = render_with(
        &mut ev,
        &format!(r#"({ONES} * (1 - mod("ramp") / 2)).play"#),
    );
    assert!((other[24_000][0] - 0.75).abs() < 1e-3);
    assert!((other[0][0] - 1.0).abs() < 1e-3);
}

#[test]
fn repeated_modulation_is_an_lfo() {
    let mut ev = with_resources();
    let saw = render_with(
        &mut ev,
        &format!(r#"({ONES}.repeat(3) * mod("ramp"):repeat(2)).play"#),
    );
    assert!((saw[12_000][0] - 0.25).abs() < 1e-3);
    assert!((saw[48_000 + 12_000][0] - 0.25).abs() < 1e-3);
    // After two passes it holds its last value.
    assert!((saw[48_000 * 2 + 12_000][0] - 1.0).abs() < 1e-3);
    assert_eq!(
        error_with(&mut ev, r#"mod("ramp"):repeat(1.5)"#),
        "repeat: expected a whole number or inf, got a number"
    );
}

#[test]
fn gated_envelope_ends_the_sound_after_its_release() {
    let mut ev = with_resources();
    let note = render_with(
        &mut ev,
        &format!(r#"({ONES}.repeat(inf) * envelope("slow"):gate(1500ms)).play"#),
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

#[test]
fn wavetables_play_notes() {
    let mut ev = evaluator();
    let a4 = render_with(&mut ev, "wavetable:note(a4).fit(1s).play");
    assert_eq!(a4.len(), 48_000);
    assert!((frequency(&a4) - 440.0).abs() <= 1.0);
    // Pitches are note numbers, so adding 12 is an octave up.
    let a5 = render_with(&mut ev, "wavetable:note(a4 + 12).fit(1s).play");
    assert!((frequency(&a5) - 880.0).abs() <= 1.0);
    // Defaults: the first frame, no warp, c3.
    let c3 = render_with(&mut ev, r#"wavetable("sine-saw").fit(1s).play"#);
    assert!((frequency(&c3) - 130.8).abs() <= 1.0);
    // A plain wavetable is a sine.
    assert_eq!(
        render_with(&mut ev, "wavetable.fit(10ms).play"),
        render_with(&mut ev, "sine.fit(10ms).play")
    );
    assert_ne!(
        render_with(&mut ev, "wavetable.fit(10ms).play"),
        render_with(&mut ev, "saw.fit(10ms).play")
    );
    assert!(
        error_with(&mut ev, r#"wavetable("nope")"#)
            .starts_with("wavetable: unknown wavetable \"nope\" (try basic, ")
    );
    assert_eq!(
        error_with(&mut ev, r#"wavetable("basic", "x")"#),
        "wavetable: pos: expected a number or a control, got a string"
    );
    assert_eq!(
        error_with(&mut ev, r#"wavetable:table(4)"#),
        "table: expected a string, got a number"
    );
}

#[test]
fn params_are_set_by_name_everywhere() {
    let mut ev = evaluator();
    run(
        &mut ev,
        r#"let pair = (wavetable + wavetable:table("sine-saw")).fit(100ms)"#,
    );
    assert_eq!(
        free(&ev, "pair"),
        pairs(&[
            ("table", "\"basic\""),
            ("pos", "0"),
            ("warp", "0"),
            ("note", "c3")
        ])
    );
    // Both notes at once.
    let a4 = render_with(&mut ev, "pair:note(a4).play");
    assert!((frequency(&a4) - 440.0).abs() <= 10.0);
    // A set param isn't free any more: only the first table is.
    run(&mut ev, r#"let set = pair:table("pulse")"#);
    assert_eq!(
        free(&ev, "set"),
        pairs(&[("pos", "0"), ("warp", "0"), ("note", "c3")])
    );
    assert_eq!(
        error_with(&mut ev, r#"set:table("bright")"#),
        "there's no free table to set (free: pos, warp, note)"
    );
    assert_eq!(
        error_with(&mut ev, "pair:nope(1)"),
        "there's no free nope to set (free: table, pos, warp, note)"
    );
    assert_eq!(
        error_with(
            &mut ev,
            "pair:note(a4):pos(0):warp(0):table(\"basic\"):note(c3)"
        ),
        "there's no free note to set (it has no free params left)"
    );
    assert_eq!(
        error_with(&mut ev, "3:note(c3)"),
        ":note: a number has no params"
    );
    assert_eq!(
        error_with(&mut ev, r#"pair:note(sample("kick.mp3"))"#),
        "note: expected a pitch (like c3, 440hz or ?note + 12), got a sound"
    );
    assert_eq!(
        error_with(&mut ev, "lowpass:freq(800)"),
        "freq: a frequency is a pitch: 800 would be note 800 (did you mean 800hz?)"
    );
}

#[test]
fn params_are_linked_with_new_holes() {
    let mut ev = evaluator();
    // Both notes now follow a new ?note, an octave apart; it keeps note's
    // default.
    run(
        &mut ev,
        "let pair = (sine + sine:note(?note + 12)).fit(100ms)",
    );
    assert_eq!(free(&ev, "pair")[1], ("note".to_string(), "c3".to_string()));
    // Under another name, it's a param of its own, with no default.
    run(&mut ev, "let up = pair:note(?pitch + 0)");
    assert_eq!(
        free(&ev, "up")[1],
        ("pitch".to_string(), "none".to_string())
    );
    let both = render_with(&mut ev, "pair:note(a4).play");
    let low = render_with(&mut ev, "sine:note(a4).fit(100ms).play");
    let high = render_with(&mut ev, "sine:note(a5).fit(100ms).play");
    let sum: Vec<f32> = low.iter().zip(&high).map(|(a, b)| a[0] + b[0]).collect();
    for (a, b) in both.iter().zip(&sum) {
        assert!((a[0] - b).abs() < 1e-5);
    }
    // A hole of the sound's own, with no default, takes one from a hole of
    // the same name.
    run(&mut ev, "let fx = sine * lowpass:freq(?note + 24)");
    assert!(free(&ev, "fx").contains(&("note".to_string(), "c3".to_string())));
    played(&mut ev, "fx.fit(10ms).play");
    // One with no default anywhere has to be set.
    run(&mut ev, "let lead = sine:note(?pitch).fit(100ms)");
    assert_eq!(
        error_with(&mut ev, "lead.play"),
        "play: ?pitch has no value (set it with :pitch(...))"
    );
    assert!((frequency(&render_with(&mut ev, "lead:pitch(a4).play")) - 440.0).abs() <= 10.0);
    assert_eq!(
        error_with(&mut ev, r#"wavetable("basic", ?pos = "x")"#),
        "?pos: expected a default like 0.2 or c2, got a string"
    );
}

#[test]
fn effects_apply_and_chain() {
    let mut ev = evaluator();
    let tone = "saw:note(a4).fit(500ms)";
    // Applying an effect is multiplying by it.
    run(&mut ev, "let fx = lowpass:freq(300hz) * drive:power(2)");
    let chained = render_with(&mut ev, &format!("({tone} * fx).play"));
    let stepwise = render_with(
        &mut ev,
        &format!("({tone} * lowpass:freq(300hz) * drive:power(2)).play"),
    );
    assert_eq!(chained, stepwise);
    assert_ne!(chained, render_with(&mut ev, &format!("{tone}.play")));
    assert_eq!(ev.describe("fx").unwrap().kind, "effect");
    assert_eq!(
        free(&ev, "fx"),
        pairs(&[("res", "0")]),
        "only the effects' free params"
    );
    assert_eq!(
        error_with(&mut ev, "fx.play"),
        "play: this is an effect: apply it to a sound, like sound * drive"
    );
    // A sound times a sound that isn't an effect is still ring modulation.
    let ring = render_with(&mut ev, "(sine:note(a4) * sine:note(a2)).fit(100ms).play");
    assert!(peak(&ring) <= 1.0);
}

#[test]
fn lists_map_and_mix() {
    let mut ev = evaluator();
    run(
        &mut ev,
        "let chord = [0, 4, 7].map(n => sine:note(?note + n)):note(c4)",
    );
    let chord = render_with(&mut ev, "(chord * 0.3).fit(100ms).play");
    let manual = render_with(
        &mut ev,
        "((sine:note(c4) + sine:note(e4) + sine:note(g4)) * 0.3).fit(100ms).play",
    );
    for (a, b) in chord.iter().zip(&manual) {
        assert!((a[0] - b[0]).abs() < 1e-5);
    }
    assert_eq!(
        error_with(&mut ev, "[1, 2].map(3)"),
        "map: expected a function (like n => n * 2), got a number"
    );
    assert_eq!(
        error_with(&mut ev, "[1, 2].map(n => n.nope)"),
        "n =>: unknown function 'nope'"
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
        sample("d.wav"):start(1s).play(at 4b, "later")
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
        "play: the sound has no free note for the pattern's notes (use x for hits without one)"
    );
    assert_eq!(
        error_with(&mut ev, r#"notes("x", 0.5b).play(sine:note(?n):n(?note))"#),
        "play: the pattern has hits without a note (x), but ?note has no default"
    );
    assert_eq!(
        error_with(&mut ev, r#"notes("c2", 0.5b).play(sine:warp(?w))"#),
        "play: ?w has no value (set it with :w(...))"
    );
    // A set note is set: the pattern's notes can't reach it.
    assert_eq!(
        error_with(&mut ev, r#"notes("c2", 0.5b).play(sine:note(c3))"#),
        "play: the sound has no free note for the pattern's notes (use x for hits without one)"
    );
    let actions = ev
        .run(&parse(r#"notes("x . x x", 0.25b).play(sample("ones.wav"), "drums")"#).unwrap())
        .unwrap();
    let [Action::Pattern { pattern, slot, .. }] = actions.as_slice() else {
        panic!()
    };
    assert_eq!((pattern.steps.len(), slot.as_deref()), (4, Some("drums")));
}

fn value_sound(ev: &Evaluator, name: &str) -> Sound {
    ev.vars[name].sound().share_defaults()
}

#[test]
fn voices_for_notes() {
    let mut ev = with_resources();
    run(&mut ev, r#"let lead = sine * envelope("slow")"#);
    // Released at the gate (0.5 s), then a second of release.
    let voice =
        value_sound(&ev, "lead").for_note(Some(Control::Constant(69.0)), Some(0.5), 0.0, None);
    let frames = render_node(voice.instantiate(48_000).as_mut(), 48_000 * 10);
    assert_eq!(frames.len(), 72_000);

    // A ramp from 0 to 1 over a second, on a sound that never ends: cut off
    // at the gate. The note starts half a second into the pattern.
    let mut ramp = |clock: &str| {
        run(
            &mut ev,
            &format!(r#"let s = sample("ones.wav").repeat(inf) * mod("ramp"){clock}"#),
        );
        let voice = value_sound(&ev, "s").for_note(None, Some(0.25), 0.5, None);
        let frames = render_node(voice.instantiate(48_000).as_mut(), 48_000 * 10);
        assert_eq!(frames.len(), 12_000);
        (frames[0][0], frames[6_000][0])
    };
    let close = |(a, b): (f32, f32), (x, y): (f32, f32)| {
        assert!((a - x).abs() < 1e-3 && (b - y).abs() < 1e-3, "{a}, {b}")
    };
    close(ramp(""), (0.5, 0.625));
    close(ramp(":retrig"), (0.0, 0.125));
    close(ramp(":retrig(0)"), (0.5, 0.625));
    close(ramp(":latch"), (0.5, 0.5));
    // Set on the whole sound, it reaches the modulation inside.
    run(
        &mut ev,
        r#"let s = (sample("ones.wav").repeat(inf) * mod("ramp")):retrig"#,
    );
    let voice = value_sound(&ev, "s").for_note(None, Some(0.25), 0.5, None);
    let frames = render_node(voice.instantiate(48_000).as_mut(), 48_000);
    close((frames[0][0], frames[6_000][0]), (0.0, 0.125));
    assert_eq!(
        error_with(&mut ev, "0.5:latch"),
        ":latch: a number has no params"
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
        error_with(&mut ev, r#"mod("ramp"):gate(1s)"#),
        "there's no free gate to set (free: repeat, retrig, latch)"
    );
    assert_eq!(
        error_with(&mut ev, r#"play(mod("ramp"))"#),
        "play: expected a sound, got a control"
    );
    assert_eq!(
        error_with(&mut ev, "envelope"),
        "envelope needs its env, like sound * envelope(\"pluck\")"
    );
    assert_eq!(
        error_with(&mut ev, r#"modulation("ramp")"#),
        "unknown function 'modulation': it's called mod now: mod(\"sweep\")"
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
    let white = render(r#"noise.fit(1s).play"#);
    assert_eq!(white.len(), 48_000);
    assert!(rms(&white) > 0.1);
    let brown = render(r#"noise("brown").fit(1s).play"#);
    assert!(brightness(&brown) < brightness(&white) * 0.2);
    let low = render(r#"(noise * lowpass:freq(500hz)).fit(1s).play"#);
    let high = render(r#"(noise * highpass(5khz, 0.3)).fit(1s).play"#);
    let band = render(r#"(noise * bandpass:freq(c6):res(0.9)).fit(1s).play"#);
    assert!(brightness(&low) < brightness(&white) * 0.3);
    assert!(brightness(&high) > brightness(&white));
    assert!(rms(&band) < rms(&white) * 0.3);
    // The cutoff can move: a lowpass opening up gets brighter.
    let mut ev = with_resources();
    let sweep = render_with(
        &mut ev,
        r#"(noise * lowpass:freq(30 + mod("ramp") * 100)).fit(1s).play"#,
    );
    assert!(brightness(&sweep[40_000..]) > 3.0 * brightness(&sweep[..8_000]));
    assert!(
        error(r#"noise("green")"#).starts_with("noise: unknown color \"green\" (try white, pink,")
    );
    assert_eq!(
        error(r#"lowpass(800)"#),
        "lowpass: freq: a frequency is a pitch: 800 would be note 800 (did you mean 800hz?)"
    );
}

#[test]
fn pan_spread_and_echo() {
    let side = |frames: &[Frame]| frames.iter().map(|f| (f[0] - f[1]).abs()).sum::<f32>();
    let tone = "sine:note(a4).fit(500ms)";
    let left = render(&format!("({tone} * pan:side(-1)).play"));
    assert!(left.iter().all(|f| f[1].abs() < 1e-6));
    assert_eq!(side(&render(&format!("{tone}.play"))), 0.0);
    assert!(side(&render(&format!("({tone} * spread).play"))) > 100.0);
    assert!(
        side(&render(&format!("({tone} * spread:width(0.2)).play")))
            < side(&render(&format!("({tone} * spread:width(1)).play")))
    );
    // Echoes: a 10 ms click every 250 ms (half a beat), dying away.
    let echoes = render(r#"(sample("kick.mp3").fit(10ms) * echo(0.5b, 0.5, 0.5)).play"#);
    let peak = |at: usize| {
        echoes[at..at + 480]
            .iter()
            .fold(0f32, |m, f| m.max(f[0].abs()))
    };
    assert!(peak(12_000) > 0.05 && peak(24_000) > 0.02 && peak(24_000) < peak(12_000));
    assert!(peak(6_000) < 1e-3, "nothing in between");
    let pingpong = render(
        r#"(sample("kick.mp3").fit(10ms) * pingpong:time(100ms):feedback(0.5):mix(1)).play"#,
    );
    assert!(pingpong[4_800..5_280].iter().all(|f| f[1].abs() < 1e-6));
    assert!(pingpong[9_600..10_080].iter().any(|f| f[1].abs() > 1e-3));
    assert_eq!(
        error(r#"echo:time(0ms)"#),
        "time: expected a delay time above 0"
    );
}

#[test]
fn scale_picks_notes_from_the_scale() {
    let mut ev = evaluator();
    run(
        &mut ev,
        r#"let r = random(10ms).range(-0.5, 6.5).scale("minor", f4)"#,
    );
    let Value::Control(c) = &ev.vars["r"] else {
        panic!()
    };
    let mut buf = vec![0.0; 48_000];
    c.instantiate(48_000).process(&mut buf);
    // F minor from F4 (65): F G Ab Bb C Db Eb, every one of them.
    let f_minor = [65.0, 67.0, 68.0, 70.0, 72.0, 73.0, 75.0];
    assert!(buf.iter().all(|n| f_minor.contains(n)), "{buf:?}");
    assert!(f_minor.iter().all(|n| buf.contains(n)));
    assert_eq!(
        error(r#"1.scale("nope", c4)"#),
        format!(
            "scale: unknown scale \"nope\" (try {})",
            control::SCALES
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", ")
        )
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
    let continuous = values("random:every(10ms).range(-1, 1)");
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
    run(&mut ev, &format!("let s = {ONES} * random(1ms):latch"));
    let voice = |since| {
        let voice = value_sound(&ev, "s").for_note(None, Some(0.1), since, None);
        render_node(voice.instantiate(48_000).as_mut(), 48_000)
    };
    let a = voice(0.5);
    assert!(a.iter().all(|f| f[0] == a[0][0]), "held");
    assert_eq!(voice(0.5)[0], a[0], "the stream's value at 0.5 s");
    assert_ne!(voice(0.7)[0], a[0], "another note, another value");
    assert_eq!(
        error("random(0ms)"),
        "random: every: expected a period above 0"
    );
}

#[test]
fn glides_mark_legato_sounds() {
    let mut ev = evaluator();
    run(&mut ev, "let slide = sine:note(glide(?note):dur(100ms))");
    run(&mut ev, "let legato = slide:legato");
    assert!(!value_sound(&ev, "slide").is_legato());
    assert!(value_sound(&ev, "legato").is_legato());
    // The glide's target holds the note: the pattern's notes still reach it.
    assert_eq!(
        free(&ev, "slide"),
        pairs(&[
            ("warp", "0"),
            ("dur", "100ms"),
            ("legato", "0"),
            ("note", "c3")
        ])
        .into_iter()
        .filter(|(n, _)| n != "dur")
        .collect::<Vec<_>>()
    );
    let a4 = render_with(&mut ev, "slide:note(a4).fit(100ms).play");
    assert!((frequency(&a4) - 440.0).abs() <= 10.0);
}

#[test]
fn functions() {
    let mut ev = evaluator();
    let kick = r#"sample("kick.mp3")"#;
    run(&mut ev, "fn louder(x, by) = x * by");
    // Called both ways.
    let plain = peak(&render_with(&mut ev, &format!("{kick}.play")));
    let twice = peak(&render_with(&mut ev, &format!("{kick}.louder(2).play")));
    assert!((twice - 2.0 * plain).abs() < 1e-4);
    let again = peak(&render_with(&mut ev, &format!("louder({kick}, 2).play")));
    assert_eq!(again, twice);

    // Free params in the body are free in what it returns.
    run(&mut ev, "fn tone(pos) = wavetable:pos(pos).fit(100ms)");
    let c3 = render_with(&mut ev, "tone(0).play");
    let a5 = render_with(&mut ev, "tone(0):note(a5).play");
    assert!((frequency(&c3) - 130.8).abs() <= 10.0);
    assert!((frequency(&a5) - 880.0).abs() <= 10.0);

    // Parameters shadow lets for the call only.
    run(&mut ev, "let x = 3\nfn id(x) = x");
    run(&mut ev, "let y = id(4)");
    assert!(matches!(ev.vars["y"], Value::Num(4.0)));
    assert!(matches!(ev.vars["x"], Value::Num(3.0)));
    assert!(!ev.vars.contains_key("by"), "no parameter left behind");

    // Names in the body are looked up when it's called.
    run(&mut ev, "fn amount() = 0.5\nfn quieter(x) = x * amount()");
    run(&mut ev, "fn amount() = 0.25");
    let quarter = peak(&render_with(&mut ev, &format!("{kick}.quieter.play")));
    assert!((quarter - 0.25 * plain).abs() < 1e-4);
}

#[test]
fn function_errors() {
    let mut ev = evaluator();
    run(&mut ev, "fn wide(x) = x * spread:width(\"lots\")");
    // At the call, not somewhere in the body's (other) code.
    let src = r#"sample("kick.mp3").wide.play"#;
    let e = ev.run(&parse(src).unwrap()).err().unwrap();
    assert_eq!(e.pos, 19);
    assert_eq!(
        e.msg,
        "wide: width: expected a number or a control, got a string"
    );
    assert_eq!(
        error_with(&mut ev, r#"sample("kick.mp3").wide(1)"#),
        "wide takes 1 argument(s), got 2"
    );
    assert_eq!(
        error_with(&mut ev, "fn play(x) = x"),
        "'play' is already a built-in"
    );
    assert_eq!(
        error_with(&mut ev, "fn sine(x) = x"),
        "'sine' is already a built-in"
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

#[test]
fn describes_values_and_builtins() {
    let mut ev = with_resources();
    run(
        &mut ev,
        r#"let swoop = mod("ramp"):retrig * 9 - 1
let wub = (
    (wavetable + 0.6 * wavetable:table("sine-saw"):pos(0.6)):note(?note + swoop)
        * lowpass:freq(?note + 24 + swoop * 3)
        * drive:power(6)
        * envelope("slow")
        * 0.6
):note(glide(?note):dur(300ms))"#,
    );
    let doc = ev.describe("wub").unwrap();
    assert_eq!(doc.kind, "sound");
    let mut names: Vec<&str> = doc.params.iter().map(|p| p.name.as_str()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "gate", "latch", "legato", "note", "pos", "repeat", "res", "table", "warp"
        ]
    );
    let note = doc.params.iter().find(|p| p.name == "note").unwrap();
    assert_eq!(note.default, "c3");
    assert!(note.doc.starts_with("3×: "), "{}", note.doc);
    // It plays, a pattern's note reaching both glides.
    let actions = ev
        .run(&parse(r#"notes("c2 x", 0.5b).play(wub, "wub")"#).unwrap())
        .unwrap();
    assert_eq!(actions.len(), 1);
    played(&mut ev, "wub:note(g2).fit(100ms).play");

    assert_eq!(ev.describe("swoop").unwrap().kind, "control");
    assert_eq!(ev.describe("wavetable").unwrap().kind, "source");
    assert_eq!(ev.describe("notes").unwrap().kind, "function");
    assert!(ev.describe("nope").is_none());
    run(&mut ev, "fn twice(x) = x + x");
    assert_eq!(ev.describe("twice").unwrap().kind, "fn twice(x)");
}
