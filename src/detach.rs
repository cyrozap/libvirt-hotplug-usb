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

//! Detach already-attached USB hostdevs whose host device matches the rules.
//!
//! This backs the `--detach-matching` mode, which is useful for recovering
//! guest USB ports that were leaked by replugs or old daemon runs.

use log::{info, warn};

use crate::error::{Error, Result};
use crate::rule::Rule;
use crate::usb::{DeviceSource, UsbDevice};
use crate::vm::VmBackend;

/// A `<hostdev type='usb'>` element found in a domain's live XML.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbHostdev {
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    /// Host bus from `<source><address .../>`.
    pub bus: Option<u32>,
    /// Host device number from `<source><address .../>`.
    pub device: Option<u32>,
    /// The exact `<hostdev>...</hostdev>` element, suitable for detaching.
    pub xml: String,
}

/// Extract all USB hostdev elements from a domain XML document.
///
/// Only the host address inside `<source>` is considered; the guest
/// `<address type='usb' .../>` is ignored.
pub fn parse_usb_hostdevs(xml: &str) -> Vec<UsbHostdev> {
    const OPEN: &str = "<hostdev";
    const CLOSE: &str = "</hostdev>";

    let mut hostdevs = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start..];
        let Some(close) = after.find(CLOSE) else {
            break;
        };
        let block = &after[..close + CLOSE.len()];
        rest = &after[close + CLOSE.len()..];

        if attr_value(opening_tag(block), "type") != Some("usb") {
            continue;
        }

        let source = element_inner(block, "source").unwrap_or(block);
        let host_address = first_tag(source, "address");
        hostdevs.push(UsbHostdev {
            vid: parse_id(source, "vendor"),
            pid: parse_id(source, "product"),
            bus: host_address
                .and_then(|tag| attr_value(tag, "bus"))
                .and_then(parse_u32),
            device: host_address
                .and_then(|tag| attr_value(tag, "device"))
                .and_then(parse_u32),
            xml: block.trim().to_string(),
        });
    }
    hostdevs
}

/// True if the hostdev's host device satisfies any rule.
///
/// When the host device is still present it is looked up by bus/device and
/// all criteria (serial, port, hub, ...) can be evaluated. When it is gone
/// (a stale hostdev), only information present in the domain XML is usable,
/// so `id=` and `bus=` rules can still match while `serial=`, `port=` and
/// `hub(...)` cannot.
pub fn hostdev_matches(
    hostdev: &UsbHostdev,
    rules: &[Rule],
    source: &dyn DeviceSource,
    devices: &[UsbDevice],
) -> bool {
    let dev = resolve_device(hostdev, devices);
    rules.iter().any(|rule| rule.matches(&dev, source))
}

/// Detach every matching USB hostdev from the (running) domain.
///
/// Returns the number of devices detached (0 in dry-run mode).
pub fn detach_matching(
    backend: &mut dyn VmBackend,
    source: &dyn DeviceSource,
    rules: &[Rule],
    dry_run: bool,
) -> Result<usize> {
    if !backend.is_active()? {
        return Err(Error::libvirt(
            "the domain is not running; it has no live USB hostdevs to detach",
        ));
    }

    let xml = backend.domain_xml()?;
    let devices = source.scan();
    let matching: Vec<UsbHostdev> = parse_usb_hostdevs(&xml)
        .into_iter()
        .filter(|hostdev| hostdev_matches(hostdev, rules, source, &devices))
        .collect();

    if matching.is_empty() {
        info!("no matching USB hostdevs are attached to the domain");
        return Ok(0);
    }

    let mut detached = 0;
    for hostdev in matching {
        let description = describe(&hostdev);
        if dry_run {
            info!("would detach {description}");
            continue;
        }
        match backend.detach(&hostdev.xml) {
            Ok(()) => {
                info!("detached {description}");
                detached += 1;
            }
            Err(e) => warn!("failed to detach {description}: {e}"),
        }
    }
    Ok(detached)
}

/// Reconstruct the host device a hostdev refers to.
fn resolve_device(hostdev: &UsbHostdev, devices: &[UsbDevice]) -> UsbDevice {
    if let (Some(bus), Some(device)) = (hostdev.bus, hostdev.device)
        && let Some(dev) = devices
            .iter()
            .find(|dev| dev.bus == bus && dev.devnum == device)
        // Guard against the address having been reused by a different device.
        && hostdev.vid.is_none_or(|vid| vid == dev.vid)
        && hostdev.pid.is_none_or(|pid| pid == dev.pid)
    {
        return dev.clone();
    }

    // The device is gone (or changed): match with what the XML tells us.
    UsbDevice {
        name: match (hostdev.bus, hostdev.device) {
            (Some(bus), Some(device)) => format!("{bus}-{device}"),
            _ => "unknown".to_string(),
        },
        sysfs_path: std::path::PathBuf::new(),
        bus: hostdev.bus.unwrap_or(0),
        devnum: hostdev.device.unwrap_or(0),
        vid: hostdev.vid.unwrap_or(0),
        pid: hostdev.pid.unwrap_or(0),
        serial: None,
        class: 0,
        product: None,
        manufacturer: None,
        ports: Vec::new(),
    }
}

