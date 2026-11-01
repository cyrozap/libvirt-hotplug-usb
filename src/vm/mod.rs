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

//! VM backends.

use crate::error::Result;

pub mod libvirt;

#[cfg(test)]
pub mod mock;

/// Operations the daemon needs from the hypervisor.
pub trait VmBackend {
    /// Whether the target domain is currently running.
    fn is_active(&mut self) -> Result<bool>;
    /// The current (live) domain XML.
    fn domain_xml(&mut self) -> Result<String>;
    /// Attach the host device described by `xml`.
    fn attach(&mut self, xml: &str) -> Result<()>;
    /// Detach the host device described by `xml`.
    fn detach(&mut self, xml: &str) -> Result<()>;
}

/// Backend used by `--dry-run`; never contacts libvirt.
pub struct DryRunBackend;

impl VmBackend for DryRunBackend {
    fn is_active(&mut self) -> Result<bool> {
        Ok(false)
    }

    fn domain_xml(&mut self) -> Result<String> {
        Ok(String::new())
    }

    fn attach(&mut self, _xml: &str) -> Result<()> {
        Ok(())
    }

    fn detach(&mut self, _xml: &str) -> Result<()> {
        Ok(())
    }
}
