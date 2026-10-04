use std::io::{BufRead, Write};
use std::path::Path;

use rocktober_engine::Session;
use rocktober_engine::bundle::{self, Bundle};

/// `repl [file.rock]`: the code runs with that file's resources (its own code
/// isn't run). Without one, there's just `kick.mp3`.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "untitled.rock".into());
    let path = Path::new(&path);
    let bundle = bundle::open(path)?.map_or_else(Bundle::with_kick, |(_, bundle)| bundle);
    let mut session = Session::start(bundle)?;
    println!(
        "output: {} ({} Hz, {} ch)",
        session.device_name, session.sample_rate, session.channels
    );

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("> ");
        std::io::stdout().flush()?;
        let Some(line) = lines.next() else { break };
        if let Err(e) = session.eval(&line?) {
            // "> " prompt is two characters wide.
            eprintln!("  {}^", " ".repeat(e.pos));
            eprintln!("error: {e}");
        }
    }

    // End of input (e.g. piped from a file): let whatever is playing finish.
    println!();
    session.wait_until_idle();
    Ok(())
}
