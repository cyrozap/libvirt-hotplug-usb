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

//! The hot-plug event loop.

use std::collections::BTreeMap;
use std::path::Path;

use log::{debug, info, warn};

use crate::error::Result;
use crate::hostdev::hostdev_xml;
use crate::monitor::{Event, EventSource};
use crate::rule::Rule;
use crate::signal;
use crate::usb::{DeviceSource, UsbDevice};
use crate::vm::VmBackend;

pub struct Daemon<S: DeviceSource, B: VmBackend> {
    rules: Vec<Rule>,
    source: S,
    backend: B,
    dry_run: bool,
    detach_on_exit: bool,
    /// Names of devices this process attached, mapped to their hostdev XML.
    attached: BTreeMap<String, String>,
    domain_active: bool,
}

impl<S: DeviceSource, B: VmBackend> Daemon<S, B> {
    pub fn new(
        rules: Vec<Rule>,
        source: S,
        backend: B,
        dry_run: bool,
        detach_on_exit: bool,
    ) -> Self {
        Daemon {
            rules,
            source,
            backend,
            dry_run,
            detach_on_exit,
            attached: BTreeMap::new(),
            domain_active: false,
        }
    }

    /// Perform the initial scan, attaching anything that already matches.
    pub fn startup(&mut self) {
        if self.dry_run {
            self.reconcile();
            return;
        }
        match self.backend.is_active() {
            Ok(true) => self.domain_active = true,
            Ok(false) => {
                self.domain_active = false;
                info!("domain is not running; matching devices will attach when it starts");
            }
            Err(e) => {
                self.domain_active = false;
                debug!("domain state unavailable at startup: {e}");
            }
        }
        self.reconcile();
    }

    /// Scan all connected devices and process each one.
    pub fn reconcile(&mut self) {
        let devices = self.source.scan();
        for dev in devices {
            self.process(dev);
        }
    }

    /// Attach a device if it matches a rule and is not a hub.
    pub fn process(&mut self, dev: UsbDevice) {
        if dev.is_hub() {
            debug!("ignoring hub {}", dev.name);
            return;
        }
        if !self
            .rules
            .iter()
            .any(|rule| rule.matches(&dev, &self.source))
        {
            return;
        }
        if self.attached.contains_key(&dev.name) {
            return;
        }

        let xml = hostdev_xml(&dev);
        if self.dry_run {
            info!(
                "would attach {} ({:04x}:{:04x}, bus {} port {})",
                dev.name,
                dev.vid,
                dev.pid,
                dev.bus,
                crate::usb::join_ports(&dev.ports)
            );
            debug!("hostdev xml:\n{xml}");
            return;
        }
        if !self.domain_active {
            info!(
                "matched {} ({:04x}:{:04x}) but the domain is not running; deferring",
                dev.name, dev.vid, dev.pid
            );
            return;
        }

        match self.backend.attach(&xml) {
            Ok(()) => {
                info!(
                    "attached USB device {} ({:04x}:{:04x})",
                    dev.name, dev.vid, dev.pid
                );
                self.attached.insert(dev.name, xml);
            }
            Err(e) => warn!(
                "failed to attach USB device {} ({:04x}:{:04x}): {e}",
                dev.name, dev.vid, dev.pid
            ),
        }
    }

    pub fn handle_add(&mut self, devpath: &str) {
        match self.source.device_from_devpath(devpath) {
            Some(dev) => self.process(dev),
            None => debug!("add event for {devpath} but the device disappeared"),
        }
    }

    pub fn handle_remove(&mut self, devpath: &str) {
        if let Some(name) = Path::new(devpath).file_name().and_then(|s| s.to_str())
            && self.attached.remove(name).is_some()
        {
            debug!("device {name} was removed from the host");
        }
    }

    /// Poll domain state, reconciling when a stopped domain comes back.
    pub fn maybe_rejoin(&mut self) {
        if self.dry_run {
            return;
        }
        match self.backend.is_active() {
            Ok(true) => {
                if !self.domain_active {
                    self.domain_active = true;
                    info!("domain is running; re-attaching matching devices");
                    self.reconcile();
                }
            }
            Ok(false) => {
                if self.domain_active {
                    self.domain_active = false;
                    info!("domain stopped");
                }
                // libvirt re-attaches managed devices to the host when the
                // domain stops, so our previous attachments no longer exist.
                self.attached.clear();
            }
            Err(e) => debug!("domain state unavailable: {e}"),
        }
    }

    /// Optionally detach everything this process attached.
    pub fn shutdown(&mut self) {
        if !self.detach_on_exit {
            return;
        }
        for (name, xml) in std::mem::take(&mut self.attached) {
            match self.backend.detach(&xml) {
                Ok(()) => info!("detached USB device {name}"),
                Err(e) => warn!("failed to detach USB device {name}: {e}"),
            }
        }
    }

