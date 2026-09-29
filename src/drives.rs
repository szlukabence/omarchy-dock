//! Removable drives: USB sticks, external disks, SD cards, phones and cameras.
//!
//! Read through GIO's volume monitor, which gvfs backs with udisks2 — the same
//! view of the hardware the file manager's sidebar has, including phones over
//! MTP, and the same polkit rules for mounting as the logged-in user. The
//! dock mounts only when a drive is clicked, and never writes to one.
//!
//! Main thread only: GIO objects are not `Send`. What crosses into the dock's
//! state is the plain [`Drive`] value, one per physical device; grouping
//! volumes into devices and telling what kind each is happen, GIO-free, in
//! `state::drive`.

use gtk4 as gtk;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use crate::event::{AppEvent, Sender};
use crate::state::drive::{DeviceInfo, VolumeInfo};

pub use crate::state::drive::Drive;

/// URI schemes of the gvfs backends for phones and cameras: MTP, PTP
/// cameras, and iPhones over AFC.
const DEVICE_SCHEMES: [&str; 3] = ["mtp", "gphoto2", "afc"];

/// Whether a volume belongs in the dock.
///
/// `drive` is its drive's `(is_removable, is_media_removable)`, when it has
/// one. A volume on a removable drive, or on a drive whose media comes out,
/// qualifies — a USB stick, a card in a reader. Without a drive, `scheme` is
/// its activation root's URI scheme: gvfs presents phones and cameras that
/// way, with no drive at all. A driveless udisks volume — LVM, RAID — has no
/// activation root, so it is left out along with every internal disk and
/// every network share.
fn is_removable(drive: Option<(bool, bool)>, scheme: Option<&str>) -> bool {
    match drive {
        Some((removable, media_removable)) => removable || media_removable,
        None => scheme.is_some_and(|s| DEVICE_SCHEMES.contains(&s)),
    }
}

/// A volume's id: its device node first, which is unique among what is
/// plugged in right now even for two cloned sticks sharing a UUID; then the
/// UUID; then, for the odd volume with neither, its name.
fn volume_id(v: &gio::Volume) -> String {
    v.identifier("unix-device")
        .or_else(|| v.uuid())
        .map(|s| s.to_string())
        .unwrap_or_else(|| v.name().to_string())
}

/// A drive's id: its device node, `/dev/sdb`, else its name.
fn drive_id(d: &gio::Drive) -> String {
    d.identifier("unix-device")
        .map(|s| s.to_string())
        .unwrap_or_else(|| d.name().to_string())
}

/// Every name in a themed icon, specific first, fallbacks after.
fn icon_names(icon: &gio::Icon) -> Vec<String> {
    icon.downcast_ref::<gio::ThemedIcon>()
        .map(|t| t.names().iter().map(|n| n.to_string()).collect())
        .unwrap_or_default()
}

/// What the dock needs to know about a volume, or `None` if it is not
/// removable.
fn read(v: &gio::Volume) -> Option<VolumeInfo> {
    let drive = v.drive();
    let flags = drive.as_ref().map(|d| (d.is_removable(), d.is_media_removable()));
    let scheme = v.activation_root().and_then(|root| root.uri_scheme());
    if !is_removable(flags, scheme.as_deref()) {
        return None;
    }
    let mount = v.get_mount();
    Some(VolumeInfo {
        id: volume_id(v),
        name: v.name().to_string(),
        icons: icon_names(&v.icon()),
        mounted: mount.is_some(),
        can_eject: mount.as_ref().map_or_else(|| v.can_eject(), |m| m.can_eject()),
        can_unmount: mount.as_ref().is_some_and(|m| m.can_unmount()),
        device: drive.map(|d| DeviceInfo {
            id: drive_id(&d),
            name: d.name().to_string(),
            icons: icon_names(&d.icon()),
            media_removable: d.is_media_removable(),
            can_eject: d.can_eject(),
        }),
    })
}

/// Whether the icon theme in use can draw `name`.
fn has_icon(name: &str) -> bool {
    gtk::gdk::Display::default().is_some_and(|d| gtk::IconTheme::for_display(&d).has_icon(name))
}

fn snapshot(monitor: &gio::VolumeMonitor) -> Vec<Drive> {
    let volumes = monitor.volumes().iter().filter_map(read).collect();
    crate::state::drive::group(volumes, has_icon)
}

/// The partition with this id.
fn volume(id: &str) -> Option<gio::Volume> {
    gio::VolumeMonitor::get()
        .volumes()
        .into_iter()
        .find(|v| read(v).is_some_and(|i| i.id == id))
}

/// Every partition on the device with this id.
fn volumes_of(id: &str) -> Vec<gio::Volume> {
    gio::VolumeMonitor::get()
        .volumes()
        .into_iter()
        .filter(|v| read(v).is_some_and(|i| i.device_id() == id))
        .collect()
}

