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

//! libvirt backend.
//!
//! By default the `libvirt.so.0` shared object is loaded at runtime with
//! `dlopen`, so the binary does not need to link against libvirt. Building with
//! `--features link-virt` instead links against libvirt at build time.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::time::{Duration, Instant};

use log::{debug, info};

use crate::error::{Error, Result};

use super::VmBackend;

const VIR_DOMAIN_AFFECT_LIVE: c_uint = 1;

type VirConnectPtr = *mut c_void;
type VirDomainPtr = *mut c_void;

type ConnectOpenFn = unsafe extern "C" fn(*const c_char) -> VirConnectPtr;
type ConnectCloseFn = unsafe extern "C" fn(VirConnectPtr) -> c_int;
type DomainLookupByNameFn = unsafe extern "C" fn(VirConnectPtr, *const c_char) -> VirDomainPtr;
type DomainFreeFn = unsafe extern "C" fn(VirDomainPtr) -> c_int;
type DomainIsActiveFn = unsafe extern "C" fn(VirDomainPtr) -> c_int;
type DomainDeviceFlagsFn = unsafe extern "C" fn(VirDomainPtr, *const c_char, c_uint) -> c_int;
type LastErrorMessageFn = unsafe extern "C" fn() -> *const c_char;
type InitializeFn = unsafe extern "C" fn() -> c_int;
type SetErrorFuncFn =
    unsafe extern "C" fn(*mut c_void, extern "C" fn(*mut c_void, *mut c_void)) -> c_int;

/// libvirt's default error logging writes to stderr; replace it with a no-op
/// because errors are surfaced through `virGetLastErrorMessage` instead.
extern "C" fn ignore_libvirt_error(_user_data: *mut c_void, _error: *mut c_void) {}

/// Resolved libvirt entry points.
struct Api {
    connect_open: ConnectOpenFn,
    connect_close: ConnectCloseFn,
    domain_lookup_by_name: DomainLookupByNameFn,
    domain_free: DomainFreeFn,
    domain_is_active: DomainIsActiveFn,
    domain_attach_device_flags: DomainDeviceFlagsFn,
    domain_detach_device_flags: DomainDeviceFlagsFn,
    last_error_message: LastErrorMessageFn,
    initialize: InitializeFn,
    set_error_func: SetErrorFuncFn,
}

