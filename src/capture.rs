//! Window thumbnails, for the hover previews.
//!
//! Hyprland captures any window by its address through
//! `hyprland_toplevel_export_v1` — including windows on workspaces that are
//! not on screen, which is the whole point: with an app open on several
//! workspaces, the preview is how you tell its windows apart. The dock already
//! knows every window's address from Hyprland's own IPC, so no mapping between
//! protocols is needed.
//!
//! Capturing runs on a thread of its own with its own Wayland connection,
//! separate from GTK's. A full-size frame is several megabytes and takes tens
//! of milliseconds, and none of that belongs on the thread that animates the
//! dock. Frames are shrunk to thumbnail size here, before they cross over, so
//! the UI only ever holds small images. Nothing is written to disk.

use std::os::fd::AsFd;
use std::sync::mpsc;
use std::time::Duration;

use wayland_client::protocol::{wl_buffer, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};

#[allow(dead_code, non_camel_case_types, non_upper_case_globals, unused_imports, clippy::all)]
mod proto {
    use wayland_client;
    use wayland_client::protocol::*;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols_wlr::foreign_toplevel::v1::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/hyprland-toplevel-export-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/hyprland-toplevel-export-v1.xml");
}
use proto::hyprland_toplevel_export_frame_v1 as frame;
use proto::hyprland_toplevel_export_manager_v1 as manager;

/// A capture to perform.
#[derive(Debug, Clone)]
pub struct Request {
    /// The window's Hyprland address, as `hyprctl clients` shows it.
    pub address: u64,
    /// Bounding box for the thumbnail, in pixels.
    pub max_w: u32,
    pub max_h: u32,
    /// Echoed back, so a stale answer to an earlier hover can be ignored.
    pub token: u64,
}

/// A thumbnail, as premultiplied BGRA — GDK's `B8g8r8a8Premultiplied`, which
/// is what Wayland's `argb8888` is in memory on a little-endian machine.
#[derive(Debug, Clone)]
pub struct Frame {
    pub address: u64,
    pub token: u64,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Handle to the capture thread.
#[derive(Clone)]
pub struct Capturer {
    requests: mpsc::Sender<Request>,
}

impl Capturer {
    /// Start the capture thread. Frames arrive on `out`. Returns `None` when
    /// the compositor offers no toplevel export, i.e. not under Hyprland.
    pub fn spawn(out: async_channel::Sender<Frame>) -> Option<Self> {
        let (tx, rx) = mpsc::channel::<Request>();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        std::thread::Builder::new()
            .name("omarchy-dock-capture".into())
            .spawn(move || match Session::connect() {
                Some(mut session) => {
                    let _ = ready_tx.send(true);
                    for req in rx {
                        match session.capture(&req) {
                            Ok(Some(frame)) => {
                                if out.send_blocking(frame).is_err() {
                                    break;
                                }
                            }
                            Ok(None) => tracing::debug!(address = req.address, "window gone"),
                            Err(e) => tracing::debug!(error = %e, "capture failed"),
                        }
                    }
                }
                None => {
                    let _ = ready_tx.send(false);
                }
            })
            .ok()?;
        ready_rx.recv_timeout(Duration::from_secs(2)).ok()?.then_some(Self { requests: tx })
    }

    pub fn request(&self, req: Request) {
        let _ = self.requests.send(req);
    }
}

// ── Wayland plumbing ────────────────────────────────────────────────────────

#[derive(Default)]
struct Globals {
    shm: Option<wl_shm::WlShm>,
    manager: Option<manager::HyprlandToplevelExportManagerV1>,
}

/// State of the one frame in flight.
#[derive(Default)]
struct Pending {
    format: Option<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

#[derive(Default)]
struct State {
    globals: Globals,
    pending: Pending,
}

struct Session {
    queue: EventQueue<State>,
    state: State,
}

impl Session {
    fn connect() -> Option<Self> {
        let conn = Connection::connect_to_env().ok()?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue.roundtrip(&mut state).ok()?;
        if state.globals.shm.is_none() || state.globals.manager.is_none() {
            tracing::info!("compositor offers no toplevel export; window previews disabled");
            return None;
        }
        Some(Self { queue, state })
    }

    /// Capture one window. `Ok(None)` when the window no longer exists.
    fn capture(&mut self, req: &Request) -> anyhow::Result<Option<Frame>> {
        let qh = self.queue.handle();
        self.state.pending = Pending::default();
        let manager = self.state.globals.manager.clone().expect("checked at connect");
        // The protocol takes the low 32 bits of the address.
        let frame = manager.capture_toplevel(0, (req.address & 0xffff_ffff) as u32, &qh, ());

        while !self.state.pending.buffer_done && !self.state.pending.failed {
            self.queue.blocking_dispatch(&mut self.state)?;
        }
        if self.state.pending.failed {
            frame.destroy();
            return Ok(None);
        }
        let Some((format, w, h, stride)) = self.state.pending.format else {
            frame.destroy();
            anyhow::bail!("no shm format offered");
        };
        anyhow::ensure!(
            matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888),
            "unsupported format {format:?}"
        );

        // Shared memory the compositor copies into: an unlinked file in the
        // runtime dir, so it lives only as long as this mapping. Created fresh
        // and readable by this user alone — it holds a window's pixels — and
        // never opened through whatever might already sit at that path.
        let size = (stride * h) as usize;
        let path = std::env::var("XDG_RUNTIME_DIR").map(std::path::PathBuf::from)?
            .join(format!("omarchy-dock-capture-{}", std::process::id()));
        std::fs::remove_file(&path).ok();
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?
        };
        std::fs::remove_file(&path).ok();
        file.set_len(size as u64)?;

