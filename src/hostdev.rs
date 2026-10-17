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

//! Generation of libvirt `<hostdev>` XML for USB passthrough.

use crate::usb::UsbDevice;

/// Build the `<hostdev>` snippet used for both attach and detach.
///
/// The host `<address bus= device=>` is included so that identical
/// VID/PID devices can be told apart at attach time. All interpolated values
/// are numbers, so no XML escaping is required.
pub fn hostdev_xml(dev: &UsbDevice) -> String {
    format!(
        "<hostdev mode='subsystem' type='usb' managed='yes'>\n  \
         <source>\n    \
         <vendor id='0x{:04x}'/>\n    \
         <product id='0x{:04x}'/>\n    \
         <address bus='{}' device='{}'/>\n  \
         </source>\n\
         </hostdev>\n",
        dev.vid, dev.pid, dev.bus, dev.devnum
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb::test_support::device;

    #[test]
    fn xml_contains_expected_fields() {
        let mut dev = device("2-1.4", 0x12ab, 0x00cd, 0);
        dev.devnum = 7;
        let xml = hostdev_xml(&dev);
        assert!(xml.contains("type='usb'"));
        assert!(xml.contains("managed='yes'"));
        assert!(xml.contains("<vendor id='0x12ab'/>"));
        assert!(xml.contains("<product id='0x00cd'/>"));
        assert!(xml.contains("<address bus='2' device='7'/>"));
    }
}
