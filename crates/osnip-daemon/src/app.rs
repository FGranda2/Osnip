//! Iced application: owns every pin window.
//!
//! Architectural notes:
//! - `iced::daemon` runs on the main thread (Wayland's main-thread
//!   requirement is non-negotiable for windowing).
//! - The tokio runtime lives on a dedicated worker thread; the IPC
//!   accept loop runs there and pushes [`AppEvent`]s into a tokio
//!   `UnboundedSender`, which the iced subscription bridges via
//!   `iced::stream::channel`.
//! - The receiver end is parked in a `OnceLock` because
//!   `iced::Subscription::run` takes a function pointer, not a
//!   closure — there is no closure-capture path. The static is set
//!   exactly once at startup and consumed exactly once on the first
//!   subscription poll.
//! - Pin pixels are read from the shared [`PinRegistry`] on demand —
//!   the registry is the source of truth for both threads.

use crate::config::Config;
use crate::image_ops;
use crate::notify::{self, Urgency};
use crate::registry::PinRegistry;
use crate::{clipboard, save};
use iced::futures::SinkExt;
use iced::theme;
use iced::widget::image::Handle;
use iced::widget::{container, image as image_widget};
use iced::{event, keyboard, window};
use iced::{Color, Element, Event, Length, Size, Subscription, Task, Theme};
use image::RgbaImage;
use osnip_core::{PinActionKind, PinId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::mpsc::UnboundedReceiver;

/// Events the IPC layer pushes into the iced runtime.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// A pin was just registered; open a window for it.
    OpenPin(PinId),
    /// A pin was closed via the IPC; tear down its window.
    ClosePin(PinId),
    /// All pins were closed at once.
    CloseAllPins,
    /// An out-of-process client asked for an action on a pin, without
    /// that pin's window ever being focused. Same six operations the
    /// keyboard exposes; see [`PinAction`].
    PinAction {
        /// The pin to act on.
        pin_id: PinId,
        /// What to do to it.
        action: PinAction,
    },
}

/// Actions a pin can be asked to perform.
///
/// Aliased to the shared IPC type rather than redeclared: the keyboard
/// handler and the socket must agree on exactly one set of operations,
/// and two enums that "obviously" mirror each other are two enums that
/// eventually will not.
///
/// Keyboard bindings on a focused pin window:
/// `Ctrl+C` copy · `Ctrl+S` save · `]` / `[` rotate · `H` / `V` flip.
pub type PinAction = PinActionKind;

/// Internal message type for the iced runtime.
#[derive(Debug, Clone)]
pub enum Message {
    Event(AppEvent),
    WindowOpened {
        id: window::Id,
        pin_id: PinId,
    },
    WindowClosed(window::Id),
    /// `id` was resized by the compositor / user.
    Resized {
        id: window::Id,
        size: Size,
    },
    /// User pressed a shortcut while a pin window held keyboard focus.
    Action {
        window_id: window::Id,
        action: PinAction,
    },
    /// Sentinel returned by every spawned async action (copy / save).
    /// The async work runs the notify-send call itself; this message
    /// only exists to satisfy `Task::perform`'s mapper requirement.
    AsyncDone,
}

/// Wayland `app-id` set on every pin window. Compositor rules match on
/// this string: Niri's `match app-id=`, Hyprland's `class`. See
/// `contrib/niri/config-snippet.kdl` and `contrib/omarchy/osnip.lua`.
pub const PIN_APP_ID: &str = "osnip-pin";

/// Global, set-exactly-once slot for the IPC→GUI receiver. See
/// module-level docs for why this can't live inside [`App`].
static IPC_RX: OnceLock<Mutex<Option<UnboundedReceiver<AppEvent>>>> = OnceLock::new();

/// Hand the IPC→GUI receiver over to the iced subscription. Must be
/// called before `iced::daemon(...).run()`.
pub fn install_ipc_receiver(rx: UnboundedReceiver<AppEvent>) {
    IPC_RX
        .set(Mutex::new(Some(rx)))
        .map_err(|_| ())
        .expect("IPC_RX must be installed exactly once");
}

