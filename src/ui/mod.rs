//! GTK widget tree. Everything here runs on the main thread.

pub mod dock;
pub mod geometry;
pub mod glide;
pub mod menu;
pub mod preview;
pub mod settings;
pub mod stack;

pub use dock::DockSurface;
pub use geometry::Geometry;

use gtk4 as gtk;

/// Point `img` at the icon a desktop entry names, whatever form it takes: an
/// absolute path, a name the icon theme knows, or a name only found as a file
/// in the icon folders (see [`crate::desktop::icon_file`]). Falls back to the
/// generic application icon.
pub fn set_app_icon(img: &gtk::Image, icon: &str, size: i32) {
    if icon.starts_with('/') {
        img.set_from_file(Some(icon));
        return;
    }
    match app_icon_paintable(icon, size) {
        Some(p) => img.set_paintable(Some(&p)),
        None => {
            let has = gtk::gdk::Display::default()
                .map(|d| gtk::IconTheme::for_display(&d))
                .is_some_and(|t| t.has_icon(icon));
            img.set_icon_name(Some(if has { icon } else { "application-x-executable" }));
        }
    }
}

/// The icon a desktop entry names, as a paintable of `size` pixels, when the
/// icon theme does not have it but a file by that name exists. `None` means
/// "use the theme": either it knows the name, or nothing does.
pub fn app_icon_paintable(icon: &str, size: i32) -> Option<gtk::IconPaintable> {
    let theme = gtk::IconTheme::for_display(&gtk::gdk::Display::default()?);
    if theme.has_icon(icon) {
        return None;
    }
    let path = crate::desktop::icon_file(icon)?;
    // Loaded at the size it is drawn at, not the file's own: AionUi's is
    // 1024 px, which as a drag image would cover half the screen.
    Some(gtk::IconPaintable::for_file(&gtk::gio::File::for_path(path), size, 1))
}
