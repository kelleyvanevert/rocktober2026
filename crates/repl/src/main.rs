use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use rocktober_engine::Session;
use rocktober_engine::bundle::{self, Bundle};
use rocktober_engine::lang::{self, Expr};
use rocktober_engine::recorder;

const USAGE: &str = "usage: repl [file.rock]
       repl render <file.rock> <length> [out.wav]
       repl code <file.rock>
       repl set-code <file.rock> <code.txt>

The REPL runs code with the file's resources (not its own code). `render`
runs the whole file, as cmd-shift-enter would, and renders <length> of it
(like 30s, 1:30 or 16bars) as fast as it can, to out.wav (by default
recordings/<file>.wav). `code` prints a file's code, and `set-code` replaces
it, keeping the file's resources.";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["render", file, length] => render(Path::new(file), length, None),
        ["render", file, length, out] => render(Path::new(file), length, Some(out.into())),
        ["code", file] => {
            let (code, _) = open(Path::new(file))?;
            print!("{code}");
            Ok(())
        }
        ["set-code", file, text] => {
            let (_, bundle) = open(Path::new(file))?;
            let code = std::fs::read_to_string(text)?;
            Ok(bundle::save(Path::new(file), &code, &bundle)?)
        }
        ["-h" | "--help" | "render" | "code" | "set-code", ..] => {
            println!("{USAGE}");
            Ok(())
        }
        [] => repl(Path::new("untitled.rock")),
        [file] => repl(Path::new(file)),
        _ => Err(USAGE.into()),
    }
}

/// `repl [file.rock]`: the code runs with that file's resources (its own code
/// isn't run). Without one, there's just `kick.mp3`.
fn repl(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
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

/// A `.rock` file's code and resources; it has to exist.
fn open(path: &Path) -> Result<(String, Bundle), Box<dyn std::error::Error>> {
    Ok(bundle::open(path)?.ok_or_else(|| format!("{}: no such file", path.display()))?)
}

/// Render a file's code without a device.
fn render(
    path: &Path,
    length: &str,
    out: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    const SAMPLE_RATE: u32 = 48_000;
    let (code, bundle) = open(path)?;
    let mut session = Session::without_output(SAMPLE_RATE, bundle);
    if let Err(e) = session.eval(&code) {
        let line = code[..e.pos].lines().count().max(1);
        return Err(format!("{}:{line}: {e}", path.display()).into());
    }
    // A length in beats is at the tempo the code set.
    let seconds = match lang::parse(length).ok().as_deref() {
        Some([e]) => match e.expr {
            Expr::Duration(s) | Expr::Num(s) => s,
            Expr::Beats(b) => b * 60.0 / session.position().0,
            _ => return Err(format!("bad length '{length}' (like 30s, 1:30 or 16bars)").into()),
        },
        _ => return Err(format!("bad length '{length}' (like 30s, 1:30 or 16bars)").into()),
    };
    let started = std::time::Instant::now();
    let frames = session.render((seconds * SAMPLE_RATE as f64) as usize);
    let out = out.unwrap_or_else(|| {
        let stem = path
            .file_stem()
            .map_or("render".into(), |s| s.to_string_lossy());
        Path::new("recordings").join(format!("{stem}.wav"))
    });
    recorder::write_wav(&out, &frames, SAMPLE_RATE)?;
    let peak = frames.iter().flatten().fold(0f32, |m, s| m.max(s.abs()));
    let clipped = match session.clipped() {
        0 => String::new(),
        n => format!(
            ", limited {:.1}% of the time: it would have clipped",
            100.0 * n as f64 / frames.len().max(1) as f64
        ),
    };
    println!(
        "rendered {seconds:.1}s to {} in {:.1}s (peak {:.1} dBFS{clipped})",
        out.display(),
        started.elapsed().as_secs_f64(),
        20.0 * peak.max(1e-9).log10()
    );
    Ok(())
}
