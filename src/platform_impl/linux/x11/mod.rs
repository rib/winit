#![cfg(x11_platform)]

mod dnd;
mod event_processor;
mod events;
pub mod ffi;
mod ime;
mod monitor;
pub mod util;
mod window;
mod xdisplay;

pub(crate) use self::{
    monitor::{MonitorHandle, VideoMode},
    window::UnownedWindow,
    xdisplay::XConnection,
};

pub use self::xdisplay::{XError, XNotSupported};

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    ffi::CStr,
    mem::{self, MaybeUninit},
    ops::Deref,
    os::raw::*,
    ptr,
    rc::Rc,
    slice,
    sync::mpsc::{Receiver, Sender, TryRecvError},
    sync::{mpsc, Arc, Weak},
    time::{Duration, Instant},
};

use libc::{self, setlocale, LC_CTYPE};

use mio::{unix::SourceFd, Events, Interest, Poll, Token, Waker};
use raw_window_handle::{RawDisplayHandle, XlibDisplayHandle};

use self::{
    dnd::{Dnd, DndState},
    event_processor::EventProcessor,
    ime::{Ime, ImeCreationError, ImeReceiver, ImeRequest, ImeSender},
    util::modifiers::ModifierKeymap,
};
use crate::{
    error::{ExternalError, OsError as RootOsError},
    event::{self, Event, PumpStatus, StartCause},
    event_loop::{
        self, ControlFlow, DeviceEventFilter, EventLoopClosed, EventLoopWindowTarget as RootELW,
    },
    platform_impl::{
        platform::{sticky_exit_callback, WindowId},
        PlatformSpecificWindowBuilderAttributes,
    },
    window::WindowAttributes,
};

use super::min_timeout;

const X_TOKEN: Token = Token(0);
const USER_REDRAW_TOKEN: Token = Token(1);

struct WakeSender<T> {
    sender: Sender<T>,
    waker: Arc<Waker>,
}

struct PeekableReceiver<T> {
    recv: Receiver<T>,
    first: Option<T>,
}

impl<T> PeekableReceiver<T> {
    pub fn from_recv(recv: Receiver<T>) -> Self {
        Self { recv, first: None }
    }
    pub fn has_incoming(&mut self) -> bool {
        if self.first.is_some() {
            return true;
        }

        match self.recv.try_recv() {
            Ok(v) => {
                self.first = Some(v);
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                warn!("Channel was disconnected when checking incoming");
                false
            }
        }
    }
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        if let Some(first) = self.first.take() {
            return Ok(first);
        }
        self.recv.try_recv()
    }
}

pub struct EventLoopWindowTarget<T> {
    xconn: Arc<XConnection>,
    wm_delete_window: ffi::Atom,
    net_wm_ping: ffi::Atom,
    ime_sender: ImeSender,
    root: ffi::Window,
    ime: RefCell<Ime>,
    windows: RefCell<HashMap<WindowId, Weak<UnownedWindow>>>,
    redraw_sender: WakeSender<WindowId>,
    device_event_filter: Cell<DeviceEventFilter>,
    _marker: ::std::marker::PhantomData<T>,
}

pub struct EventLoop<T: 'static> {
    loop_running: bool,
    control_flow: ControlFlow,
    poll: Poll,
    events: Events,
    waker: Arc<Waker>,
    event_processor: EventProcessor<T>,
    redraw_receiver: PeekableReceiver<WindowId>,
    user_receiver: PeekableReceiver<T>, //waker.wake needs to be called whenever something gets sent
    user_sender: Sender<T>,
    target: Rc<RootELW<T>>,
}

pub struct EventLoopProxy<T: 'static> {
    user_sender: Sender<T>,
    waker: Arc<Waker>,
}

impl<T: 'static> Clone for EventLoopProxy<T> {
    fn clone(&self) -> Self {
        EventLoopProxy {
            user_sender: self.user_sender.clone(),
            waker: self.waker.clone(),
        }
    }
}

