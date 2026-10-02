//! Decode an audio file and print what came out: `cargo run -p rocktober-engine --example probe -- file.m4a`

fn main() {
    let path = std::env::args().nth(1).expect("usage: probe <file>");
    let start = std::time::Instant::now();
    match rocktober_engine::sample::load(path.as_ref()) {
        Ok(data) => {
            let seconds = data.frames.len() as f64 / data.sample_rate as f64;
            let peak = data
                .frames
                .iter()
                .flatten()
                .fold(0f32, |m, s| m.max(s.abs()));
            println!(
                "{} Hz, {:.3} s, peak {peak:.3}, {:.0} MB in memory, decoded in {:.2?}",
                data.sample_rate,
                seconds,
                (data.frames.len() * 8) as f64 / 1e6,
                start.elapsed()
            );
        }
        Err(e) => eprintln!("error: {e}"),
    }
}
