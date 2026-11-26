//! The Wayland backend reports the `suspended` state of an `xdg_toplevel` as
//! `WindowEvent::Occluded`, driven end to end: an in-process compositor serves the event loop's
//! connection and states each configure, and the test reads the window events winit delivers.

#![cfg(all(
    feature = "wayland",
    unix,
    not(any(
        target_os = "redox",
        target_family = "wasm",
        target_os = "android",
        target_os = "ios",
        target_os = "macos"
    ))
))]

use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use wayland_protocols::xdg::shell::server::{xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use wayland_server::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_server::{Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::pump_events::EventLoopExtPumpEvents;
use winit::platform::wayland::EventLoopBuilderExtWayland;
use winit::window::{Window, WindowAttributes, WindowId};

use xdg_toplevel::State::{Activated, Maximized, Suspended};

/// What the test asks of the compositor.
enum Command {
    /// The states the first configure of each toplevel opened from now on states; the sender
    /// answers once the compositor holds them.
    FirstStates(Vec<xdg_toplevel::State>, Sender<()>),
    /// A configure of the toplevel opened `index`th, stating this size and these states.
    Configure { index: usize, width: i32, height: i32, states: Vec<xdg_toplevel::State> },
}

/// A toplevel the client opened.
struct Toplevel {
    surface: wl_surface::WlSurface,
    xdg_surface: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    first_states: Vec<xdg_toplevel::State>,
    configured: bool,
}

/// The compositor: the toplevels in the order the client opened them.
#[derive(Default)]
struct Compositor {
    toplevels: Vec<Toplevel>,
    first_states: Vec<xdg_toplevel::State>,
    serial: u32,
}

impl Compositor {
    fn configure(&mut self, index: usize, width: i32, height: i32, states: &[xdg_toplevel::State]) {
        self.serial += 1;
        let toplevel = &self.toplevels[index];
        let states = states.iter().flat_map(|state| u32::from(*state).to_ne_bytes()).collect();
        toplevel.toplevel.configure(width, height, states);
        toplevel.xdg_surface.configure(self.serial);
    }

    /// A toplevel's first commit asks for its first configure.
    fn commit(&mut self, surface: &wl_surface::WlSurface) {
        let Some(index) =
            self.toplevels.iter().position(|t| !t.configured && &t.surface == surface)
        else {
            return;
        };
        self.toplevels[index].configured = true;
        let states = self.toplevels[index].first_states.clone();
        self.configure(index, 0, 0, &states);
    }
}

struct TestClient;

impl ClientData for TestClient {
    fn initialized(&self, _: ClientId) {}

    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

/// Serve `stream` until the test drops its command sender.
fn serve(stream: UnixStream, commands: Receiver<Command>) {
    let mut display = Display::<Compositor>::new().expect("the compositor's display");
    let mut handle = display.handle();
    handle.create_global::<Compositor, wl_compositor::WlCompositor, ()>(4, ());
    handle.create_global::<Compositor, wl_shm::WlShm, ()>(1, ());
    handle.create_global::<Compositor, xdg_wm_base::XdgWmBase, ()>(6, ());
    handle.insert_client(stream, Arc::new(TestClient)).expect("the client connects");
    let mut state = Compositor::default();
    loop {
        loop {
            match commands.try_recv() {
                Ok(Command::FirstStates(states, done)) => {
                    state.first_states = states;
                    let _ = done.send(());
                },
                Ok(Command::Configure { index, width, height, states }) => {
                    state.configure(index, width, height, &states);
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        display.dispatch_clients(&mut state).expect("the compositor reads its client");
        display.flush_clients().expect("the compositor writes to its client");
        thread::sleep(Duration::from_millis(2));
    }
}

impl GlobalDispatch<wl_compositor::WlCompositor, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_compositor::WlCompositor>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                data_init.init(id, ());
            },
            wl_compositor::Request::CreateRegion { id } => {
                data_init.init(id, ());
            },
            _ => {},
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        surface: &wl_surface::WlSurface,
        request: wl_surface::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_surface::Request::Frame { callback } => {
                data_init.init(callback, ());
            },
            wl_surface::Request::Commit => state.commit(surface),
            _ => {},
        }
    }
}

impl Dispatch<wl_region::WlRegion, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_region::WlRegion,
        _: wl_region::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_callback::WlCallback,
        _: wl_callback::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<wl_shm::WlShm, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_shm::WlShm>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let shm = data_init.init(resource, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
    }
}

impl Dispatch<wl_shm::WlShm, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm::WlShm,
        request: wl_shm::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, .. } = request {
            data_init.init(id, ());
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm_pool::Request::CreateBuffer { id, .. } = request {
            data_init.init(id, ());
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<xdg_wm_base::XdgWmBase, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<xdg_wm_base::XdgWmBase>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &xdg_wm_base::XdgWmBase,
        request: xdg_wm_base::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_wm_base::Request::CreatePositioner { id } => {
                data_init.init(id, ());
            },
            xdg_wm_base::Request::GetXdgSurface { id, surface } => {
                data_init.init(id, surface);
            },
            _ => {},
        }
    }
}

impl Dispatch<xdg_positioner::XdgPositioner, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &xdg_positioner::XdgPositioner,
        _: xdg_positioner::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<xdg_surface::XdgSurface, wl_surface::WlSurface> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        xdg_surface: &xdg_surface::XdgSurface,
        request: xdg_surface::Request,
        surface: &wl_surface::WlSurface,
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let xdg_surface::Request::GetToplevel { id } = request {
            let toplevel = data_init.init(id, ());
            state.toplevels.push(Toplevel {
                surface: surface.clone(),
                xdg_surface: xdg_surface.clone(),
                toplevel,
                first_states: state.first_states.clone(),
                configured: false,
            });
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &xdg_toplevel::XdgToplevel,
        _: xdg_toplevel::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

/// What the test reads of a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Resized(u32, u32),
    Occluded(bool),
}