/// Per-window UI state that the iced runtime owns.
///
/// `aspect` is the image's width/height ratio captured at open time.
/// We use it to snap the window back to the image AR after the user
/// (or compositor) resizes, so the rendered pixels always fill the
/// window without letterboxing. `last_size` is the last size we
/// observed/snapped to; comparing the next `Resized` against it tells
/// us which axis the user dragged so we can let that axis drive the
/// AR computation.
#[derive(Debug, Clone, Copy)]
struct WindowState {
    aspect: f32,
    last_size: Size,
}

pub struct App {
    registry: Arc<PinRegistry>,
    config: Arc<Config>,
    pin_to_window: HashMap<PinId, window::Id>,
    window_to_pin: HashMap<window::Id, PinId>,
    windows: HashMap<window::Id, WindowState>,
    /// Cached `Handle` per pin. `Handle::from_rgba` mints a fresh
    /// `Id::unique()` every call, and the renderer keys its GPU
    /// texture cache by that id — rebuilding the handle on every
    /// `view()` would re-upload every pin's texture every frame and
    /// cause visible blinks whenever any unrelated update redraws
    /// other windows. Building once at open time keeps the id (and
    /// the GPU upload) stable for the lifetime of the pin.
    handles: HashMap<PinId, Handle>,
}

impl App {
    pub fn new(registry: Arc<PinRegistry>, config: Arc<Config>) -> (Self, Task<Message>) {
        (
            Self {
                registry,
                config,
                pin_to_window: HashMap::new(),
                window_to_pin: HashMap::new(),
                windows: HashMap::new(),
                handles: HashMap::new(),
            },
            Task::none(),
        )
    }

    pub fn title(&self, _window: window::Id) -> String {
        "osnip".into()
    }

    /// Pin-window background. Default iced theme uses a near-white
    /// palette colour; on every redraw the renderer clears the surface
    /// to that colour before drawing the image. While the user resizes,
    /// the clear happens before the image arrives at the new size, so
    /// the user sees a flash of background each frame. Painting black
    /// (and zero text) makes the gap invisible against the image while
    /// keeping the surface fully opaque (Wayland requirement).
    pub fn style(&self, _theme: &Theme) -> theme::Style {
        theme::Style {
            background_color: Color::BLACK,
            text_color: Color::WHITE,
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Event(AppEvent::OpenPin(pin_id)) => self.open_pin(pin_id),
            Message::Event(AppEvent::ClosePin(pin_id)) => self.close_pin(pin_id),
            Message::Event(AppEvent::CloseAllPins) => self.close_all_pins(),
            Message::Event(AppEvent::PinAction { pin_id, action }) => {
                self.dispatch_action(pin_id, action)
            }
            Message::WindowOpened { id, pin_id } => {
                self.pin_to_window.insert(pin_id, id);
                self.window_to_pin.insert(id, pin_id);
                if let Some(img) = self.registry.image(pin_id) {
                    self.handles.entry(pin_id).or_insert_with(|| {
                        Handle::from_rgba(img.width(), img.height(), img.as_raw().clone())
                    });
                }
                let (lw, lh) = self
                    .registry
                    .logical_size(pin_id)
                    .or_else(|| self.registry.image(pin_id).map(|i| (i.width(), i.height())))
                    .unwrap_or((1, 1));
                let aspect = (lw.max(1) as f32) / (lh.max(1) as f32);
                self.windows.insert(
                    id,
                    WindowState {
                        aspect,
                        last_size: Size::new(lw as f32, lh as f32),
                    },
                );
                tracing::debug!(pin_id = %pin_id, ?id, aspect, "window opened");
                Task::none()
            }
            Message::WindowClosed(id) => {
                self.windows.remove(&id);
                if let Some(pin_id) = self.window_to_pin.remove(&id) {
                    self.pin_to_window.remove(&pin_id);
                    self.handles.remove(&pin_id);
                    self.registry.close(pin_id);
                    tracing::info!(pin_id = %pin_id, "window closed by compositor");
                }
                Task::none()
            }
            Message::Resized { id, size } => self.on_resized(id, size),
            Message::Action { window_id, action } => self.on_action(window_id, action),
            Message::AsyncDone => Task::none(),
        }
    }

