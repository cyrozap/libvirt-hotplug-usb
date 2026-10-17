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

//! Test double for [`crate::vm::VmBackend`].

use std::cell::RefCell;

use crate::error::{Error, Result};

use super::VmBackend;

#[derive(Default)]
pub struct MockBackend {
    pub active: bool,
    pub fail_attach: bool,
    pub attaches: RefCell<Vec<String>>,
    pub detaches: RefCell<Vec<String>>,
}

impl MockBackend {
    pub fn active() -> Self {
        MockBackend {
            active: true,
            ..Default::default()
        }
    }
}

impl VmBackend for MockBackend {
    fn is_active(&mut self) -> Result<bool> {
        Ok(self.active)
    }

    fn attach(&mut self, xml: &str) -> Result<()> {
        if self.fail_attach {
            return Err(Error::libvirt("mock attach failure"));
        }
        self.attaches.borrow_mut().push(xml.to_string());
        Ok(())
    }

    fn detach(&mut self, xml: &str) -> Result<()> {
        self.detaches.borrow_mut().push(xml.to_string());
        Ok(())
    }
}
