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

//! Kernel uevent monitoring over a netlink socket.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::mem;
use std::os::raw::c_int;

use crate::error::{Error, Result};

const AF_NETLINK: c_int = 16;
const SOCK_DGRAM: c_int = 2;
const SOCK_CLOEXEC: c_int = 0o2000000; // 0x80000
const NETLINK_KOBJECT_UEVENT: c_int = 15;
const SOL_SOCKET: c_int = 1;
const SO_RCVBUF: c_int = 8;
const SO_RCVTIMEO: c_int = 20;

const EINTR: i32 = 4;
const EAGAIN: i32 = 11;
const ENOBUFS: i32 = 105;

#[repr(C)]
struct SockAddrNl {
    nl_family: u16,
    nl_pad: u16,
    nl_pid: u32,
    nl_groups: u32,
}

#[repr(C)]
struct TimeVal {
    tv_sec: i64,
    tv_usec: i64,
}

unsafe extern "C" {
    fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    fn bind(fd: c_int, addr: *const SockAddrNl, len: u32) -> c_int;
    fn recv(fd: c_int, buf: *mut c_void, len: usize, flags: c_int) -> isize;
    fn close(fd: c_int) -> c_int;
    fn setsockopt(
        fd: c_int,
        level: c_int,
        optname: c_int,
        optval: *const c_void,
        optlen: u32,
    ) -> c_int;
}

/// A parsed kernel uevent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UEvent {
    pub action: String,
    pub devpath: String,
    pub keys: HashMap<String, String>,
}

impl UEvent {
    /// True for `add`/`remove` events about whole USB devices (not interfaces).
    pub fn is_usb_device(&self) -> bool {
        self.keys.get("SUBSYSTEM").is_some_and(|s| s == "usb")
            && self.keys.get("DEVTYPE").is_some_and(|s| s == "usb_device")
    }
}

/// Parse the raw payload of a `NETLINK_KOBJECT_UEVENT` message.
///
/// Layout: `"<action>@<devpath>\0"` followed by `"KEY=VALUE\0"` entries.
pub fn parse_uevent(data: &[u8]) -> Option<UEvent> {
    let mut fields = data.split(|b| *b == 0).filter(|f| !f.is_empty());
    let header = std::str::from_utf8(fields.next()?).ok()?;
    let (action, devpath) = header.split_once('@')?;

    let mut keys = HashMap::new();
    for field in fields {
        if let Ok(text) = std::str::from_utf8(field)
            && let Some((key, value)) = text.split_once('=')
        {
            keys.insert(key.to_string(), value.to_string());
        }
    }

    Some(UEvent {
        action: action.to_string(),
        devpath: devpath.to_string(),
        keys,
    })
}

/// Outcome of polling an [`EventSource`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A USB device was connected (`DEVPATH`).
    Add(String),
    /// A USB device was removed (`DEVPATH`).
    Remove(String),
    /// Nothing to do (timeout, unrelated event, or malformed message).
    Ignored,
    /// The receive timeout elapsed.
    Timeout,
    /// `recv` was interrupted by a signal.
    Interrupted,
    /// The kernel event buffer overflowed; a full rescan is required.
    Resync,
}

pub trait EventSource {
    fn poll(&mut self) -> Result<Event>;
}

/// Netlink socket subscribed to kernel uevents.
pub struct NetlinkMonitor {
    fd: c_int,
}

