//! Check that decoding a window gives the same audio as decoding the whole file:
//! `cargo run -p rocktober-engine --example seekcheck -- file start end`

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &rocktober_engine::sample::Source::File(args[1].clone().into());
    let (start, end): (f64, f64) = (args[2].parse().unwrap(), args[3].parse().unwrap());
    let whole = rocktober_engine::sample::load(path).unwrap();
    let window = rocktober_engine::sample::load_range(path, start, Some(end)).unwrap();
    let offset = (start * whole.sample_rate as f64).round() as usize;
    let fade = (0.003 * whole.sample_rate as f64) as usize + 1;
    let inner = &window.frames[fade..window.frames.len() - fade];
    let diff = inner
        .iter()
        .zip(&whole.frames[offset + fade..])
        .map(|(a, b)| (a[0] - b[0]).abs().max((a[1] - b[1]).abs()))
        .fold(0f32, f32::max);
    println!(
        "window {} frames (expected {}), max difference {diff:.6}",
        window.frames.len(),
        ((end - start) * whole.sample_rate as f64).round()
    );
}
