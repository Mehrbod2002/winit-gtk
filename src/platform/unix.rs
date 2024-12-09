use std::rc::Rc;
use std::sync::atomic::AtomicBool;

use cursor_icon::CursorIcon;
use dpi::Size;

use crate::event_loop::EventLoop;
use crate::icon::Icon;
use crate::monitor::MonitorHandle;
use crate::platform_impl::wayland::ActiveEventLoop;
use crate::platform_impl::{self, Fullscreen};
use crate::utils::AsAny;
use crate::window::{UserAttentionType, WindowAttributes, WindowId};

pub trait WindowExtUnix {
    fn gtk_window(&self) -> &gtk::ApplicationWindow;

    fn default_box(&self) -> Option<&gtk::Box>;

    fn set_skip_taskbar(&self, skip: bool);
}

pub trait MonitorExtUnix {
    fn native(&self) -> u32;
}

impl MonitorExtUnix for MonitorHandle {
    #[inline]
    fn native(&self) -> u32 {
        self.inner.native_identifier()
    }
}

pub trait WindowAttributesExtUnix {
    fn with_name(self, general: impl Into<String>, instance: impl Into<String>) -> Self;
}

pub trait EventLoopExtUnix {
    fn is_unix(&self) -> bool;
}

impl EventLoopExtUnix for EventLoop {
    fn is_unix(&self) -> bool {
        self.event_loop.is_unix()
    }
}

pub trait ActiveEventLoopExtUnix {
    fn is_unix(&self) -> bool;
}

impl ActiveEventLoopExtUnix for ActiveEventLoop {
    #[inline]
    fn is_unix(&self) -> bool {
        self.as_any().downcast_ref::<platform_impl::unix::ActiveEventLoop>().is_some()
    }
}

impl WindowAttributesExtUnix for WindowAttributes {
    #[inline]
    fn with_name(mut self, general: impl Into<String>, instance: impl Into<String>) -> Self {
        self.platform_specific.name =
            Some(crate::platform_impl::ApplicationName::new(general.into(), instance.into()));
        self
    }
}

impl WindowExtUnix for Window {
    fn gtk_window(&self) -> &gtk::ApplicationWindow {
        &self.window
    }

    fn default_box(&self) -> Option<&gtk::Box> {
        self.default_box()
    }

    fn set_skip_taskbar(&self, skip: bool) {
        self.set_skip_taskbar(skip);
    }
}

pub struct Window {
    pub(crate) window_id: WindowId,
    pub(crate) window: gtk::ApplicationWindow,

    pub(crate) window_request_tx: glib::MainContext,
}

pub(crate) enum WindowRequest {
    Title(String),
    Position((i32, i32)),
    Size((i32, i32)),
    SizeConstraints(Option<Size>, Option<Size>),
    Visible(bool),
    Focus,
    Resizable(bool),
    // Closable(bool),
    Minimized(bool),
    Maximized(bool),
    DragWindow,
    Fullscreen(Option<Fullscreen>),
    Decorations(bool),
    AlwaysOnBottom(bool),
    AlwaysOnTop(bool),
    WindowIcon(Option<Icon>),
    UserAttention(Option<UserAttentionType>),
    SetSkipTaskbar(bool),
    CursorIcon(Option<CursorIcon>),
    CursorPosition((i32, i32)),
    CursorIgnoreEvents(bool),
    WireUpEvents { transparent: Rc<AtomicBool> },
    // SetVisibleOnAllWorkspaces(bool),
    // ProgressBarState(ProgressBarState),
}
