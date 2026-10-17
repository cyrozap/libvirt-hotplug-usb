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

//! Matching rules and their CLI grammar.
//!
//! Grammar:
//! ```text
//! rule      := criterion (',' criterion)*
//! criterion := 'id=' (HEX ':' | ':' HEX | HEX ':' HEX)
//!            | 'serial=' STR
//!            | 'bus=' (DEC | PATH)
//!            | 'port=' (DEC | DEC ('.' DEC)*)
//!            | 'hub(' rule ')'
//! ```
//! Criteria within a rule are ANDed; top-level rules are ORed.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::usb::{DeviceSource, UsbDevice};

/// How a bus criterion identifies a bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusSpec {
    Number(u32),
    /// A sysfs path to a bus/root hub, resolved to a number before matching.
    Path(PathBuf),
}

/// How a port criterion identifies a port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortSpec {
    /// A top-level port number; matches its whole subtree.
    Number(u32),
    /// An explicit port chain; matches that subtree.
    Path(Vec<u32>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Criterion {
    Id { vid: Option<u16>, pid: Option<u16> },
    Serial(String),
    Bus(BusSpec),
    Port(PortSpec),
    Hub(Box<Rule>),
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Rule {
    pub criteria: Vec<Criterion>,
}

impl Rule {
    /// True if the device satisfies every criterion.
    pub fn matches(&self, dev: &UsbDevice, src: &dyn DeviceSource) -> bool {
        self.criteria.iter().all(|c| c.matches(dev, src))
    }
}

impl Criterion {
    fn matches(&self, dev: &UsbDevice, src: &dyn DeviceSource) -> bool {
        match self {
            Criterion::Id { vid, pid } => {
                vid.is_none_or(|v| v == dev.vid) && pid.is_none_or(|p| p == dev.pid)
            }
            Criterion::Serial(s) => dev.serial.as_deref() == Some(s.as_str()),
            Criterion::Bus(BusSpec::Number(n)) => dev.bus == *n,
            // Unresolved paths never match; `resolve_bus_paths` should have run.
            Criterion::Bus(BusSpec::Path(_)) => false,
            Criterion::Port(PortSpec::Number(n)) => dev.ports.first() == Some(n),
            Criterion::Port(PortSpec::Path(p)) => !p.is_empty() && dev.ports.starts_with(p),
            Criterion::Hub(inner) => dev
                .ancestors()
                .iter()
                .filter_map(|name| src.lookup(name))
                .any(|ancestor| ancestor.is_hub() && inner.matches(&ancestor, src)),
        }
    }
}

/// Resolve every `bus=PATH` criterion to a bus number.
///
/// Called once at startup; afterwards the matcher only sees `BusSpec::Number`.
pub fn resolve_bus_paths(rules: &mut [Rule]) -> Result<()> {
    for rule in rules {
        resolve_rule(rule)?;
    }
    Ok(())
}

fn resolve_rule(rule: &mut Rule) -> Result<()> {
    for criterion in &mut rule.criteria {
        match criterion {
            Criterion::Bus(BusSpec::Path(path)) => {
                let bus = resolve_bus_path(path)?;
                *criterion = Criterion::Bus(BusSpec::Number(bus));
            }
            Criterion::Hub(inner) => resolve_rule(inner)?,
            _ => {}
        }
    }
    Ok(())
}

fn resolve_bus_path(path: &Path) -> Result<u32> {
    let canon = std::fs::canonicalize(path)
        .map_err(|e| Error::usb(format!("cannot resolve bus path {}: {e}", path.display())))?;
    if let Ok(text) = std::fs::read_to_string(canon.join("busnum"))
        && let Ok(n) = text.trim().parse::<u32>()
    {
        return Ok(n);
    }
    // Fall back to a `usbN` path component.
    for component in canon.components() {
        if let std::path::Component::Normal(os) = component
            && let Some(n) = os
                .to_str()
                .and_then(|s| s.strip_prefix("usb"))
                .and_then(|s| s.parse::<u32>().ok())
        {
            return Ok(n);
        }
    }
    Err(Error::usb(format!(
        "no bus number found at {}",
        canon.display()
    )))
}

/// Parse a single rule expression.
pub fn parse_rule(input: &str) -> Result<Rule> {
    let mut parser = RuleParser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    let rule = parser.parse_rule()?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(Error::parse(format!(
            "unexpected trailing input at position {} in {input:?}",
            parser.pos
        )));
    }
    Ok(rule)
}

struct RuleParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl RuleParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ') | Some(b'\t')) {
            self.pos += 1;
        }
    }

    fn starts_with(&self, pat: &str) -> bool {
        self.bytes[self.pos..].starts_with(pat.as_bytes())
    }

    fn parse_rule(&mut self) -> Result<Rule> {
        let mut criteria = vec![self.parse_criterion()?];
        loop {
            self.skip_ws();
            if self.peek() == Some(b',') {
                self.pos += 1;
                criteria.push(self.parse_criterion()?);
            } else {
                break;
            }
        }
        Ok(Rule { criteria })
    }

    fn parse_criterion(&mut self) -> Result<Criterion> {
        self.skip_ws();
        if self.starts_with("hub(") {
            self.pos += 4;
            let inner = self.parse_rule()?;
            self.skip_ws();
            if self.peek() != Some(b')') {
                return Err(Error::parse("expected ')' to close hub(...)"));
            }
            self.pos += 1;
            return Ok(Criterion::Hub(Box::new(inner)));
        }
        if self.starts_with("id=") {
            self.pos += 3;
            return self.parse_id();
        }
        if self.starts_with("serial=") {
            self.pos += 7;
            let value = self.parse_value();
            if value.is_empty() {
                return Err(Error::parse("serial= requires a value"));
            }
            return Ok(Criterion::Serial(value));
        }
        if self.starts_with("bus=") {
            self.pos += 4;
            let value = self.parse_value();
            return parse_bus(&value);
        }
        if self.starts_with("port=") {
            self.pos += 5;
            let value = self.parse_value();
            return parse_port(&value);
        }
        Err(Error::parse(format!(
            "expected a criterion (id=, serial=, bus=, port=, hub(...)) at position {}",
            self.pos
        )))
    }

    /// Read until `,`, `)` or end of input.
    fn parse_value(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == b',' || c == b')' {
                break;
            }
            self.pos += 1;
        }
        String::from_utf8_lossy(&self.bytes[start..self.pos])
            .trim()
            .to_string()
    }

    fn parse_id(&mut self) -> Result<Criterion> {
        let value = self.parse_value();
        let (vid_s, pid_s) = value
            .split_once(':')
            .ok_or_else(|| Error::parse(format!("id= requires a ':' (got {value:?})")))?;
        if pid_s.contains(':') {
            return Err(Error::parse(format!(
                "id= has too many ':' (got {value:?})"
            )));
        }
        let vid = if vid_s.is_empty() {
            None
        } else {
            Some(parse_hex(vid_s, "VID")?)
        };
        let pid = if pid_s.is_empty() {
            None
        } else {
            Some(parse_hex(pid_s, "PID")?)
        };
        if vid.is_none() && pid.is_none() {
            return Err(Error::parse("id=: matches neither VID nor PID"));
        }
        Ok(Criterion::Id { vid, pid })
    }
}

