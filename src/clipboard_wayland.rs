//! Two-way clipboard sync on Wayland through ext-data-control-v1, on its own connection.

use crate::desktop::{ClipContent, ClipRequest, ClipboardHandle, ClipboardMode, MAX_IMAGE};
use crate::hub::Hub;
use nix::fcntl::OFlag;
use nix::poll::{poll, PollFd, PollFlags};
use nix::unistd::{pipe2, read};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

/// Offered with everything voxl puts on the clipboard, to tell its own selection apart.
const OWN: &str = "application/x-voxl-clipboard";
const PNG: &str = "image/png";
/// Text types in order of preference when reading, all offered when writing.
const TEXT: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];
/// How long a transfer may sit without progress before it is given up.
const STALL: Duration = Duration::from_secs(3);

struct State {
    hub: Arc<Hub>,
    mode: ClipboardMode,
    seat: Option<wl_seat::WlSeat>,
    manager: Option<ExtDataControlManagerV1>,
    /// MIME types of the offers announced and not yet used.
    offers: HashMap<ObjectId, Vec<String>>,
    /// The selection voxl owns, and its bytes.
    owned: Option<(ExtDataControlSourceV1, Arc<Vec<u8>>)>,
    last_image: Arc<Mutex<Option<u64>>>,
    finished: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        s: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "wl_seat" if s.seat.is_none() => s.seat = Some(registry.bind(name, 1, q, ())),
                "ext_data_control_manager_v1" => s.manager = Some(registry.bind(name, 1, q, ())),
                _ => (),
            }
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_device_v1::Event;
        match event {
            Event::DataOffer { id } => {
                s.offers.insert(id.id(), Vec::new());
            }
            Event::Selection { id: Some(offer) } => {
                let types = s.offers.remove(&offer.id()).unwrap_or_default();
                if s.mode.to_client && !types.iter().any(|t| t == OWN) {
                    // Text wins when both are offered (rich text copies carry both).
                    let want = TEXT
                        .iter()
                        .chain([&PNG])
                        .find(|want| types.iter().any(|t| t == *want));
                    if let Some(&mime) = want {
                        s.fetch(&offer, mime);
                    }
                }
                offer.destroy();
            }
            Event::PrimarySelection { id: Some(offer) } => {
                s.offers.remove(&offer.id());
                offer.destroy();
            }
            Event::Finished => s.finished = true,
            _ => (),
        }
    }

    wayland_client::event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        s: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            if let Some(types) = s.offers.get_mut(&offer.id()) {
                types.push(mime_type);
            }
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(
        s: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { fd, .. } => {
                let Some((_, data)) = s.owned.as_ref().filter(|(owned, _)| owned == source) else {
                    return;
                };
                // The reader sets the pace, so write off the event loop.
                let data = data.clone();
                std::thread::spawn(move || {
                    let _ = File::from(fd).write_all(&data);
                });
            }
            ext_data_control_source_v1::Event::Cancelled => {
                source.destroy();
                if s.owned.as_ref().is_some_and(|(owned, _)| owned == source) {
                    s.owned = None;
                }
            }
            _ => (),
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ExtDataControlManagerV1);

impl State {
    /// Asks the selection's owner for `mime` and sends what arrives to the clients.
    fn fetch(&self, offer: &ExtDataControlOfferV1, mime: &'static str) {
        let Ok((r, w)) = pipe2(OFlag::O_CLOEXEC) else {
            return;
        };
        offer.receive(mime.into(), w.as_fd());
        drop(w);
        let hub = self.hub.clone();
        let last_image = self.last_image.clone();
        std::thread::spawn(move || {
            let Some(data) = read_all(r) else {
                return;
            };
            if mime == PNG {
                crate::desktop::publish_image(&hub, data, &mut last_image.lock());
            } else if let Ok(text) = String::from_utf8(data) {
                crate::desktop::publish_text(&hub, text);
            }
        });
    }
}

/// Reads a transfer to its end; `None` if it stalls or outgrows the largest image.
fn read_all(fd: OwnedFd) -> Option<Vec<u8>> {
    let mut file = File::from(fd);
    let mut data = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        let mut p = [PollFd::new(file.as_fd(), PollFlags::POLLIN)];
        if poll(&mut p, STALL.as_millis() as u16).ok()? == 0 {
            return None;
        }
        match file.read(&mut chunk).ok()? {
            0 => return Some(data),
            n => data.extend_from_slice(&chunk[..n]),
        }
        if data.len() > MAX_IMAGE {
            return None;
        }
    }
}

pub fn spawn(
    cfg: &crate::wayland::Config,
    hub: Arc<Hub>,
    mode: ClipboardMode,
) -> Result<ClipboardHandle, String> {
    let conn = crate::wayland::connect(cfg.display.as_deref())?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State {
        hub,
        mode,
        seat: None,
        manager: None,
        offers: HashMap::new(),
        owned: None,
        last_image: Arc::new(Mutex::new(None)),
        finished: false,
    };
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    let seat = state.seat.clone().ok_or("compositor has no wl_seat")?;
    let manager = state
        .manager
        .clone()
        .ok_or("compositor lacks ext-data-control-v1")?;
    let device = manager.get_data_device(&seat, &qh, ());
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel::<ClipRequest>();
    let (wake_r, wake_w) =
        pipe2(OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).map_err(|e| e.to_string())?;
    std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || loop {
            let mut drain = [0u8; 64];
            while matches!(read(&wake_r, &mut drain), Ok(n) if n > 0) {}
            while let Ok((content, done)) = rx.try_recv() {
                if state.mode.to_server && !state.finished {
                    let source = manager.create_data_source(&qh, ());
                    source.offer(OWN.into());
                    let data = match content {
                        ClipContent::Text(text) => {
                            *state.hub.last_clipboard.lock() = Some(text.clone());
                            for mime in TEXT {
                                source.offer((*mime).into());
                            }
                            text.into_bytes()
                        }
                        ClipContent::Image(png) => {
                            *state.last_image.lock() = Some(crate::desktop::hash(&png));
                            source.offer(PNG.into());
                            png
                        }
                    };
                    device.set_selection(Some(&source));
                    if let Some((old, _)) = state.owned.replace((source, Arc::new(data))) {
                        old.destroy();
                    }
                    // Round trip: the selection is in effect once this returns, so a
                    // paste shortcut injected right afterwards sees the new contents.
                    let _ = queue.roundtrip(&mut state);
                }
                let _ = done.send(());
            }
            let _ = queue.dispatch_pending(&mut state);
            if conn.flush().is_err() {
                log::error!("Wayland clipboard connection lost");
                return;
            }
            let Some(guard) = queue.prepare_read() else {
                continue;
            };
            let mut p = [
                PollFd::new(conn.as_fd(), PollFlags::POLLIN),
                PollFd::new(wake_r.as_fd(), PollFlags::POLLIN),
            ];
            let _ = poll(&mut p, 1000u16);
            if p[0].revents().is_some_and(|r| !r.is_empty()) {
                match guard.read() {
                    Ok(_) => (),
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        log::error!("Wayland clipboard connection: {e}");
                        return;
                    }
                }
            }
        })
        .unwrap();
    Ok(ClipboardHandle::new(tx, Arc::new(wake_w)))
}
