//! Removable devices as the dock shows them: one icon per physical device,
//! however many partitions it carries.
//!
//! GTK- and GIO-free, so the grouping and the choice of glyph are tested
//! headless. `crate::drives` reads GIO into [`VolumeInfo`]s and hands them to
//! [`group`].

/// What kind of device a drive is, for its glyph and icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    UsbStick,
    SdCard,
    ExternalDisk,
    Optical,
    Phone,
    Camera,
}

impl DeviceKind {
    /// Read from the themed icon names udisks and gvfs give a drive and its
    /// volumes, e.g. `drive-removable-media-flash-sd` or `phone`.
    ///
    /// Every name counts, not just the first: GIO lists fallbacks, and the
    /// telling one — `media-flash-sd` behind `drive-removable-media` — is
    /// often further down.
    pub fn from_icons(names: &[String]) -> Self {
        let any = |words: &[&str]| names.iter().any(|n| words.iter().any(|w| n.contains(w)));
        if any(&["phone", "multimedia-player"]) {
            DeviceKind::Phone
        } else if any(&["camera"]) {
            DeviceKind::Camera
        } else if any(&["flash", "card-reader"]) {
            DeviceKind::SdCard
        } else if any(&["optical"]) {
            DeviceKind::Optical
        } else if any(&["harddisk"]) {
            DeviceKind::ExternalDisk
        } else {
            DeviceKind::UsbStick
        }
    }

    /// Material Design glyphs from the Nerd Font, the set the recording
    /// button already uses.
    pub fn glyph(self) -> &'static str {
        match self {
            DeviceKind::UsbStick => "\u{f129e}",
            DeviceKind::SdCard => "\u{f0479}",
            DeviceKind::ExternalDisk => "\u{f02ca}",
            DeviceKind::Optical => "\u{f05ee}",
            DeviceKind::Phone => "\u{f011c}",
            DeviceKind::Camera => "\u{f0100}",
        }
    }
}

/// One mountable volume on a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub id: String,
    /// Its label, or what the file manager calls it: "EFI", "2.1 GB Volume".
    pub name: String,
    pub mounted: bool,
}

/// One physical device, as one dock icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    pub id: String,
    pub name: String,
    /// Themed icon name, for when the dock draws icons rather than glyphs.
    pub icon: String,
    pub kind: DeviceKind,
    /// The whole device can be ejected.
    pub can_eject: bool,
    /// Something on it is mounted and can be unmounted.
    pub can_unmount: bool,
    pub partitions: Vec<Partition>,
}

/// The physical drive a volume sits on, as GIO reports it.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub icons: Vec<String>,
    pub can_eject: bool,
}

/// One removable volume, as GIO reports it.
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub id: String,
    pub name: String,
    pub icons: Vec<String>,
    pub mounted: bool,
    /// The volume, or its mount, can be ejected. Used only without a device.
    pub can_eject: bool,
    pub can_unmount: bool,
    /// `None` for a phone or camera, which gvfs presents without a drive.
    pub device: Option<DeviceInfo>,
}

/// Last resort when the theme can draw none of a device's names.
const DEFAULT_ICON: &str = "drive-removable-media";

impl VolumeInfo {
    /// The device this volume belongs to: its drive, or itself when it has
    /// none.
    pub fn device_id(&self) -> &str {
        self.device.as_ref().map_or(&self.id, |d| &d.id)
    }
}

/// Icon names to try, best first, for a device of `kind` whose GIO icons are
/// `names`.
///
/// Few icon themes draw an SD card, and the generic removable-media icon
/// behind it reads as a USB stick; a disk is the nearer likeness, so a card
/// tries its own icons, then a disk's, then the rest.
pub fn icon_candidates(kind: DeviceKind, names: &[String]) -> Vec<String> {
    let mut tries: Vec<String> = Vec::new();
    if kind == DeviceKind::SdCard {
        tries.extend(names.iter().filter(|n| n.contains("flash") || n.contains("card")).cloned());
        tries.extend(["drive-harddisk-usb", "drive-harddisk"].map(String::from));
    }
    tries.extend(names.iter().filter(|n| !tries.contains(n)).cloned().collect::<Vec<_>>());
    tries
}

