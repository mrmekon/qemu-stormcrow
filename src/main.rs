use dbus::blocking::Connection as DbusConnection;
use dbus_crossroads::{Context, Crossroads};
use mio::{Events, Interest, Poll, Token};
use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::Duration;
use udev::MonitorBuilder;
use virt::connect::Connect;
use virt::domain::Domain;

#[derive(Debug)]
pub enum DbusCommand {
    Add,
    Remove,
    Shutdown,
}

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
pub struct UsbFilter {
    vid: Option<String>,
    pid: Option<String>,
}

struct DbusDevice {
    sender: Sender<(DbusCommand, String, UsbFilter)>,
}

// $ dbus-send --type=method_call --print-reply --dest=com.stormcrow.device /device com.stormcrow.device.Add string:<VM> string:<VID> string:<PID>
fn dbus_server(sender: Sender<(DbusCommand, String, UsbFilter)>) -> Result<(), Box<dyn Error>> {
    let c = DbusConnection::new_session()?;
    c.request_name("com.stormcrow.device", false, true, false)?;
    let mut cr = Crossroads::new();
    let iface_token = cr.register("com.stormcrow.device", |b| {
        b.method(
            "Add",
            ("vm", "vid", "pid"),
            ("reply",),
            move |_ctx: &mut Context,
                  dev: &mut DbusDevice,
                  (vm, vid, pid): (String, String, String)| {
                println!("Incoming Add call for {}:{}!", vid, pid);
                let filter = UsbFilter {
                    vid: match vid.len() == 4 {
                        true => Some(vid),
                        _ => None,
                    },
                    pid: match pid.len() == 4 {
                        true => Some(pid),
                        _ => None,
                    },
                };
                dev.sender
                    .send((DbusCommand::Add, vm, filter))
                    .expect("failed to transmit from dbus channel");
                let reply = "OK";
                Ok((reply,))
            },
        );
        b.method(
            "Remove",
            ("vm", "vid", "pid"),
            ("reply",),
            move |_ctx: &mut Context,
                  dev: &mut DbusDevice,
                  (vm, vid, pid): (String, String, String)| {
                println!("Incoming Remove call for {}:{}!", vid, pid);
                let filter = UsbFilter {
                    vid: match vid.len() == 4 {
                        true => Some(vid),
                        _ => None,
                    },
                    pid: match pid.len() == 4 {
                        true => Some(pid),
                        _ => None,
                    },
                };
                dev.sender
                    .send((DbusCommand::Remove, vm, filter))
                    .expect("failed to transmit from dbus channel");
                let reply = "OK";
                Ok((reply,))
            },
        );
        b.method(
            "Quit",
            (),
            ("reply",),
            move |_ctx: &mut Context, dev: &mut DbusDevice, (): ()| {
                dev.sender
                    .send((
                        DbusCommand::Shutdown,
                        "".into(),
                        UsbFilter {
                            vid: None,
                            pid: None,
                        },
                    ))
                    .expect("failed to transmit from dbus channel");
                Ok(("BYE",))
            },
        );
    });

    cr.insert("/device", &[iface_token], DbusDevice { sender });

    // Serve clients forever.
    cr.serve(&c)?;
    Ok(())
}

fn usb_xml(vid: &str, pid: &str, bus: &str, dev: &str) -> String {
    format!(
        r"
<hostdev mode='subsystem' type='usb'>
  <source>
    <vendor id='0x{}'/>
    <product id='0x{}'/>
    <address bus='{}' device='{}'/>
  </source>
</hostdev>
",
        vid, pid, bus, dev
    )
}