    /// Run until a termination signal arrives.
    pub fn run(&mut self, events: &mut dyn EventSource) -> Result<()> {
        self.startup();
        while !signal::terminated() {
            match events.poll()? {
                Event::Add(devpath) => self.handle_add(&devpath),
                Event::Remove(devpath) => self.handle_remove(&devpath),
                Event::Resync => {
                    info!("kernel event buffer overflowed; rescanning USB devices");
                    self.reconcile();
                }
                Event::Timeout => self.maybe_rejoin(),
                Event::Interrupted => {}
                Event::Ignored => {}
            }
        }
        info!("shutting down");
        self.shutdown();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::parse_rule;
    use crate::usb::test_support::{MapSource, device};
    use crate::vm::mock::MockBackend;

    fn daemon(
        rules: &[&str],
        devices: Vec<UsbDevice>,
        backend: MockBackend,
        dry_run: bool,
        detach_on_exit: bool,
    ) -> Daemon<MapSource, MockBackend> {
        let rules = rules
            .iter()
            .map(|r| parse_rule(r).expect("valid test rule"))
            .collect();
        let mut d = Daemon::new(
            rules,
            MapSource::new(devices),
            backend,
            dry_run,
            detach_on_exit,
        );
        d.domain_active = true;
        d
    }

    #[test]
    fn attaches_match_only_once() {
        let dev = device("1-1", 0x1234, 0xabcd, 0);
        let mut d = daemon(
            &["id=1234:abcd"],
            vec![dev.clone()],
            MockBackend::active(),
            false,
            false,
        );
        d.process(dev.clone());
        d.process(dev);
        assert_eq!(d.backend.attaches.borrow().len(), 1);
    }

    #[test]
    fn ignores_non_matches_and_hubs() {
        let mut d = daemon(
            &["id=1234:abcd"],
            vec![],
            MockBackend::active(),
            false,
            false,
        );
        d.process(device("1-1", 0x9999, 0x9999, 0));
        // A hub can match the rule but must never be attached.
        d.process(device("1-2", 0x1234, 0xabcd, 0x09));
        assert!(d.backend.attaches.borrow().is_empty());
    }

    #[test]
    fn dry_run_does_not_attach() {
        let dev = device("1-1", 0x1234, 0xabcd, 0);
        let mut d = daemon(
            &["id=1234:"],
            vec![dev.clone()],
            MockBackend::active(),
            true,
            false,
        );
        d.process(dev);
        assert!(d.backend.attaches.borrow().is_empty());
    }

    #[test]
    fn defers_until_domain_active() {
        let dev = device("1-1", 0x1234, 0xabcd, 0);
        let mut d = daemon(
            &["id=1234:"],
            vec![dev.clone()],
            MockBackend::default(),
            false,
            false,
        );
        d.domain_active = false;
        d.process(dev);
        assert!(d.backend.attaches.borrow().is_empty());

        d.backend.active = true;
        d.maybe_rejoin();
        assert_eq!(d.backend.attaches.borrow().len(), 1);
    }

    #[test]
    fn clears_tracking_when_domain_stops_and_reattaches() {
        let dev = device("1-1", 0x1234, 0xabcd, 0);
        let mut d = daemon(
            &["id=1234:"],
            vec![dev.clone()],
            MockBackend::active(),
            false,
            false,
        );
        d.process(dev.clone());
        assert_eq!(d.backend.attaches.borrow().len(), 1);

        d.backend.active = false;
        d.maybe_rejoin();
        assert!(d.attached.is_empty());

        d.backend.active = true;
        d.maybe_rejoin();
        assert_eq!(d.backend.attaches.borrow().len(), 2);
    }

    #[test]
    fn remove_event_allows_reattach() {
        let dev = device("1-1", 0x1234, 0xabcd, 0);
        let mut d = daemon(
            &["id=1234:"],
            vec![dev.clone()],
            MockBackend::active(),
            false,
            false,
        );
        d.process(dev.clone());
        d.handle_remove("/devices/pci0000:00/usb1/1-1");
        assert!(d.attached.is_empty());
        d.process(dev);
        assert_eq!(d.backend.attaches.borrow().len(), 2);
    }

    #[test]
    fn detach_on_exit_behaviour() {
        let dev = device("1-1", 0x1234, 0xabcd, 0);
        let mut detaching = daemon(
            &["id=1234:"],
            vec![dev.clone()],
            MockBackend::active(),
            false,
            true,
        );
        detaching.process(dev.clone());
        detaching.shutdown();
        assert_eq!(detaching.backend.detaches.borrow().len(), 1);

        let mut keeping = daemon(
            &["id=1234:"],
            vec![dev.clone()],
            MockBackend::active(),
            false,
            false,
        );
        keeping.process(dev);
        keeping.shutdown();
        assert!(keeping.backend.detaches.borrow().is_empty());
    }

    #[test]
    fn rules_are_ored() {
        let a = device("1-1", 0x1234, 0x0000, 0);
        let b = device("1-2", 0xcafe, 0x0000, 0);
        let c = device("1-3", 0x0000, 0x0000, 0);
        let mut d = daemon(
            &["id=1234:", "id=cafe:"],
            vec![a.clone(), b.clone(), c.clone()],
            MockBackend::active(),
            false,
            false,
        );
        d.reconcile();
        assert_eq!(d.backend.attaches.borrow().len(), 2);
    }

    /// The daemon loop with a scripted event source terminating immediately.
    struct Scripted(Vec<Event>);

    impl EventSource for Scripted {
        fn poll(&mut self) -> Result<Event> {
            if self.0.is_empty() {
                crate::signal::set_for_test();
                Ok(Event::Interrupted)
            } else {
                Ok(self.0.remove(0))
            }
        }
    }

    #[test]
    fn run_processes_events_and_shuts_down() {
        crate::signal::reset_for_test();
        let dev = device("1-1", 0x1234, 0xabcd, 0);
        let mut d = daemon(
            &["id=1234:"],
            vec![dev.clone()],
            MockBackend::active(),
            false,
            true,
        );
        let mut source = Scripted(vec![
            Event::Add("/devices/pci0000:00/usb1/1-1".into()),
            Event::Remove("/devices/pci0000:00/usb1/1-1".into()),
            Event::Resync,
            Event::Timeout,
        ]);
        d.run(&mut source).expect("run succeeds");
        // startup + Resync attach; Add is deduplicated, Remove clears tracking.
        assert_eq!(d.backend.attaches.borrow().len(), 2);
        // detach-on-exit detached the device attached by Resync.
        assert_eq!(d.backend.detaches.borrow().len(), 1);
    }
}
