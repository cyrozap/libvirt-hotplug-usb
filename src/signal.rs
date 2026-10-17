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

//! Minimal SIGINT/SIGTERM handling.
//!
//! The handler only sets an atomic flag; the netlink `recv()` loop is left
//! interruptible (no `SA_RESTART`) so it observes `EINTR` and can exit cleanly.

use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Error, Result};

const SIGINT: c_int = 2;
const SIGTERM: c_int = 15;

static TERMINATED: AtomicBool = AtomicBool::new(false);

// glibc `struct sigaction` layout for Linux (x86_64/aarch64).
#[repr(C)]
struct SigAction {
    sa_handler: extern "C" fn(c_int),
    sa_mask: [u64; 16], // __sigset_t is 128 bytes
    sa_flags: c_int,
    sa_restorer: usize,
}

unsafe extern "C" {
    fn sigaction(sig: c_int, act: *const SigAction, old: *mut SigAction) -> c_int;
}

extern "C" fn on_signal(_sig: c_int) {
    TERMINATED.store(true, Ordering::SeqCst);
}

/// Install handlers for SIGINT and SIGTERM.
pub fn install() -> Result<()> {
    let act = SigAction {
        sa_handler: on_signal,
        sa_mask: [0u64; 16],
        sa_flags: 0,
        sa_restorer: 0,
    };
    for sig in [SIGINT, SIGTERM] {
        // SAFETY: `act` is a valid, fully-initialised `struct sigaction` and
        // `old` is null, which is permitted.
        let r = unsafe { sigaction(sig, &act, std::ptr::null_mut()) };
        if r < 0 {
            return Err(Error::signal(format!(
                "sigaction({sig}): {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}

/// Whether a termination signal has been received.
pub fn terminated() -> bool {
    TERMINATED.load(Ordering::SeqCst)
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    TERMINATED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn set_for_test() {
    TERMINATED.store(true, Ordering::SeqCst);
}