fn parse_hex(value: &str, what: &str) -> Result<u16> {
    let t = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if t.is_empty() {
        return Err(Error::parse(format!("{what} is empty")));
    }
    u16::from_str_radix(t, 16).map_err(|e| Error::parse(format!("invalid {what} {value:?}: {e}")))
}

fn parse_bus(value: &str) -> Result<Criterion> {
    if value.is_empty() {
        return Err(Error::parse("bus= requires a value"));
    }
    if value.starts_with('/') {
        return Ok(Criterion::Bus(BusSpec::Path(PathBuf::from(value))));
    }
    let n: u32 = value
        .parse()
        .map_err(|_| Error::parse(format!("invalid bus number {value:?}")))?;
    Ok(Criterion::Bus(BusSpec::Number(n)))
}

fn parse_port(value: &str) -> Result<Criterion> {
    if value.is_empty() {
        return Err(Error::parse("port= requires a value"));
    }
    if value.contains('.') {
        let mut ports = Vec::new();
        for part in value.split('.') {
            if part.is_empty() {
                return Err(Error::parse(format!("invalid port path {value:?}")));
            }
            let n: u32 = part
                .parse()
                .map_err(|_| Error::parse(format!("invalid port path {value:?}")))?;
            ports.push(n);
        }
        Ok(Criterion::Port(PortSpec::Path(ports)))
    } else {
        let n: u32 = value
            .parse()
            .map_err(|_| Error::parse(format!("invalid port number {value:?}")))?;
        Ok(Criterion::Port(PortSpec::Number(n)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb::test_support::{MapSource, device};

    fn parse_ok(input: &str) -> Rule {
        parse_rule(input).expect("rule should parse")
    }

    #[test]
    fn parses_id_forms() {
        assert_eq!(
            parse_ok("id=1234:"),
            Rule {
                criteria: vec![Criterion::Id {
                    vid: Some(0x1234),
                    pid: None
                }]
            }
        );
        assert_eq!(
            parse_ok("id=:abcd"),
            Rule {
                criteria: vec![Criterion::Id {
                    vid: None,
                    pid: Some(0xabcd)
                }]
            }
        );
        assert_eq!(
            parse_ok("id=1234:abcd"),
            Rule {
                criteria: vec![Criterion::Id {
                    vid: Some(0x1234),
                    pid: Some(0xabcd)
                }]
            }
        );
        assert_eq!(
            parse_ok("id=0x1234:0XABCD"),
            Rule {
                criteria: vec![Criterion::Id {
                    vid: Some(0x1234),
                    pid: Some(0xabcd)
                }]
            }
        );
    }

    #[test]
    fn parses_bus_and_port() {
        assert_eq!(
            parse_ok("bus=3"),
            Rule {
                criteria: vec![Criterion::Bus(BusSpec::Number(3))]
            }
        );
        assert_eq!(
            parse_ok("bus=/sys/bus/usb/devices/usb3"),
            Rule {
                criteria: vec![Criterion::Bus(BusSpec::Path(PathBuf::from(
                    "/sys/bus/usb/devices/usb3"
                )))]
            }
        );
        assert_eq!(
            parse_ok("port=1"),
            Rule {
                criteria: vec![Criterion::Port(PortSpec::Number(1))]
            }
        );
        assert_eq!(
            parse_ok("port=1.4"),
            Rule {
                criteria: vec![Criterion::Port(PortSpec::Path(vec![1, 4]))]
            }
        );
    }

    #[test]
    fn parses_nested_hub_and_combinations() {
        let rule = parse_ok("hub(id=1234:abcd,bus=2),serial=12345678");
        assert_eq!(rule.criteria.len(), 2);
        match &rule.criteria[0] {
            Criterion::Hub(inner) => {
                assert_eq!(inner.criteria.len(), 2);
                assert!(matches!(
                    inner.criteria[0],
                    Criterion::Id {
                        vid: Some(0x1234),
                        pid: Some(0xabcd)
                    }
                ));
                assert!(matches!(
                    inner.criteria[1],
                    Criterion::Bus(BusSpec::Number(2))
                ));
            }
            other => panic!("expected hub, got {other:?}"),
        }
        assert!(matches!(rule.criteria[1], Criterion::Serial(ref s) if s == "12345678"));

        let nested = parse_ok("hub(id=1:,hub(id=:2))");
        assert!(matches!(nested.criteria[0], Criterion::Hub(_)));
    }

    #[test]
    fn rejects_bad_rules() {
        for bad in [
            "",
            "id=1234",
            "id=:",
            "id=zzzz:",
            "id=1234:zz",
            "id=1234:5:6",
            "bus=",
            "bus=abc",
            "port=",
            "port=1..2",
            "port=a.b",
            "serial=",
            "hub()",
            "hub(id=1234:",
            "id=1234:,junk",
            "nonsense=1",
        ] {
            assert!(parse_rule(bad).is_err(), "expected {bad:?} to be rejected");
        }
    }

    #[test]
    fn id_matching() {
        let mut dev = device("1-1", 0x1234, 0xabcd, 0);
        dev.serial = Some("SER".into());
        let src = MapSource::default();
        assert!(parse_ok("id=1234:").matches(&dev, &src));
        assert!(parse_ok("id=:abcd").matches(&dev, &src));
        assert!(parse_ok("id=1234:abcd").matches(&dev, &src));
        assert!(!parse_ok("id=9999:").matches(&dev, &src));
        assert!(!parse_ok("id=:9999").matches(&dev, &src));
        assert!(!parse_ok("id=1234:9999").matches(&dev, &src));
        assert!(parse_ok("serial=SER").matches(&dev, &src));
        assert!(!parse_ok("serial=OTHER").matches(&dev, &src));
    }

    #[test]
    fn bus_and_port_matching() {
        let src = MapSource::default();
        let direct = device("1-3", 1, 2, 0); // bus 1, port 3
        let deep = device("1-3.2", 1, 2, 0); // bus 1, port 3.2
        let other = device("1-4", 1, 2, 0); // bus 1, port 4
        let bus2 = device("2-1", 1, 2, 0); // bus 2

        assert!(parse_ok("bus=1").matches(&direct, &src));
        assert!(!parse_ok("bus=1").matches(&bus2, &src));
        // port=N matches the whole subtree under that top-level port
        assert!(parse_ok("port=3").matches(&direct, &src));
        assert!(parse_ok("port=3").matches(&deep, &src));
        assert!(!parse_ok("port=3").matches(&other, &src));
        // port=1.4 matches the exact port path and its subtree
        assert!(parse_ok("port=3.2").matches(&deep, &src));
        assert!(!parse_ok("port=3.2").matches(&direct, &src));
        // AND within a rule
        assert!(parse_ok("bus=1,port=3").matches(&deep, &src));
        assert!(!parse_ok("bus=2,port=3").matches(&deep, &src));
    }

    #[test]
    fn hub_matching_is_transitive() {
        let root = device("usb1", 0x1d6b, 0x0002, 0x09);
        let mut hub = device("1-1", 0x1234, 0xabcd, 0x09);
        hub.serial = Some("HUBSER".into());
        let mut hub2 = device("1-1.4", 0x5678, 0x0001, 0x09);
        hub2.serial = Some("HUB2".into());
        let leaf = device("1-1.4.2", 0xdead, 0xbeef, 0);
        let direct = device("1-7", 0xdead, 0xbeef, 0);

        let src = MapSource::new(vec![root, hub, hub2, leaf.clone(), direct.clone()]);

        // leaf is behind 1-1.4 which is behind 1-1 (transitive)
        assert!(parse_ok("hub(id=1234:abcd)").matches(&leaf, &src));
        assert!(parse_ok("hub(serial=HUBSER)").matches(&leaf, &src));
        assert!(parse_ok("hub(serial=HUB2)").matches(&leaf, &src));
        // nested hub: the matching hub must itself be behind a matching hub
        // (leaf -> 1-1.4 [HUB2] -> 1-1 [HUBSER] -> usb1)
        assert!(parse_ok("hub(serial=HUB2,hub(serial=HUBSER))").matches(&leaf, &src));
        assert!(!parse_ok("hub(serial=HUBSER,hub(serial=HUB2))").matches(&leaf, &src));
        assert!(!parse_ok("hub(id=1234:abcd)").matches(&direct, &src));
        // multiple criteria on the leaf
        assert!(parse_ok("id=dead:beef,hub(serial=HUB2)").matches(&leaf, &src));
        // hubs themselves can match too (caller decides not to attach them)
        assert!(parse_ok("id=1234:abcd").matches(&src.lookup("1-1").unwrap(), &src));
    }

    #[test]
    fn identical_devices_both_match() {
        let a = device("1-1", 0x1234, 0xabcd, 0);
        let b = device("1-2", 0x1234, 0xabcd, 0);
        let src = MapSource::default();
        let rule = parse_ok("id=1234:abcd");
        assert!(rule.matches(&a, &src));
        assert!(rule.matches(&b, &src));
    }
}
