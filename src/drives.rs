//! Removable drives: USB sticks, external disks, SD cards, phones and cameras.
//!
//! Read through GIO's volume monitor, which gvfs backs with udisks2 — the same
//! view of the hardware the file manager's sidebar has, including phones over
//! MTP, and the same polkit rules for mounting as the logged-in user. The
//! dock mounts only when a drive is clicked, and never writes to one.
//!
//! Main thread only: GIO objects are not `Send`. What crosses into the dock's
//! state is the plain [`Drive`] value.

use gtk4 as gtk;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use std::cell::RefCell;
use std::rc::Rc;

use crate::event::{AppEvent, Sender};

/// One drive, as the dock shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    /// Stable while the drive stays plugged in; see `volume_id`.
    pub id: String,
    /// What the file manager calls it: the filesystem label, or a size.
    pub name: String,
    /// Themed icon name, e.g. `drive-removable-media-usb`.
    pub icon: String,
    pub mounted: bool,
    /// Whether it can be ejected, rather than only unmounted.
    pub can_eject: bool,
}

/// Whether a volume belongs in the dock.
///
/// `drive` is its drive's `(is_removable, is_media_removable)`, when it has
/// one. A volume on a removable drive, or on a drive whose media comes out,
/// qualifies — a USB stick, a card in a reader. Without a drive, only a
/// `device` volume does: that is how gvfs presents phones and cameras. Every
/// internal disk, and every network share, is left out.
fn is_removable(drive: Option<(bool, bool)>, class: Option<&str>) -> bool {
    match drive {
        Some((removable, media_removable)) => removable || media_removable,
        None => class == Some("device"),
    }
}

/// Fallback when a volume's icon is not a themed one.
const DEFAULT_ICON: &str = "drive-removable-media";

/// A volume's id: its device node first, which is unique among what is
/// plugged in right now even for two cloned sticks sharing a UUID; then the
/// UUID; then, for the odd volume with neither, its name.
fn volume_id(v: &gio::Volume) -> String {
    v.identifier("unix-device")
        .or_else(|| v.uuid())
        .map(|s| s.to_string())
        .unwrap_or_else(|| v.name().to_string())
}

fn icon_name(icon: &gio::Icon) -> String {
    icon.downcast_ref::<gio::ThemedIcon>()
        .and_then(|t| t.names().first().map(|n| n.to_string()))
        .unwrap_or_else(|| DEFAULT_ICON.to_string())
}

/// The dock's view of a volume, or `None` if it is not removable.
fn describe(v: &gio::Volume) -> Option<Drive> {
    let drive = v.drive().map(|d| (d.is_removable(), d.is_media_removable()));
    if !is_removable(drive, v.identifier("class").as_deref()) {
        return None;
    }
    let mount = v.get_mount();
    Some(Drive {
        id: volume_id(v),
        name: v.name().to_string(),
        icon: icon_name(&v.icon()),
        mounted: mount.is_some(),
        can_eject: mount.as_ref().map_or_else(|| v.can_eject(), |m| m.can_eject()),
    })
}

fn snapshot(monitor: &gio::VolumeMonitor) -> Vec<Drive> {
    monitor.volumes().iter().filter_map(describe).collect()
}

fn volume(id: &str) -> Option<gio::Volume> {
    gio::VolumeMonitor::get()
        .volumes()
        .into_iter()
        .find(|v| describe(v).is_some_and(|d| d.id == id))
}

/// The drive with this id, as it is right now.
pub fn find(id: &str) -> Option<Drive> {
    volume(id).and_then(|v| describe(&v))
}

/// Watches for drives coming and going while it lives.
///
/// Sends the whole list as [`AppEvent::Drives`] once at start and again
/// whenever it changes. Dropping it stops the watching.
pub struct DriveWatcher {
    monitor: gio::VolumeMonitor,
    handlers: Vec<glib::SignalHandlerId>,
}

