//! File drags arriving through a seat's `wl_data_device`.

use std::ffi::OsStr;
use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use percent_encoding::percent_decode;
use tracing::warn;

use sctk::data_device_manager::data_device::{DataDeviceData, DataDeviceHandler};
use sctk::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use sctk::data_device_manager::data_source::DataSourceHandler;
use sctk::data_device_manager::WritePipe;
use sctk::reexports::calloop::{PostAction, RegistrationToken};
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_data_source::WlDataSource;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy, QueueHandle};

use crate::dpi::{LogicalPosition, PhysicalPosition};
use crate::event::WindowEvent;
use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::{make_wid, WindowId};

/// The one offer a file drag is read through.
const URI_LIST: &str = "text/uri-list";

/// The most bytes a drag's list of files may hold.
const URI_LIST_LIMIT: usize = 16 * 1024 * 1024;

/// The drag standing over one of this client's windows.
#[derive(Debug)]
pub struct DragState {
    window_id: WindowId,
    offer: DragOffer,
    position: LogicalPosition<f64>,
    /// Its files, once the source has written their list.
    paths: Option<Vec<PathBuf>>,
    /// The source of the list while the list is being written.
    reader: Option<RegistrationToken>,
    dropped: bool,
}

impl WinitState {
    fn drag_position(&self, drag: &DragState) -> PhysicalPosition<f64> {
        let scale_factor = self
            .windows
            .borrow()
            .get(&drag.window_id)
            .map_or(1.0, |window| window.lock().unwrap().scale_factor());
        drag.position.to_physical(scale_factor)
    }

    /// The list of files has arrived: the drag is entered, and a drop that landed while the list
    /// was on its way completes.
    fn drag_paths_arrived(&mut self, paths: Vec<PathBuf>) {
        let Some(drag) = self.drag.as_ref() else {
            return;
        };
        let position = self.drag_position(drag);
        let (window_id, dropped) = (drag.window_id, drag.dropped);
        for path in &paths {
            self.events_sink.push_window_event(WindowEvent::HoveredFile(path.clone()), window_id);
        }
        self.events_sink.push_window_event(
            WindowEvent::DragEntered { paths: paths.clone(), position },
            window_id,
        );
        let drag = self.drag.as_mut().unwrap();
        drag.paths = Some(paths);
        drag.reader = None;
        if dropped {
            self.finish_drop();
        }
    }

    /// Report the drop and tell the source it is over.
    fn finish_drop(&mut self) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        let position = self.drag_position(&drag);
        let paths = drag.paths.unwrap_or_default();
        for path in &paths {
            self.events_sink
                .push_window_event(WindowEvent::DroppedFile(path.clone()), drag.window_id);
        }
        self.events_sink
            .push_window_event(WindowEvent::DragDropped { paths, position }, drag.window_id);
        if drag.offer.selected_action == DndAction::Copy {
            drag.offer.finish();
        }
        drag.offer.destroy();
    }
}

impl DataDeviceHandler for WinitState {
    fn enter(
        &mut self,
        conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
        device: &WlDataDevice,
        x: f64,
        y: f64,
        surface: &WlSurface,
    ) {
        let Some(offer) = device.data::<DataDeviceData>().and_then(DataDeviceData::drag_offer)
        else {
            return;
        };
        let window_id = make_wid(surface);
        let carries_files = offer.with_mime_types(|mimes| mimes.iter().any(|m| m == URI_LIST));
        if !carries_files || !self.windows.get_mut().contains_key(&window_id) {
            offer.accept_mime_type(offer.serial, None);
            offer.set_actions(DndAction::empty(), DndAction::empty());
            return;
        }
        offer.accept_mime_type(offer.serial, Some(URI_LIST.to_owned()));
        offer.set_actions(DndAction::Copy, DndAction::Copy);

        let pipe = match offer.receive(URI_LIST.to_owned()) {
            Ok(pipe) => pipe,
            Err(err) => {
                warn!("Failed to read the files of a drag: {err}");
                return;
            },
        };
        // The source writes the list only once the compositor has passed the request on.
        if let Err(err) = conn.flush() {
            warn!("Failed to ask a drag's source for its files: {err}");
            return;
        }
        // SAFETY: `pipe` owns the descriptor, and `fcntl` with these commands touches its flags.
        unsafe {
            let fd = pipe.as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        let mut list = Vec::new();
        let reader = self.loop_handle.insert_source(pipe, move |_, file, state| {
            // Required to handle the wakeups from this source.
            state.dispatched_events = true;
            // SAFETY: the file is read, and neither replaced nor closed.
            let file = unsafe { file.get_mut() };
            let mut chunk = [0u8; 4096];
            loop {
                match file.read(&mut chunk) {
                    Ok(0) => {
                        state.drag_paths_arrived(parse_uri_list(&list));
                        return PostAction::Remove;
                    },
                    Ok(read) if list.len() + read > URI_LIST_LIMIT => {
                        warn!("A drag's list of files is past {URI_LIST_LIMIT} bytes");
                        state.drag = None;
                        return PostAction::Remove;
                    },
                    Ok(read) => list.extend_from_slice(&chunk[..read]),
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        return PostAction::Continue
                    },
                    Err(err) if err.kind() == ErrorKind::Interrupted => {},
                    Err(err) => {
                        warn!("Failed to read the files of a drag: {err}");
                        state.drag = None;
                        return PostAction::Remove;
                    },
                }
            }
        });
        let reader = match reader {
            Ok(reader) => reader,
            Err(err) => {
                warn!("Failed to read the files of a drag: {err}");
                return;
            },
        };
        self.drag = Some(DragState {
            window_id,
            offer,
            position: LogicalPosition::new(x, y),
            paths: None,
            reader: Some(reader),
            dropped: false,
        });
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _device: &WlDataDevice,
    ) {
        // A drop is followed by a leave too, with the offer still to be read.
        if self.drag.as_ref().map_or(true, |drag| drag.dropped) {
            return;
        }
        let drag = self.drag.take().unwrap();
        if let Some(reader) = drag.reader {
            self.loop_handle.remove(reader);
        }
        if drag.paths.is_some() {
            self.events_sink.push_window_event(WindowEvent::HoveredFileCancelled, drag.window_id);
            self.events_sink
                .push_window_event(WindowEvent::DragLeft { position: None }, drag.window_id);
        }
    }