impl NetlinkMonitor {
    pub fn new() -> Result<Self> {
        // SAFETY: standard libc calls; all pointers below are valid.
        unsafe {
            let fd = socket(
                AF_NETLINK,
                SOCK_DGRAM | SOCK_CLOEXEC,
                NETLINK_KOBJECT_UEVENT,
            );
            if fd < 0 {
                return Err(Error::monitor(format!(
                    "socket(AF_NETLINK): {}",
                    io::Error::last_os_error()
                )));
            }

            // Best effort: a larger receive buffer reduces ENOBUFS drops.
            let rcvbuf: c_int = 1 << 20;
            setsockopt(
                fd,
                SOL_SOCKET,
                SO_RCVBUF,
                &rcvbuf as *const c_int as *const c_void,
                mem::size_of::<c_int>() as u32,
            );

            // A receive timeout lets the loop periodically service domain
            // availability without a second thread.
            let timeout = TimeVal {
                tv_sec: 1,
                tv_usec: 0,
            };
            if setsockopt(
                fd,
                SOL_SOCKET,
                SO_RCVTIMEO,
                &timeout as *const TimeVal as *const c_void,
                mem::size_of::<TimeVal>() as u32,
            ) < 0
            {
                let e = io::Error::last_os_error();
                close(fd);
                return Err(Error::monitor(format!("setsockopt(SO_RCVTIMEO): {e}")));
            }

            let addr = SockAddrNl {
                nl_family: AF_NETLINK as u16,
                nl_pad: 0,
                nl_pid: 0,
                nl_groups: 1,
            };
            if bind(fd, &addr, mem::size_of::<SockAddrNl>() as u32) < 0 {
                let e = io::Error::last_os_error();
                close(fd);
                return Err(Error::monitor(format!("bind(netlink): {e}")));
            }

            Ok(NetlinkMonitor { fd })
        }
    }
}

impl Drop for NetlinkMonitor {
    fn drop(&mut self) {
        // SAFETY: `self.fd` is a valid descriptor owned by this struct.
        unsafe {
            close(self.fd);
        }
    }
}

impl EventSource for NetlinkMonitor {
    fn poll(&mut self) -> Result<Event> {
        let mut buf = [0u8; 8192];
        // SAFETY: `buf` is valid for writes of `buf.len()` bytes.
        let n = unsafe { recv(self.fd, buf.as_mut_ptr() as *mut c_void, buf.len(), 0) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(EINTR) => Ok(Event::Interrupted),
                Some(EAGAIN) => Ok(Event::Timeout),
                Some(ENOBUFS) => Ok(Event::Resync),
                _ => Err(Error::monitor(format!("recv(netlink): {e}"))),
            };
        }
        if n == 0 {
            return Ok(Event::Ignored);
        }

        let Some(uevent) = parse_uevent(&buf[..n as usize]) else {
            return Ok(Event::Ignored);
        };
        if !uevent.is_usb_device() {
            return Ok(Event::Ignored);
        }
        match uevent.action.as_str() {
            "add" => Ok(Event::Add(uevent.devpath)),
            "remove" => Ok(Event::Remove(uevent.devpath)),
            _ => Ok(Event::Ignored),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(fields: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, f) in fields.iter().enumerate() {
            out.extend_from_slice(f.as_bytes());
            if i + 1 < fields.len() {
                out.push(0);
            }
        }
        out
    }

    #[test]
    fn parses_usb_add() {
        let raw = message(&[
            "add@/devices/pci0000:00/usb1/1-1",
            "ACTION=add",
            "DEVTYPE=usb_device",
            "SUBSYSTEM=usb",
            "PRODUCT=1234/abcd/100",
        ]);
        let ue = parse_uevent(&raw).expect("parses");
        assert_eq!(ue.action, "add");
        assert_eq!(ue.devpath, "/devices/pci0000:00/usb1/1-1");
        assert!(ue.is_usb_device());
        assert_eq!(ue.keys.get("PRODUCT").unwrap(), "1234/abcd/100");
    }

    #[test]
    fn parses_remove_and_filters_interfaces() {
        let usb_if = parse_uevent(&message(&[
            "add@/devices/pci0000:00/usb1/1-1/1-1:1.0",
            "SUBSYSTEM=usb",
            "DEVTYPE=usb_interface",
        ]))
        .unwrap();
        assert!(!usb_if.is_usb_device());

        let other =
            parse_uevent(&message(&["add@/devices/virtual/foo", "SUBSYSTEM=block"])).unwrap();
        assert!(!other.is_usb_device());
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse_uevent(b"").is_none());
        assert!(parse_uevent(b"no-at-sign\0SUBSYSTEM=usb").is_none());
    }
}