impl DriveWatcher {
    pub fn start(tx: Sender) -> Self {
        let monitor = gio::VolumeMonitor::get();
        let last: RefCell<Option<Vec<Drive>>> = RefCell::new(None);
        // Mounting alone fires volume-changed and mount-added; only a list
        // that actually differs is worth a dock update.
        let publish = Rc::new(move |m: &gio::VolumeMonitor| {
            let now = snapshot(m);
            if last.borrow().as_ref() == Some(&now) {
                return;
            }
            *last.borrow_mut() = Some(now.clone());
            let _ = tx.try_send(AppEvent::Drives(now));
        });

        let mut handlers = Vec::new();
        let p = publish.clone();
        handlers.push(monitor.connect_volume_added(move |m, _| p(m)));
        let p = publish.clone();
        handlers.push(monitor.connect_volume_removed(move |m, _| p(m)));
        let p = publish.clone();
        handlers.push(monitor.connect_volume_changed(move |m, _| p(m)));
        let p = publish.clone();
        handlers.push(monitor.connect_mount_added(move |m, _| p(m)));
        let p = publish.clone();
        handlers.push(monitor.connect_mount_removed(move |m, _| p(m)));
        let p = publish.clone();
        handlers.push(monitor.connect_mount_changed(move |m, _| p(m)));

        publish(&monitor);
        Self { monitor, handlers }
    }
}

impl Drop for DriveWatcher {
    fn drop(&mut self) {
        for id in self.handlers.drain(..) {
            self.monitor.disconnect(id);
        }
    }
}

/// Asks for a password or shows what is keeping a drive busy, in GTK's own
/// dialogs. Unparented: a layer surface cannot parent a dialog window.
fn operation() -> gtk::MountOperation {
    gtk::MountOperation::new(None::<&gtk::Window>)
}

/// Show a mounted drive in the file manager.
fn show(mount: &gio::Mount) {
    let root = mount.root();
    match root.path() {
        Some(path) => crate::stacks::open(&path),
        // A phone over MTP has no local path unless gvfs's FUSE bridge runs.
        None => {
            if let Err(e) =
                gio::AppInfo::launch_default_for_uri(&root.uri(), None::<&gio::AppLaunchContext>)
            {
                tracing::warn!(uri = %root.uri(), error = %e, "cannot open drive");
            }
        }
    }
}

/// Cancelling a password prompt or a busy dialog is a choice, not a failure,
/// and a "handled" error is one GTK already showed.
fn is_quiet(e: &glib::Error) -> bool {
    e.matches(gio::IOErrorEnum::Cancelled) || e.matches(gio::IOErrorEnum::FailedHandled)
}

fn report_failure(verb: &str, name: &str, e: &glib::Error) {
    if is_quiet(e) {
        return;
    }
    tracing::warn!(%name, error = %e, "cannot {verb} drive");
    crate::omarchy::notify(&format!("Couldn't {verb} {name}"), Some(e.message()), None);
}

/// Open a drive in the file manager, mounting it first if need be.
pub fn open(id: &str) {
    let Some(v) = volume(id) else { return };
    if let Some(m) = v.get_mount() {
        show(&m);
        return;
    }
    let name = v.name().to_string();
    let vol = v.clone();
    v.mount(
        gio::MountMountFlags::NONE,
        Some(&operation()),
        None::<&gio::Cancellable>,
        move |r| match r {
            Ok(()) => {
                if let Some(m) = vol.get_mount() {
                    show(&m);
                }
            }
            Err(e) => report_failure("open", &name, &e),
        },
    );
}

/// Eject a drive, or unmount it when it cannot be ejected, and say when it is
/// safe to pull out.
pub fn eject(id: &str) {
    let Some(v) = volume(id) else { return };
    let name = v.name().to_string();
    let glyph = crate::state::drive_glyph(&icon_name(&v.icon()));
    let done = move |r: Result<(), glib::Error>| match r {
        Ok(()) => crate::omarchy::notify(&format!("Safe to remove {name}"), None, Some(glyph)),
        Err(e) => report_failure("eject", &name, &e),
    };
    let flags = gio::MountUnmountFlags::NONE;
    let none = None::<&gio::Cancellable>;
    match v.get_mount() {
        Some(m) if m.can_eject() => m.eject_with_operation(flags, Some(&operation()), none, done),
        Some(m) if m.can_unmount() => {
            m.unmount_with_operation(flags, Some(&operation()), none, done)
        }
        Some(_) => {}
        None if v.can_eject() => v.eject_with_operation(flags, Some(&operation()), none, done),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removable_drives_and_media_are_kept() {
        assert!(is_removable(Some((true, false)), Some("device")));
        assert!(is_removable(Some((false, true)), Some("device")));
    }

    #[test]
    fn internal_disks_are_left_out() {
        // The NVMe's Windows and Data partitions: a fixed drive.
        assert!(!is_removable(Some((false, false)), Some("device")));
    }

    #[test]
    fn a_phone_without_a_drive_is_kept_but_a_network_share_is_not() {
        assert!(is_removable(None, Some("device")));
        assert!(!is_removable(None, Some("network")));
        assert!(!is_removable(None, None));
    }
}
