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

use std::fmt;

/// Crate-wide result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Error type for the daemon.
///
/// Deliberately hand-written (no `anyhow`/`thiserror`): a small closed enum is
/// enough for this program.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Parse(String),
    Usb(String),
    Libvirt(String),
    Signal(String),
    Monitor(String),
}

impl Error {
    pub fn parse(msg: impl Into<String>) -> Self {
        Error::Parse(msg.into())
    }

    pub fn usb(msg: impl Into<String>) -> Self {
        Error::Usb(msg.into())
    }

    pub fn libvirt(msg: impl Into<String>) -> Self {
        Error::Libvirt(msg.into())
    }

    pub fn signal(msg: impl Into<String>) -> Self {
        Error::Signal(msg.into())
    }

    pub fn monitor(msg: impl Into<String>) -> Self {
        Error::Monitor(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Parse(m) => write!(f, "parse error: {m}"),
            Error::Usb(m) => write!(f, "USB error: {m}"),
            Error::Libvirt(m) => write!(f, "libvirt error: {m}"),
            Error::Signal(m) => write!(f, "signal error: {m}"),
            Error::Monitor(m) => write!(f, "monitor error: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<std::ffi::NulError> for Error {
    fn from(e: std::ffi::NulError) -> Self {
        Error::Libvirt(format!("string contains a NUL byte: {e}"))
    }
}
