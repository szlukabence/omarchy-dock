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
use std::path::Path;
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
        device: drive.map(|d| device_info(&d)),
    })
}

fn device_info(d: &gio::Drive) -> DeviceInfo {
    DeviceInfo {
        id: drive_id(d),
        name: d.name().to_string(),
        icons: icon_names(&d.icon()),
        media_removable: d.is_media_removable(),
        can_eject: d.can_eject(),
    }
}

/// Partitions gvfs keeps no volume for but which are mounted anyway, with
/// the removable drive each is on.
///
/// udisks tells gvfs to hide some partitions — Ventoy's `VTOYEFI`, recovery
/// partitions — so they have no volume and their mount belongs to no drive.
/// Mounted by hand, they are still on the stick: found through the kernel's
/// mount table and sysfs, so the picker lists them and Eject unmounts them.
fn loose_mounts(monitor: &gio::VolumeMonitor) -> Vec<(gio::Mount, VolumeInfo)> {
    let loose: Vec<gio::Mount> =
        monitor.mounts().into_iter().filter(|m| m.volume().is_none()).collect();
    if loose.is_empty() {
        return Vec::new();
    }
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    let drives = monitor.connected_drives();
    loose
        .into_iter()
        .filter_map(|m| {
            let point = m.root().path()?;
            let source = std::fs::canonicalize(mount_source(&mountinfo, &point)?).ok()?;
            let name = source.file_name()?.to_str()?.to_string();
            let sysfs = std::fs::canonicalize(format!("/sys/class/block/{name}")).ok()?;
            let disk = format!("/dev/{}", disk_of(&sysfs)?);
            let drive = drives.iter().find(|d| drive_id(d) == disk)?;
            if !is_removable(Some((drive.is_removable(), drive.is_media_removable())), None) {
                return None;
            }
            let info = VolumeInfo {
                id: source.to_string_lossy().into_owned(),
                name: m.name().to_string(),
                icons: icon_names(&m.icon()),
                mounted: true,
                can_eject: false,
                can_unmount: m.can_unmount(),
                device: Some(device_info(drive)),
            };
            Some((m, info))
        })
        .collect()
}

/// Whether the icon theme in use can draw `name`.
fn has_icon(name: &str) -> bool {
    gtk::gdk::Display::default().is_some_and(|d| gtk::IconTheme::for_display(&d).has_icon(name))
}

fn snapshot(monitor: &gio::VolumeMonitor) -> Vec<Drive> {
    let mut volumes: Vec<VolumeInfo> = monitor.volumes().iter().filter_map(read).collect();
    volumes.extend(loose_mounts(monitor).into_iter().map(|(_, info)| info));
    crate::state::drive::group(volumes, has_icon)
}

