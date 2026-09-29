//! Removable drives: USB sticks, external disks, SD cards, phones and cameras.
//!
//! Read through GIO's volume monitor, which gvfs backs with udisks2 — the same
//! view of the hardware the file manager's sidebar has, including phones over
//! MTP, and the same polkit rules for mounting as the logged-in user. The
//! dock mounts only when a drive is clicked, and never writes to one.
//!
//! Main thread only: GIO objects are not `Send`. What crosses into the dock's
//! state is the plain [`Drive`] value.

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
