#![allow(deprecated)]
#![cfg(free_unix)]
#[cfg(all(not(x11_platform), not(wayland_platform)))]
compile_error!("Please select a feature to build for unix: `x11`, `wayland`");

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
mod keyboard;

use dpi::{LogicalPosition, LogicalSize};
use gdk::cairo::{RectangleInt, Region};
use gdk::glib::MainContext;
use gdk::prelude::{DeviceExt, SeatExt};
use gdk::{cairo, Cursor, EventKey, EventMask, ScrollDirection, WindowEdge, WindowState};
use gio::prelude::ApplicationExt;
use gio::Cancellable;
use glib::Priority;
use gtk::prelude::{GtkApplicationExt, GtkWindowExt, WidgetExt, WidgetExtManual};
use smol_str::SmolStr;

pub(crate) use self::common::xkb::{physicalkey_to_scancode, scancode_to_physicalkey};
use crate::application::ApplicationHandler;
pub(crate) use crate::cursor::OnlyCursorImageSource as PlatformCustomCursorSource;
use crate::error::EventLoopError;
use crate::event::{
    DeviceId as RootDeviceId, ElementState, Event, MouseButton, MouseScrollDelta, TouchPhase,
    WindowEvent,
};
use crate::event_loop::ControlFlow;
pub(crate) use crate::icon::RgbaIcon as PlatformIcon;
use crate::keyboard::{Key, ModifiersState};
use crate::platform::pump_events::PumpStatus;
use crate::platform::unix::WindowRequest;
use crate::window::{ActivationToken, WindowId};

