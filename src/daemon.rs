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
use std::time::{Duration, Instant};

use log::{debug, info, warn};

use crate::error::Result;
use crate::hostdev::hostdev_xml;
use crate::monitor::{Event, EventSource};
use crate::rule::Rule;
use crate::signal;
use crate::usb::{DeviceSource, UsbDevice};
use crate::vm::VmBackend;

/// How often to remove hostdevs whose host device has disappeared.
const STALE_SWEEP_INTERVAL: Duration = Duration::from_secs(5);

pub struct Daemon<S: DeviceSource, B: VmBackend> {
    rules: Vec<Rule>,
    source: S,
    backend: B,
    dry_run: bool,
    detach_on_exit: bool,
    /// Names of devices this process attached, mapped to their hostdev XML.
    attached: BTreeMap<String, String>,
    domain_active: bool,
    last_sweep: Instant,
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
            last_sweep: Instant::now(),
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
        if self.domain_active {
            self.sweep_stale();
        }
        self.reconcile();
    }

    /// Scan all connected devices and process each one.
    pub fn reconcile(&mut self) {
        let devices = self.source.scan();
        for dev in devices {
            // A rescan is not evidence that a device was re-plugged, so do not
            // tear down hostdevs we already attached.
            self.process(dev);
        }
    }

    /// Attach a device if it matches a rule and is not a hub.
    pub fn process(&mut self, dev: UsbDevice) {
        self.process_with(dev, false);
    }

    fn process_with(&mut self, dev: UsbDevice, replace_existing: bool) {
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
            if !replace_existing {
                return;
            }
            // The device re-appeared on a port we previously attached. QEMU
            // keeps the old usb-host device (and its guest port) when the
            // physical device is unplugged, so detach it before re-attaching;
            // otherwise every replug leaks a guest USB port.
            self.detach_stale(&dev.name);
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
            Err(e) => {
                let message = e.to_string();
                if already_attached(&message) {
                    // e.g. a device that survived a daemon restart: libvirt
                    // refuses the duplicate, which means it is already in the
                    // guest. Track it so replugs can replace it later.
                    debug!(
                        "{} ({:04x}:{:04x}) is already attached; tracking it",
                        dev.name, dev.vid, dev.pid
                    );
                    self.attached.insert(dev.name, xml);
                } else if message.contains("No free USB ports") {
                    let freed = self.sweep_stale();
                    if freed > 0 {
                        info!(
                            "freed {freed} stale USB hostdev(s); retrying attach of {}",
                            dev.name
                        );
                        match self.backend.attach(&xml) {
                            Ok(()) => {
                                info!(
                                    "attached USB device {} ({:04x}:{:04x})",
                                    dev.name, dev.vid, dev.pid
                                );
                                self.attached.insert(dev.name, xml);
                            }
                            Err(retry_error) => warn!(
                                "failed to attach USB device {} ({:04x}:{:04x}) after cleanup: {retry_error}",
                                dev.name, dev.vid, dev.pid
                            ),
                        }
                    } else {
                        warn!(
                            "failed to attach USB device {} ({:04x}:{:04x}): {message}. \
                             The guest has no free USB ports: increase the USB controller's \
                             'ports' attribute (qemu-xhci defaults to 4) or add another USB \
                             controller.",
                            dev.name, dev.vid, dev.pid
                        );
                    }
                } else {
                    warn!(
                        "failed to attach USB device {} ({:04x}:{:04x}): {e}",
                        dev.name, dev.vid, dev.pid
                    );
                }
            }
        }
    }

    /// Detach the hostdev we previously created for `name`, freeing its guest
    /// USB port. Tracking is dropped even if libvirt reports an error.
    fn detach_stale(&mut self, name: &str) {
        let Some(xml) = self.attached.remove(name) else {
            return;
        };
        if self.dry_run || !self.domain_active {
            return;
        }
        match self.backend.detach(&xml) {
            Ok(()) => info!("detached stale hostdev for {name} before re-attaching"),
            Err(e) => debug!("could not detach stale hostdev for {name}: {e}"),
        }
    }

    pub fn handle_add(&mut self, devpath: &str) {
        match self.source.device_from_devpath(devpath) {
            // An `add` for a port we already track means the device was
            // re-plugged or re-enumerated; replace the stale hostdev.
            Some(dev) => self.process_with(dev, true),
            None => debug!("add event for {devpath} but the device disappeared"),
        }
    }

    pub fn handle_remove(&mut self, devpath: &str) {
        let Some(name) = Path::new(devpath)
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
        else {
            return;
        };
        let Some(xml) = self.attached.get(&name).cloned() else {
            return;
        };
        if self.dry_run || !self.domain_active {
            self.attached.remove(&name);
            debug!("device {name} was removed from the host");
            return;
        }
        match self.backend.detach(&xml) {
            Ok(()) => {
                self.attached.remove(&name);
                info!("detached {name} from the guest after it was unplugged");
            }
            Err(e) => {
                // Keep tracking it so a replug retries the detach instead of
                // silently leaking another guest USB port.
                debug!("could not detach {name} after unplug: {e}");
            }
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
                    self.sweep_stale();
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

        if self.domain_active && self.last_sweep.elapsed() >= STALE_SWEEP_INTERVAL {
            self.sweep_stale();
        }
    }

    /// Detach hostdevs whose host device has disappeared and which match our
    /// rules. Returns the number detached.
    ///
    /// This is a backstop for missed unplug events and for hostdevs left behind
    /// by previous daemon runs. It frees guest USB ports that would otherwise
    /// eventually cause "No free USB ports" failures.
    fn sweep_stale(&mut self) -> usize {
        self.last_sweep = Instant::now();
        if self.dry_run || !self.domain_active {
            return 0;
        }

        let xml = match self.backend.domain_xml() {
            Ok(xml) => xml,
            Err(e) => {
                debug!("cannot read domain XML for stale-hostdev sweep: {e}");
                return 0;
            }
        };
        let devices = self.source.scan();

        let mut detached = 0;
        for hostdev in crate::detach::parse_usb_hostdevs(&xml) {
            if !crate::detach::is_stale(&hostdev, &devices) {
                continue;
            }
            if !crate::detach::hostdev_matches(&hostdev, &self.rules, &self.source, &devices) {
                continue;
            }
            match self.backend.detach(&hostdev.xml) {
                Ok(()) => {
                    info!(
                        "detached stale USB hostdev {}",
                        crate::detach::describe(&hostdev)
                    );
                    detached += 1;
                }
                Err(e) => debug!(
                    "failed to detach stale USB hostdev {}: {e}",
                    crate::detach::describe(&hostdev)
                ),
            }
        }
        detached
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
                    self.sweep_stale();
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

/// libvirt's wording when a USB hostdev is already assigned to a domain.
fn already_attached(message: &str) -> bool {
    message.contains("already in use") || message.contains("is in use by driver")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::parse_rule;
    use crate::usb::test_support::{MapSource, device};
    use crate::vm::mock::MockBackend;

    /// A domain XML with one USB hostdev whose host device (bus 1, device 99)
    /// is not present in the test device source unless explicitly added.
    const STALE_DOMAIN_XML: &str = r#"
<domain type='kvm'>
  <devices>
    <hostdev mode='subsystem' type='usb' managed='yes'>
      <source>
        <vendor id='0x2e8a'/>
        <product id='0x0003'/>
        <address bus='1' device='99'/>
      </source>
    </hostdev>
  </devices>
</domain>
"#;

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
        // Unplugging detaches the hostdev so the guest port is freed.
        assert_eq!(d.backend.detaches.borrow().len(), 1);
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
        // startup attaches; Add replaces the stale hostdev; Remove detaches it;
        // Resync attaches again.
        assert_eq!(d.backend.attaches.borrow().len(), 3);
        // Replace, unplug cleanup, and detach-on-exit.
        assert_eq!(d.backend.detaches.borrow().len(), 3);
    }

    #[test]
    fn replug_replaces_stale_hostdev() {
        let mut dev = device("1-1", 0x1234, 0xabcd, 0);
        dev.devnum = 2;
        let mut d = daemon(
            &["id=1234:"],
            vec![dev.clone()],
            MockBackend::active(),
            false,
            false,
        );
        d.process(dev);
        assert_eq!(d.backend.attaches.borrow().len(), 1);

        // Same physical port, but the kernel assigned a new device number
        // after a replug. The old usb-host device still occupies a guest port.
        d.source.devices[0].devnum = 3;
        d.handle_add("/devices/pci0000:00/usb1/1-1");

        assert_eq!(d.backend.detaches.borrow().len(), 1);
        assert_eq!(d.backend.attaches.borrow().len(), 2);
    }

    #[test]
    fn resync_does_not_tear_down_attachments() {
        let dev = device("1-1", 0x1234, 0xabcd, 0);
        let mut d = daemon(
            &["id=1234:"],
            vec![dev.clone()],
            MockBackend::active(),
            false,
            false,
        );
        d.process(dev);
        d.reconcile();
        assert_eq!(d.backend.attaches.borrow().len(), 1);
        assert!(d.backend.detaches.borrow().is_empty());
    }

    #[test]
    fn already_in_use_is_tracked_as_attached() {
        let dev = device("1-1", 0x1234, 0xabcd, 0);
        let backend = MockBackend {
            attach_error: Some(
                "USB device 001.002 is in use by driver QEMU, domain vm".to_string(),
            ),
            ..MockBackend::active()
        };
        let mut d = daemon(&["id=1234:"], vec![dev.clone()], backend, false, false);

        d.process(dev);
        assert!(d.backend.attaches.borrow().is_empty());
        assert!(d.attached.contains_key("1-1"));
    }

    #[test]
    fn no_free_ports_leaves_device_untracked() {
        let dev = device("1-1", 0x1234, 0xabcd, 0);
        let backend = MockBackend {
            attach_error: Some("internal error: No free USB ports".to_string()),
            ..MockBackend::active()
        };
        let mut d = daemon(&["id=1234:"], vec![dev.clone()], backend, false, false);

        d.process(dev);
        assert!(d.backend.attaches.borrow().is_empty());
        assert!(d.attached.is_empty());
    }

    #[test]
    fn sweeps_stale_matching_hostdevs() {
        let backend = MockBackend {
            domain_xml: STALE_DOMAIN_XML.to_string(),
            ..MockBackend::active()
        };
        let mut d = daemon(&["id=2e8a:0003"], vec![], backend, false, false);

        assert_eq!(d.sweep_stale(), 1);
        assert_eq!(d.backend.detaches.borrow().len(), 1);
    }

    #[test]
    fn sweep_leaves_present_devices_alone() {
        let mut present = device("1-1", 0x2e8a, 0x0003, 0);
        present.devnum = 99;
        let backend = MockBackend {
            domain_xml: STALE_DOMAIN_XML.to_string(),
            ..MockBackend::active()
        };
        let mut d = daemon(&["id=2e8a:0003"], vec![present], backend, false, false);

        assert_eq!(d.sweep_stale(), 0);
        assert!(d.backend.detaches.borrow().is_empty());
    }

    #[test]
    fn sweep_ignores_nonmatching_stale_hostdevs() {
        let backend = MockBackend {
            domain_xml: STALE_DOMAIN_XML.to_string(),
            ..MockBackend::active()
        };
        let mut d = daemon(&["id=dead:"], vec![], backend, false, false);

        assert_eq!(d.sweep_stale(), 0);
        assert!(d.backend.detaches.borrow().is_empty());
    }

    #[test]
    fn startup_sweeps_stale_hostdevs() {
        let backend = MockBackend {
            domain_xml: STALE_DOMAIN_XML.to_string(),
            ..MockBackend::active()
        };
        let mut d = daemon(&["id=2e8a:0003"], vec![], backend, false, false);

        d.startup();
        assert_eq!(d.backend.detaches.borrow().len(), 1);
    }

    #[test]
    fn periodic_sweep_cleans_stale_hostdevs() {
        let backend = MockBackend {
            domain_xml: STALE_DOMAIN_XML.to_string(),
            ..MockBackend::active()
        };
        let mut d = daemon(&["id=2e8a:0003"], vec![], backend, false, false);
        d.last_sweep = Instant::now() - STALE_SWEEP_INTERVAL;

        d.maybe_rejoin();
        assert_eq!(d.backend.detaches.borrow().len(), 1);
    }

    #[test]
    fn no_free_ports_sweeps_and_retries() {
        let dev = device("1-1", 0x2e8a, 0x0003, 0);
        let backend = MockBackend {
            attach_error_once: Some("internal error: No free USB ports".to_string()),
            domain_xml: STALE_DOMAIN_XML.to_string(),
            ..MockBackend::active()
        };
        let mut d = daemon(&["id=2e8a:0003"], vec![dev.clone()], backend, false, false);

        d.process(dev);
        // The stale hostdev was detached, freeing a port, and the attach retried.
        assert_eq!(d.backend.detaches.borrow().len(), 1);
        assert_eq!(d.backend.attaches.borrow().len(), 1);
        assert!(d.attached.contains_key("1-1"));
    }
}
