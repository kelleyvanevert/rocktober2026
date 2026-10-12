// `cargo xtask bundle [--release]`: builds vstmidi and bundles it as .vst3 and .clap in
// target/bundled.
//
// nih_plug_xtask::main() takes the outermost directory with a Cargo.toml as the workspace root,
// which here would be the repo's own workspace. So we point it at vstmidi/ ourselves.

use std::path::Path;

fn main() -> nih_plug_xtask::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    std::env::set_current_dir(root)?;

    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("bundle") {
        eprintln!("usage: cargo xtask bundle [--release]");
        std::process::exit(1);
    }
    let args: Vec<String> = args.collect();
    nih_plug_xtask::build(&["vstmidi".to_string()], &args)?;
    nih_plug_xtask::bundle(&root.join("target"), "vstmidi", &args, false)
}
