//! Wayland backend for wlroots-based compositors (Hyprland, Sway, ...) via
//! the `wlr-data-control` protocol, which lets a background client see
//! selection changes without having keyboard focus.

use crate::{ClipboardChanged, Error, Result};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::{
    self, ZwlrDataControlDeviceV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::{
    self, ZwlrDataControlOfferV1,
};

/// MIME types that mark content as sensitive. KeePassXC and other password
/// managers set KDE's hint on Linux.
const EXCLUDED_MIME_TYPES: &[&str] = &["x-kde-passwordManagerHint"];

/// Whether the current selection is marked sensitive; set by the watcher.
static EXCLUDED: AtomicBool = AtomicBool::new(false);

pub fn watch() -> Result<Receiver<ClipboardChanged>> {
    let conn = Connection::connect_to_env().map_err(other)?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(other)?;
    let qh = queue.handle();
    let manager: ZwlrDataControlManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .map_err(|e| other(format!("compositor lacks wlr-data-control: {e}")))?;
    let seat: WlSeat = globals.bind(&qh, 1..=8, ()).map_err(other)?;
    let device = manager.get_data_device(&seat, &qh, ());

    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("clipboard-watcher".into())
        .spawn(move || {
            // Keep the protocol objects alive for the life of the thread.
            let _keep = (conn, manager, seat, device);
            let mut state = State { tx, offers: HashMap::new(), alive: true };
            while state.alive {
                if let Err(e) = queue.blocking_dispatch(&mut state) {
                    tracing::warn!("Wayland clipboard watcher stopped: {e}");
                    break;
                }
            }
            tracing::debug!("clipboard watcher stopped");
        })?;
    Ok(rx)
}

pub fn is_excluded() -> bool {
    EXCLUDED.load(Ordering::Relaxed)
}

fn other(e: impl ToString) -> Error {
    Error::Os(io::Error::other(e.to_string()))
}

struct State {
    tx: Sender<ClipboardChanged>,
    /// Offers announced by the compositor, with the MIME types they carry.
    offers: HashMap<ObjectId, (ZwlrDataControlOfferV1, Vec<String>)>,
    alive: bool,
}

impl State {
    /// Destroys every offer except `keep`; old offers are never used again.
    fn drop_offers_except(&mut self, keep: Option<&ObjectId>) {
        self.offers.retain(|id, (offer, _)| {
            let retain = Some(id) == keep;
            if !retain {
                offer.destroy();
            }
            retain
        });
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _device: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event;
        match event {
            Event::DataOffer { id } => {
                state.offers.insert(id.id(), (id, Vec::new()));
            }
            Event::Selection { id } => {
                let id = id.map(|offer| offer.id());
                let excluded = id
                    .as_ref()
                    .and_then(|id| state.offers.get(id))
                    .is_some_and(|(_, mimes)| mimes.iter().any(|m| EXCLUDED_MIME_TYPES.contains(&m.as_str())));
                EXCLUDED.store(excluded, Ordering::Relaxed);
                state.drop_offers_except(id.as_ref());
                if state.tx.send(ClipboardChanged).is_err() {
                    state.alive = false;
                }
            }
            Event::PrimarySelection { id: Some(offer) } => {
                // Middle-click selection; not synced.
                if let Some((offer, _)) = state.offers.remove(&offer.id()) {
                    offer.destroy();
                }
            }
            Event::Finished => state.alive = false,
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event
            && let Some((_, mimes)) = state.offers.get_mut(&offer.id())
        {
            mimes.push(mime_type);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(_: &mut Self, _: &WlSeat, _: <WlSeat as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