pub fn poll(
    mut socket: udev::MonitorSocket,
    receiver: Receiver<(DbusCommand, String, UsbFilter)>,
) -> io::Result<()> {
    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(1024);

    let mut filters = BTreeMap::<String, HashSet<UsbFilter>>::new();
    let mut sysdevs = BTreeMap::<PathBuf, UsbFilter>::new();
    let mut xmls = BTreeMap::<String, Vec<(PathBuf, String)>>::new();

    let uri = "qemu:///system";
    println!("Attempting to connect to hypervisor: '{}'...", uri);
    let mut conn = match Connect::open(uri) {
        Ok(c) => c,
        Err(e) => panic!("No connection to hypervisor: {}", e),
    };

    poll.registry().register(
        &mut socket,
        Token(0),
        Interest::READABLE | Interest::WRITABLE,
    )?;

    println!("Polling udev monitor...");
    'event: loop {
        poll.poll(&mut events, Some(Duration::from_millis(200)))?;
        while let Ok(msg) = receiver.try_recv() {
            match msg.0 {
                DbusCommand::Shutdown => {
                    break 'event;
                }
                DbusCommand::Add => {
                    let vm = msg.1;
                    let filter = msg.2;
                    if !filters.contains_key(&vm) {
                        filters.insert(vm.clone(), HashSet::new());
                    }
                    if let Some(usb_filters) = filters.get_mut(&vm) {
                        if !usb_filters.contains(&filter) {
                            println!("udev add: {:?}:{:?}", filter.vid, filter.pid);
                            usb_filters.insert(filter);
                        }
                    }
                }
                DbusCommand::Remove => {
                    let vm = msg.1;
                    let filter = msg.2;
                    if let Some(usb_filters) = filters.get_mut(&vm) {
                        if usb_filters.contains(&filter) {
                            println!("udev rem: {:?}:{:?}", filter.vid, filter.pid);
                            usb_filters.remove(&filter);
                        }
                    }
                }
            }
        }

        for event in &events {
            if event.token() == Token(0) && event.is_writable() {
                socket.iter().for_each(|x| {
                    let syspath = x.device().syspath().to_owned();
                    let mut vidpath = syspath.clone();
                    let mut pidpath = syspath.clone();
                    let mut buspath = syspath.clone();
                    let mut devpath = syspath.clone();
                    vidpath.push("idVendor");
                    pidpath.push("idProduct");
                    buspath.push("busnum");
                    devpath.push("devnum");
                    match x.event_type() {
                        udev::EventType::Add => {
                            let mut usb_vid = String::new();
                            let mut usb_pid = String::new();
                            let mut usb_bus = String::new();
                            let mut usb_dev = String::new();
                            let mut f = std::fs::File::open(vidpath).expect("couldn't open USB vendor");
                            f.read_to_string(&mut usb_vid).expect("failed to read USB vendor");
                            let mut f = std::fs::File::open(pidpath).expect("couldn't open USB product");
                            f.read_to_string(&mut usb_pid).expect("failed to read USB vendor");
                            let mut f = std::fs::File::open(buspath).expect("couldn't open USB bus");
                            f.read_to_string(&mut usb_bus).expect("failed to read USB vendor");
                            let mut f = std::fs::File::open(devpath).expect("couldn't open USB device");
                            f.read_to_string(&mut usb_dev).expect("failed to read USB vendor");
                            let usb_vid = usb_vid.trim();
                            let usb_pid = usb_pid.trim();
                            let usb_bus = usb_bus.trim();
                            let usb_dev = usb_dev.trim();
                            let usb_filter = UsbFilter {vid: Some(usb_vid.into()), pid: Some(usb_pid.into())};
                            for (vm, vm_filter) in filters.iter() {
                                if vm_filter.contains(&usb_filter) {
                                    println!("Adding syspath: {} for vm {} [VID:{} PID:{}]", syspath.display(), vm, usb_vid, usb_pid);
                                    sysdevs.insert(syspath.clone(), usb_filter.clone());
                                    if let Ok(domain) = Domain::lookup_by_name(&conn, vm) {
                                        let xml = usb_xml(usb_vid, usb_pid, usb_bus, usb_dev);
                                        domain.attach_device(&xml).expect("failed to attach USB XML!");
                                        if !xmls.contains_key(vm) {
                                            xmls.insert(vm.to_owned(), Vec::new());
                                        }
                                        if let Some(vm_xmls) = xmls.get_mut(vm) {
                                            vm_xmls.push((syspath.clone(), xml));
                                        }
                                    }
                                }
                            }
                        },
                        udev::EventType::Remove => {
                            match sysdevs.contains_key(&syspath) {
                                true => {
                                    println!("Removing syspath: {}", syspath.display());
                                    sysdevs.remove(&syspath);
                                    for (vm, vm_xmls) in xmls.iter_mut() {
                                        for (vm_syspath, xml_str) in vm_xmls.iter() {
                                            if vm_syspath == &syspath {
                                                if let Ok(domain) = Domain::lookup_by_name(&conn, vm) {
                                                    if let Err(e) = domain.detach_device(xml_str) {
                                                        println!("WARNING: failed to hot-unplug from domain {}: {}", vm, e);
                                                    }
                                                }
                                            }
                                        }
                                        vm_xmls.retain(|i| i.0 != syspath);
                                    }
                                },
                                false => {
                                },
                            }
                        },
                        _ => {},
                    }
                });
            }
        }
    }

    println!("Shutting down by request.");
    if let Err(e) = conn.close() {
        panic!("Failed to disconnect from hypervisor: {}", e);
    }
    Ok(())
}

fn main() {
    println!("Starting qemu-stormcrow.");

    println!("Starting dbus monitor...");
    let (sender, receiver) = channel::<(DbusCommand, String, UsbFilter)>();
    thread::spawn(move || {
        dbus_server(sender).expect("failed to launch dbus server");
    });

    println!("Making udev monitor...");
    let socket = MonitorBuilder::new()
        .expect("failed to create new udev monitor")
        .match_subsystem_devtype("usb", "usb_device")
        .expect("failed to create usb matcher")
        .listen()
        .expect("failed to register udev monitor");

    poll(socket, receiver).expect("failed to poll udev monitor");
    println!("Done!");
}
