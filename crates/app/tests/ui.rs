//! Headless UI test: renders the real `Workspace` with Metal (in a hidden window),
//! drives it with key presses, and writes screenshots to `target/ui-snapshots/`.
//!
//! Run with `cargo test -p rocktober --test ui`. No audio is played.

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
    #[cfg(not(target_os = "macos"))]
    println!("ui: skipped, GPUI has no headless renderer on this platform");
}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use gpui_kit::component::input::Position;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::*;
    use rocktober::workspace::Workspace;
    use rocktober_engine::Session;

    struct Harness {
        // Fields drop in order: the entity handle must go before the app context,
        // or GPUI reports it as leaked.
        workspace: Entity<Workspace>,
        window: AnyWindowHandle,
        out: PathBuf,
        cx: HeadlessAppContext,
    }

    impl Harness {
        fn new() -> Self {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .canonicalize()
                .unwrap();
            let out = root.join("target/ui-snapshots");
            std::fs::create_dir_all(&out).unwrap();

            let mut cx = HeadlessAppContext::with_platform(
                gpui_kit::platform::current_platform(true).text_system(),
                Arc::new(gpui_kit::assets::Assets),
                gpui_kit::platform::current_headless_renderer,
            );
            cx.update(|cx| {
                gpui_kit::init(cx);
                rocktober::init(cx);
            });

            // A path that doesn't exist, so the workspace starts with its example code.
            let path = out.join("untitled.rock");
            let session = Session::without_output(48_000, vec![root.join("samples")]);
            let (window, workspace) = cx
                .update(|cx| {
                    let options = WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(900.), px(560.)),
                        })),
                        show: false,
                        ..Default::default()
                    };
                    gpui_kit::open_window(options, cx, |window, cx| {
                        cx.new(|cx| Workspace::new(path, Ok(session), window, cx))
                    })
                })
                .unwrap();
            Self {
                cx,
                window,
                workspace,
                out,
            }
        }

        /// Put the cursor at (line, column), both 0-based, then press `keys`.
        fn press_at(&mut self, line: u32, column: u32, keys: &str) {
            let editor = self
                .cx
                .update(|cx| self.workspace.read(cx).editor().clone());
            self.cx
                .update_window(self.window, |_, window, cx| {
                    editor.update(cx, |state, cx| {
                        state.set_cursor_position(Position::new(line, column), window, cx)
                    });
                    window.press(keys, cx);
                    window.render_frame(cx);
                })
                .unwrap();
        }

        fn text(&mut self) -> String {
            self.cx.update(|cx| {
                self.workspace
                    .read(cx)
                    .editor()
                    .read(cx)
                    .value()
                    .to_string()
            })
        }

        fn set_text(&mut self, text: &str) {
            let workspace = self.workspace.clone();
            self.cx
                .update_window(self.window, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| workspace.set_text(text, window, cx))
                })
                .unwrap();
        }

        fn last_log(&mut self) -> String {
            self.cx.update(|cx| {
                self.workspace
                    .read(cx)
                    .log_lines()
                    .last()
                    .unwrap_or("")
                    .to_string()
            })
        }

        fn snapshot(&mut self, name: &str) {
            self.cx
                .update_window(self.window, |_, window, cx| window.render_frame(cx))
                .unwrap();
            let image = self
                .cx
                .capture_screenshot(self.window)
                .expect("Metal rendering");
            let path = self.out.join(format!("{name}.png"));
            image.save(&path).unwrap();
            println!("  wrote {}", path.display());
        }
    }

    pub fn run() {
        let mut h = Harness::new();
        h.snapshot("1-initial");

        // cmd-enter runs the block under the cursor, without inserting a newline.
        let before = h.text();
        h.press_at(6, 5, "cmd-enter");
        assert_eq!(h.text(), before, "cmd-enter must not edit the text");
        assert_eq!(h.last_log(), r#"sample("kick.mp3").fit(500ms).play"#);
        h.snapshot("2-run-block");

        // A type error lands in the console with its position.
        h.set_text("-- oops\nsample(\"kick.mp3\").play\n\nsample(\"kick.mp3\").fit(4).play\n");
        h.press_at(3, 0, "cmd-enter");
        assert_eq!(
            h.last_log(),
            "4:24: fit: expected a duration (like 500ms), got a number"
        );
        h.snapshot("3-error");

        // cmd-shift-enter runs everything; cmd-. stops.
        h.set_text("sample(\"kick.mp3\").play\nsample(\"kick.mp3\").play\n");
        h.press_at(0, 0, "cmd-shift-enter");
        assert_eq!(h.last_log(), r#"sample("kick.mp3").play …"#);
        h.press_at(0, 0, "cmd-.");
        assert_eq!(h.last_log(), "stop");
        h.snapshot("4-stop");

        println!("ui: passed");
    }
}