    /// Resolve a keyboard action against the focused pin's window.
    /// Unknown windows (or windows whose pin has been closed) are
    /// silently ignored — the user pressed a key against nothing
    /// actionable.
    fn on_action(&mut self, window_id: window::Id, action: PinAction) -> Task<Message> {
        let pin_id = match self.window_to_pin.get(&window_id) {
            Some(id) => *id,
            None => return Task::none(),
        };
        self.dispatch_action(pin_id, action)
    }

    /// Apply an action to a pin by id, whichever way it was requested.
    /// The keyboard path resolves its window first; the IPC path
    /// addresses the pin directly. Both land here so a pin acted on
    /// from the bar behaves identically to one acted on with the
    /// keyboard, notifications included.
    fn dispatch_action(&mut self, pin_id: PinId, action: PinAction) -> Task<Message> {
        match action {
            PinAction::Copy => self.spawn_copy(pin_id),
            PinAction::Save => self.spawn_save(pin_id),
            PinAction::RotateRight => self.transform(pin_id, true, image_ops::rotate_right),
            PinAction::RotateLeft => self.transform(pin_id, true, image_ops::rotate_left),
            PinAction::FlipH => self.transform(pin_id, false, image_ops::flip_horizontal),
            PinAction::FlipV => self.transform(pin_id, false, image_ops::flip_vertical),
        }
    }

    fn spawn_copy(&self, pin_id: PinId) -> Task<Message> {
        let image = match self.registry.image(pin_id) {
            Some(img) => img,
            None => return Task::none(),
        };
        Task::perform(
            async move {
                match clipboard::write_clipboard_image(image).await {
                    Ok(()) => {
                        tracing::info!(pin_id = %pin_id, "copied to clipboard");
                        notify::notify(
                            "Copied to clipboard",
                            Some("Image is now on the Wayland selection."),
                            Urgency::Normal,
                        )
                        .await;
                    }
                    Err(e) => {
                        tracing::warn!(pin_id = %pin_id, error = %e, "copy failed");
                        notify::notify("Copy failed", Some(&e.to_string()), Urgency::Critical)
                            .await;
                    }
                }
            },
            |()| Message::AsyncDone,
        )
    }

    fn spawn_save(&self, pin_id: PinId) -> Task<Message> {
        let image = match self.registry.image(pin_id) {
            Some(img) => img,
            None => return Task::none(),
        };
        let cfg = Arc::clone(&self.config);
        Task::perform(
            async move {
                match save::save_pin(image, cfg).await {
                    Ok(path) => {
                        tracing::info!(pin_id = %pin_id, path = %path.display(), "saved");
                        let body = format!("Saved to {}", path.display());
                        notify::notify("Saved", Some(&body), Urgency::Normal).await;
                    }
                    Err(e) => {
                        tracing::warn!(pin_id = %pin_id, error = %e, "save failed");
                        notify::notify("Save failed", Some(&e.to_string()), Urgency::Critical)
                            .await;
                    }
                }
            },
            |()| Message::AsyncDone,
        )
    }

