# qemu-stormcrow

## WHAT

A little utility to dynamically passthrough USB devices to running libvirt virtual machines.

## WHY

libvirt supports permanently mapping a USB device to a VM, or dynamically attaching a device once during runtime.  It does not have very good support for temporarily passing through USB devices that are frequently reconnected.  qemu-stormcrow is for that case.

## HOW

qemu-stormcrow should be launched as a daemon first.  `cargo run` in a terminal, or write a systemd service, or spawn it in the background from a script, or whatever.  It does not self-daemonize.

A USB device with a (`Vendor ID`, `Product ID`) pair is registered for a running libvirt VM via D-Bus:

```bash
$ dbus-send --type=method_call --print-reply --dest=com.stormcrow.device /device com.stormcrow.device.Add string:<VM> string:<VID> string:<PID>
```

qemu-stormcrow monitors the udev subsystem for attach/remove events of the VID/PID pair.  When one is attached, qemu-stormcrow generates a libvirt hostdev XML snippet for the device and attaches it to the running VM.  Likewise, it detaches the hostdev device when removed.

When finished, the device can be unregistered.  qemu-stormcrow will no longer monitor for such devices:

```bash
$ dbus-send --type=method_call --print-reply --dest=com.stormcrow.device /device com.stormcrow.device.Remove string:<VM> string:<VID> string:<PID>
```

qemu-stormcrow can be shutdown via D-Bus as well:

```bash
$ dbus-send --type=method_call --print-reply --dest=com.stormcrow.device /device com.stormcrow.device.Quit
```

## SHOULD I USE THIS?

No.  It's a hacky little script for personal use.