#[cfg(not(feature = "link-virt"))]
mod loading {
    use super::Api;
    use crate::error::{Error, Result};
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_int, c_void};

    const RTLD_NOW: c_int = 2;

    unsafe extern "C" {
        fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn dlerror() -> *mut c_char;
    }

    impl Api {
        pub fn load() -> Result<Self> {
            // SAFETY: arguments are valid C strings; symbol pointers are
            // transmuted to their matching function-pointer types.
            unsafe {
                let mut handle = std::ptr::null_mut();
                for soname in ["libvirt.so.0", "libvirt.so"] {
                    let name = CString::new(soname).unwrap();
                    handle = dlopen(name.as_ptr(), RTLD_NOW);
                    if !handle.is_null() {
                        break;
                    }
                }
                if handle.is_null() {
                    return Err(Error::libvirt(format!(
                        "could not load libvirt ({}); install libvirt so that libvirt.so.0 \
                         is available, or build with --features link-virt",
                        last_dlerror()
                    )));
                }
                Ok(Api {
                    connect_open: resolve(handle, "virConnectOpen")?,
                    connect_close: resolve(handle, "virConnectClose")?,
                    domain_lookup_by_name: resolve(handle, "virDomainLookupByName")?,
                    domain_free: resolve(handle, "virDomainFree")?,
                    domain_is_active: resolve(handle, "virDomainIsActive")?,
                    domain_attach_device_flags: resolve(handle, "virDomainAttachDeviceFlags")?,
                    domain_detach_device_flags: resolve(handle, "virDomainDetachDeviceFlags")?,
                    last_error_message: resolve(handle, "virGetLastErrorMessage")?,
                    initialize: resolve(handle, "virInitialize")?,
                    set_error_func: resolve(handle, "virSetErrorFunc")?,
                })
            }
        }
    }

    unsafe fn resolve<T: Copy>(handle: *mut c_void, symbol: &str) -> Result<T> {
        let name = CString::new(symbol).unwrap();
        // SAFETY: `handle` came from `dlopen`; `name` is NUL-terminated.
        let ptr = unsafe { dlsym(handle, name.as_ptr()) };
        if ptr.is_null() {
            return Err(Error::libvirt(format!(
                "libvirt is missing the {symbol} symbol"
            )));
        }
        // SAFETY: every `T` used here is a function-pointer type, which has the
        // same size/layout as the `*mut c_void` returned by `dlsym`.
        Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&ptr) })
    }

    fn last_dlerror() -> String {
        // SAFETY: `dlerror` returns a NUL-terminated string or null.
        unsafe {
            let p = dlerror();
            if p.is_null() {
                "unknown error".to_string()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }
}

#[cfg(feature = "link-virt")]
mod loading {
    use super::Api;
    use crate::error::Result;
    use std::os::raw::{c_char, c_uint, c_void};

    #[link(name = "virt")]
    unsafe extern "C" {
        fn virConnectOpen(name: *const c_char) -> *mut c_void;
        fn virConnectClose(conn: *mut c_void) -> std::os::raw::c_int;
        fn virDomainLookupByName(conn: *mut c_void, name: *const c_char) -> *mut c_void;
        fn virDomainFree(domain: *mut c_void) -> std::os::raw::c_int;
        fn virDomainIsActive(domain: *mut c_void) -> std::os::raw::c_int;
        fn virDomainAttachDeviceFlags(
            domain: *mut c_void,
            xml: *const c_char,
            flags: c_uint,
        ) -> std::os::raw::c_int;
        fn virDomainDetachDeviceFlags(
            domain: *mut c_void,
            xml: *const c_char,
            flags: c_uint,
        ) -> std::os::raw::c_int;
        fn virGetLastErrorMessage() -> *const c_char;
        fn virInitialize() -> std::os::raw::c_int;
        fn virSetErrorFunc(
            user_data: *mut c_void,
            handler: extern "C" fn(*mut c_void, *mut c_void),
        ) -> std::os::raw::c_int;
    }

    impl Api {
        pub fn load() -> Result<Self> {
            Ok(Api {
                connect_open: virConnectOpen,
                connect_close: virConnectClose,
                domain_lookup_by_name: virDomainLookupByName,
                domain_free: virDomainFree,
                domain_is_active: virDomainIsActive,
                domain_attach_device_flags: virDomainAttachDeviceFlags,
                domain_detach_device_flags: virDomainDetachDeviceFlags,
                last_error_message: virGetLastErrorMessage,
                initialize: virInitialize,
                set_error_func: virSetErrorFunc,
            })
        }
    }
}

pub struct LibvirtBackend {
    api: Api,
    uri: String,
    domain_name: String,
    conn: VirConnectPtr,
    domain: VirDomainPtr,
    next_retry: Instant,
    retry_delay: Duration,
}

impl LibvirtBackend {
    /// Load the libvirt library and prepare a lazily-connected backend.
    pub fn new(uri: impl Into<String>, domain: impl Into<String>) -> Result<Self> {
        let api = Api::load()?;
        // Silence libvirt's own stderr logging; errors are reported by us.
        unsafe {
            (api.initialize)();
            (api.set_error_func)(std::ptr::null_mut(), ignore_libvirt_error);
        }
        Ok(LibvirtBackend {
            api,
            uri: uri.into(),
            domain_name: domain.into(),
            conn: std::ptr::null_mut(),
            domain: std::ptr::null_mut(),
            next_retry: Instant::now(),
            retry_delay: Duration::from_secs(1),
        })
    }

    fn last_error(&self) -> String {
        // SAFETY: the function returns a thread-local static string or null.
        let p = unsafe { (self.api.last_error_message)() };
        if p.is_null() {
            "unknown libvirt error".to_string()
        } else {
            // SAFETY: non-null, NUL-terminated string.
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    }

    fn backoff(&mut self) {
        self.next_retry = Instant::now() + self.retry_delay;
        self.retry_delay = (self.retry_delay * 2).min(Duration::from_secs(30));
    }

    fn invalidate_domain(&mut self) {
        if !self.domain.is_null() {
            unsafe { (self.api.domain_free)(self.domain) };
            self.domain = std::ptr::null_mut();
        }
    }

    /// Ensure a connection and domain handle exist (with simple backoff).
    fn ensure(&mut self) -> Result<()> {
        if !self.conn.is_null() && !self.domain.is_null() {
            return Ok(());
        }
        if Instant::now() < self.next_retry {
            return Err(Error::libvirt("reconnect backoff in effect"));
        }

        if self.conn.is_null() {
            let uri = CString::new(self.uri.as_str())?;
            // SAFETY: `uri` is a valid C string.
            let conn = unsafe { (self.api.connect_open)(uri.as_ptr()) };
            if conn.is_null() {
                let msg = self.last_error();
                self.backoff();
                return Err(Error::libvirt(format!(
                    "cannot connect to {:?}: {msg}",
                    self.uri
                )));
            }
            self.conn = conn;
            self.retry_delay = Duration::from_secs(1);
            self.next_retry = Instant::now();
            debug!("connected to libvirt at {}", self.uri);
        }

        if self.domain.is_null() {
            let name = CString::new(self.domain_name.as_str())?;
            // SAFETY: connection is non-null and `name` is a valid C string.
            let domain = unsafe { (self.api.domain_lookup_by_name)(self.conn, name.as_ptr()) };
            if domain.is_null() {
                let msg = self.last_error();
                self.backoff();
                return Err(Error::libvirt(format!(
                    "cannot find domain {:?}: {msg}",
                    self.domain_name
                )));
            }
            self.domain = domain;
            self.next_retry = Instant::now();
        }

        Ok(())
    }
}

impl VmBackend for LibvirtBackend {
    fn is_active(&mut self) -> Result<bool> {
        self.ensure()?;
        // SAFETY: `self.domain` is non-null after `ensure`.
        let r = unsafe { (self.api.domain_is_active)(self.domain) };
        if r < 0 {
            let msg = self.last_error();
            self.invalidate_domain();
            return Err(Error::libvirt(msg));
        }
        Ok(r == 1)
    }

    fn attach(&mut self, xml: &str) -> Result<()> {
        self.ensure()?;
        let xml = CString::new(xml)?;
        // SAFETY: domain and xml are valid.
        let r = unsafe {
            (self.api.domain_attach_device_flags)(self.domain, xml.as_ptr(), VIR_DOMAIN_AFFECT_LIVE)
        };
        if r < 0 {
            return Err(Error::libvirt(self.last_error()));
        }
        Ok(())
    }

    fn detach(&mut self, xml: &str) -> Result<()> {
        self.ensure()?;
        let xml = CString::new(xml)?;
        // SAFETY: domain and xml are valid.
        let r = unsafe {
            (self.api.domain_detach_device_flags)(self.domain, xml.as_ptr(), VIR_DOMAIN_AFFECT_LIVE)
        };
        if r < 0 {
            return Err(Error::libvirt(self.last_error()));
        }
        Ok(())
    }
}

impl Drop for LibvirtBackend {
    fn drop(&mut self) {
        // SAFETY: freeing null-or-valid handles.
        unsafe {
            if !self.domain.is_null() {
                (self.api.domain_free)(self.domain);
            }
            if !self.conn.is_null() {
                (self.api.connect_close)(self.conn);
            }
        }
        info!("disconnected from libvirt");
    }
}

#[cfg(test)]
mod tests {
    use super::Api;

    #[test]
    fn libvirt_library_loads_when_present() {
        let candidates = [
            "/usr/lib/x86_64-linux-gnu/libvirt.so.0",
            "/usr/lib/aarch64-linux-gnu/libvirt.so.0",
            "/usr/lib64/libvirt.so.0",
            "/usr/lib/libvirt.so.0",
        ];
        let installed = candidates.iter().any(|p| std::path::Path::new(p).exists());

        match Api::load() {
            Ok(_) => {}
            Err(e) => {
                if installed || cfg!(feature = "link-virt") {
                    panic!("libvirt is installed but loading symbols failed: {e}");
                }
                eprintln!("skipping: libvirt not installed ({e})");
            }
        }
    }
}