/// True if the hostdev's host device is no longer present, or its host address
/// has been reused by a different device.
///
/// Hostdevs without a host `<address>` cannot be checked and are never
/// considered stale.
pub fn is_stale(hostdev: &UsbHostdev, devices: &[UsbDevice]) -> bool {
    let (Some(bus), Some(device)) = (hostdev.bus, hostdev.device) else {
        return false;
    };
    match devices
        .iter()
        .find(|dev| dev.bus == bus && dev.devnum == device)
    {
        Some(dev) => {
            !(hostdev.vid.is_none_or(|vid| vid == dev.vid)
                && hostdev.pid.is_none_or(|pid| pid == dev.pid))
        }
        None => true,
    }
}

pub(crate) fn describe(hostdev: &UsbHostdev) -> String {
    let vid = hostdev
        .vid
        .map(|vid| format!("{vid:04x}"))
        .unwrap_or_else(|| "????".to_string());
    let pid = hostdev
        .pid
        .map(|pid| format!("{pid:04x}"))
        .unwrap_or_else(|| "????".to_string());
    let address = match (hostdev.bus, hostdev.device) {
        (Some(bus), Some(device)) => format!("bus {bus} device {device}"),
        (Some(bus), None) => format!("bus {bus}"),
        _ => "unknown address".to_string(),
    };
    format!("USB hostdev {vid}:{pid} ({address})")
}

fn opening_tag(element: &str) -> &str {
    match element.find('>') {
        Some(end) => &element[..end],
        None => element,
    }
}

/// Return the first `<name ...>` opening tag (without the closing `>`).
fn first_tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}");
    let start = text.find(&open)?;
    let end = text[start..].find('>')? + start;
    Some(&text[start..end])
}

/// Return the inner text of the first `<name>...</name>` element.
fn element_inner<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let start = text.find(&open)?;
    let body_start = text[start..].find('>')? + start + 1;
    let body_end = text[body_start..].find(&close)? + body_start;
    Some(&text[body_start..body_end])
}

fn parse_id(source: &str, element: &str) -> Option<u16> {
    let tag = first_tag(source, element)?;
    attr_value(tag, "id").and_then(parse_u16)
}

fn parse_u16(value: &str) -> Option<u16> {
    match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u16::from_str_radix(hex, 16).ok(),
        None => value.parse().ok(),
    }
}

fn parse_u32(value: &str) -> Option<u32> {
    match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => value.parse().ok(),
    }
}

