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
    use rocktober_engine::envelope::Envelope;
    use rocktober_engine::modulation::Modulation;
    use rocktober_engine::resource::Resources;

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
            let resources = Resources {
                root: out.clone(),
                sample_dirs: vec![root.join("samples")],
            };
            let session = Session::without_output(48_000, resources);
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

        /// Put the cursor at (line, column), both 0-based, and let any loading
        /// that starts finish.
        fn move_to(&mut self, line: u32, column: u32) {
            let editor = self
                .cx
                .update(|cx| self.workspace.read(cx).editor().clone());
            self.cx
                .update_window(self.window, |_, window, cx| {
                    editor.update(cx, |state, cx| {
                        state.set_cursor_position(Position::new(line, column), window, cx)
                    });
                    window.render_frame(cx);
                })
                .unwrap();
            self.cx.run_until_parked();
        }

        /// The name of the resource under the cursor, and whether it's a
        /// sample that loaded.
        fn resource(&mut self) -> Option<(String, bool)> {
            self.cx.update(|cx| {
                let workspace = self.workspace.read(cx);
                let name = workspace.resource()?.name.clone();
                Some((name, workspace.sample_loaded(cx)))
            })
        }

        /// Do something with the window, then draw a frame.
        fn with_window(&mut self, f: impl FnOnce(&mut Window, &mut App)) {
            self.cx
                .update_window(self.window, |_, window, cx| {
                    f(window, cx);
                    window.render_frame(cx);
                })
                .unwrap();
        }

        fn bounds_of(&mut self, id: &'static str) -> Bounds<Pixels> {
            let mut bounds = None;
            self.with_window(|window, _| bounds = Some(window.find(id).bounds()));
            bounds.unwrap()
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
        h.press_at(7, 5, "cmd-enter");
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

        // The cursor on a resource reference opens its editor below the code.
        h.set_text(concat!(
            "sample(\"kick.mp3\", 0:00:100).gain(-6db).play\n",
            "\n",
            "sample(\"nope.wav\").play\n",
            "\n",
            "envelope(\"pluck\")\n",
        ));
        h.move_to(0, 3);
        assert_eq!(h.resource(), Some(("kick.mp3".into(), true)));
        h.snapshot("5-sample");
        h.move_to(2, 10);
        assert_eq!(h.resource(), Some(("nope.wav".into(), false)));
        h.snapshot("6-missing-sample");
        h.move_to(4, 0);
        assert_eq!(h.resource(), Some(("pluck".into(), false)));
        h.move_to(1, 0);
        assert_eq!(h.resource(), None);

        // A missing envelope can be created, then edited by dragging.
        let envelopes = h.out.join("envelopes");
        let modulations = h.out.join("modulations");
        let _ = std::fs::remove_dir_all(&envelopes);
        let _ = std::fs::remove_dir_all(&modulations);
        h.set_text("envelope(\"pluck\")\n\nmodulation(\"sweep\")\n");
        h.move_to(0, 3);
        h.snapshot("7-missing-envelope");
        h.with_window(|window, cx| window.click("create", cx));
        let pluck = envelopes.join("pluck.json");
        assert_eq!(Envelope::load(&pluck).unwrap(), Envelope::default());

        // Drag the end of the release 40 pixels to the right. (This repeats the
        // editor's layout: an 8px inset, 20% of the width for the sustain, and
        // all stages taking 80% of the rest.)
        let b = h.bounds_of("envelope-curve");
        let width = b.size.width.as_f32() - 16.0;
        let pixels_per_second = width * 0.8 / (1.201 * 1.25);
        let release_end = b.origin.x.as_f32() + 8.0 + 1.201 * pixels_per_second + 0.2 * width;
        let floor = b.bottom().as_f32() - 8.0;
        h.with_window(|window, cx| {
            window.drag(
                point(px(release_end), px(floor)),
                point(px(release_end + 40.0), px(floor)),
                cx,
            )
        });
        let release = Envelope::load(&pluck).unwrap().release.time;
        let expected = 0.6 + 40.0 / pixels_per_second as f64;
        assert!(
            (release - expected).abs() < 0.005,
            "{release} vs {expected}"
        );
        h.snapshot("8-envelope");

        // Same for a modulation; clicking adds a point.
        h.move_to(2, 3);
        h.with_window(|window, cx| window.click("create", cx));
        let sweep = modulations.join("sweep.json");
        assert_eq!(Modulation::load(&sweep).unwrap(), Modulation::default());
        let b = h.bounds_of("modulation-curve");
        let center = b.center();
        h.with_window(|window, cx| window.drag(center, center + point(px(0.), px(-30.)), cx));
        let m = Modulation::load(&sweep).unwrap();
        assert_eq!(m.points.len(), 3);
        assert!((m.points[1].at - 0.5).abs() < 0.01);
        assert!(m.points[1].value > 0.6, "{:?}", m.points[1]);
        h.snapshot("9-modulation");

        println!("ui: passed");
    }
}
