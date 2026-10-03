use std::path::PathBuf;

use gpui_kit::*;
use rocktober::workspace::Workspace;
use rocktober_engine::Session;
use rocktober_engine::resource::Resources;

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

            let session = Session::start(Resources::for_code(&path));
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1000.), px(720.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("rocktober".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workspace::new(path, session, window, cx))
            })
            .expect("failed to open window");
            cx.activate(true);
        });
}