pub(crate) mod common;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Backend {
    #[cfg(x11_platform)]
    X,
    #[cfg(wayland_platform)]
    Wayland,
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PlatformSpecificEventLoopAttributes {
    pub(crate) forced_backend: Option<Backend>,
    pub(crate) any_thread: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationName {
    pub general: String,
    pub instance: String,
}

impl ApplicationName {
    pub fn new(general: String, instance: String) -> Self {
        Self { general, instance }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlatformSpecificWindowAttributes {
    pub name: Option<ApplicationName>,
    pub activation_token: Option<ActivationToken>,
    pub parent: Option<gtk::Window>,
    pub skip_taskbar: bool,
    pub auto_transparent: bool,
    pub double_buffered: bool,
    pub rgba_visual: bool,
    pub default_vbox: bool,
    pub app_paintable: bool,
}

#[cfg_attr(not(x11_platform), allow(clippy::derivable_impls))]
impl Default for PlatformSpecificWindowAttributes {
    fn default() -> Self {
        Self {
            name: None,
            activation_token: None,
            skip_taskbar: Default::default(),
            auto_transparent: true,
            default_vbox: true,
            rgba_visual: true,
            double_buffered: true,
            app_paintable: true,
            parent: None,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(usize);

pub(crate) const DEVICE_ID: RootDeviceId = RootDeviceId::from_raw(DeviceId(0).0 as i64);

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct KeyEventExtra {
    pub text_with_all_modifiers: Option<SmolStr>,
    pub key_without_modifiers: Key,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum PlatformCustomCursor {
    GTK,
}

pub struct EventLoop<T> {
    window_target: ActiveEventLoop<T>,
    pub(crate) user_event_rx: crossbeam_channel::Sender<Event>,

    events: crossbeam_channel::Receiver<EventLoop<T>>,
    draws: crossbeam_channel::Receiver<WindowId>,
}

pub struct ActiveEventLoop<T> {
    pub(crate) display: gdk::Display,
    /// Gtk application
    pub(crate) app: gtk::Application,
    /// Window Ids of the application
    pub(crate) windows: Rc<RefCell<HashSet<WindowId>>>,
    /// Window requests sender
    pub(crate) window_requests_tx: glib::Sender<(WindowId, WindowRequest)>,
    /// Draw event sender
    pub(crate) draw_tx: crossbeam_channel::Sender<WindowId>,
    _marker: std::marker::PhantomData<T>,

    exit: Cell<bool>,
    control_flow: Cell<ControlFlow>,
}

impl<T: 'static> EventLoop<T> {
    pub(crate) fn new(
        attributes: &PlatformSpecificEventLoopAttributes,
    ) -> Result<Self, EventLoopError> {
        let context = MainContext::default();
        let app = gtk::Application::new(None, gio::ApplicationFlags::empty());
        let app_ = app.clone();
        let cancellable: Option<&Cancellable> = None;
        app.register(cancellable).expect("failed to initialize the main thread");

        let (event_tx, event_rs) = crossbeam_channel::unbounded();
        let (draw_tx, draw_rs) = crossbeam_channel::unbounded();
        let event_tx_ = event_tx.clone();
        let draw_tx_ = draw_tx.clone();
        let user_event_tx = event_tx.clone();

        let (window_requests_tx, window_reuqest_rx) =
            glib::MainContext::channel(Priority::default());
        let display = gdk::Display::default().expect("unable to get display");

        let window_target = ActiveEventLoop {
            display,
            app,
            windows: Rc::new(RefCell::new(HashSet::new())),
            window_requests_tx,
            draw_tx: draw_tx_,
            _marker: std::marker::PhantomData,
        };

        window_reuqest_rx.attach(Some(&context), move |(id, request)| {
            if let Some(window) = app_.window_by_id(id.into_raw() as u32) {
                match request {
                    WindowRequest::Title(title) => window.set_title(&title),
                    WindowRequest::Position((x, y)) => window.move_(x, y),
                    WindowRequest::Size((x, y)) => window.resize(x, y),
                    WindowRequest::SizeConstraints(size, size1) => todo!(),
                    WindowRequest::Visible(visible) => {
                        if visible {
                            window.show_all();
                        } else {
                            window.hide();
                        }
                    },
                    WindowRequest::Focus => todo!(),
                    WindowRequest::Resizable(resizable) => window.set_resizable(resizable),
                    WindowRequest::Minimized(min) => {
                        if min {
                            window.iconify();
                        } else {
                            window.deiconify();
                        }
                    },
                    WindowRequest::Maximized(max) => {
                        if max {
                            window.maximize();
                        } else {
                            window.unmaximize();
                        }
                    },
                    WindowRequest::DragWindow => {
                        if let Some(cursor) =
                            window.display().default_seat().and_then(|seat| seat.pointer())
                        {
                            let (_, x, y) = cursor.position();
                            window.begin_move_drag(1, x, y, 0);
                        }
                    },
                    WindowRequest::Fullscreen(fullscreen) => todo!(),
                    WindowRequest::Decorations(dec) => window.set_decorated(dec),
                    WindowRequest::AlwaysOnBottom(top) => window.set_keep_below(top),
                    WindowRequest::AlwaysOnTop(bottom) => window.set_keep_above(bottom),
                    WindowRequest::WindowIcon(window_icon) => {
                        if let Some(icon) = window_icon {
                            window.set_icon(Some(&icon.inner));
                        }
                    },
                    WindowRequest::UserAttention(request_type) => {
                        window.set_urgency_hint(request_type.is_some())
                    },
                    WindowRequest::SetSkipTaskbar(skip) => {
                        window.set_skip_taskbar_hint(skip);
                        window.set_skip_pager_hint(skip)
                    },
                    WindowRequest::CursorIcon(cursor_icon) => todo!(),
                    WindowRequest::CursorPosition((x, y)) => {
                        if let Some(cursor) =
                            window.display().default_seat().and_then(|seat| seat.pointer())
                        {
                            if let Some(screen) = GtkWindowExt::screen(&window) {
                                cursor.warp(&screen, x, y);
                            };
                        }
                    },
                    WindowRequest::CursorIgnoreEvents(ignore) => {
                        if ignore {
                            let empty_region =
                                Region::create_rectangle(&RectangleInt::new(0, 0, 1, 1));
                            window.window().unwrap().input_shape_combine_region(
                                &empty_region,
                                0,
                                0,
                            );
                        } else {
                            window.input_shape_combine_region(None)
                        };
                    },
                    WindowRequest::WireUpEvents { transparent } => {
                        window.add_events(
                            EventMask::POINTER_MOTION_MASK
                                | EventMask::BUTTON1_MOTION_MASK
                                | EventMask::BUTTON_PRESS_MASK
                                | EventMask::TOUCH_MASK
                                | EventMask::STRUCTURE_MASK
                                | EventMask::FOCUS_CHANGE_MASK
                                | EventMask::SCROLL_MASK,
                        );

                        // Allow resizing unmaximized borderless window
                        window.connect_motion_notify_event(|window, event| {
                            if !window.is_decorated()
                                && window.is_resizable()
                                && !window.is_maximized()
                            {
                                if let Some(window) = window.window() {
                                    let (cx, cy) = event.root();
                                    let edge = hit_test(&window, cx, cy);
                                    window.set_cursor(
                                        Cursor::from_name(&window.display(), match edge {
                                            WindowEdge::North => "n-resize",
                                            WindowEdge::South => "s-resize",
                                            WindowEdge::East => "e-resize",
                                            WindowEdge::West => "w-resize",
                                            WindowEdge::NorthWest => "nw-resize",
                                            WindowEdge::NorthEast => "ne-resize",
                                            WindowEdge::SouthEast => "se-resize",
                                            WindowEdge::SouthWest => "sw-resize",
                                            _ => "default",
                                        })
                                        .as_ref(),
                                    );
                                }
                            }
                            glib::Propagation::Proceed
                        });
                        window.connect_touch_event(|window, event| {
                            if !window.is_decorated() && window.is_resizable() {
                                if let Some(window) = window.window() {
                                    if let Some((cx, cy)) = event.root_coords() {
                                        if let Some(device) = event.device() {
                                            let result = hit_test(&window, cx, cy);

                                            // Ignore the `__Unknown` variant so the window receives
                                            // the click correctly if it is not on the edges.
                                            match result {
                                                WindowEdge::__Unknown(_) => (),
                                                _ => window.begin_resize_drag_for_device(
                                                    result,
                                                    &device,
                                                    0,
                                                    cx as i32,
                                                    cy as i32,
                                                    event.time(),
                                                ),
                                            }
                                        }
                                    }
                                }
                            }

                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_delete_event(move |_, _| {
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: id,
                                event: WindowEvent::CloseRequested,
                            }) {
                                log::warn!(
                                    "Failed to send window close event to event channel: {}",
                                    e
                                );
                            }
                            glib::Propagation::Stop
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_configure_event(move |window, event| {
                            let scale_factor = window.scale_factor();

                            let (x, y) = event.position();
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: id,
                                event: WindowEvent::Moved(
                                    LogicalPosition::new(x, y).to_physical(scale_factor as f64),
                                ),
                            }) {
                                log::warn!(
                                    "Failed to send window moved event to event channel: {}",
                                    e
                                );
                            }

                            let (w, h) = event.size();
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: id,
                                event: WindowEvent::Resized(
                                    LogicalSize::new(w, h).to_physical(scale_factor as f64),
                                ),
                            }) {
                                log::warn!(
                                    "Failed to send window resized event to event channel: {}",
                                    e
                                );
                            }
                            false
                        });
                        let tx_clone = event_tx.clone();
                        window.connect_focus_in_event(move |_, _| {
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: id,
                                event: WindowEvent::Focused(true),
                            }) {
                                log::warn!(
                                    "Failed to send window focus-in event to event channel: {}",
                                    e
                                );
                            }
                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_focus_out_event(move |_, _| {
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: id,
                                event: WindowEvent::Focused(false),
                            }) {
                                log::warn!(
                                    "Failed to send window focus-out event to event channel: {}",
                                    e
                                );
                            }
                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_destroy(move |_| {
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: id,
                                event: WindowEvent::Destroyed,
                            }) {
                                log::warn!(
                                    "Failed to send window destroyed event to event channel: {}",
                                    e
                                );
                            }
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_enter_notify_event(move |_, _| {
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: id,
                                event: WindowEvent::CursorEntered { device_id: DEVICE_ID },
                            }) {
                                log::warn!(
                                    "Failed to send cursor entered event to event channel: {}",
                                    e
                                );
                            }
                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_motion_notify_event(move |window, motion| {
                            if let Some(cursor) = motion.device() {
                                let scale_factor = window.scale_factor();
                                let (_, x, y) = cursor.window_at_position();
                                if let Err(e) = tx_clone.send(Event::WindowEvent {
                                    window_id: id,
                                    event: WindowEvent::CursorMoved {
                                        position: LogicalPosition::new(x, y)
                                            .to_physical(scale_factor as f64),
                                        device_id: DEVICE_ID,
                                        // this field is depracted so it is fine to pass empty state
                                        modifiers: ModifiersState::empty(),
                                    },
                                }) {
                                    log::warn!(
                                        "Failed to send cursor moved event to event channel: {}",
                                        e
                                    );
                                }
                            }
                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_leave_notify_event(move |_, _| {
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: id,
                                event: WindowEvent::CursorLeft { device_id: DEVICE_ID },
                            }) {
                                log::warn!(
                                    "Failed to send cursor left event to event channel: {}",
                                    e
                                );
                            }
                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_button_press_event(move |_, event| {
                            let button = event.button();
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: id,
                                event: WindowEvent::MouseInput {
                                    button: match button {
                                        1 => MouseButton::Left,
                                        2 => MouseButton::Middle,
                                        3 => MouseButton::Right,
                                        _ => MouseButton::Other(button as u16),
                                    },
                                    state: ElementState::Pressed,
                                    device_id: DEVICE_ID,
                                    // this field is depracted so it is fine to pass empty state
                                    modifiers: ModifiersState::empty(),
                                },
                            }) {
                                log::warn!(
                                    "Failed to send mouse input preseed event to event channel: {}",
                                    e
                                );
                            }
                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_button_release_event(move |_, event| {
                            let button = event.button();
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: id,
                                event: WindowEvent::MouseInput {
                                    button: match button {
                                        1 => MouseButton::Left,
                                        2 => MouseButton::Middle,
                                        3 => MouseButton::Right,
                                        _ => MouseButton::Other(button as u16),
                                    },
                                    state: ElementState::Released,
                                    device_id: DEVICE_ID,
                                    // this field is depracted so it is fine to pass empty state
                                    modifiers: ModifiersState::empty(),
                                },
                            }) {
                                log::warn!(
                                    "Failed to send mouse input released event to event channel: \
                                     {}",
                                    e
                                );
                            }
                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_scroll_event(move |_, event| {
                            let (x, y) = event.delta();
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: id,
                                event: WindowEvent::MouseWheel {
                                    device_id: Some(DEVICE_ID),
                                    delta: MouseScrollDelta::LineDelta(-x as f32, -y as f32),
                                    phase: match event.direction() {
                                        ScrollDirection::Smooth => TouchPhase::Moved,
                                        _ => TouchPhase::Ended,
                                    },
                                    modifiers: ModifiersState::empty(),
                                },
                            }) {
                                log::warn!("Failed to send scroll event to event channel: {}", e);
                            }
                            glib::Propagation::Proceed
                        });
                        let tx_clone = event_tx.clone();
                        let modifiers = AtomicU32::new(ModifiersState::empty().bits());
                        let keyboard_handler =
                            Rc::new(move |event_key: EventKey, element_state| {
                                // if we have a modifier lets send it
                                let new_mods = keyboard::get_modifiers(&event_key);
                                if new_mods.bits() != modifiers.load(Ordering::Relaxed) {
                                    modifiers.store(new_mods.bits(), Ordering::Relaxed);
                                    if let Err(e) = tx_clone.send(Event::WindowEvent {
                                        window_id: id,
                                        event: WindowEvent::ModifiersChanged(new_mods),
                                    }) {
                                        log::warn!(
                                            "Failed to send modifiers changed event to event \
                                             channel: {}",
                                            e
                                        );
                                    }
                                }

                                let virtual_key =
                                    keyboard::gdk_key_to_virtual_key(event_key.keyval());
                                #[allow(deprecated)]
                                if let Err(e) = tx_clone.send(Event::WindowEvent {
                                    window_id: id,
                                    event: WindowEvent::KeyboardInput {
                                        device_id: Some(DEVICE_ID),
                                        is_synthetic: false,
                                        event: todo!(),
                                    },
                                }) {
                                    log::warn!(
                                        "Failed to send keyboard event to event channel: {}",
                                        e
                                    );
                                }

                                glib::ControlFlow::Continue
                            });

                        //     let tx_clone = event_tx.clone();
                        //     // TODO Add actual IME from system
                        //     let ime = gtk::IMContextSimple::default();
                        //     ime.set_client_window(window.window().as_ref());
                        //     ime.focus_in();
                        //     ime.connect_commit(move |_, s| {
                        // let c = s.chars().collect::<Vec<char>>();
                        //         if let Err(e) = tx_clone.send(Event::WindowEvent {
                        //             window_id: id,
                        //             event: WindowEvent::ReceivedCharacter(c[0]),
                        //         }) {
                        //             log::warn!(
                        //                 "Failed to send received IME text event to event channel:
                        // {}",                 e
                        //             );
                        //         }
                        //     });

                        let handler = keyboard_handler.clone();
                        window.connect_key_press_event(move |_, event_key| {
                            handler(event_key.to_owned(), ElementState::Pressed);
                            // ime.filter_keypress(event_key);

                            glib::Propagation::Proceed
                        });

                        let handler = keyboard_handler.clone();
                        window.connect_key_release_event(move |_, event_key| {
                            handler(event_key.to_owned(), ElementState::Released);
                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_window_state_event(move |window, event| {
                            let state = event.changed_mask();
                            if state.contains(WindowState::ICONIFIED)
                                || state.contains(WindowState::MAXIMIZED)
                            {
                                let scale_factor = window.scale_factor();

                                let (x, y) = window.position();
                                if let Err(e) = tx_clone.send(Event::WindowEvent {
                                    window_id: id,
                                    event: WindowEvent::Moved(
                                        LogicalPosition::new(x, y).to_physical(scale_factor as f64),
                                    ),
                                }) {
                                    log::warn!(
                                        "Failed to send window moved event to event channel: {}",
                                        e
                                    );
                                }

                                let (w, h) = window.size();
                                if let Err(e) = tx_clone.send(Event::WindowEvent {
                                    window_id: id,
                                    event: WindowEvent::Resized(
                                        LogicalSize::new(w, h).to_physical(scale_factor as f64),
                                    ),
                                }) {
                                    log::warn!(
                                        "Failed to send window moved event to event channel: {}",
                                        e
                                    );
                                }
                            }
                            glib::Propagation::Proceed
                        });

                        // Receive draw events of the window.
                        let draw_clone = draw_tx.clone();
                        window.connect_draw(move |_, cr| {
                            if transparent.load(Ordering::Relaxed) {
                                cr.set_source_rgba(0., 0., 0., 0.);
                                cr.set_operator(cairo::Operator::Source);
                                let _ = cr.paint();
                                cr.set_operator(cairo::Operator::Over);
                            }

                            glib::Propagation::Proceed
                        });
                    },
                };
            }
            glib::ControlFlow::Continue
        });

        Ok(Self {
            window_target: ActiveEventLoop {
                _marker: std::marker::PhantomData,
                display,
                app,
                windows: window_target.windows,
                window_requests_tx,
                draw_tx,
                exit: false.into(),
                control_flow: false,
            },
            events: event_rs,
            draws: draw_rs,
            user_event_rx: None,
        })
    }

    #[cfg(wayland_platform)]
    fn new_any_thread(
        attributes: &PlatformSpecificEventLoopAttributes,
    ) -> Result<EventLoop<T>, EventLoopError> {
        EventLoop::new(attributes).map(|evlp| EventLoop::Wayland(Box::new(evlp)))
    }

    pub fn run_app_on_demand<A: ApplicationHandler>(self, app: A) -> Result<(), EventLoopError> {
        let context = MainContext::default();

        enum EvenState {
            NewStart,
            EventQueue,
            DrawQueue,
        }

        context.with_thread_default(|| {
            let mut control = ControlFlow::default();
            let window_target = &self.window_target;
            let event = &self.events;
            let draws = &self.draws;

            window_target.app.activate();

            let mut state = EvenState::NewStart;

            let exit_code = loop {
                let mut blocking = false;

                match state {
                    EvenState::NewStart => match control {
                        ControlFlow::Poll => {
                            app.new_events(event_loop, cause);
                        },
                        ControlFlow::Wait => {
                            app.about_to_wait(event_loop);
                        },
                        ControlFlow::WaitUntil(instant) => {
                            let start = Instant::now();
                            if start >= instant {
                                app.about_to_wait(event_loop);
                            }
                        },
                    },
                    EvenState::EventQueue => todo!(),
                    EvenState::DrawQueue => todo!(),
                };
            };
        });
    }

    pub fn run<A: ApplicationHandler>(&mut self, app: A) -> Result<(), EventLoopError> {
        self.window_target.exit.set(true);
        {
            let runner = &self.window_target.runner_shared;

            let event_loop_windows_ref = &self.window_target;
            // # Safety
            // We make sure to call runner.clear_event_handler() before
            // returning
            unsafe {
                runner.set_event_handler(move |event| match event {
                    Event::NewEvents(cause) => app.new_events(event_loop_windows_ref, cause),
                    Event::WindowEvent { window_id, event } => {
                        app.window_event(event_loop_windows_ref, window_id, event)
                    },
                    Event::DeviceEvent { device_id, event } => {
                        app.device_event(event_loop_windows_ref, device_id, event)
                    },
                    Event::UserWakeUp => app.proxy_wake_up(event_loop_windows_ref),
                    Event::Suspended => app.suspended(event_loop_windows_ref),
                    Event::Resumed => app.resumed(event_loop_windows_ref),
                    Event::CreateSurfaces => app.can_create_surfaces(event_loop_windows_ref),
                    Event::AboutToWait => app.about_to_wait(event_loop_windows_ref),
                    Event::LoopExiting => app.exiting(event_loop_windows_ref),
                    Event::MemoryWarning => app.memory_warning(event_loop_windows_ref),
                });
            }
        }

        let exit_code = loop {
            self.wait_for_messages(None);
            // wait_for_messages calls user application before and after waiting
            // so it may have decided to exit.
            if let Some(code) = self.exit_code() {
                break code;
            }

            self.dispatch_peeked_messages();

            if let Some(code) = self.exit_code() {
                break code;
            }
        };

        let runner = &self.window_target.runner_shared;
        runner.loop_destroyed();

        // # Safety
        // We assume that this will effectively call `runner.clear_event_handler()`
        // to meet the safety requirements for calling `runner.set_event_handler()` above.
        runner.reset_runner();

        if exit_code == 0 {
            Ok(())
        } else {
            Err(EventLoopError::ExitFailure(exit_code))
        }
    }

    pub fn pump_app_events<A: ApplicationHandler>(
        &mut self,
        timeout: Option<Duration>,
        app: A,
    ) -> PumpStatus {
        self.window_target._marker
    }

    pub fn window_target(&self) -> &ActiveEventLoop<T> {
        &self.window_target
    }
}

impl<T> AsFd for EventLoop<T> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.as_fd()
    }
}

impl<T> AsRawFd for EventLoop<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.as_raw_fd()
    }
}

/// Returns the minimum `Option<Duration>`, taking into account that `None`
/// equates to an infinite timeout, not a zero timeout (so can't just use
/// `Option::min`)
fn min_timeout(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    a.map_or(b, |a_timeout| b.map_or(Some(a_timeout), |b_timeout| Some(a_timeout.min(b_timeout))))
}

#[cfg(target_os = "linux")]
fn is_main_thread() -> bool {
    rustix::thread::gettid() == rustix::process::getpid()
}

#[cfg(any(target_os = "dragonfly", target_os = "freebsd", target_os = "openbsd"))]
fn is_main_thread() -> bool {
    use libc::pthread_main_np;

    unsafe { pthread_main_np() == 1 }
}

#[cfg(target_os = "netbsd")]
fn is_main_thread() -> bool {
    std::thread::current().name() == Some("main")
}

pub const BORDERLESS_RESIZE_INSET: i32 = 5;
pub fn hit_test(window: &gdk::Window, cx: f64, cy: f64) -> WindowEdge {
    let (left, top) = window.position();
    let (w, h) = (window.width(), window.height());
    let (right, bottom) = (left + w, top + h);
    let (cx, cy) = (cx as i32, cy as i32);

    const LEFT: i32 = 0b0001;
    const RIGHT: i32 = 0b0010;
    const TOP: i32 = 0b0100;
    const BOTTOM: i32 = 0b1000;
    const TOPLEFT: i32 = TOP | LEFT;
    const TOPRIGHT: i32 = TOP | RIGHT;
    const BOTTOMLEFT: i32 = BOTTOM | LEFT;
    const BOTTOMRIGHT: i32 = BOTTOM | RIGHT;

    let inset = BORDERLESS_RESIZE_INSET * window.scale_factor();
    #[rustfmt::skip]
  let result =
      (LEFT * (if cx < (left + inset) { 1 } else { 0 }))
    | (RIGHT * (if cx >= (right - inset) { 1 } else { 0 }))
    | (TOP * (if cy < (top + inset) { 1 } else { 0 }))
    | (BOTTOM * (if cy >= (bottom - inset) { 1 } else { 0 }));

    match result {
        LEFT => WindowEdge::West,
        TOP => WindowEdge::North,
        RIGHT => WindowEdge::East,
        BOTTOM => WindowEdge::South,
        TOPLEFT => WindowEdge::NorthWest,
        TOPRIGHT => WindowEdge::NorthEast,
        BOTTOMLEFT => WindowEdge::SouthWest,
        BOTTOMRIGHT => WindowEdge::SouthEast,
        // we return `WindowEdge::__Unknown` to be ignored later.
        // we must return 8 or bigger, otherwise it will be the same as one of the other 7 variants
        // of `WindowEdge` enum.
        _ => WindowEdge::__Unknown(8),
    }
}

impl DeviceId {
    pub const unsafe fn dummy() -> Self {
        Self(0)
    }
}