/// Read an attribute value from an opening tag, accepting either quote style.
fn attr_value<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let bytes = tag.as_bytes();
    let mut search = 0;
    while let Some(offset) = tag[search..].find(name) {
        let index = search + offset;
        let boundary_before =
            index == 0 || bytes[index - 1].is_ascii_whitespace() || bytes[index - 1] == b'<';
        let mut cursor = index + name.len();
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if boundary_before && cursor < bytes.len() && bytes[cursor] == b'=' {
            cursor += 1;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor < bytes.len() && (bytes[cursor] == b'\'' || bytes[cursor] == b'"') {
                let quote = bytes[cursor] as char;
                let value_start = cursor + 1;
                if let Some(len) = tag[value_start..].find(quote) {
                    return Some(&tag[value_start..value_start + len]);
                }
            }
        }
        search = index + name.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::parse_rule;
    use crate::usb::test_support::{MapSource, device};
    use crate::vm::mock::MockBackend;

    const DOMAIN_XML: &str = r#"
<domain type='kvm'>
  <devices>
    <controller type='usb' index='0' model='qemu-xhci'/>
    <hostdev mode='subsystem' type='usb' managed='yes'>
      <source>
        <vendor id='0x2e8a'/>
        <product id='0x0003'/>
        <address bus='1' device='5'/>
      </source>
      <address type='usb' bus='0' port='2'/>
    </hostdev>
    <hostdev mode='subsystem' type='usb' managed='yes'>
      <source>
        <vendor id="0x1234"/>
        <product id="0xbeef"/>
        <address bus="2" device="7"/>
      </source>
    </hostdev>
    <hostdev mode='subsystem' type='pci' managed='yes'>
      <source>
        <address domain='0x0000' bus='0x01' slot='0x00' function='0x0'/>
      </source>
    </hostdev>
  </devices>
</domain>
"#;

    #[test]
    fn parses_only_usb_hostdevs_and_uses_host_address() {
        let hostdevs = parse_usb_hostdevs(DOMAIN_XML);
        assert_eq!(hostdevs.len(), 2);
        assert_eq!(hostdevs[0].vid, Some(0x2e8a));
        assert_eq!(hostdevs[0].pid, Some(0x0003));
        // Host address inside <source>, not the guest address (bus 0 port 2).
        assert_eq!(hostdevs[0].bus, Some(1));
        assert_eq!(hostdevs[0].device, Some(5));
        // Double-quoted attributes are supported too.
        assert_eq!(hostdevs[1].vid, Some(0x1234));
        assert_eq!(hostdevs[1].bus, Some(2));
        assert_eq!(hostdevs[1].device, Some(7));
    }

    #[test]
    fn stale_device_matches_id_and_bus_but_not_serial() {
        let source = MapSource::default();
        let devices = source.scan();
        let hostdevs = parse_usb_hostdevs(DOMAIN_XML);

        let id_rule = parse_rule("id=2e8a:0003").unwrap();
        let bus_rule = parse_rule("bus=1").unwrap();
        let serial_rule = parse_rule("serial=NOPE").unwrap();

        assert!(hostdev_matches(
            &hostdevs[0],
            std::slice::from_ref(&id_rule),
            &source,
            &devices
        ));
        assert!(hostdev_matches(
            &hostdevs[0],
            std::slice::from_ref(&bus_rule),
            &source,
            &devices
        ));
        assert!(!hostdev_matches(
            &hostdevs[0],
            std::slice::from_ref(&serial_rule),
            &source,
            &devices
        ));
        assert!(!hostdev_matches(
            &hostdevs[1],
            std::slice::from_ref(&id_rule),
            &source,
            &devices
        ));
    }

    #[test]
    fn present_device_uses_full_attributes() {
        let mut dev = device("1-1", 0x2e8a, 0x0003, 0);
        dev.devnum = 5;
        dev.serial = Some("SER123".to_string());
        let source = MapSource::new(vec![dev]);
        let devices = source.scan();
        let hostdevs = parse_usb_hostdevs(DOMAIN_XML);

        assert!(hostdev_matches(
            &hostdevs[0],
            &[parse_rule("serial=SER123").unwrap()],
            &source,
            &devices
        ));
        assert!(!hostdev_matches(
            &hostdevs[0],
            &[parse_rule("serial=OTHER").unwrap()],
            &source,
            &devices
        ));
    }

    #[test]
    fn detaches_matching_hostdevs() {
        let source = MapSource::default();
        let rules = vec![parse_rule("id=2e8a:0003").unwrap()];
        let mut backend = MockBackend {
            domain_xml: DOMAIN_XML.to_string(),
            ..MockBackend::active()
        };

        let count = detach_matching(&mut backend, &source, &rules, false).unwrap();
        assert_eq!(count, 1);
        let detaches = backend.detaches.borrow();
        assert_eq!(detaches.len(), 1);
        assert!(detaches[0].contains("0x2e8a"));
        // The guest address must not be mistaken for the host address.
        assert!(detaches[0].contains("bus='1' device='5'"));
    }

    #[test]
    fn dry_run_detaches_nothing() {
        let source = MapSource::default();
        let rules = vec![parse_rule("id=2e8a:0003").unwrap()];
        let mut backend = MockBackend {
            domain_xml: DOMAIN_XML.to_string(),
            ..MockBackend::active()
        };

        let count = detach_matching(&mut backend, &source, &rules, true).unwrap();
        assert_eq!(count, 0);
        assert!(backend.detaches.borrow().is_empty());
    }

    #[test]
    fn staleness_detection() {
        let hostdevs = parse_usb_hostdevs(DOMAIN_XML);

        // Nothing connected: both are stale.
        assert!(is_stale(&hostdevs[0], &[]));
        assert!(is_stale(&hostdevs[1], &[]));

        // The referenced device is present with matching IDs.
        let mut present = device("1-1", 0x2e8a, 0x0003, 0);
        present.devnum = 5;
        assert!(!is_stale(&hostdevs[0], &[present]));

        // The host address was reused by a different device.
        let mut reused = device("1-1", 0x9999, 0x9999, 0);
        reused.devnum = 5;
        assert!(is_stale(&hostdevs[0], &[reused]));

        // Without a host address, staleness cannot be determined.
        let unknown = UsbHostdev {
            vid: Some(1),
            pid: Some(2),
            bus: None,
            device: None,
            xml: String::new(),
        };
        assert!(!is_stale(&unknown, &[]));
    }

    #[test]
    fn inactive_domain_is_an_error() {
        let source = MapSource::default();
        let rules = vec![parse_rule("id=2e8a:0003").unwrap()];
        let mut backend = MockBackend {
            domain_xml: DOMAIN_XML.to_string(),
            ..MockBackend::default()
        };

        assert!(detach_matching(&mut backend, &source, &rules, true).is_err());
    }
}
