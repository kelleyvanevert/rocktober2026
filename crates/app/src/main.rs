use std::path::PathBuf;

use gpui_kit::*;
use rocktober::workspace::Workspace;
use rocktober_engine::Session;
use rocktober_engine::bundle::{self, Bundle};

fn main() {
    let path = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("main.rock"), PathBuf::from);

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            rocktober::init(cx);
            cx.on_window_closed(|cx, _| cx.quit()).detach();

            let (code, bundle) = match bundle::open(&path) {
                Ok(Some((code, bundle))) => (Ok(Some(code)), bundle),
                // A new file starts with the example code, which plays a kick.
                Ok(None) => (Ok(None), Bundle::with_kick()),
                Err(e) => (Err(e), Bundle::default()),
            };
            let session = Session::start(bundle.clone());
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.), px(800.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("rocktober".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workspace::new(path, code, bundle, session, window, cx))
            })
            .expect("failed to open window");
            cx.activate(true);
        });
}