impl<T: 'static> EventLoop<T> {
    pub(crate) fn new(xconn: Arc<XConnection>) -> EventLoop<T> {
        let root = unsafe { (xconn.xlib.XDefaultRootWindow)(xconn.display) };

        let wm_delete_window = unsafe { xconn.get_atom_unchecked(b"WM_DELETE_WINDOW\0") };

        let net_wm_ping = unsafe { xconn.get_atom_unchecked(b"_NET_WM_PING\0") };

        let dnd = Dnd::new(Arc::clone(&xconn))
            .expect("Failed to call XInternAtoms when initializing drag and drop");

        let (ime_sender, ime_receiver) = mpsc::channel();
        let (ime_event_sender, ime_event_receiver) = mpsc::channel();
        // Input methods will open successfully without setting the locale, but it won't be
        // possible to actually commit pre-edit sequences.
        unsafe {
            // Remember default locale to restore it if target locale is unsupported
            // by Xlib
            let default_locale = setlocale(LC_CTYPE, ptr::null());
            setlocale(LC_CTYPE, b"\0".as_ptr() as *const _);

            // Check if set locale is supported by Xlib.
            // If not, calls to some Xlib functions like `XSetLocaleModifiers`
            // will fail.
            let locale_supported = (xconn.xlib.XSupportsLocale)() == 1;
            if !locale_supported {
                let unsupported_locale = setlocale(LC_CTYPE, ptr::null());
                warn!(
                    "Unsupported locale \"{}\". Restoring default locale \"{}\".",
                    CStr::from_ptr(unsupported_locale).to_string_lossy(),
                    CStr::from_ptr(default_locale).to_string_lossy()
                );
                // Restore default locale
                setlocale(LC_CTYPE, default_locale);
            }
        }
        let ime = RefCell::new({
            let result = Ime::new(Arc::clone(&xconn), ime_event_sender);
            if let Err(ImeCreationError::OpenFailure(ref state)) = result {
                panic!("Failed to open input method: {state:#?}");
            }
            result.expect("Failed to set input method destruction callback")
        });

        let randr_event_offset = xconn
            .select_xrandr_input(root)
            .expect("Failed to query XRandR extension");

        let xi2ext = unsafe {
            let mut ext = XExtension::default();

            let res = (xconn.xlib.XQueryExtension)(
                xconn.display,
                b"XInputExtension\0".as_ptr() as *const c_char,
                &mut ext.opcode,
                &mut ext.first_event_id,
                &mut ext.first_error_id,
            );

            if res == ffi::False {
                panic!("X server missing XInput extension");
            }

            ext
        };

        unsafe {
            let mut xinput_major_ver = ffi::XI_2_Major;
            let mut xinput_minor_ver = ffi::XI_2_Minor;
            if (xconn.xinput2.XIQueryVersion)(
                xconn.display,
                &mut xinput_major_ver,
                &mut xinput_minor_ver,
            ) != ffi::Success as libc::c_int
            {
                panic!(
                    "X server has XInput extension {xinput_major_ver}.{xinput_minor_ver} but does not support XInput2",
                );
            }
        }

        xconn.update_cached_wm_info(root);

        let mut mod_keymap = ModifierKeymap::new();
        mod_keymap.reset_from_x_connection(&xconn);

        let poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), USER_REDRAW_TOKEN).unwrap());

        poll.registry()
            .register(&mut SourceFd(&xconn.x11_fd), X_TOKEN, Interest::READABLE)
            .unwrap();

        let (user_sender, user_channel) = std::sync::mpsc::channel();
        let (redraw_sender, redraw_channel) = std::sync::mpsc::channel();

        let window_target = EventLoopWindowTarget {
            ime,
            root,
            windows: Default::default(),
            _marker: ::std::marker::PhantomData,
            ime_sender,
            xconn,
            wm_delete_window,
            net_wm_ping,
            redraw_sender: WakeSender {
                sender: redraw_sender, // not used again so no clone
                waker: waker.clone(),
            },
            device_event_filter: Default::default(),
        };

        // Set initial device event filter.
        window_target.update_device_event_filter(true);

        let target = Rc::new(RootELW {
            p: super::EventLoopWindowTarget::X(window_target),
            _marker: ::std::marker::PhantomData,
        });

        let event_processor = EventProcessor {
            target: target.clone(),
            dnd,
            devices: Default::default(),
            randr_event_offset,
            ime_receiver,
            ime_event_receiver,
            xi2ext,
            mod_keymap,
            device_mod_state: Default::default(),
            num_touch: 0,
            first_touch: None,
            active_window: None,
            is_composing: false,
        };

        // Register for device hotplug events
        // (The request buffer is flushed during `init_device`)
        get_xtarget(&target)
            .xconn
            .select_xinput_events(root, ffi::XIAllDevices, ffi::XI_HierarchyChangedMask)
            .queue();

        event_processor.init_device(ffi::XIAllDevices);

        EventLoop {
            loop_running: false,
            control_flow: ControlFlow::default(),
            poll,
            events: Events::with_capacity(8),
            waker,
            event_processor,
            redraw_receiver: PeekableReceiver::from_recv(redraw_channel),
            user_receiver: PeekableReceiver::from_recv(user_channel),
            user_sender,
            target,
        }
    }

    pub fn create_proxy(&self) -> EventLoopProxy<T> {
        EventLoopProxy {
            user_sender: self.user_sender.clone(),
            waker: self.waker.clone(),
        }
    }

    pub(crate) fn window_target(&self) -> &RootELW<T> {
        &self.target
    }

    pub fn run_ondemand<F>(&mut self, mut event_handler: F) -> Result<(), ExternalError>
    where
        F: FnMut(event::Event<'_, T>, &event_loop::EventLoopWindowTarget<T>, &mut ControlFlow),
    {
        if self.loop_running {
            return Err(ExternalError::AlreadyRunning);
        }

        loop {
            if let PumpStatus::Exit(code) = self.pump_events_with_timeout(None, &mut event_handler)
            {
                if code == 0 {
                    break Ok(());
                } else {
                    break Err(ExternalError::ExitFailure(code));
                }
            }
        }
    }

    pub fn pump_events<F>(&mut self, event_handler: F) -> PumpStatus
    where
        F: FnMut(event::Event<'_, T>, &RootELW<T>, &mut ControlFlow),
    {
        self.pump_events_with_timeout(Some(Duration::ZERO), event_handler)
    }

    fn pump_events_with_timeout<F>(
        &mut self,
        timeout: Option<Duration>,
        mut callback: F,
    ) -> PumpStatus
    where
        F: FnMut(event::Event<'_, T>, &RootELW<T>, &mut ControlFlow),
    {
        if !self.loop_running {
            self.loop_running = true;

            // Reset the internal state for the loop as we start running to
            // ensure consistent behaviour in case the loop runs and exits more
            // than once
            self.control_flow = ControlFlow::Poll;

            // run the initial loop iteration
            self.single_iteration(&mut callback, StartCause::Init);
        }

        self.poll_events_with_timeout(timeout, &mut callback);
        if let ControlFlow::ExitWithCode(code) = self.control_flow {
            self.loop_running = false;

            let mut dummy = self.control_flow;
            sticky_exit_callback(
                event::Event::LoopDestroyed,
                self.window_target(),
                &mut dummy,
                &mut callback,
            );

            PumpStatus::Exit(code)
        } else {
            PumpStatus::Continue
        }
    }

    fn has_pending(&mut self) -> bool {
        self.event_processor.poll()
            || self.user_receiver.has_incoming()
            || self.redraw_receiver.has_incoming()
    }

    fn poll_events_with_timeout<F>(&mut self, mut timeout: Option<Duration>, mut callback: F)
    where
        F: FnMut(Event<'_, T>, &RootELW<T>, &mut ControlFlow),
    {
        let start = Instant::now();

        let has_pending = self.has_pending();

        timeout = if has_pending {
            // If we already have work to do then we don't want to block on the next poll
            Some(Duration::from_millis(0))
        } else {
            let control_flow_timeout = match self.control_flow {
                ControlFlow::Wait => None,
                ControlFlow::Poll => Some(Duration::from_millis(0)),
                ControlFlow::WaitUntil(wait_deadline) => {
                    if wait_deadline > start {
                        Some(wait_deadline - start)
                    } else {
                        Some(Duration::from_millis(0))
                    }
                }
                // `ExitWithCode()` will be reset to `Poll` before polling
                ControlFlow::ExitWithCode(_code) => unreachable!(),
            };

            min_timeout(control_flow_timeout, timeout)
        };

        self.events.clear();
        if let Err(e) = self.poll.poll(&mut self.events, timeout) {
            if e.raw_os_error() != Some(libc::EINTR) {
                panic!("epoll returned an error: {e:?}");
            }
        }
        let readable = self.events.iter().any(|e| e.is_readable());

        // False positive / spurious wake ups could lead to us spamming
        // redundant iterations of the event loop with no new events to
        // dispatch.
        //
        // If there's no readable event source then we just double check if we
        // have any pending `_receiver` events and if not we return without
        // running a loop iteration.
        if !self.has_pending() && !readable {
            return;
        }

        // NB: `StartCause::Init` is handled as a special case and doesn't need
        // to be considered here
        let cause = match self.control_flow {
            ControlFlow::Poll => StartCause::Poll,
            ControlFlow::Wait => StartCause::WaitCancelled {
                start,
                requested_resume: None,
            },
            ControlFlow::WaitUntil(deadline) => {
                if Instant::now() < deadline {
                    StartCause::WaitCancelled {
                        start,
                        requested_resume: Some(deadline),
                    }
                } else {
                    StartCause::ResumeTimeReached {
                        start,
                        requested_resume: deadline,
                    }
                }
            }
            // `ExitWithCode()` will be reset to `Poll` before polling
            ControlFlow::ExitWithCode(_code) => unreachable!(),
        };

        self.single_iteration(&mut callback, cause);
    }

    fn single_iteration<F>(&mut self, callback: &mut F, cause: StartCause)
    where
        F: FnMut(Event<'_, T>, &RootELW<T>, &mut ControlFlow),
    {
        let mut control_flow = self.control_flow;

        sticky_exit_callback(
            crate::event::Event::NewEvents(cause),
            &self.target,
            &mut control_flow,
            callback,
        );

        // NB: For consistency all platforms must emit a 'resumed' event even though X11
        // applications don't themselves have a formal suspend/resume lifecycle.
        if cause == StartCause::Init {
            sticky_exit_callback(
                crate::event::Event::Resumed,
                &self.target,
                &mut control_flow,
                callback,
            );
        }

        // Process all pending events
        self.drain_events(callback, &mut control_flow);

        // Empty the user event buffer
        {
            while let Ok(event) = self.user_receiver.try_recv() {
                sticky_exit_callback(
                    crate::event::Event::UserEvent(event),
                    &self.target,
                    &mut control_flow,
                    callback,
                );
            }
        }
        // send MainEventsCleared
        {
            sticky_exit_callback(
                crate::event::Event::MainEventsCleared,
                &self.target,
                &mut control_flow,
                callback,
            );
        }
        // Empty the redraw requests
        {
            let mut windows = HashSet::new();

            while let Ok(window_id) = self.redraw_receiver.try_recv() {
                windows.insert(window_id);
            }

            for window_id in windows {
                let window_id = crate::window::WindowId(window_id);
                sticky_exit_callback(
                    Event::RedrawRequested(window_id),
                    &self.target,
                    &mut control_flow,
                    callback,
                );
            }
        }
        // send RedrawEventsCleared
        {
            sticky_exit_callback(
                crate::event::Event::RedrawEventsCleared,
                &self.target,
                &mut control_flow,
                callback,
            );
        }

        self.control_flow = control_flow;
    }

    fn drain_events<F>(&mut self, callback: &mut F, control_flow: &mut ControlFlow)
    where
        F: FnMut(Event<'_, T>, &RootELW<T>, &mut ControlFlow),
    {
        let target = &self.target;
        let mut xev = MaybeUninit::uninit();
        let wt = get_xtarget(&self.target);

        while unsafe { self.event_processor.poll_one_event(xev.as_mut_ptr()) } {
            let mut xev = unsafe { xev.assume_init() };
            self.event_processor.process_event(&mut xev, |event| {
                sticky_exit_callback(
                    event,
                    target,
                    control_flow,
                    &mut |event, window_target, control_flow| {
                        if let Event::RedrawRequested(crate::window::WindowId(wid)) = event {
                            wt.redraw_sender.sender.send(wid).unwrap();
                            wt.redraw_sender.waker.wake().unwrap();
                        } else {
                            callback(event, window_target, control_flow);
                        }
                    },
                );
            });
        }
    }
}

pub(crate) fn get_xtarget<T>(target: &RootELW<T>) -> &EventLoopWindowTarget<T> {
    match target.p {
        super::EventLoopWindowTarget::X(ref target) => target,
        #[cfg(wayland_platform)]
        _ => unreachable!(),
    }
}

impl<T> EventLoopWindowTarget<T> {
    /// Returns the `XConnection` of this events loop.
    #[inline]
    pub(crate) fn x_connection(&self) -> &Arc<XConnection> {
        &self.xconn
    }

    pub fn set_device_event_filter(&self, filter: DeviceEventFilter) {
        self.device_event_filter.set(filter);
    }

    /// Update the device event filter based on window focus.
    pub fn update_device_event_filter(&self, focus: bool) {
        let filter_events = self.device_event_filter.get() == DeviceEventFilter::Never
            || (self.device_event_filter.get() == DeviceEventFilter::Unfocused && !focus);

        let mut mask = 0;
        if !filter_events {
            mask = ffi::XI_RawMotionMask
                | ffi::XI_RawButtonPressMask
                | ffi::XI_RawButtonReleaseMask
                | ffi::XI_RawKeyPressMask
                | ffi::XI_RawKeyReleaseMask;
        }

        self.xconn
            .select_xinput_events(self.root, ffi::XIAllMasterDevices, mask)
            .queue();
    }

    pub fn raw_display_handle(&self) -> raw_window_handle::RawDisplayHandle {
        let mut display_handle = XlibDisplayHandle::empty();
        display_handle.display = self.xconn.display as *mut _;
        display_handle.screen =
            unsafe { (self.xconn.xlib.XDefaultScreen)(self.xconn.display as *mut _) };
        RawDisplayHandle::Xlib(display_handle)
    }
}

impl<T: 'static> EventLoopProxy<T> {
    pub fn send_event(&self, event: T) -> Result<(), EventLoopClosed<T>> {
        self.user_sender
            .send(event)
            .map_err(|e| EventLoopClosed(e.0))
            .map(|_| self.waker.wake().unwrap())
    }
}

struct DeviceInfo<'a> {
    xconn: &'a XConnection,
    info: *const ffi::XIDeviceInfo,
    count: usize,
}

impl<'a> DeviceInfo<'a> {
    fn get(xconn: &'a XConnection, device: c_int) -> Option<Self> {
        unsafe {
            let mut count = 0;
            let info = (xconn.xinput2.XIQueryDevice)(xconn.display, device, &mut count);
            xconn.check_errors().ok()?;

            if info.is_null() || count == 0 {
                None
            } else {
                Some(DeviceInfo {
                    xconn,
                    info,
                    count: count as usize,
                })
            }
        }
    }
}

impl<'a> Drop for DeviceInfo<'a> {
    fn drop(&mut self) {
        assert!(!self.info.is_null());
        unsafe { (self.xconn.xinput2.XIFreeDeviceInfo)(self.info as *mut _) };
    }
}

impl<'a> Deref for DeviceInfo<'a> {
    type Target = [ffi::XIDeviceInfo];
    fn deref(&self) -> &Self::Target {
        unsafe { slice::from_raw_parts(self.info, self.count) }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(c_int);

impl DeviceId {
    #[allow(unused)]
    pub const unsafe fn dummy() -> Self {
        DeviceId(0)
    }
}

pub(crate) struct Window(Arc<UnownedWindow>);

impl Deref for Window {
    type Target = UnownedWindow;
    #[inline]
    fn deref(&self) -> &UnownedWindow {
        &self.0
    }
}

impl Window {
    pub(crate) fn new<T>(
        event_loop: &EventLoopWindowTarget<T>,
        attribs: WindowAttributes,
        pl_attribs: PlatformSpecificWindowBuilderAttributes,
    ) -> Result<Self, RootOsError> {
        let window = Arc::new(UnownedWindow::new(event_loop, attribs, pl_attribs)?);
        event_loop
            .windows
            .borrow_mut()
            .insert(window.id(), Arc::downgrade(&window));
        Ok(Window(window))
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let window = self.deref();
        let xconn = &window.xconn;
        unsafe {
            (xconn.xlib.XDestroyWindow)(xconn.display, window.id().0 as ffi::Window);
            // If the window was somehow already destroyed, we'll get a `BadWindow` error, which we don't care about.
            let _ = xconn.check_errors();
        }
    }
}

/// XEvents of type GenericEvent store their actual data in an XGenericEventCookie data structure. This is a wrapper to
/// extract the cookie from a GenericEvent XEvent and release the cookie data once it has been processed
struct GenericEventCookie<'a> {
    xconn: &'a XConnection,
    cookie: ffi::XGenericEventCookie,
}

impl<'a> GenericEventCookie<'a> {
    fn from_event(xconn: &XConnection, event: ffi::XEvent) -> Option<GenericEventCookie<'_>> {
        unsafe {
            let mut cookie: ffi::XGenericEventCookie = From::from(event);
            if (xconn.xlib.XGetEventData)(xconn.display, &mut cookie) == ffi::True {
                Some(GenericEventCookie { xconn, cookie })
            } else {
                None
            }
        }
    }
}

impl<'a> Drop for GenericEventCookie<'a> {
    fn drop(&mut self) {
        unsafe {
            (self.xconn.xlib.XFreeEventData)(self.xconn.display, &mut self.cookie);
        }
    }
}

#[derive(Debug, Default, Copy, Clone)]
struct XExtension {
    opcode: c_int,
    first_event_id: c_int,
    first_error_id: c_int,
}

fn mkwid(w: ffi::Window) -> crate::window::WindowId {
    crate::window::WindowId(crate::platform_impl::platform::WindowId(w as _))
}
fn mkdid(w: c_int) -> crate::event::DeviceId {
    crate::event::DeviceId(crate::platform_impl::DeviceId::X(DeviceId(w)))
}

#[derive(Debug)]
struct Device {
    _name: String,
    scroll_axes: Vec<(i32, ScrollAxis)>,
    // For master devices, this is the paired device (pointer <-> keyboard).
    // For slave devices, this is the master.
    attachment: c_int,
}

#[derive(Debug, Copy, Clone)]
struct ScrollAxis {
    increment: f64,
    orientation: ScrollOrientation,
    position: f64,
}

#[derive(Debug, Copy, Clone)]
enum ScrollOrientation {
    Vertical,
    Horizontal,
}

impl Device {
    fn new(info: &ffi::XIDeviceInfo) -> Self {
        let name = unsafe { CStr::from_ptr(info.name).to_string_lossy() };
        let mut scroll_axes = Vec::new();

        if Device::physical_device(info) {
            // Identify scroll axes
            for class_ptr in Device::classes(info) {
                let class = unsafe { &**class_ptr };
                if class._type == ffi::XIScrollClass {
                    let info = unsafe {
                        mem::transmute::<&ffi::XIAnyClassInfo, &ffi::XIScrollClassInfo>(class)
                    };
                    scroll_axes.push((
                        info.number,
                        ScrollAxis {
                            increment: info.increment,
                            orientation: match info.scroll_type {
                                ffi::XIScrollTypeHorizontal => ScrollOrientation::Horizontal,
                                ffi::XIScrollTypeVertical => ScrollOrientation::Vertical,
                                _ => unreachable!(),
                            },
                            position: 0.0,
                        },
                    ));
                }
            }
        }

        let mut device = Device {
            _name: name.into_owned(),
            scroll_axes,
            attachment: info.attachment,
        };
        device.reset_scroll_position(info);
        device
    }

    fn reset_scroll_position(&mut self, info: &ffi::XIDeviceInfo) {
        if Device::physical_device(info) {
            for class_ptr in Device::classes(info) {
                let class = unsafe { &**class_ptr };
                if class._type == ffi::XIValuatorClass {
                    let info = unsafe {
                        mem::transmute::<&ffi::XIAnyClassInfo, &ffi::XIValuatorClassInfo>(class)
                    };
                    if let Some(&mut (_, ref mut axis)) = self
                        .scroll_axes
                        .iter_mut()
                        .find(|&&mut (axis, _)| axis == info.number)
                    {
                        axis.position = info.value;
                    }
                }
            }
        }
    }

    #[inline]
    fn physical_device(info: &ffi::XIDeviceInfo) -> bool {
        info._use == ffi::XISlaveKeyboard
            || info._use == ffi::XISlavePointer
            || info._use == ffi::XIFloatingSlave
    }

    #[inline]
    fn classes(info: &ffi::XIDeviceInfo) -> &[*const ffi::XIAnyClassInfo] {
        unsafe {
            slice::from_raw_parts(
                info.classes as *const *const ffi::XIAnyClassInfo,
                info.num_classes as usize,
            )
        }
    }
}
