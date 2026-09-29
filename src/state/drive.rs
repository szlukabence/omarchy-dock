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
    /// What a phone or camera is, from its volume's icon names: gvfs gives
    /// them no drive to go by.
    pub fn from_icons(names: &[String]) -> Self {
        let any = |w: &str| names.iter().any(|n| n.contains(w));
        if any("phone") || any("multimedia-player") {
            DeviceKind::Phone
        } else if any("camera") {
            DeviceKind::Camera
        } else {
            DeviceKind::UsbStick
        }
    }

    /// What a drive is, from its own icon names and whether its media comes
    /// out of it.
    ///
    /// Never from its partitions': gvfs draws the partition of a USB stick as
    /// a USB hard disk. Whether the media comes out is what tells a card
    /// reader — which may call itself nothing but "Generic STORAGE DEVICE" —
    /// from a stick, which is its own media.
    pub fn of_drive(icons: &[String], media_removable: bool) -> Self {
        let any = |w: &str| icons.iter().any(|n| n.contains(w));
        if any("optical") {
            DeviceKind::Optical
        } else if media_removable || any("flash") || any("card-reader") {
            DeviceKind::SdCard
        } else if any("harddisk") {
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
    /// Its media comes out of it — a card from a reader, a disc from a drive.
    pub media_removable: bool,
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
    match kind {
        DeviceKind::SdCard => {
            tries.extend(names.iter().filter(|n| n.contains("flash") || n.contains("card")).cloned());
            tries.extend(["drive-harddisk-usb", "drive-harddisk"].map(String::from));
        }
        // gvfs names a stick "media-removable", which themes draw as a disk.
        DeviceKind::UsbStick => tries.push("drive-removable-media-usb".into()),
        DeviceKind::ExternalDisk => tries.push("drive-harddisk-usb".into()),
        _ => {}
    }
    tries.extend(names.iter().filter(|n| !tries.contains(n)).cloned().collect::<Vec<_>>());
    tries
}

/// Gather volumes into devices, in the order their first volume came.
///
/// `has_icon` says whether the icon theme can draw a name.
pub fn group(volumes: Vec<VolumeInfo>, has_icon: impl Fn(&str) -> bool) -> Vec<Drive> {
    // Per device: the drive, what GIO says of its drive (none for a phone),
    // and the icon names of its volumes.
    let mut devices: Vec<(Drive, Option<DeviceInfo>, Vec<String>)> = Vec::new();
    for v in volumes {
        let id = v.device_id().to_string();
        let at = match devices.iter().position(|(d, ..)| d.id == id) {
            Some(i) => i,
            None => {
                let drive = Drive {
                    id,
                    name: String::new(),
                    icon: String::new(),
                    kind: DeviceKind::UsbStick,
                    can_eject: v.device.as_ref().map_or(v.can_eject, |d| d.can_eject),
                    can_unmount: false,
                    partitions: Vec::new(),
                };
                devices.push((drive, v.device.clone(), Vec::new()));
                devices.len() - 1
            }
        };
        let (drive, _, icons) = &mut devices[at];
        drive.can_unmount |= v.mounted && v.can_unmount;
        icons.extend(v.icons);
        drive.partitions.push(Partition { id: v.id, name: v.name, mounted: v.mounted });
    }

    devices
        .into_iter()
        .map(|(mut drive, device, volume_icons)| {
            // One partition goes by its own label, which is what the user
            // named it; several by the name of the thing they plugged in.
            let only = match drive.partitions.as_slice() {
                [only] => Some(only.name.clone()),
                _ => None,
            };
            let (kind, icons) = match &device {
                Some(d) => (DeviceKind::of_drive(&d.icons, d.media_removable), d.icons.clone()),
                None => (DeviceKind::from_icons(&volume_icons), volume_icons),
            };
            drive.name = only
                .or_else(|| device.map(|d| d.name))
                .unwrap_or_else(|| drive.partitions[0].name.clone());
            drive.kind = kind;
            drive.icon = icon_candidates(kind, &icons)
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
    fn a_phone_or_camera_is_known_by_its_icon_names() {
        use DeviceKind::*;
        let kind = |n: &[&str]| DeviceKind::from_icons(&names(n));
        assert_eq!(kind(&["phone-apple-iphone"]), Phone);
        assert_eq!(kind(&["multimedia-player"]), Phone);
        assert_eq!(kind(&["camera-photo"]), Camera);
        assert_eq!(kind(&[]), UsbStick);
    }

    #[test]
    fn a_drive_is_known_by_its_own_icons_and_whether_its_media_comes_out() {
        use DeviceKind::*;
        let kind = |n: &[&str], media_removable| DeviceKind::of_drive(&names(n), media_removable);
        // udisks: Media "thumb" — a stick is its own media.
        assert_eq!(kind(&["media-removable", "media"], false), UsbStick);
        assert_eq!(kind(&["drive-removable-media-usb"], false), UsbStick);
        assert_eq!(kind(&["drive-harddisk-usb", "drive-harddisk"], false), ExternalDisk);
        // A reader whose card comes out, even one calling itself nothing more
        // than "Generic STORAGE DEVICE".
        assert_eq!(kind(&["drive-removable-media-usb", "drive-removable-media"], true), SdCard);
        assert_eq!(kind(&["drive-removable-media-flash-sd"], true), SdCard);
        assert_eq!(kind(&["drive-optical"], true), Optical);
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
            media_removable: false,
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
    fn a_partition_does_not_decide_what_its_drive_is() {
        // The Kingston DataTraveler as gvfs reports it: the stick is "media",
        // its Ventoy partition a USB hard disk.
        let mut stick = device("/dev/sdb").unwrap();
        stick.icons = names(&["media-removable", "media"]);
        let mut ventoy = volume("/dev/sdb1", "Ventoy", Some(stick));
        ventoy.icons = names(&["drive-harddisk-usb", "drive-harddisk"]);
        assert_eq!(group(vec![ventoy], |_| true)[0].kind, DeviceKind::UsbStick);
    }

    #[test]
    fn the_icon_is_the_first_the_theme_can_draw() {
        let mut reader = device("/dev/sdd").unwrap();
        reader.media_removable = true;
        let mut card = volume("/dev/sdd1", "CARD", Some(reader));
        card.icons = names(&["media-flash-sd", "media-removable"]);
        let drives = group(vec![card], |n| n == "drive-harddisk-usb" || n == "media-removable");
        assert_eq!(drives[0].kind, DeviceKind::SdCard);
        assert_eq!(drives[0].icon, "drive-harddisk-usb");
    }
}