    /// Apply a pure pixel transform, persist it back into the registry,
    /// rebuild the iced `Handle`, and (for rotations) flip the
    /// window's tracked aspect and resize.
    fn transform(
        &mut self,
        pin_id: PinId,
        swap_logical: bool,
        op: fn(&RgbaImage) -> RgbaImage,
    ) -> Task<Message> {
        let src = match self.registry.image(pin_id) {
            Some(img) => img,
            None => return Task::none(),
        };
        let new_img = Arc::new(op(&src));
        if !self
            .registry
            .replace_image(pin_id, Arc::clone(&new_img), swap_logical)
        {
            return Task::none();
        }
        // Rebuild the cached Handle so the renderer picks up new pixels
        // — see comment on `App::handles` for why the Handle id is
        // stable across redraws otherwise.
        self.handles.insert(
            pin_id,
            Handle::from_rgba(new_img.width(), new_img.height(), new_img.as_raw().clone()),
        );
        let window_id = match self.pin_to_window.get(&pin_id) {
            Some(id) => *id,
            None => return Task::none(),
        };
        if !swap_logical {
            // Flip: dimensions unchanged, just request a redraw via the
            // handle swap above.
            return Task::none();
        }
        let state = match self.windows.get_mut(&window_id) {
            Some(s) => s,
            None => return Task::none(),
        };
        if state.aspect > 0.0 {
            state.aspect = 1.0 / state.aspect;
        }
        let new_size = Size::new(state.last_size.height, state.last_size.width);
        state.last_size = new_size;
        window::resize(window_id, new_size)
    }

    /// Snap the window back to the image's aspect ratio after the
    /// compositor reports a new size. Returns a `window::resize` task
    /// only when the new size visibly drifts from the locked AR — a
    /// 0.5 px tolerance prevents the snap from feeding itself another
    /// `Resized` event.
    fn on_resized(&mut self, id: window::Id, size: Size) -> Task<Message> {
        let state = match self.windows.get_mut(&id) {
            Some(s) => s,
            None => return Task::none(),
        };
        let prev = state.last_size;
        state.last_size = size;
        let aspect = state.aspect;
        if aspect <= 0.0 || size.width <= 0.0 || size.height <= 0.0 {
            return Task::none();
        }
        // Pick the dimension the user dragged the most as the driver,
        // so the other axis adapts. Falls back to width-driven on the
        // first event (when prev == size).
        let dw = (size.width - prev.width).abs();
        let dh = (size.height - prev.height).abs();
        let target = if dh > dw {
            Size::new(size.height * aspect, size.height)
        } else {
            Size::new(size.width, size.width / aspect)
        };
        if (target.width - size.width).abs() < 0.5 && (target.height - size.height).abs() < 0.5 {
            return Task::none();
        }
        state.last_size = target;
        window::resize(id, target)
    }

    pub fn view(&self, window_id: window::Id) -> Element<'_, Message> {
        let pin_id = match self.window_to_pin.get(&window_id) {
            Some(id) => *id,
            None => {
                return container(iced::widget::text("(no pin)"))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into();
            }
        };
        let handle = match self.handles.get(&pin_id) {
            Some(h) => h.clone(),
            None => {
                return container(iced::widget::text("(pin gone)"))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into();
            }
        };
        image_widget(handle)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            window::close_events().map(Message::WindowClosed),
            Subscription::run(ipc_event_stream),
            event::listen_with(filter_event),
        ])
    }

    fn open_pin(&mut self, pin_id: PinId) -> Task<Message> {
        if self.pin_to_window.contains_key(&pin_id) {
            return Task::none();
        }
        let img = match self.registry.image(pin_id) {
            Some(img) => img,
            None => {
                tracing::warn!(pin_id = %pin_id, "open_pin for unknown id");
                return Task::none();
            }
        };
        // Window size is in compositor logical units. For captures the
        // registry holds the slurp region size; for clipboard pins (or
        // any path without logical info) we fall back to the image's
        // physical pixel dimensions.
        let (lw, lh) = self
            .registry
            .logical_size(pin_id)
            .unwrap_or_else(|| (img.width(), img.height()));
        let size = iced::Size::new(lw as f32, lh as f32);
        let settings = window::Settings {
            size,
            resizable: true,
            decorations: true,
            transparent: false,
            platform_specific: window::settings::PlatformSpecific {
                application_id: PIN_APP_ID.to_string(),
                ..Default::default()
            },
            ..window::Settings::default()
        };
        let (_, open_task) = window::open(settings);
        open_task.map(move |id| Message::WindowOpened { id, pin_id })
    }

    fn close_pin(&mut self, pin_id: PinId) -> Task<Message> {
        if let Some(id) = self.pin_to_window.remove(&pin_id) {
            self.window_to_pin.remove(&id);
            self.windows.remove(&id);
            self.handles.remove(&pin_id);
            tracing::debug!(pin_id = %pin_id, ?id, "closing window");
            return window::close(id);
        }
        Task::none()
    }

    fn close_all_pins(&mut self) -> Task<Message> {
        let ids: Vec<window::Id> = self.window_to_pin.keys().copied().collect();
        self.pin_to_window.clear();
        self.window_to_pin.clear();
        self.windows.clear();
        self.handles.clear();
        Task::batch(ids.into_iter().map(window::close))
    }
}

