# libvirt-hotplug-usb

A daemon that watches Linux USB hot-plug events and passes matching devices through to a libvirt guest.
No udev rules or other host configuration are required.

[![The latest version of the libvirt-hotplug-usb crate on crates.io](https://img.shields.io/crates/v/libvirt-hotplug-usb)](https://crates.io/crates/libvirt-hotplug-usb)


## Building

```console
$ cargo build --release
```

By default the binary loads `libvirt.so.0` at runtime, so building does not require libvirt headers.
To link against libvirt at build time instead (requires `libvirt-dev`/`pkg-config`), build with the `link-virt` feature:

```console
$ cargo build --release --features link-virt
```


## Usage

```console
libvirt-hotplug-usb [OPTIONS] --domain <NAME> --rule <EXPR>...
  -c, --connect <URI>    libvirt URI (default qemu:///system)
  -d, --domain <NAME>    target domain (required)
  -r, --rule <EXPR>      match rule, repeatable (rules are ORed)
      --detach-matching  detach matching hostdevs from the running domain, then exit
      --detach-on-exit   detach devices this process attached on exit
      --dry-run          log matching devices without attaching
  -v, --verbose          increase verbosity (repeatable)
```


### Rule grammar

```text
rule      := criterion (',' criterion)*
criterion := 'id=' (HEX ':' | ':' HEX | HEX ':' HEX)
           | 'serial=' STR
           | 'bus=' (DEC | PATH)
           | 'port=' (DEC | DEC ('.' DEC)*)
           | 'hub(' rule ')'
```

Criteria within a rule are ANDed, separate `--rule` arguments are ORed.

- `id=1234:`: any device with VID `1234`
- `id=:abcd`: any device with PID `abcd`
- `id=1234:abcd`: exact VID:PID pair
- `serial=12345678`: exact serial number
- `bus=3`: devices on USB bus 3
- `bus=/sys/bus/usb/devices/usb3`: same, but by sysfs path
- `port=1`: devices under top-level port 1 of the bus
- `port=1.4`: devices under the `1.4` port path (i.e. behind the hub on port 1)
- `hub(...)`: devices with any ancestor hub matching the nested rule

Examples:

```console

# All bus-3 devices with VID 1234
$ libvirt-hotplug-usb -d myvm -r 'bus=3,id=1234:'


# Devices under a 1234:abcd hub that is itself on bus 2
$ libvirt-hotplug-usb -d myvm -r 'hub(id=1234:abcd,bus=2)'


# Union: exact pair, VID-only, or a serial number
$ libvirt-hotplug-usb -d myvm -r 'id=5678:1234' -r 'id=cafe:' -r 'serial=11111111'
```

`bus=` accepts either a bus number or a path to the bus/root hub, which is resolved once at startup.


## Privileges

- `qemu:///system` normally requires root or membership in the `libvirt` group (plus a polkit rule).
- Subscribing to kernel uevents may also require elevated privileges depending on the kernel/net-namespace configuration.

Either run the daemon as root, or with sufficient privileges to both access libvirt's system socket and receive kernel uevents.


## Example systemd unit

See [`contrib/libvirt-hotplug-usb.service`](contrib/libvirt-hotplug-usb.service).


## How it works

At startup the daemon scans `/sys/bus/usb/devices` and processes every device that already matches.
It then subscribes to kernel uevents via a `NETLINK_KOBJECT_UEVENT` socket and attaches matching devices as soon as they are connected.
Hubs (`bDeviceClass == 0x09`) are never passed through--only devices connected through them.

Matching devices are hot-plugged into the target domain with libvirt's `virDomainAttachDeviceFlags(..., VIR_DOMAIN_AFFECT_LIVE)`, using:

```xml
<hostdev mode='subsystem' type='usb' managed='yes'>
  <source>
    <vendor id='0x1234'/><product id='0xabcd'/>
    <address bus='1' device='4'/>
  </source>
</hostdev>
```

When a device is unplugged, the daemon detaches its hostdev so the guest USB port is freed, and a periodic sweep removes hostdevs whose host device has disappeared. This prevents repeated replugs from exhausting the guest's USB ports.


## Troubleshooting

### `failed to attach ...: internal error: No free USB ports`

libvirt assigns one *guest* USB port to each passed-through device. `qemu-xhci` controllers default to only **4 ports**, and QEMU keeps a passed-through `usb-host` device (and its guest port) when the physical device is unplugged.
Repeated replugs, or attaching more devices than there are ports, exhaust the guest's USB topology.

Inspect the live domain:

```console
$ virsh dumpxml --domain myvm | grep -c "hostdev mode='subsystem' type='usb'"
$ virsh dumpxml --domain myvm | grep "controller type='usb'"
$ virsh qemu-monitor-command myvm --hmp 'info usb'
```

The daemon normally prevents this automatically:

- When a device is unplugged, the daemon immediately detaches its hostdev from the guest, freeing the guest port.
- At startup and every few seconds it reads the live domain XML and detaches hostdevs whose host device has disappeared and which match your rules.

If ports are still exhausted (for example, leftovers from a missed event), free them immediately with `--detach-matching`, which detaches every hostdev (working or stale) whose host device matches your rules:

```console
# Preview what would be detached, then detach:
$ libvirt-hotplug-usb -d myvm -r 'id=1234:abcd' --detach-matching --dry-run
$ libvirt-hotplug-usb -d myvm -r 'id=1234:abcd' --detach-matching
```

The domain must be running.
For a device that is no longer present on the host, only information contained in the domain XML is available, so `id=` and `bus=` rules can still match while `serial=`, `port=` and `hub(...)` rules cannot.
However, the automatic per-unplug cleanup uses the hostdev the daemon itself attached, so it works for all rule types.

Alternatively, because hostdevs attached with `--live` (as this daemon does) are not in the persistent configuration, a guest restart clears all of them:

```console
$ virsh shutdown myvm && virsh start myvm   # or: virsh destroy myvm
```

Increase capacity permanently (requires a guest power cycle; USB controllers cannot be hot-added):

```console
$ virsh edit myvm
```

```xml
<controller type='usb' index='0' model='qemu-xhci' ports='15'/>
<!-- and/or a second controller -->
<controller type='usb' index='1' model='qemu-xhci' ports='8'/>
```

`ports` is effectively capped at 15 per controller.

The daemon detaches a hostdev it previously created before re-attaching a device that reappears on the same port, which prevents most replug-related port leaks.
It also recognises libvirt's "already in use" error (e.g. after a daemon restart) and tracks the device as already attached instead of retrying.


## Known limitations

- No configuration file yet (command line only).
- The daemon needs to be restarted to add/remove rules.
- One target domain per process.
- The daemon is single-threaded--a slow libvirt call briefly delays event handling.
- If the guest's USB ports are exhausted, attach fails and is logged; the daemon does not add controllers or ports itself (see Troubleshooting).
