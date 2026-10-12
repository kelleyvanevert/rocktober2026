// Make Cmd+Z, Cmd+Shift+Z, Cmd+X/C/V/A and Cmd+/ reach the editor instead of the host.
//
// On macOS, AppKit first offers Cmd+key presses to the key window's views through
// `performKeyEquivalent:`, and only sends them as normal key presses if nobody (including the
// app's menu) wants them. baseview's view doesn't implement that method, so Ableton's Edit menu
// got Cmd+Z and undid things in the set. We add the method to baseview's view class (a fresh,
// uniquely named class per window) and pass these shortcuts on as key presses, which egui
// already knows how to handle.

use nih_plug::prelude::{Editor, GuiContext, ParentWindowHandle};
use std::any::Any;
use std::sync::Arc;

/// Wraps an editor so its window handles the text-editing shortcuts itself.
pub struct EditingShortcuts(pub Box<dyn Editor>);

impl Editor for EditingShortcuts {
    fn spawn(&self, parent: ParentWindowHandle, context: Arc<dyn GuiContext>) -> Box<dyn Any + Send> {
        let window = self.0.spawn(parent, context);
        #[cfg(target_os = "macos")]
        if let ParentWindowHandle::AppKitNsView(view) = parent {
            // baseview has added its view to the host's view by now.
            unsafe { macos::claim_shortcuts(view) };
        }
        window
    }

    fn size(&self) -> (u32, u32) {
        self.0.size()
    }

    fn set_scale_factor(&self, factor: f32) -> bool {
        self.0.set_scale_factor(factor)
    }

    fn param_value_changed(&self, id: &str, normalized_value: f32) {
        self.0.param_value_changed(id, normalized_value)
    }

    fn param_modulation_changed(&self, id: &str, modulation_offset: f32) {
        self.0.param_modulation_changed(id, modulation_offset)
    }

    fn param_values_changed(&self) {
        self.0.param_values_changed()
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc::runtime::{BOOL, Class, Imp, NO, Object, Sel, YES, class_addMethod, class_getName, object_getClass};
    use objc::{msg_send, sel, sel_impl};
    use std::ffi::{CStr, c_char, c_void};

    type Id = *mut Object;

    /// Keys (as typed without modifiers, lowercased) that we take when Cmd is held.
    const SHORTCUTS: [&str; 6] = ["z", "x", "c", "v", "a", "/"];

    // NSEventModifierFlags
    const COMMAND: u64 = 1 << 20;
    const CONTROL: u64 = 1 << 18;
    const OPTION: u64 = 1 << 19;

    /// Objective-C type encoding of `- (BOOL)performKeyEquivalent:(NSEvent *)event`.
    #[cfg(target_arch = "aarch64")]
    const TYPES: &CStr = c"B@:@";
    #[cfg(not(target_arch = "aarch64"))]
    const TYPES: &CStr = c"c@:@";

    /// # Safety
    /// `parent` must be the `NSView` that baseview just opened its view in.
    pub unsafe fn claim_shortcuts(parent: *mut c_void) {
        unsafe {
            let subviews: Id = msg_send![parent as Id, subviews];
            let count: usize = msg_send![subviews, count];
            for i in 0..count {
                let view: Id = msg_send![subviews, objectAtIndex: i];
                let class = object_getClass(view) as *mut Class;
                if CStr::from_ptr(class_getName(class)).to_bytes().starts_with(b"BaseviewNSView_") {
                    let imp = std::mem::transmute::<extern "C" fn(&Object, Sel, Id) -> BOOL, Imp>(perform_key_equivalent);
                    // Fails harmlessly if the class already has it.
                    class_addMethod(class, sel!(performKeyEquivalent:), imp, TYPES.as_ptr());
                }
            }
        }
    }

    extern "C" fn perform_key_equivalent(this: &Object, _: Sel, event: Id) -> BOOL {
        unsafe {
            // Only while our view has keyboard focus (the user clicked into the editor).
            let window: Id = msg_send![this, window];
            let first_responder: Id = msg_send![window, firstResponder];
            if !std::ptr::eq(first_responder, this) {
                return NO;
            }

            let flags: u64 = msg_send![event, modifierFlags];
            if flags & COMMAND == 0 || flags & (CONTROL | OPTION) != 0 {
                return NO;
            }
            let chars: Id = msg_send![event, charactersIgnoringModifiers];
            let utf8: *const c_char = if chars.is_null() { std::ptr::null() } else { msg_send![chars, UTF8String] };
            if utf8.is_null() {
                return NO;
            }
            let key = CStr::from_ptr(utf8).to_string_lossy().to_lowercase();
            if !SHORTCUTS.contains(&key.as_str()) {
                return NO;
            }

            let () = msg_send![this, keyDown: event];
            YES
        }
    }
}
