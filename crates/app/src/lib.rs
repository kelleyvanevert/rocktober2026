pub mod blocks;
pub mod comments;
pub mod curve_view;
pub mod docs;
pub mod envelope_editor;
pub mod modulation_editor;
pub mod sample_editor;
pub mod workspace;

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::*;

/// Something a resource editor did: worth a line in the console, or a change
/// to the resources, which makes the `.rock` file need saving.
pub enum ResourceEvent {
    Info(String),
    Error(String),
    Changed,
}

actions!(
    rocktober,
    [
        RunBlock,
        RunAll,
        StopBlock,
        StopAll,
        Save,
        ToggleRecording,
        ToggleComment,
        Quit
    ]
);

/// App-wide setup: theme and key bindings. Call after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);

    // The editor binds cmd-enter and cmd-. itself (in the "Input" context), so
    // ours are bound in that context too; later bindings win.
    //
    // Shift with a key that isn't a letter arrives as the character it types,
    // without the shift: cmd-shift-. is `cmd->` (on a US layout), so that's
    // what to bind. (Enter is the exception: cmd-shift-enter stays itself.)
    for context in [None, Some("Input")] {
        cx.bind_keys([
            KeyBinding::new("cmd-enter", RunBlock, context),
            KeyBinding::new("cmd-shift-enter", RunAll, context),
            KeyBinding::new("cmd-.", StopBlock, context),
            KeyBinding::new("cmd->", StopAll, context),
            KeyBinding::new("cmd-s", Save, context),
            KeyBinding::new("cmd-r", ToggleRecording, context),
            KeyBinding::new("cmd-/", ToggleComment, context),
        ]);
    }
    cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
    cx.on_action(|_: &Quit, cx| cx.quit());
}
