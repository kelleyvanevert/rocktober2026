//! How much of one CPU core a reverb takes: `cargo run --release -p rocktober-engine --example bench_reverb`
use rocktober_engine::nodes::{Node, SampleData, Sampler};
use rocktober_engine::reverb::{Impulse, Reverb, preset};
use std::sync::Arc;

fn main() {
    let rate = 48_000;
    for space in ["small_room", "hall", "cathedral"] {
        let impulse = Arc::new(Impulse::new(preset(space, rate).unwrap()));
        let input = SampleData {
            frames: vec![[0.1, 0.1]; rate as usize * 5],
            sample_rate: rate,
        };
        let mut reverb = Reverb::new(Box::new(Sampler::new(Arc::new(input), rate)), impulse, 0.3);
        let mut buf = vec![[0.0; 2]; 512];
        let start = std::time::Instant::now();
        let mut frames = 0;
        while frames < rate as usize * 5 {
            frames += reverb.process(&mut buf);
        }
        let cpu = start.elapsed().as_secs_f64() / 5.0;
        println!("{space:>10}: {:.1}% of a core per voice", cpu * 100.0);
    }
}
