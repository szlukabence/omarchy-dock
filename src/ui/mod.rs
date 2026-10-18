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

use std::path::Path;

use gtk4 as gtk;
use gtk::gdk;
use gtk::prelude::*;

/// Point `img` at the icon a desktop entry names, whatever form it takes: an
/// absolute path, a name the icon theme knows, or a name only found as a file
/// in the icon folders (see [`crate::desktop::icon_file`]). Falls back to the
/// generic application icon.
pub fn set_app_icon(img: &gtk::Image, icon: &str, size: i32) {
    // Loaded at twice the size it is drawn at, to stay sharp on a scaled
    // screen; the image draws it at `size` whatever it holds.
    if icon.starts_with('/') {
        match mislabelled_icon(Path::new(icon), size * 2) {
            Some(t) => img.set_paintable(Some(&t)),
            None => img.set_from_file(Some(icon)),
        }
        return;
    }
    match app_icon_paintable(icon, size * 2) {
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
/// icon theme would not draw it by name: it does not have it but a file by
/// that name exists, or the file it has is not what its name says (see
/// [`crate::desktop::mislabelled_png`]). `None` means "use the theme": either
/// it draws the name, or nothing does.
pub fn app_icon_paintable(icon: &str, size: i32) -> Option<gdk::Paintable> {
    let theme = gtk::IconTheme::for_display(&gdk::Display::default()?);
    if theme.has_icon(icon) {
        let found = theme.lookup_icon(icon, &[], size, 1, gtk::TextDirection::None, gtk::IconLookupFlags::empty());
        let path = found.file()?.path()?;
        return mislabelled_icon(&path, size).map(|t| t.upcast());
    }
    let path = crate::desktop::icon_file(icon)?;
    if let Some(t) = mislabelled_icon(&path, size) {
        return Some(t.upcast());
    }
    // Loaded at the size it is drawn at, not the file's own: AionUi's is
    // 1024 px, which as a drag image would cover half the screen.
    Some(gtk::IconPaintable::for_file(&gtk::gio::File::for_path(path), size, 1).upcast())
}

/// A `.png` icon that holds other data, loaded by what it holds and shrunk
/// to fit `size` pixels. `None` for any other file.
fn mislabelled_icon(path: &Path, size: i32) -> Option<gdk::Texture> {
    if !crate::desktop::mislabelled_png(path) {
        return None;
    }
    let texture = gdk::Texture::from_filename(path).ok()?;
    const FORMAT: gdk::MemoryFormat = gdk::MemoryFormat::R8g8b8a8Premultiplied;
    let mut download = gdk::TextureDownloader::new(&texture);
    download.set_format(FORMAT);
    let (bytes, stride) = download.download_bytes();
    let (w, h) = (texture.width() as usize, texture.height() as usize);
    Some(match shrink(&bytes, w, h, stride, size.max(1) as usize) {
        Some((px, w, h)) => {
            let bytes = gtk::glib::Bytes::from_owned(px);
            gdk::MemoryTexture::new(w as i32, h as i32, FORMAT, &bytes, w * 4).upcast()
        }
        None => texture,
    })
}

/// Premultiplied RGBA pixels shrunk by a whole factor until neither side is
/// over `max`, each new pixel the average of the block it covers. `None` when
/// they already fit.
fn shrink(px: &[u8], w: usize, h: usize, stride: usize, max: usize) -> Option<(Vec<u8>, usize, usize)> {
    let k = w.max(h).div_ceil(max.max(1));
    if k <= 1 {
        return None;
    }
    let (ow, oh) = (w.div_ceil(k), h.div_ceil(k));
    let mut out = Vec::with_capacity(ow * oh * 4);
    for oy in 0..oh {
        for ox in 0..ow {
            let (mut sum, mut n) = ([0u32; 4], 0u32);
            for y in oy * k..((oy + 1) * k).min(h) {
                for x in ox * k..((ox + 1) * k).min(w) {
                    let at = y * stride + x * 4;
                    sum.iter_mut().zip(&px[at..at + 4]).for_each(|(s, v)| *s += u32::from(*v));
                    n += 1;
                }
            }
            out.extend(sum.map(|s| ((s + n / 2) / n) as u8));
        }
    }
    Some((out, ow, oh))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_large_icon_is_shrunk_by_averaging_and_a_small_one_kept() {
        // 4x2 RGBA, stride padded to 20: two 2x2 blocks, one bright, one dark.
        #[rustfmt::skip]
        let px: Vec<u8> = [
            [200, 100, 0, 255], [100, 100, 0, 255], [0, 0, 0, 255], [0, 0, 0, 255],
            [200, 100, 0, 255], [100, 100, 0, 255], [0, 0, 0, 255], [0, 0, 40, 255],
        ]
        .chunks(4)
        .flat_map(|row| row.iter().flatten().copied().chain([0; 4]).collect::<Vec<_>>())
        .collect();
        let (out, w, h) = super::shrink(&px, 4, 2, 20, 2).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, vec![150, 100, 0, 255, 0, 0, 10, 255]);
        assert!(super::shrink(&px, 4, 2, 20, 4).is_none(), "already fits");
    }
}