        let shm = self.state.globals.shm.clone().expect("checked at connect");
        let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
        let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, &qh, ());
        frame.copy(&buffer, 1);

        while !self.state.pending.ready && !self.state.pending.failed {
            self.queue.blocking_dispatch(&mut self.state)?;
        }
        let ok = self.state.pending.ready;
        frame.destroy();
        buffer.destroy();
        pool.destroy();
        if !ok {
            return Ok(None);
        }

        let map = unsafe { memmap2::Mmap::map(&file)? };
        let opaque = format == wl_shm::Format::Xrgb8888;
        let (tw, th, pixels) =
            shrink(&map, w, h, stride, req.max_w, req.max_h, self.state.pending.y_invert, opaque);
        Ok(Some(Frame { address: req.address, token: req.token, width: tw, height: th, pixels }))
    }
}

/// Box-filter `src` (BGRA, `stride` bytes per row) down to fit `max_w`×`max_h`,
/// keeping its aspect ratio; never upscaled. Flips vertically if the frame is
/// y-inverted, and forces alpha opaque for formats that carry none.
#[allow(clippy::too_many_arguments)]
fn shrink(
    src: &[u8],
    w: u32,
    h: u32,
    stride: u32,
    max_w: u32,
    max_h: u32,
    y_invert: bool,
    opaque: bool,
) -> (u32, u32, Vec<u8>) {
    let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64).min(1.0);
    let tw = ((w as f64 * scale).round() as u32).max(1);
    let th = ((h as f64 * scale).round() as u32).max(1);
    let mut out = vec![0u8; (tw * th * 4) as usize];

    for ty in 0..th {
        let y0 = (ty as u64 * h as u64 / th as u64) as u32;
        let y1 = (((ty + 1) as u64 * h as u64 / th as u64) as u32).max(y0 + 1);
        for tx in 0..tw {
            let x0 = (tx as u64 * w as u64 / tw as u64) as u32;
            let x1 = (((tx + 1) as u64 * w as u64 / tw as u64) as u32).max(x0 + 1);
            let mut acc = [0u32; 4];
            for sy in y0..y1 {
                let row = if y_invert { h - 1 - sy } else { sy };
                let base = (row * stride) as usize;
                for sx in x0..x1 {
                    let i = base + (sx * 4) as usize;
                    for c in 0..4 {
                        acc[c] += src[i + c] as u32;
                    }
                }
            }
            let n = (y1 - y0) * (x1 - x0);
            let o = ((ty * tw + tx) * 4) as usize;
            for c in 0..4 {
                out[o + c] = (acc[c] / n) as u8;
            }
            if opaque {
                out[o + 3] = 255;
            }
        }
    }
    (tw, th, out)
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_shm" => state.globals.shm = Some(registry.bind(name, 1, qh, ())),
                "hyprland_toplevel_export_manager_v1" => {
                    state.globals.manager = Some(registry.bind(name, version.min(1), qh, ()));
                }
                _ => {}
            }
        }
    }
}

macro_rules! ignore_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, manager::HyprlandToplevelExportManagerV1);

impl Dispatch<frame::HyprlandToplevelExportFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &frame::HyprlandToplevelExportFrameV1,
        event: frame::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let p = &mut state.pending;
        match event {
            frame::Event::Buffer { format: WEnum::Value(format), width, height, stride } => {
                // Several buffer types may be offered; the first shm one wins.
                if p.format.is_none() {
                    p.format = Some((format, width, height, stride));
                }
            }
            frame::Event::Flags { flags } => {
                p.y_invert = u32::from(flags) & 1 != 0;
            }
            frame::Event::BufferDone => p.buffer_done = true,
            frame::Event::Ready { .. } => p.ready = true,
            frame::Event::Failed => p.failed = true,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
        (0..w * h).flat_map(|_| px).collect()
    }

    #[test]
    fn a_frame_shrinks_to_fit_keeping_its_aspect() {
        let src = solid(400, 200, [10, 20, 30, 255]);
        let (w, h, px) = shrink(&src, 400, 200, 400 * 4, 100, 100, false, false);
        assert_eq!((w, h), (100, 50));
        assert_eq!(px.len(), 100 * 50 * 4);
        assert_eq!(&px[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn a_small_frame_is_never_upscaled() {
        let src = solid(40, 20, [1, 2, 3, 4]);
        let (w, h, _) = shrink(&src, 40, 20, 160, 400, 400, false, false);
        assert_eq!((w, h), (40, 20));
    }

    #[test]
    fn stride_padding_is_skipped() {
        // 2x1 image in rows padded to 16 bytes; the padding must not bleed in.
        let mut src = vec![0xff; 16];
        src[..8].copy_from_slice(&[1, 1, 1, 255, 3, 3, 3, 255]);
        let (w, h, px) = shrink(&src, 2, 1, 16, 1, 1, false, false);
        assert_eq!((w, h), (1, 1));
        assert_eq!(&px, &[2, 2, 2, 255]);
    }

    #[test]
    fn y_inverted_frames_are_flipped_and_xrgb_is_made_opaque() {
        // Top row black, bottom row white; inverted, the thumbnail's top is white.
        let mut src = solid(1, 2, [0, 0, 0, 0]);
        src[4..8].copy_from_slice(&[255, 255, 255, 0]);
        let (_, _, px) = shrink(&src, 1, 2, 4, 1, 2, true, true);
        assert_eq!(&px[..4], &[255, 255, 255, 255]);
        assert_eq!(&px[4..], &[0, 0, 0, 255]);
    }
}
