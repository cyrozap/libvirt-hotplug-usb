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

//! Command-line interface.

use clap::Parser;

pub use crate::rule::parse_rule;

/// Pass matching USB devices through to a libvirt guest, continuously.
#[derive(Debug, Parser)]
#[command(name = "libvirt-hotplug-usb", version, about, long_about = None)]
pub struct Cli {
    /// libvirt connection URI.
    #[arg(
        short = 'c',
        long = "connect",
        value_name = "URI",
        default_value = "qemu:///system"
    )]
    pub connect: String,

    /// Name of the target domain.
    #[arg(short = 'd', long = "domain", value_name = "NAME")]
    pub domain: String,

    /// Match rule; may be given multiple times (rules are ORed).
    ///
    /// Examples: 'id=1234:abcd', 'bus=3,id=1234:', 'hub(id=1234:abcd,bus=2)',
    /// 'port=1.4', 'serial=12345678'
    #[arg(short = 'r', long = "rule", value_name = "EXPR", required = true)]
    pub rules: Vec<String>,

    /// Detach USB hostdevs matching the rules from the running domain, then exit.
    ///
    /// Useful for freeing guest USB ports occupied by stale hostdevs.
    #[arg(long = "detach-matching")]
    pub detach_matching: bool,

    /// Detach devices this process attached when it exits.
    #[arg(long = "detach-on-exit")]
    pub detach_on_exit: bool,

    /// Log matching devices without attaching them.
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    /// Increase log verbosity (repeatable).
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,
}
