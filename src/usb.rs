// SPDX-License-Identifier: GPL-3.0-or-later

// Copyright (C) 2026  Forest Crossman <cyrozap@gmail.com>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! USB device model backed by sysfs.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use log::{debug, warn};

use crate::error::{Error, Result};

/// Directory containing symlinks to every USB device on the host.
pub const USB_DEVICES_DIR: &str = "/sys/bus/usb/devices";

/// A USB device as described by its sysfs directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbDevice {
    /// sysfs kobject name, e.g. `1-1.4` or `usb2`. Topology source of truth.
    pub name: String,
    /// Full sysfs path of the device directory.
    pub sysfs_path: PathBuf,
    /// USB bus number (`busnum`).
    pub bus: u32,
    /// Device number on the bus (`devnum`); changes on every re-enumeration.
    pub devnum: u32,
    pub vid: u16,
    pub pid: u16,
    pub serial: Option<String>,
    /// `bDeviceClass`; `0x09` means hub.
    pub class: u8,
    pub product: Option<String>,
    pub manufacturer: Option<String>,
    /// Port chain parsed from the name: `1-1.4` -> `[1, 4]`; root hub -> `[]`.
    pub ports: Vec<u32>,
}

impl UsbDevice {
    /// Hubs are never passed through.
    pub fn is_hub(&self) -> bool {
        self.class == 0x09
    }

    /// Names of ancestor hubs, nearest first, including the root hub.
    ///
    /// `1-1.4.2` -> `["1-1.4", "1-1", "usb1"]`; a root hub has no ancestors.
    pub fn ancestors(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.ports.is_empty() {
            return out;
        }
        for i in (1..self.ports.len()).rev() {
            out.push(format!("{}-{}", self.bus, join_ports(&self.ports[..i])));
        }
        out.push(format!("usb{}", self.bus));
        out
    }

    /// Build a device by reading its sysfs directory.
    pub fn from_sysfs_dir(dir: &Path) -> Result<Self> {
        let name = dir
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| Error::usb(format!("bad device path {}", dir.display())))?
            .to_string();
        let (parsed_bus, ports) = parse_topology(&name)
            .ok_or_else(|| Error::usb(format!("unrecognised USB device name {name:?}")))?;

        let vid = read_hex_u16(dir, "idVendor")?;
        let pid = read_hex_u16(dir, "idProduct")?;
        let class = read_hex_u8(dir, "bDeviceClass")?;
        let devnum = read_dec_u32(dir, "devnum")?;
        // Trust busnum when present; fall back to the name.
        let bus = read_dec_u32(dir, "busnum").unwrap_or(parsed_bus);

        Ok(UsbDevice {
            name,
            sysfs_path: dir.to_path_buf(),
            bus,
            devnum,
            vid,
            pid,
            serial: read_string(dir, "serial"),
            class,
            product: read_string(dir, "product"),
            manufacturer: read_string(dir, "manufacturer"),
            ports,
        })
    }
}

/// Parse a sysfs USB name into `(bus, ports)`.
///
/// - `usb2` -> `(2, [])`
/// - `1-1` -> `(1, [1])`
/// - `1-1.4.2` -> `(1, [1, 4, 2])`
/// - interface names such as `1-1:1.0` -> `None`
pub fn parse_topology(name: &str) -> Option<(u32, Vec<u32>)> {
    if let Some(rest) = name.strip_prefix("usb") {
        if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
            return Some((rest.parse().ok()?, Vec::new()));
        }
        return None;
    }

    let (bus_s, ports_s) = name.split_once('-')?;
    if bus_s.is_empty() || !bus_s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if ports_s.is_empty() {
        return None;
    }
    let bus: u32 = bus_s.parse().ok()?;
    let mut ports = Vec::new();
    for p in ports_s.split('.') {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        ports.push(p.parse().ok()?);
    }
    Some((bus, ports))
}

/// True for actual device names (not interfaces, not root hubs).
pub fn is_device_name(name: &str) -> bool {
    matches!(parse_topology(name), Some((_, ports)) if !ports.is_empty())
}