    fn motion(
        &mut self,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _device: &WlDataDevice,
        x: f64,
        y: f64,
    ) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        drag.position = LogicalPosition::new(x, y);
        if drag.paths.is_none() {
            return;
        }
        let drag = self.drag.as_ref().unwrap();
        let position = self.drag_position(drag);
        let window_id = drag.window_id;
        self.events_sink.push_window_event(WindowEvent::DragMoved { position }, window_id);
    }

    fn selection(
        &mut self,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _device: &WlDataDevice,
    ) {
    }

    fn drop_performed(
        &mut self,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
        device: &WlDataDevice,
    ) {
        let offer = device.data::<DataDeviceData>().and_then(DataDeviceData::drag_offer);
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        drag.dropped = true;
        // The copy of the offer the drag entered with predates the action the compositor chose.
        if let Some(offer) = offer {
            drag.offer = offer;
        }
        if drag.paths.is_some() {
            self.finish_drop();
        }
    }
}

impl DataOfferHandler for WinitState {
    fn source_actions(
        &mut self,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _offer: &mut DragOffer,
        _actions: DndAction,
    ) {
    }

    fn selected_action(
        &mut self,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _offer: &mut DragOffer,
        _actions: DndAction,
    ) {
    }
}

/// Winit offers no data of its own, so it holds no source for these to reach.
impl DataSourceHandler for WinitState {
    fn accept_mime(
        &mut self,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _source: &WlDataSource,
        _mime: Option<String>,
    ) {
    }

    fn send_request(
        &mut self,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _source: &WlDataSource,
        _mime: String,
        _fd: WritePipe,
    ) {
    }

    fn cancelled(
        &mut self,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _source: &WlDataSource,
    ) {
    }

    fn dnd_dropped(
        &mut self,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _source: &WlDataSource,
    ) {
    }

    fn dnd_finished(
        &mut self,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _source: &WlDataSource,
    ) {
    }

    fn action(
        &mut self,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _source: &WlDataSource,
        _action: DndAction,
    ) {
    }
}

sctk::delegate_data_device!(WinitState);

/// The local files a `text/uri-list` names.
fn parse_uri_list(list: &[u8]) -> Vec<PathBuf> {
    list.split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty() && !line.starts_with(b"#"))
        .filter_map(|uri| {
            let rest = uri.strip_prefix(b"file://")?;
            let slash = rest.iter().position(|byte| *byte == b'/')?;
            let (host, path) = rest.split_at(slash);
            if !host.is_empty() && host != b"localhost" {
                return None;
            }
            let path: Vec<u8> = percent_decode(path).collect();
            Some(PathBuf::from(OsStr::from_bytes(&path)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse_uri_list;
    use std::path::PathBuf;

    #[test]
    fn a_uri_list_names_its_local_files_decoded() {
        let list = b"# a comment\r\nfile:///home/a/two%20words.png\r\nfile://localhost/tmp/b.txt\r\n";
        assert_eq!(
            parse_uri_list(list),
            vec![PathBuf::from("/home/a/two words.png"), PathBuf::from("/tmp/b.txt")]
        );
    }

    #[test]
    fn a_uri_list_leaves_out_what_is_not_a_local_file() {
        let list = b"https://example.com/a.png\nfile://other-host/c.txt\nfile:///d.txt";
        assert_eq!(parse_uri_list(list), vec![PathBuf::from("/d.txt")]);
    }
}
