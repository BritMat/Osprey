//! Linux only. Tauri stacks extra web views inside a vertical box on
//! Linux and ignores the positions given to them. The tabs need to sit
//! over the content area while the browser's own interface fills the
//! window, so on Linux every web view is moved into a GtkFixed container
//! that places widgets exactly where they are put.

use gtk::prelude::*;
use tauri::{Webview, Window};

/// The window's GtkFixed, created on first use. Main thread only.
fn fixed_for(window: &Window) -> Option<gtk::Fixed> {
    let vbox = window.default_vbox().ok()?;
    if let Some(f) = vbox
        .children()
        .into_iter()
        .find_map(|c| c.downcast::<gtk::Fixed>().ok())
    {
        return Some(f);
    }
    let fixed = gtk::Fixed::new();
    vbox.pack_start(&fixed, true, true, 0);
    fixed.show();
    Some(fixed)
}

/// Puts a web view at an exact spot and size, in logical pixels.
pub fn place(window: Window, webview: &Webview, x: i32, y: i32, width: i32, height: i32) {
    let _ = webview.with_webview(move |platform| {
        let Some(fixed) = fixed_for(&window) else {
            return;
        };
        let widget: gtk::Widget = platform.inner().upcast();
        let in_fixed = widget
            .parent()
            .is_some_and(|p| p == *fixed.upcast_ref::<gtk::Widget>());
        if in_fixed {
            fixed.move_(&widget, x, y);
        } else {
            if let Some(parent) = widget
                .parent()
                .and_then(|p| p.downcast::<gtk::Container>().ok())
            {
                parent.remove(&widget);
            }
            fixed.put(&widget, x, y);
        }
        widget.set_size_request(width.max(1), height.max(1));
    });
}