/// The device with this id, as it is right now.
pub fn find(id: &str) -> Option<Drive> {
    snapshot(&gio::VolumeMonitor::get()).into_iter().find(|d| d.id == id)
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

thread_local! {
    /// Drives with a mount, unmount or eject still under way.
    static BUSY: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Claim a drive for one operation. `false` while another is still running:
/// gvfs would refuse the second one, and that refusal is no news to report.
fn begin(id: &str) -> bool {
    BUSY.with(|b| b.borrow_mut().insert(id.to_string()))
}

fn end(id: &str) {
    BUSY.with(|b| b.borrow_mut().remove(id));
}

/// Open a partition in the file manager, mounting it first if need be.
pub fn open(id: &str) {
    let Some(v) = volume(id) else { return };
    if let Some(m) = v.get_mount() {
        show(&m);
        return;
    }
    if !begin(id) {
        return;
    }
    let (id, name) = (id.to_string(), v.name().to_string());
    let vol = v.clone();
    v.mount(
        gio::MountMountFlags::NONE,
        Some(&operation()),
        None::<&gio::Cancellable>,
        move |r| {
            end(&id);
            match r {
                Ok(()) => {
                    if let Some(m) = vol.get_mount() {
                        show(&m);
                    }
                }
                Err(e) => report_failure("open", &name, &e),
            }
        },
    );
}

/// A callback shared by the `parts` operations of one eject, which speaks
/// once they have all finished: "Safe to remove" only if every one worked,
/// otherwise the first real failure.
fn finisher(
    id: String,
    name: String,
    glyph: &'static str,
    parts: usize,
) -> Rc<dyn Fn(Result<(), glib::Error>)> {
    let (left, failed) = (Cell::new(parts), Cell::new(false));
    Rc::new(move |r| {
        if let Err(e) = r {
            if !failed.replace(true) {
                report_failure("eject", &name, &e);
            }
        }
        left.set(left.get().saturating_sub(1));
        if left.get() == 0 {
            end(&id);
            if !failed.get() {
                crate::omarchy::notify(&format!("Safe to remove {name}"), None, Some(glyph));
            }
        }
    })
}

/// Eject a whole device — every partition on it — or, when it cannot be
/// ejected, unmount every partition that is mounted; then say when it is
/// safe to pull out.
pub fn eject(id: &str) {
    let Some(drive) = find(id) else { return };
    let volumes = volumes_of(id);
    let Some(first) = volumes.first() else { return };
    if !(drive.can_eject || drive.can_unmount) || !begin(id) {
        return;
    }
    let flags = gio::MountUnmountFlags::NONE;
    let none = None::<&gio::Cancellable>;
    let glyph = drive.kind.glyph();

    if drive.can_eject {
        let done = finisher(id.to_string(), drive.name, glyph, 1);
        let done = move |r| done(r);
        match (first.drive(), first.get_mount()) {
            // gvfs unmounts every partition on the drive before ejecting it.
            (Some(d), _) => d.eject_with_operation(flags, Some(&operation()), none, done),
            // A phone or camera: its one volume is the device.
            (None, Some(m)) if m.can_eject() => {
                m.eject_with_operation(flags, Some(&operation()), none, done)
            }
            (None, _) => first.eject_with_operation(flags, Some(&operation()), none, done),
        }
        return;
    }

    let mounts: Vec<gio::Mount> =
        volumes.iter().filter_map(|v| v.get_mount()).filter(|m| m.can_unmount()).collect();
    if mounts.is_empty() {
        end(id);
        return;
    }
    let done = finisher(id.to_string(), drive.name, glyph, mounts.len());
    for m in mounts {
        let done = done.clone();
        m.unmount_with_operation(flags, Some(&operation()), none, move |r| done(r));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removable_drives_and_media_are_kept() {
        assert!(is_removable(Some((true, false)), None));
        assert!(is_removable(Some((false, true)), None));
    }

    #[test]
    fn internal_disks_are_left_out() {
        // The NVMe's Windows and Data partitions: a fixed drive.
        assert!(!is_removable(Some((false, false)), None));
    }

    #[test]
    fn phones_and_cameras_are_kept_by_their_gvfs_scheme() {
        // gvfs gives MTP, gphoto2 and AFC volumes no drive and no class, only
        // an activation root.
        assert!(is_removable(None, Some("mtp")));
        assert!(is_removable(None, Some("gphoto2")));
        assert!(is_removable(None, Some("afc")));
    }

    #[test]
    fn driveless_internal_volumes_and_shares_are_left_out() {
        // An LVM or RAID volume: no udisks drive, no activation root.
        assert!(!is_removable(None, None));
        assert!(!is_removable(None, Some("smb")));
        assert!(!is_removable(None, Some("nfs")));
    }

    #[test]
    fn a_second_click_waits_for_the_first() {
        assert!(begin("sdx1"));
        // A double-click, or a click while the password dialog is up: gvfs
        // would refuse it with "a mount operation is already pending".
        assert!(!begin("sdx1"));
        assert!(begin("sdy1"), "another drive is not held up");
        end("sdx1");
        assert!(begin("sdx1"));
        end("sdx1");
        end("sdy1");
    }
}