/// The drive with this device id.
fn gio_drive(id: &str) -> Option<gio::Drive> {
    gio::VolumeMonitor::get().connected_drives().into_iter().find(|d| drive_id(d) == id)
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
    let Some(v) = volume(id) else {
        let loose = loose_mounts(&gio::VolumeMonitor::get());
        if let Some((m, _)) = loose.into_iter().find(|(_, i)| i.id == id) {
            show(&m);
        }
        return;
    };
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

/// A callback shared by the `parts` operations of one step of an eject. Once
/// all have finished it runs `then` if every one worked; otherwise it reports
/// the first real failure and releases the device.
fn after_all(
    id: String,
    name: String,
    parts: usize,
    then: impl FnOnce() + 'static,
) -> Rc<dyn Fn(Result<(), glib::Error>)> {
    let then = RefCell::new(Some(then));
    let (left, failed) = (Cell::new(parts), Cell::new(false));
    Rc::new(move |r| {
        if let Err(e) = r {
            if !failed.replace(true) {
                report_failure("eject", &name, &e);
            }
        }
        left.set(left.get().saturating_sub(1));
        if left.get() == 0 {
            if failed.get() {
                end(&id);
            } else if let Some(then) = then.borrow_mut().take() {
                then();
            }
        }
    })
}

/// The last step of every eject: release the device and say so.
fn safe_to_remove(id: String, name: String, glyph: &'static str) -> impl FnOnce() {
    move || {
        end(&id);
        crate::omarchy::notify(&format!("Safe to remove {name}"), None, Some(glyph));
    }
}

/// Unmount each of `mounts`, then run `then` if all of them went.
fn unmount_all(id: &str, name: &str, mounts: Vec<gio::Mount>, then: impl FnOnce() + 'static) {
    if mounts.is_empty() {
        then();
        return;
    }
    let done = after_all(id.to_string(), name.to_string(), mounts.len(), then);
    for m in mounts {
        let done = done.clone();
        m.unmount_with_operation(
            gio::MountUnmountFlags::NONE,
            Some(&operation()),
            None::<&gio::Cancellable>,
            move |r| done(r),
        );
    }
}

/// Eject the device itself. gvfs unmounts every partition it has a volume
/// for first; the ones it hides are the caller's to unmount.
fn eject_device(id: String, name: String, glyph: &'static str) {
    let flags = gio::MountUnmountFlags::NONE;
    let none = None::<&gio::Cancellable>;
    let first = volumes_of(&id).into_iter().next();
    let done = after_all(id.clone(), name.clone(), 1, safe_to_remove(id.clone(), name, glyph));
    let done = move |r| done(r);
    match (gio_drive(&id), first) {
        (Some(d), _) => d.eject_with_operation(flags, Some(&operation()), none, done),
        // A phone or camera: its one volume is the device.
        (None, Some(v)) => match v.get_mount() {
            Some(m) if m.can_eject() => m.eject_with_operation(flags, Some(&operation()), none, done),
            _ => v.eject_with_operation(flags, Some(&operation()), none, done),
        },
        (None, None) => end(&id),
    }
}

/// Eject a whole device — every partition on it — or, when it cannot be
/// ejected, unmount every partition that is mounted; then say when it is
/// safe to pull out.
pub fn eject(id: &str) {
    let Some(drive) = find(id) else { return };
    if !(drive.can_eject || drive.can_unmount) || !begin(id) {
        return;
    }
    let glyph = drive.kind.glyph();
    let hidden: Vec<gio::Mount> = loose_mounts(&gio::VolumeMonitor::get())
        .into_iter()
        .filter(|(_, i)| i.device_id() == id)
        .map(|(m, _)| m)
        .collect();

    if drive.can_eject {
        let (id2, name) = (id.to_string(), drive.name.clone());
        unmount_all(id, &drive.name, hidden, move || eject_device(id2, name, glyph));
        return;
    }

    let mut mounts: Vec<gio::Mount> = volumes_of(id).iter().filter_map(|v| v.get_mount()).collect();
    mounts.extend(hidden);
    mounts.retain(|m| m.can_unmount());
    let done = safe_to_remove(id.to_string(), drive.name.clone(), glyph);
    unmount_all(id, &drive.name, mounts, done);
}

/// Undo mountinfo's octal escapes: `\040` for a space, and so on.
fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(i) = rest.find('\\') {
        out.push_str(&rest[..i]);
        let code = rest.get(i + 1..i + 4).and_then(|o| u8::from_str_radix(o, 8).ok());
        match code {
            Some(c) => {
                out.push(c as char);
                rest = &rest[i + 4..];
            }
            None => {
                out.push('\\');
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The device mounted at `mount_point`, from `/proc/self/mountinfo` text.
///
/// Each line is `id parent major:minor root mount-point options… - type
/// source super-options`. Of several mounts stacked on one point the last
/// listed is the one on top, so the search runs from the end.
fn mount_source(mountinfo: &str, mount_point: &Path) -> Option<String> {
    mountinfo.lines().rev().find_map(|line| {
        let (head, tail) = line.split_once(" - ")?;
        let point = head.split(' ').nth(4)?;
        (Path::new(&unescape(point)) == mount_point)
            .then(|| tail.split(' ').nth(1).map(unescape))
            .flatten()
    })
}

/// The disk a block device belongs to, from its resolved sysfs path:
/// `…/block/sdb/sdb2` is on `sdb`, and `…/block/sdb` is a disk itself.
fn disk_of(sysfs: &Path) -> Option<String> {
    let name = sysfs.file_name()?.to_str()?;
    let parent = sysfs.parent()?.file_name()?.to_str()?;
    Some(if parent == "block" { name } else { parent }.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
22 1 259:9 / / rw,relatime shared:1 - ext4 /dev/mapper/omarchy_root rw
112 30 8:17 / /run/media/me/Ventoy rw,nosuid,nodev,relatime shared:60 - exfat /dev/sdb1 rw
118 30 8:18 / /run/media/me/VTOYEFI rw,nosuid,nodev,relatime shared:64 - vfat /dev/sdb2 rw,fmask=0022
120 30 8:33 / /run/media/me/MY\\040STICK rw,relatime shared:66 - vfat /dev/sdc1 rw
";

    #[test]
    fn the_device_behind_a_mount_point_is_read_from_mountinfo() {
        let at = |p: &str| mount_source(MOUNTINFO, Path::new(p));
        assert_eq!(at("/run/media/me/VTOYEFI").as_deref(), Some("/dev/sdb2"));
        assert_eq!(at("/run/media/me/Ventoy").as_deref(), Some("/dev/sdb1"));
        // Spaces in a mount point are written as \040.
        assert_eq!(at("/run/media/me/MY STICK").as_deref(), Some("/dev/sdc1"));
        assert_eq!(at("/run/media/me/elsewhere"), None);
    }

    #[test]
    fn of_mounts_stacked_on_one_point_the_last_is_the_one_in_use() {
        // Mounting over a mount point hides what was there; mountinfo lists
        // the newer mount later.
        let stacked = "\
112 30 8:17 / /run/media/me/X rw shared:60 - exfat /dev/sdb1 rw
130 112 8:33 / /run/media/me/X rw shared:70 - vfat /dev/sdc1 rw
";
        assert_eq!(
            mount_source(stacked, Path::new("/run/media/me/X")).as_deref(),
            Some("/dev/sdc1")
        );
    }

    #[test]
    fn a_partition_belongs_to_the_disk_above_it_in_sysfs() {
        let disk = |p: &str| disk_of(Path::new(p));
        let usb = "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host0/target0:0:0/0:0:0:0/block";
        assert_eq!(disk(&format!("{usb}/sdb/sdb2")).as_deref(), Some("sdb"));
        // A filesystem on the whole disk, no partition table.
        assert_eq!(disk(&format!("{usb}/sdb")).as_deref(), Some("sdb"));
        assert_eq!(disk("/sys/devices/virtual/block/dm-0").as_deref(), Some("dm-0"));
    }

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