/// The application: it opens the windows asked for and records the events of each.
#[derive(Default)]
struct App {
    to_open: Vec<WindowAttributes>,
    windows: Vec<Window>,
    seen: Vec<(WindowId, Seen)>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, _: &ActiveEventLoop) {}

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        for attributes in std::mem::take(&mut self.to_open) {
            self.windows.push(event_loop.create_window(attributes).expect("the window opens"));
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::Resized(size) => self.seen.push((id, Seen::Resized(size.width, size.height))),
            WindowEvent::Occluded(occluded) => self.seen.push((id, Seen::Occluded(occluded))),
            _ => {},
        }
    }
}

impl App {
    /// The occlusion reports of the `index`th window, from the `from`th event recorded.
    fn occluded_since(&self, index: usize, from: usize) -> Vec<bool> {
        let id = self.windows[index].id();
        self.seen[from..]
            .iter()
            .filter_map(|(window, seen)| match seen {
                Seen::Occluded(occluded) if *window == id => Some(*occluded),
                _ => None,
            })
            .collect()
    }
}

struct Harness {
    event_loop: EventLoop<()>,
    app: App,
    commands: Sender<Command>,
}

impl Harness {
    /// Run the event loop until `done` holds, for at most ten seconds.
    fn pump_until(&mut self, what: &str, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(&self.app) {
            assert!(Instant::now() < deadline, "{what} within 10 s; the window saw {:?}", self.app.seen);
            let _ = self.event_loop.pump_app_events(Some(Duration::from_millis(20)), &mut self.app);
        }
    }

    /// Open a window whose first configure states `first_states`, and answer its index and the
    /// occlusion reports its first configure brought.
    fn open(&mut self, first_states: Vec<xdg_toplevel::State>) -> (usize, Vec<bool>) {
        let (done, held) = mpsc::channel();
        self.commands.send(Command::FirstStates(first_states, done)).unwrap();
        held.recv().expect("the compositor holds the first states");
        let index = self.app.windows.len();
        let from = self.app.seen.len();
        self.app.to_open.push(
            Window::default_attributes()
                .with_inner_size(LogicalSize::new(200.0, 100.0))
                .with_decorations(false),
        );
        self.pump_until("the window opens", |app| app.windows.len() > index);
        let id = self.app.windows[index].id();
        self.pump_until("the window's first configure reaches it", |app| {
            app.seen[from..].iter().any(|(window, seen)| *window == id && matches!(seen, Seen::Resized(..)))
        });
        (index, self.app.occluded_since(index, from))
    }

    /// Configure the `index`th window to a size it does not have yet and `states`, and answer
    /// the occlusion reports that configure brought. The window's resize to that size marks the
    /// configure as handled: winit delivers both from the same configure.
    fn configure(&mut self, index: usize, width: u32, states: &[xdg_toplevel::State]) -> Vec<bool> {
        let from = self.app.seen.len();
        self.commands
            .send(Command::Configure { index, width: width as i32, height: 100, states: states.to_vec() })
            .unwrap();
        let id = self.app.windows[index].id();
        self.pump_until(&format!("the configure to {width}x100 reaches the window"), |app| {
            app.seen[from..].contains(&(id, Seen::Resized(width, 100)))
        });
        self.app.occluded_since(index, from)
    }
}

#[test]
fn a_suspended_toplevel_is_reported_occluded_and_a_resumed_one_shown() {
    let (server, client) = UnixStream::pair().expect("a socket pair");
    let (commands, received) = mpsc::channel();
    thread::spawn(move || serve(server, received));
    std::env::remove_var("WAYLAND_DISPLAY");
    std::env::set_var("WAYLAND_SOCKET", client.into_raw_fd().to_string());
    let mut builder = EventLoop::builder();
    builder.with_wayland();
    builder.with_any_thread(true);
    let event_loop = builder.build().expect("the event loop connects to the compositor");
    let mut harness = Harness { event_loop, app: App::default(), commands };

    let (shown, first) = harness.open(vec![]);
    assert_eq!(first, Vec::<bool>::new(), "a window shown from its first configure is reported nothing");

    assert_eq!(harness.configure(shown, 300, &[Suspended]), [true], "suspending it reports it occluded");
    assert_eq!(
        harness.configure(shown, 301, &[Suspended, Activated]),
        Vec::<bool>::new(),
        "a configure that keeps it suspended reports nothing"
    );
    assert_eq!(harness.configure(shown, 302, &[]), [false], "resuming it reports it shown");
    assert_eq!(
        harness.configure(shown, 303, &[Activated]),
        Vec::<bool>::new(),
        "a configure that changes other states reports nothing"
    );
    assert_eq!(
        harness.configure(shown, 304, &[Activated, Maximized, Suspended]),
        [true],
        "suspending it among other states reports it occluded"
    );
    assert_eq!(harness.configure(shown, 305, &[Suspended]), Vec::<bool>::new());
    assert_eq!(harness.configure(shown, 306, &[Activated]), [false]);

    let (hidden, first) = harness.open(vec![Suspended]);
    assert_eq!(first, [true], "a window suspended from its first configure is reported occluded");
    assert_eq!(harness.configure(hidden, 400, &[]), [false], "resuming it reports it shown");
    assert_eq!(
        harness.app.occluded_since(shown, 0),
        [true, false, true, false],
        "the other window's configures report nothing for this one"
    );
}