/// Render a port chain as a dotted string.
pub fn join_ports(ports: &[u32]) -> String {
    ports
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

fn read_string(dir: &Path, attr: &str) -> Option<String> {
    let raw = fs::read_to_string(dir.join(attr)).ok()?;
    let trimmed = raw.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn read_hex_u16(dir: &Path, attr: &str) -> Result<u16> {
    let s = read_string(dir, attr)
        .ok_or_else(|| Error::usb(format!("missing {attr} for {}", dir.display())))?;
    let t = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(&s);
    u16::from_str_radix(t, 16)
        .map_err(|e| Error::usb(format!("invalid {attr} {s:?} for {}: {e}", dir.display())))
}

fn read_hex_u8(dir: &Path, attr: &str) -> Result<u8> {
    let s = read_string(dir, attr)
        .ok_or_else(|| Error::usb(format!("missing {attr} for {}", dir.display())))?;
    let t = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(&s);
    u8::from_str_radix(t, 16)
        .map_err(|e| Error::usb(format!("invalid {attr} {s:?} for {}: {e}", dir.display())))
}

fn read_dec_u32(dir: &Path, attr: &str) -> Result<u32> {
    let s = read_string(dir, attr)
        .ok_or_else(|| Error::usb(format!("missing {attr} for {}", dir.display())))?;
    s.parse()
        .map_err(|e| Error::usb(format!("invalid {attr} {s:?} for {}: {e}", dir.display())))
}

/// A source of [`UsbDevice`]s.
///
/// Abstracted so the matcher and event loop can be tested without sysfs.
pub trait DeviceSource {
    /// Look up a device (or root hub) by its sysfs kobject name.
    fn lookup(&self, name: &str) -> Option<UsbDevice>;
    /// Build a device from a uevent `DEVPATH`, retrying briefly while the
    /// kernel populates attributes.
    fn device_from_devpath(&self, devpath: &str) -> Option<UsbDevice>;
    /// All currently connected USB devices (excluding root hubs).
    fn scan(&self) -> Vec<UsbDevice>;
}

/// [`DeviceSource`] backed by `/sys/bus/usb/devices`.
pub struct SysfsSource {
    pub devices_dir: PathBuf,
}

impl SysfsSource {
    pub fn new() -> Self {
        SysfsSource {
            devices_dir: PathBuf::from(USB_DEVICES_DIR),
        }
    }

    #[allow(dead_code)]
    pub fn with_dir(dir: impl Into<PathBuf>) -> Self {
        SysfsSource {
            devices_dir: dir.into(),
        }
    }
}

impl Default for SysfsSource {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a name is safe to join onto the devices directory.
fn is_lookupable_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && name != "."
        && name != ".."
        && parse_topology(name).is_some()
}

impl DeviceSource for SysfsSource {
    fn lookup(&self, name: &str) -> Option<UsbDevice> {
        if !is_lookupable_name(name) {
            return None;
        }
        UsbDevice::from_sysfs_dir(&self.devices_dir.join(name)).ok()
    }

    fn device_from_devpath(&self, devpath: &str) -> Option<UsbDevice> {
        let dir = PathBuf::from("/sys").join(devpath.trim_start_matches('/'));
        // The uevent may arrive before all attribute files exist.
        for _ in 0..40 {
            if dir.join("idVendor").exists() && dir.join("idProduct").exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        UsbDevice::from_sysfs_dir(&dir).ok()
    }

    fn scan(&self) -> Vec<UsbDevice> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.devices_dir) {
            Ok(e) => e,
            Err(e) => {
                warn!("cannot read {}: {e}", self.devices_dir.display());
                return out;
            }
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !is_device_name(&name) {
                continue;
            }
            match UsbDevice::from_sysfs_dir(&entry.path()) {
                Ok(dev) => out.push(dev),
                Err(e) => debug!("skipping {name}: {e}"),
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// In-memory [`DeviceSource`] for tests.
    #[derive(Default, Clone)]
    pub struct MapSource {
        pub devices: Vec<UsbDevice>,
    }

    impl MapSource {
        pub fn new(devices: Vec<UsbDevice>) -> Self {
            MapSource { devices }
        }

        fn find(&self, name: &str) -> Option<UsbDevice> {
            self.devices.iter().find(|d| d.name == name).cloned()
        }
    }

    impl DeviceSource for MapSource {
        fn lookup(&self, name: &str) -> Option<UsbDevice> {
            self.find(name)
        }

        fn device_from_devpath(&self, devpath: &str) -> Option<UsbDevice> {
            let base = devpath.rsplit('/').next().unwrap_or("");
            self.find(base)
        }

        fn scan(&self) -> Vec<UsbDevice> {
            self.devices.clone()
        }
    }

    /// Create a device, deriving bus/ports from its name.
    pub fn device(name: &str, vid: u16, pid: u16, class: u8) -> UsbDevice {
        let (bus, ports) = parse_topology(name).expect("valid test device name");
        UsbDevice {
            name: name.to_string(),
            sysfs_path: PathBuf::from(format!("/sys/bus/usb/devices/{name}")),
            bus,
            devnum: 1,
            vid,
            pid,
            serial: None,
            class,
            product: None,
            manufacturer: None,
            ports,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::device;
    use super::*;

    #[test]
    fn topology_parsing() {
        assert_eq!(parse_topology("1-1"), Some((1, vec![1])));
        assert_eq!(parse_topology("2-1.4.2"), Some((2, vec![1, 4, 2])));
        assert_eq!(parse_topology("usb3"), Some((3, vec![])));
        assert_eq!(parse_topology("1-1:1.0"), None);
        assert_eq!(parse_topology("garbage"), None);
        assert_eq!(parse_topology("usb"), None);
        assert_eq!(parse_topology("1-"), None);
        assert_eq!(parse_topology("-1"), None);
        assert_eq!(parse_topology("1-1."), None);
    }

    #[test]
    fn device_name_filter() {
        assert!(is_device_name("1-1"));
        assert!(is_device_name("1-1.4.2"));
        assert!(!is_device_name("usb1"));
        assert!(!is_device_name("1-1:1.0"));
    }

    #[test]
    fn ancestry() {
        assert_eq!(
            device("1-1.4.2", 1, 2, 0).ancestors(),
            vec!["1-1.4", "1-1", "usb1"]
        );
        assert_eq!(device("1-1", 1, 2, 0).ancestors(), vec!["usb1"]);
        assert!(device("usb1", 1, 2, 9).ancestors().is_empty());
    }

    #[test]
    fn hub_detection() {
        assert!(device("1-1", 1, 2, 0x09).is_hub());
        assert!(!device("1-1", 1, 2, 0x00).is_hub());
    }

    #[test]
    fn join_ports_render() {
        assert_eq!(join_ports(&[1, 4, 2]), "1.4.2");
        assert_eq!(join_ports(&[]), "");
    }
}
