use std::io::{BufRead, Write};
use std::path::PathBuf;

use rocktober_engine::Session;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut session = Session::start(vec![PathBuf::from("."), PathBuf::from("samples")])?;
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