/// Gather volumes into devices, in the order their first volume came.
///
/// `has_icon` says whether the icon theme can draw a name.
pub fn group(volumes: Vec<VolumeInfo>, has_icon: impl Fn(&str) -> bool) -> Vec<Drive> {
    // Per device: the drive itself, its icon names, and its volumes.
    let mut devices: Vec<(Drive, Vec<String>, String)> = Vec::new();
    for v in volumes {
        let id = v.device_id().to_string();
        let at = match devices.iter().position(|(d, ..)| d.id == id) {
            Some(i) => i,
            None => {
                let (can_eject, name, icons) = match &v.device {
                    Some(d) => (d.can_eject, d.name.clone(), d.icons.clone()),
                    None => (v.can_eject, v.name.clone(), Vec::new()),
                };
                let drive = Drive {
                    id,
                    name: String::new(),
                    icon: String::new(),
                    kind: DeviceKind::UsbStick,
                    can_eject,
                    can_unmount: false,
                    partitions: Vec::new(),
                };
                devices.push((drive, icons, name));
                devices.len() - 1
            }
        };
        let (drive, icons, _) = &mut devices[at];
        drive.can_unmount |= v.mounted && v.can_unmount;
        icons.extend(v.icons);
        drive.partitions.push(Partition { id: v.id, name: v.name, mounted: v.mounted });
    }

    devices
        .into_iter()
        .map(|(mut drive, icons, device_name)| {
            // One partition goes by its own label, which is what the user
            // named it; several by the name of the thing they plugged in.
            drive.name = match drive.partitions.as_slice() {
                [only] => only.name.clone(),
                _ => device_name,
            };
            drive.kind = DeviceKind::from_icons(&icons);
            drive.icon = icon_candidates(drive.kind, &icons)
                .into_iter()
                .find(|n| has_icon(n))
                .unwrap_or_else(|| DEFAULT_ICON.to_string());
            drive
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_kind_is_read_from_any_of_the_icon_names() {
        use DeviceKind::*;
        let kind = |n: &[&str]| DeviceKind::from_icons(&names(n));
        assert_eq!(kind(&["phone-apple-iphone"]), Phone);
        assert_eq!(kind(&["multimedia-player"]), Phone);
        assert_eq!(kind(&["camera-photo"]), Camera);
        // The SD name is a fallback, not the first name.
        assert_eq!(kind(&["drive-removable-media", "media-flash-sd-xc"]), SdCard);
        assert_eq!(kind(&["drive-removable-media-flash-mmc"]), SdCard);
        assert_eq!(kind(&["media-flash-cf"]), SdCard);
        assert_eq!(kind(&["drive-card-reader"]), SdCard);
        assert_eq!(kind(&["drive-optical"]), Optical);
        assert_eq!(kind(&["drive-harddisk-usb"]), ExternalDisk);
        assert_eq!(kind(&["drive-removable-media-usb"]), UsbStick);
        assert_eq!(kind(&[]), UsbStick);
    }

    #[test]
    fn every_kind_has_its_own_glyph() {
        use DeviceKind::*;
        let all = [UsbStick, SdCard, ExternalDisk, Optical, Phone, Camera];
        for k in all {
            assert!(!k.glyph().is_empty(), "{k:?}");
            assert_eq!(all.iter().filter(|o| o.glyph() == k.glyph()).count(), 1, "{k:?}");
        }
        assert_eq!(UsbStick.glyph(), "\u{f129e}");
        assert_eq!(SdCard.glyph(), "\u{f0479}");
    }

    #[test]
    fn an_sd_card_without_its_own_icon_looks_like_a_disk() {
        let gio = names(&["media-flash-sd", "media-removable"]);
        let tries = icon_candidates(DeviceKind::SdCard, &gio);
        let yaru = |n: &str| matches!(n, "drive-harddisk-usb" | "media-removable");
        assert_eq!(tries.iter().find(|n| yaru(n)).map(String::as_str), Some("drive-harddisk-usb"));
        // A theme that has an SD icon uses it.
        assert_eq!(tries.first().map(String::as_str), Some("media-flash-sd"));
    }

    #[test]
    fn other_kinds_try_their_gio_icons_first() {
        let gio = names(&["drive-removable-media-usb", "drive-removable-media"]);
        assert_eq!(icon_candidates(DeviceKind::UsbStick, &gio)[..2], gio[..]);
    }

    fn device(id: &str) -> Option<DeviceInfo> {
        Some(DeviceInfo {
            id: id.into(),
            name: "SanDisk Cruzer".into(),
            icons: names(&["drive-removable-media-usb"]),
            can_eject: true,
        })
    }

    fn volume(id: &str, name: &str, device: Option<DeviceInfo>) -> VolumeInfo {
        VolumeInfo {
            id: id.into(),
            name: name.into(),
            icons: names(&["drive-removable-media"]),
            mounted: false,
            can_eject: false,
            can_unmount: false,
            device,
        }
    }

    #[test]
    fn partitions_on_one_device_make_one_drive() {
        let drives = group(
            vec![
                volume("/dev/sdb1", "EFI", device("/dev/sdb")),
                volume("/dev/sdc1", "KINGSTON", device("/dev/sdc")),
                volume("/dev/sdb2", "DATA", device("/dev/sdb")),
            ],
            |_| true,
        );
        assert_eq!(drives.len(), 2);
        assert_eq!(drives[0].id, "/dev/sdb");
        assert_eq!(drives[0].name, "SanDisk Cruzer", "several partitions: the drive's name");
        let parts: Vec<&str> = drives[0].partitions.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(parts, ["EFI", "DATA"]);
        assert!(drives[0].can_eject);
        assert_eq!(drives[1].name, "KINGSTON", "one partition: its own label");
    }

    #[test]
    fn a_phone_is_a_device_of_its_own() {
        let mut phone = volume("/dev/bus/usb/003/005", "Pixel 8", None);
        phone.icons = names(&["phone"]);
        phone.mounted = true;
        phone.can_unmount = true;
        let drives = group(vec![phone], |_| true);
        assert_eq!(drives.len(), 1);
        assert_eq!(drives[0].id, "/dev/bus/usb/003/005");
        assert_eq!(drives[0].kind, DeviceKind::Phone);
        assert!(!drives[0].can_eject);
        assert!(drives[0].can_unmount);
        assert_eq!(drives[0].partitions.len(), 1);
    }

    #[test]
    fn unmounting_is_offered_only_when_something_is_mounted() {
        let mut a = volume("/dev/sdb1", "A", device("/dev/sdb"));
        a.can_unmount = true; // but not mounted
        let drives = group(vec![a.clone()], |_| true);
        assert!(!drives[0].can_unmount);
        a.mounted = true;
        let drives = group(vec![a], |_| true);
        assert!(drives[0].can_unmount);
    }

    #[test]
    fn the_icon_is_the_first_the_theme_can_draw() {
        let mut card = volume("/dev/sdd1", "CARD", device("/dev/sdd"));
        card.icons = names(&["media-flash-sd", "media-removable"]);
        let drives = group(vec![card], |n| n == "drive-harddisk-usb" || n == "media-removable");
        assert_eq!(drives[0].kind, DeviceKind::SdCard);
        assert_eq!(drives[0].icon, "drive-harddisk-usb");
    }
}