/// `fn`-pointer event filter for `event::listen_with`. Forwards the
/// resize/AR-snap signal plus our six keyboard shortcuts; everything
/// else is dropped. Window-id is supplied by the runtime, so per-window
/// state can be looked up in the update loop.
fn filter_event(event: Event, _status: event::Status, id: window::Id) -> Option<Message> {
    match event {
        Event::Window(window::Event::Resized(size)) => Some(Message::Resized { id, size }),
        Event::Keyboard(keyboard::Event::KeyPressed {
            ref key, modifiers, ..
        }) => key_to_action(key, modifiers).map(|action| Message::Action {
            window_id: id,
            action,
        }),
        _ => None,
    }
}

/// Map a keypress to a [`PinAction`].
///
/// Bindings:
/// - `Ctrl+C` → Copy, `Ctrl+S` → Save (Ctrl required, no Alt).
/// - `]` / `[` → rotate right / left (no Ctrl, no Alt; Shift ignored).
/// - `H` / `V` → flip horizontal / vertical (case-insensitive; no
///   Ctrl, no Alt).
///
/// Pin windows have no text input, so plain unmodified letters as
/// shortcuts won't conflict with anything — see Phase 9 plan.
fn key_to_action(key: &keyboard::Key, modifiers: keyboard::Modifiers) -> Option<PinAction> {
    let ch = match key {
        keyboard::Key::Character(c) => c.as_str(),
        _ => return None,
    };
    let ctrl_only = modifiers.control() && !modifiers.alt();
    let no_ctrl_alt = !modifiers.control() && !modifiers.alt();

    if ctrl_only {
        match ch.to_ascii_lowercase().as_str() {
            "c" => return Some(PinAction::Copy),
            "s" => return Some(PinAction::Save),
            _ => {}
        }
    }
    if no_ctrl_alt {
        match ch {
            "]" => return Some(PinAction::RotateRight),
            "[" => return Some(PinAction::RotateLeft),
            _ => {}
        }
        match ch.to_ascii_lowercase().as_str() {
            "h" => return Some(PinAction::FlipH),
            "v" => return Some(PinAction::FlipV),
            _ => {}
        }
    }
    None
}

/// Free `fn` (required by `Subscription::run`) that drains the
/// IPC→GUI receiver and feeds it into the iced runtime.
fn ipc_event_stream() -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(
        64,
        |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            let slot = match IPC_RX.get() {
                Some(s) => s,
                None => {
                    tracing::error!("IPC_RX queried before install_ipc_receiver");
                    return;
                }
            };
            let mut rx = match slot.lock() {
                Ok(mut g) => match g.take() {
                    Some(rx) => rx,
                    None => {
                        tracing::error!(
                            "ipc subscription stream restarted (receiver already taken)"
                        );
                        return;
                    }
                },
                Err(poisoned) => match poisoned.into_inner().take() {
                    Some(rx) => rx,
                    None => return,
                },
            };
            while let Some(event) = rx.recv().await {
                if output.send(Message::Event(event)).await.is_err() {
                    break;
                }
            }
        },
    )
}
