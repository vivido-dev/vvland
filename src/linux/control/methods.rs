//! Core control methods and the single-owner desktop actor.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crate::producer::TerminalInjector;
use crate::producer::keysynth::KeyStroke;
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cli::{
    MsgCommand, MsgKey, MsgLaunch, MsgMouseAction, MsgScreenshot, MsgSubscribe, MsgTyping,
    ScreenshotFormat,
};
use crate::control_cli::{
    IpcError, MAX_CHORD_MODIFIERS, MAX_CONNECTIONS, MAX_DRAG_STEPS, MAX_HOLD_MS,
    MAX_IN_FLIGHT_REQUESTS, MAX_INPUT_BYTES, MAX_KEY_REPEAT, MAX_REPLY_FRAME_BYTES,
    MAX_REQUEST_FRAME_BYTES, MAX_SCREENSHOT_INLINE_BYTES, MAX_SCROLL_UNITS, MAX_SUBSCRIBER_EVENTS,
    MAX_SUBSCRIPTIONS, MAX_TIMEOUT_MS, MIN_TIMEOUT_MS, PROTOCOL_VERSION, SCROLL_UNITS_PER_DETENT,
    validate_message,
};
use crate::linux::compositor::ResolvedCompositor;
use crate::linux::compositor::check_pointer_bounds;
use crate::linux::compositor::sway::query_windows;
use crate::linux::host::DesktopHost;
use crate::linux::pipeline::{self, PresenterSource};

use super::screenshot;
use super::watch::{ScreenSnapshot, ScreenWatch, WatchLease};
use super::{
    ActorRequest, AttachParams, ControlContext, ERROR_CODES, EVENT_KINDS, EventSendError,
    EventSink, HostInputCall, METHODS, PresenterInput, Responder, SWAY_METHODS,
};

const ACTOR_TICK: Duration = Duration::from_millis(5);
const MAX_PENDING_WAITS: usize = MAX_IN_FLIGHT_REQUESTS;
const CAPTURE_STALL: Duration = Duration::from_secs(2);
const EVENT_REPLAY: usize = MAX_SUBSCRIBER_EVENTS;
const WINDOW_POLL: Duration = Duration::from_millis(100);

pub struct Actor {
    host: DesktopHost,
    context: ControlContext,
    requests: Receiver<ActorRequest>,
    stopping: Arc<AtomicBool>,
    input_live: bool,
    exit_status: Option<ExitStatus>,
    exit_waiters: Vec<ExitWaiter>,
    key_holds: Vec<KeyHold>,
    drags: Vec<PendingDrag>,
    wait_workers: Arc<AtomicUsize>,
    watch: ScreenWatch,
    last_watch: Option<ScreenSnapshot>,
    capture_stalled: bool,
    subscribers: Vec<Subscriber>,
    replay: VecDeque<StoredEvent>,
    event_sequence: u64,
    next_subscription_id: u64,
    presenter: Option<Presenter>,
}

struct Presenter {
    connection_id: u64,
    connection_alive: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    ready: Receiver<io::Result<Value>>,
    done: Receiver<Result<(), String>>,
    state: Arc<Mutex<Value>>,
    join: Option<thread::JoinHandle<()>>,
    input: Receiver<HostInputCall>,
    response: Option<Responder>,
    announced: bool,
}

struct ExitWaiter {
    deadline: Instant,
    response: Responder,
}

struct KeyHold {
    deadline: Instant,
    stroke: KeyStroke,
    response: Responder,
}

struct PendingDrag {
    button: u32,
    from: (u32, u32),
    to: (u32, u32),
    steps: u16,
    next_step: u16,
    interval: Duration,
    next_at: Instant,
    response: Responder,
}

#[derive(Clone)]
struct StoredEvent {
    sequence: u64,
    kind: String,
    data: Value,
}

struct Subscriber {
    id: u64,
    kinds: HashSet<String>,
    sink: EventSink,
    overflow: Option<(u64, u64)>,
    _watch: WatchLease,
}

impl Subscriber {
    fn matches(&self, kind: &str) -> bool {
        self.kinds.contains(kind)
    }

    fn send(&mut self, event: &StoredEvent) {
        if let Some((first, last)) = self.overflow {
            let overflow = crate::control_cli::SubscriptionEventEnvelope {
                version: PROTOCOL_VERSION,
                subscription_id: self.id,
                event_sequence: last,
                window_id: None,
                event: json!({
                    "type": "overflow",
                    "data": {
                        "first_dropped_sequence": first,
                        "last_dropped_sequence": last,
                    }
                }),
            };
            if self.sink.send(overflow).is_err() {
                self.overflow = Some((first, event.sequence));
                return;
            }
            self.overflow = None;
        }
        if !self.matches(&event.kind) {
            return;
        }
        let envelope = crate::control_cli::SubscriptionEventEnvelope {
            version: PROTOCOL_VERSION,
            subscription_id: self.id,
            event_sequence: event.sequence,
            window_id: None,
            event: json!({"type": event.kind, "data": event.data}),
        };
        if let Err(error) = self.sink.send(envelope) {
            match error {
                EventSendError::Full => {
                    self.overflow = Some(match self.overflow {
                        Some((first, _)) => (first, event.sequence),
                        None => (event.sequence, event.sequence),
                    });
                }
                EventSendError::Closed => {}
            }
        }
    }
}

impl Actor {
    pub fn new(
        host: DesktopHost,
        context: ControlContext,
        requests: Receiver<ActorRequest>,
        stopping: Arc<AtomicBool>,
    ) -> Self {
        let watch = ScreenWatch::new(host.latest().clone());
        let audio_disabled = !host.audio_enabled();
        let mut replay = VecDeque::new();
        if audio_disabled {
            replay.push_back(StoredEvent {
                sequence: 1,
                kind: "audio_disabled".into(),
                data: json!({}),
            });
        }
        Self {
            host,
            context,
            requests,
            stopping,
            input_live: true,
            exit_status: None,
            exit_waiters: Vec::new(),
            key_holds: Vec::new(),
            drags: Vec::new(),
            wait_workers: Arc::new(AtomicUsize::new(0)),
            watch,
            last_watch: None,
            capture_stalled: false,
            subscribers: Vec::new(),
            replay,
            event_sequence: u64::from(audio_disabled),
            next_subscription_id: 0,
            presenter: None,
        }
    }

    pub fn run(mut self) -> io::Result<()> {
        while !self.stopping.load(Ordering::Acquire) {
            match self.requests.recv_timeout(ACTOR_TICK) {
                Ok(request) => self.dispatch(request),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.tick()?;
            if self.exit_status.is_some() {
                break;
            }
        }
        self.finish_pending();
        Ok(())
    }

    fn dispatch(&mut self, request: ActorRequest) {
        let ActorRequest {
            method,
            params,
            attach,
            response,
        } = request;
        let result = match method.as_str() {
            "hello" | "capabilities" => expect_empty(&params).map(|()| self.capabilities()),
            "ping" => expect_empty(&params).map(|()| json!({})),
            "inspect" => expect_empty(&params).and_then(|()| self.inspect()),
            "attach" => return self.attach(attach, response),
            "presenter_input" => return self.presenter_input(params, response),
            "presenter_ping" => return self.presenter_ping(params, response),
            "presenter_run" => return self.presenter_run(params, response),
            "key" => return self.key(params, response),
            "typing" => self.typing(params).map(|()| json!({})),
            "mouse" => return self.mouse(params, response),
            "launch" => self.launch(params).map(|()| json!({})),
            "list_windows" => return self.list_windows(params, response),
            "screenshot" => return self.screenshot(params, response),
            "wait_frame" => return self.wait_frame(params, response),
            "wait_screen_change" => {
                return self.wait_screen_change(params, response);
            }
            "wait_screen_stable" => {
                return self.wait_screen_stable(params, response);
            }
            "wait_exit" => return self.wait_exit(params, response),
            "wait_window" => return self.wait_window(params, response),
            "subscribe" => return self.subscribe(params, response),
            "shutdown" => {
                let result = expect_empty(&params).map(|()| json!({}));
                if result.is_ok() {
                    self.stopping.store(true, Ordering::Release);
                }
                result
            }
            _ => Err(IpcError::new(
                "unsupported",
                format!("unknown control method {method:?}"),
            )),
        };
        complete(response, result);
    }

    fn capabilities(&self) -> Value {
        let (width, height) = self.host.dimensions();
        let resolved = self.host.resolved();
        let mut capabilities = json!({
            "protocol_version": PROTOCOL_VERSION,
            "server": {"name": "vvland", "version": env!("CARGO_PKG_VERSION")},
            "session": self.context.session,
            "methods": methods_for(resolved),
            "event_kinds": EVENT_KINDS,
            "error_codes": ERROR_CODES,
            "limits": capability_limits(width, height),
            "key": {
                "grammar": ["code:N", "unicode_scalar", "xkb_keysym", "evdev_name"],
                "modifiers": ["ctrl", "alt", "shift", "super", "altgr", "caps", "num"],
            },
            "input": {"acknowledgement": "flushed"},
            "launch": {"acknowledgement": "spawned"},
            "screenshot": {
                "formats": ["png", "jpeg", "raw"],
                "scales": ["1", "1/2", "1/4"],
                "staleness": "reported; optional max_age_ms rejection",
                "inline_encoding": "base64",
            },
            "screen_hash": {
                "algorithm": "fnv1a64",
                "sampling": "1/16",
                "exact_available": true,
            }
        });
        if resolved == ResolvedCompositor::Sway {
            capabilities["windows"] = json!({
                "enumeration": true,
                "wait_match": "exact_app_id",
                "compositor": "sway",
            });
        }
        capabilities
    }

    fn inspect(&mut self) -> Result<Value, IpcError> {
        let (width, height) = self.host.dimensions();
        let capture = self
            .host
            .latest()
            .snapshot_with_age()
            .map_err(capture_error)?;
        let capture_value = match capture {
            Some((serial, frame, age)) => json!({
                "live": true,
                "frame_serial": serial,
                "frame_age_ms": millis(age),
                "format": raw_format(frame.format),
            }),
            None => json!({"live": true, "frame_serial": 0, "frame_age_ms": null, "format": null}),
        };
        let (pressed_keys, pressed_buttons) = self.host.pressed_counts();
        let pointer = self.host.last_pointer().map(|(x, y)| [x, y]);
        let audio = match self.host.audio_sink_name() {
            Some(sink) => json!({"sink": sink.to_string_lossy(), "live": true}),
            None => json!({"sink": null, "live": false}),
        };
        let resolved = self.host.resolved();
        Ok(json!({
            "session": self.context.session,
            "compositor": match resolved {
                crate::linux::compositor::ResolvedCompositor::Weston => "weston",
                crate::linux::compositor::ResolvedCompositor::Sway => "sway",
            },
            "backend": self.host.backend_name(),
            "wire_name": resolved.wire_name(),
            "width": width,
            "height": height,
            "fps": self.context.fps,
            "capture": capture_value,
            "input": {
                "live": self.input_live,
                "pressed_keys": pressed_keys,
                "pressed_buttons": pressed_buttons,
                "pointer": pointer,
            },
            "audio": audio,
            "app": self.context.app,
            "launches": self.host.launches(),
            "compositor_pid": self.host.compositor_pid(),
            "uptime_ms": millis(self.host.uptime()),
            "xkb": {
                "model": self.context.xkb_model.as_deref().unwrap_or("pc105"),
                "layout": self.context.xkb_layout,
                "variant": self.context.xkb_variant,
                "options": self.context.xkb_options,
            },
            "presenter": self.presenter.as_ref().map_or_else(
                || json!({"attached": false}),
                |presenter| lock(&presenter.state).clone(),
            )
        }))
    }

    fn attach(&mut self, params: Option<AttachParams>, response: Responder) {
        let Some(params) = params else {
            return response.error(IpcError::new(
                "invalid_params",
                "attach parameters are required",
            ));
        };
        if params.fps == 0 || params.fps > 240 {
            return response.error(IpcError::new(
                "invalid_params",
                "fps must be between 1 and 240",
            ));
        }
        if !(64_000..=200_000_000).contains(&params.bitrate) {
            return response.error(IpcError::new(
                "invalid_params",
                "bitrate must be between 64000 and 200000000",
            ));
        }
        if self.presenter.is_some() {
            if !params.replace {
                return response.error(IpcError::new(
                    "invalid_state",
                    "a presenter is already attached; use replace to disconnect it",
                ));
            }
            self.stop_presenter();
        }
        let pulse = self.host.pulse().map(|pulse| {
            (
                pulse.monitor_name().to_owned(),
                pulse.server().to_os_string(),
            )
        });
        let source = PresenterSource {
            latest: self.host.latest().clone(),
            pulse,
            dimensions: self.host.dimensions(),
            compositor: self.host.resolved(),
            origin: self.host.origin(),
        };
        let connection_id = response.connection_id();
        let connection_alive = response.connection_alive();
        let (input_tx, input) = std::sync::mpsc::sync_channel(256);
        match pipeline::spawn_presenter(self.context.config.clone(), params, source, input_tx) {
            Ok(spawn) => {
                self.presenter = Some(Presenter {
                    connection_id,
                    connection_alive,
                    running: spawn.running,
                    ready: spawn.ready,
                    done: spawn.done,
                    state: spawn.state,
                    join: Some(spawn.join),
                    input,
                    response: Some(response),
                    announced: false,
                });
            }
            Err(error) => response.error(IpcError::new("invalid_state", error.to_string())),
        }
    }

    fn presenter_input(&mut self, params: Value, response: Responder) {
        if !self.presenter_owner(&response) {
            return response.error(IpcError::new(
                "invalid_state",
                "connection does not own the presenter",
            ));
        }
        let action: PresenterInput = match parse_params(params) {
            Ok(action) => action,
            Err(error) => return response.error(error),
        };
        complete(
            response,
            self.apply_presenter_input(action).map(|()| json!({})),
        );
    }

    fn presenter_ping(&mut self, params: Value, response: Responder) {
        if let Err(error) = expect_empty(&params) {
            return response.error(error);
        }
        if !self.presenter_owner(&response) {
            return response.error(IpcError::new(
                "invalid_state",
                "connection does not own the presenter",
            ));
        }
        let state = self
            .presenter
            .as_ref()
            .map(|presenter| lock(&presenter.state).clone())
            .unwrap_or_else(|| json!({"attached": false}));
        response.success(state);
    }

    fn presenter_run(&mut self, params: Value, response: Responder) {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            command: String,
        }
        if !self.presenter_owner(&response) {
            return response.error(IpcError::new(
                "invalid_state",
                "connection does not own the presenter",
            ));
        }
        let params: Params = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return response.error(error),
        };
        if params.command.is_empty()
            || params.command.len() > MAX_INPUT_BYTES
            || params.command.contains(['\n', '\r'])
        {
            return response.error(IpcError::new(
                "invalid_params",
                "command must be a non-empty single line",
            ));
        }
        complete(
            response,
            self.host
                .launch(&[OsString::from(params.command)], true)
                .map(|()| json!({}))
                .map_err(input_error),
        );
    }

    fn presenter_owner(&self, response: &Responder) -> bool {
        self.presenter
            .as_ref()
            .is_some_and(|presenter| presenter.connection_id == response.connection_id())
    }

    fn apply_presenter_input(&mut self, action: PresenterInput) -> Result<(), IpcError> {
        match action {
            PresenterInput::Key { code, pressed } if (1..=0x2ff).contains(&code) => {
                self.host.input().key(code, pressed).map_err(input_error)
            }
            PresenterInput::Key { .. } => Err(IpcError::new(
                "invalid_params",
                "key code exceeds the Linux input range",
            )),
            PresenterInput::PointerAbsolute { x, y } => {
                self.check_position(x, y)?;
                self.host.pointer_move(x, y).map_err(input_error)
            }
            PresenterInput::PointerButton { button, pressed }
                if (0x100..=0x2ff).contains(&button) =>
            {
                self.host
                    .pointer_button(button, pressed)
                    .map_err(input_error)
            }
            PresenterInput::PointerButton { .. } => Err(IpcError::new(
                "invalid_params",
                "button code exceeds the Linux input range",
            )),
            PresenterInput::PointerAxis { axis, delta }
                if axis <= 1 && delta.unsigned_abs() <= MAX_SCROLL_UNITS as u32 =>
            {
                self.host.pointer_axis(axis, delta).map_err(input_error)
            }
            PresenterInput::PointerAxis { .. } => Err(IpcError::new(
                "invalid_params",
                "pointer axis or delta is outside the advertised limits",
            )),
            PresenterInput::ReleaseAll => self.host.input().release_all().map_err(input_error),
        }
    }

    fn key(&mut self, params: Value, response: Responder) {
        if !self.key_holds.is_empty() {
            return response.error(IpcError::new(
                "invalid_state",
                "another held key is still pending",
            ));
        }
        let params: MsgKey = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return response.error(error),
        };
        if let Err(error) = validate_message(&MsgCommand::Key(params.clone())) {
            return response.error(invalid_params(error));
        }
        let stroke = match self.host.resolve_key(&params.key, &params.mods) {
            Ok(stroke) => stroke,
            Err(error) => {
                return response.error(
                    IpcError::new("key_not_in_keymap", error.to_string())
                        .with_data(json!({"key": params.key, "layout": self.context.xkb_layout})),
                );
            }
        };
        if let Some(hold_ms) = params.hold_ms {
            if let Err(error) = self.host.press_key(&stroke) {
                return response.error(input_error(error));
            }
            self.key_holds.push(KeyHold {
                deadline: Instant::now() + Duration::from_millis(hold_ms),
                stroke,
                response,
            });
            return;
        }
        for _ in 0..params.repeat {
            if let Err(error) = self.host.tap_key(&stroke) {
                return response.error(input_error(error));
            }
        }
        response.success(json!({}));
    }

    fn typing(&mut self, params: Value) -> Result<(), IpcError> {
        if !self.key_holds.is_empty() {
            return Err(IpcError::new(
                "invalid_state",
                "another held key is still pending",
            ));
        }
        let params: MsgTyping = parse_params(params)?;
        validate_message(&MsgCommand::Typing(params.clone())).map_err(invalid_params)?;
        self.host.type_text(&params.text).map_err(|error| {
            if error.kind() == io::ErrorKind::InvalidInput {
                IpcError::new("key_not_in_keymap", error.to_string())
                    .with_data(json!({"layout": self.context.xkb_layout}))
            } else {
                input_error(error)
            }
        })
    }

    fn mouse(&mut self, params: Value, response: Responder) {
        if !self.drags.is_empty() {
            return response.error(IpcError::new(
                "invalid_state",
                "another pointer drag is still pending",
            ));
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct MouseParams {
            action: MsgMouseAction,
        }
        let params: MouseParams = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return response.error(error),
        };
        if let Err(error) = validate_message(&MsgCommand::Mouse {
            action: params.action.clone(),
        }) {
            return response.error(invalid_params(error));
        }
        use MsgMouseAction::*;
        let result = match params.action {
            Move { x, y } => self.move_pointer(x, y),
            Click {
                button,
                x,
                y,
                count,
            } => self.at_optional_position(x, y).and_then(|position| {
                for _ in 0..count {
                    self.host
                        .pointer_button(button.evdev_code(), true)
                        .map_err(input_error)?;
                    self.host
                        .pointer_button(button.evdev_code(), false)
                        .map_err(input_error)?;
                }
                Ok(position)
            }),
            Down { button, x, y } => self.at_optional_position(x, y).and_then(|position| {
                self.host
                    .pointer_button(button.evdev_code(), true)
                    .map_err(input_error)?;
                Ok(position)
            }),
            Up { button, x, y } => self.at_optional_position(x, y).and_then(|position| {
                self.host
                    .pointer_button(button.evdev_code(), false)
                    .map_err(input_error)?;
                Ok(position)
            }),
            Scroll {
                vertical,
                horizontal,
                x,
                y,
            } => self.at_optional_position(x, y).and_then(|position| {
                if vertical != 0 {
                    self.host
                        .pointer_axis(0, vertical * SCROLL_UNITS_PER_DETENT)
                        .map_err(input_error)?;
                }
                if horizontal != 0 {
                    self.host
                        .pointer_axis(1, horizontal * SCROLL_UNITS_PER_DETENT)
                        .map_err(input_error)?;
                }
                Ok(position)
            }),
            Drag {
                button,
                from_x,
                from_y,
                to_x,
                to_y,
                steps,
                step_ms,
            } => {
                if let Err(error) = self.move_pointer(from_x, from_y).and_then(|_| {
                    self.check_position(to_x, to_y)?;
                    self.host
                        .pointer_button(button.evdev_code(), true)
                        .map_err(input_error)
                }) {
                    return response.error(error);
                }
                self.drags.push(PendingDrag {
                    button: button.evdev_code(),
                    from: (from_x, from_y),
                    to: (to_x, to_y),
                    steps,
                    next_step: 1,
                    interval: Duration::from_millis(u64::from(step_ms)),
                    next_at: Instant::now() + Duration::from_millis(u64::from(step_ms)),
                    response,
                });
                return;
            }
        };
        complete(response, result.map(|(x, y)| json!({"x": x, "y": y})));
    }

    fn launch(&mut self, params: Value) -> Result<(), IpcError> {
        let params: MsgLaunch = parse_params(params)?;
        validate_message(&MsgCommand::Launch(params.clone())).map_err(invalid_params)?;
        let program: Vec<OsString> = params.program.into_iter().map(Into::into).collect();
        let result = self
            .host
            .launch(&program, params.shell)
            .map_err(|error| IpcError::new("launch_failed", error.to_string()));
        match &result {
            Ok(()) => self.emit("launch_started", json!({"launches": self.host.launches()})),
            Err(error) => self.emit("launch_failed", json!({"code": error.code})),
        }
        result
    }

    fn list_windows(&mut self, params: Value, response: Responder) {
        if let Err(error) = expect_empty(&params) {
            return response.error(error);
        }
        let Some(socket) = self.host.sway_ipc_socket() else {
            return response.error(window_unsupported());
        };
        self.spawn_worker("vvland-list-windows", response, move || {
            let windows = query_windows(&socket).map_err(window_query_error)?;
            Ok(json!({"windows": windows}))
        });
    }

    fn screenshot(&mut self, params: Value, response: Responder) {
        let params: MsgScreenshot = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return response.error(error),
        };
        if let Err(error) = validate_message(&MsgCommand::Screenshot(params.clone())) {
            return response.error(invalid_params(error));
        }
        let latest = self.host.latest().clone();
        self.spawn_worker("vvland-screenshot", response, move || {
            if params.fresh {
                let mut serial = latest
                    .snapshot()
                    .map_err(capture_error)?
                    .map_or(0, |(serial, _)| serial);
                match latest
                    .wait_next(&mut serial, Duration::from_millis(params.timeout_ms))
                    .map_err(capture_error)?
                {
                    Some(_) => {}
                    None => return Err(IpcError::new("timeout", "fresh screenshot timed out")),
                }
            }
            let (frame_serial, frame, age) = latest
                .snapshot_with_age()
                .map_err(capture_error)?
                .ok_or_else(|| {
                    IpcError::new("desktop_not_ready", "no desktop frame has been captured")
                })?;
            if params
                .max_age_ms
                .is_some_and(|maximum| millis(age) > maximum)
            {
                return Err(IpcError::new(
                    "capture_stalled",
                    "retained screenshot frame is too old",
                )
                .with_data(json!({
                    "frame_age_ms": millis(age),
                    "max_age_ms": params.max_age_ms,
                })));
            }
            let encoded = screenshot::encode(&frame, params.format, params.scale, params.quality)
                .map_err(|error| IpcError::new("encode_failed", error.to_string()))?;
            let format = screenshot_format(encoded.format);
            let mut result = json!({
                "width": encoded.width,
                "height": encoded.height,
                "format": format,
                "pixel_format": encoded.pixel_format,
                "frame_serial": frame_serial,
                "frame_age_ms": millis(age),
                "bytes": encoded.bytes.len(),
            });
            if let Some(path) = params.output {
                let path = screenshot::write_output(&path, &encoded.bytes)
                    .map_err(|error| IpcError::new("encode_failed", error.to_string()))?;
                result["path"] = Value::String(path.to_string_lossy().into_owned());
            } else {
                let byte_len = encoded.bytes.len();
                ensure_inline_size(byte_len)?;
                let data = base64::engine::general_purpose::STANDARD.encode(encoded.bytes);
                if data.len() + 4096 > MAX_REPLY_FRAME_BYTES {
                    return Err(inline_limit_error(byte_len));
                }
                result["data"] = Value::String(data);
            }
            Ok(result)
        });
    }

    fn wait_frame(&mut self, params: Value, response: Responder) {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            after_frame: Option<u64>,
            timeout_ms: u64,
        }
        let params: Params = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return response.error(error),
        };
        if let Err(error) = validate_timeout(params.timeout_ms) {
            return response.error(error);
        }
        if self.wait_workers.load(Ordering::Acquire) >= MAX_PENDING_WAITS {
            return response.error(IpcError::new("limit_exceeded", "too many pending waits"));
        }
        let latest = self.host.latest().clone();
        let mut serial = match params.after_frame {
            Some(serial) => serial,
            None => match latest.snapshot() {
                Ok(Some((serial, _))) => serial,
                Ok(None) => 0,
                Err(error) => return response.error(capture_error(error)),
            },
        };
        let workers = self.wait_workers.clone();
        let response_slot = Arc::new(Mutex::new(Some(response)));
        let worker_response = response_slot.clone();
        workers.fetch_add(1, Ordering::AcqRel);
        let spawn = thread::Builder::new()
            .name("vvland-wait-frame".into())
            .spawn(move || {
                let response = worker_response
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                    .expect("wait worker owns its response");
                let result =
                    match latest.wait_next(&mut serial, Duration::from_millis(params.timeout_ms)) {
                        Ok(Some(_)) => Ok(json!({"frame_serial": serial})),
                        Ok(None) => Err(IpcError::new("timeout", "wait_frame timed out")),
                        Err(error) => Err(capture_error(error)),
                    };
                complete(response, result);
                workers.fetch_sub(1, Ordering::AcqRel);
            });
        if let Err(error) = spawn {
            self.wait_workers.fetch_sub(1, Ordering::AcqRel);
            if let Some(response) = response_slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                response.error(IpcError::new(
                    "limit_exceeded",
                    format!("could not start wait worker: {error}"),
                ));
            }
        }
    }

    fn wait_screen_change(&mut self, params: Value, response: Responder) {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            after_screen: Option<u64>,
            #[serde(default)]
            exact: bool,
            timeout_ms: u64,
        }
        let params: Params = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return response.error(error),
        };
        if let Err(error) = validate_timeout(params.timeout_ms) {
            return response.error(error);
        }
        let watch = self.watch.clone();
        self.spawn_worker("vvland-wait-screen-change", response, move || {
            match watch
                .wait_change(
                    params.exact,
                    params.after_screen,
                    Duration::from_millis(params.timeout_ms),
                )
                .map_err(capture_error)?
            {
                Some(snapshot) => Ok(screen_result(snapshot)),
                None => Err(IpcError::new("timeout", "wait_screen_change timed out")),
            }
        });
    }

    fn wait_screen_stable(&mut self, params: Value, response: Responder) {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            quiet_ms: u64,
            after_screen: Option<u64>,
            #[serde(default)]
            exact: bool,
            timeout_ms: u64,
        }
        let params: Params = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return response.error(error),
        };
        if let Err(error) =
            validate_timeout(params.timeout_ms).and_then(|()| validate_timeout(params.quiet_ms))
        {
            return response.error(error);
        }
        let watch = self.watch.clone();
        self.spawn_worker("vvland-wait-screen-stable", response, move || {
            match watch
                .wait_stable(
                    params.exact,
                    Duration::from_millis(params.quiet_ms),
                    params.after_screen,
                    Duration::from_millis(params.timeout_ms),
                )
                .map_err(capture_error)?
            {
                Some(snapshot) => {
                    let mut result = screen_result(snapshot);
                    result["stable_for_ms"] = Value::from(params.quiet_ms);
                    Ok(result)
                }
                None => Err(IpcError::new("timeout", "wait_screen_stable timed out")),
            }
        });
    }

    fn wait_window(&mut self, params: Value, response: Responder) {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            app_id: String,
            timeout_ms: u64,
        }
        let params: Params = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return response.error(error),
        };
        if let Err(error) = validate_message(&MsgCommand::Wait {
            condition: crate::cli::MsgWaitCondition::Window {
                app_id: params.app_id.clone(),
                timeout_ms: params.timeout_ms,
            },
        }) {
            return response.error(invalid_params(error));
        }
        let Some(socket) = self.host.sway_ipc_socket() else {
            return response.error(window_unsupported());
        };
        self.spawn_worker("vvland-wait-window", response, move || {
            let deadline = Instant::now() + Duration::from_millis(params.timeout_ms);
            loop {
                let windows = query_windows(&socket).map_err(window_query_error)?;
                if let Some(window) = windows
                    .into_iter()
                    .find(|window| window.app_id.as_deref() == Some(params.app_id.as_str()))
                {
                    return Ok(json!({"window": window}));
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(IpcError::new("timeout", "wait_window timed out")
                        .with_data(json!({"app_id": params.app_id})));
                }
                thread::sleep(remaining.min(WINDOW_POLL));
            }
        });
    }

    fn subscribe(&mut self, params: Value, response: Responder) {
        let params: MsgSubscribe = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return response.error(error),
        };
        if let Err(error) = validate_message(&MsgCommand::Subscribe(params.clone())) {
            return response.error(invalid_params(error));
        }
        if self.subscribers.len() >= MAX_SUBSCRIPTIONS {
            return response.error(IpcError::new(
                "limit_exceeded",
                "at most 32 subscriptions may be active",
            ));
        }
        if params
            .since_event
            .is_some_and(|sequence| sequence > self.event_sequence)
        {
            return response.error(IpcError::new(
                "invalid_params",
                "since_event is newer than the current event sequence",
            ));
        }
        let watch = match self.watch.acquire(false) {
            Ok(watch) => watch,
            Err(error) => return response.error(capture_error(error)),
        };
        let kinds = subscription_kinds(&params.events);
        self.next_subscription_id = self.next_subscription_id.saturating_add(1);
        let mut subscriber = Subscriber {
            id: self.next_subscription_id,
            kinds,
            sink: response.event_sink(),
            overflow: None,
            _watch: watch,
        };
        response.success(json!({
            "subscription_id": subscriber.id,
            "event_sequence": self.event_sequence,
        }));
        if let Some(since) = params.since_event {
            let oldest = self
                .replay
                .front()
                .map_or(self.event_sequence.saturating_add(1), |event| {
                    event.sequence
                });
            if since.saturating_add(1) < oldest {
                let gap = StoredEvent {
                    sequence: self.event_sequence,
                    kind: "overflow".into(),
                    data: json!({
                        "requested_sequence": since,
                        "oldest_sequence": oldest,
                        "current_event_sequence": self.event_sequence,
                    }),
                };
                subscriber.send(&gap);
            } else {
                for event in self.replay.iter().filter(|event| event.sequence > since) {
                    subscriber.send(event);
                }
            }
        }
        self.subscribers.push(subscriber);
    }

    fn wait_exit(&mut self, params: Value, response: Responder) {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            timeout_ms: u64,
        }
        let params: Params = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return response.error(error),
        };
        if let Err(error) = validate_timeout(params.timeout_ms) {
            return response.error(error);
        }
        if let Some(status) = self.exit_status {
            return response.success(exit_result(status));
        }
        if self.exit_waiters.len() >= MAX_PENDING_WAITS {
            return response.error(IpcError::new("limit_exceeded", "too many pending waits"));
        }
        self.exit_waiters.push(ExitWaiter {
            deadline: Instant::now() + Duration::from_millis(params.timeout_ms),
            response,
        });
    }

    fn spawn_worker<F>(&mut self, name: &str, response: Responder, work: F)
    where
        F: FnOnce() -> Result<Value, IpcError> + Send + 'static,
    {
        if self.wait_workers.load(Ordering::Acquire) >= MAX_PENDING_WAITS {
            return response.error(IpcError::new("limit_exceeded", "too many pending workers"));
        }
        let workers = self.wait_workers.clone();
        let response_slot = Arc::new(Mutex::new(Some(response)));
        let worker_response = response_slot.clone();
        workers.fetch_add(1, Ordering::AcqRel);
        let spawn = thread::Builder::new().name(name.into()).spawn(move || {
            let response = worker_response
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
                .expect("worker owns its response");
            complete(response, work());
            workers.fetch_sub(1, Ordering::AcqRel);
        });
        if let Err(error) = spawn {
            self.wait_workers.fetch_sub(1, Ordering::AcqRel);
            if let Some(response) = response_slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                response.error(IpcError::new(
                    "limit_exceeded",
                    format!("could not start worker: {error}"),
                ));
            }
        }
    }

    fn emit(&mut self, kind: &str, data: Value) {
        self.event_sequence = self.event_sequence.saturating_add(1);
        let event = StoredEvent {
            sequence: self.event_sequence,
            kind: kind.to_owned(),
            data,
        };
        self.replay.push_back(event.clone());
        while self.replay.len() > EVENT_REPLAY {
            self.replay.pop_front();
        }
        for subscriber in &mut self.subscribers {
            subscriber.send(&event);
        }
        self.subscribers
            .retain(|subscriber| subscriber.sink.is_alive());
    }

    fn tick_watch(&mut self) {
        if self.subscribers.is_empty() {
            self.last_watch = None;
            return;
        }
        let snapshot = match self.watch.snapshot(false) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) | Err(_) => return,
        };
        let previous = self.last_watch.replace(snapshot);
        if self
            .subscribers
            .iter()
            .any(|subscriber| subscriber.matches("frame_captured"))
            && previous.is_none_or(|previous| previous.frame_serial != snapshot.frame_serial)
        {
            self.emit(
                "frame_captured",
                json!({
                    "frame_serial": snapshot.frame_serial,
                    "screen_sequence": snapshot.screen_sequence,
                }),
            );
        }
        if previous.is_none_or(|previous| previous.screen_sequence != snapshot.screen_sequence) {
            self.emit("screen_changed", screen_result(snapshot));
        }
    }

    fn tick_capture_health(&mut self) {
        let age = self
            .host
            .latest()
            .snapshot_with_age()
            .ok()
            .flatten()
            .map(|(_, _, age)| age)
            .unwrap_or_else(|| self.host.uptime());
        let stalled = age >= CAPTURE_STALL;
        if stalled != self.capture_stalled {
            self.capture_stalled = stalled;
            self.emit(
                if stalled {
                    "capture_stalled"
                } else {
                    "capture_resumed"
                },
                json!({"frame_age_ms": millis(age)}),
            );
        }
    }

    fn move_pointer(&mut self, x: u32, y: u32) -> Result<(u32, u32), IpcError> {
        self.check_position(x, y)?;
        self.host.pointer_move(x, y).map_err(input_error)?;
        Ok((x, y))
    }

    fn check_position(&self, x: u32, y: u32) -> Result<(), IpcError> {
        let (width, height) = self.host.dimensions();
        check_pointer_bounds(x, y, width, height, "vvland").map_err(|error| {
            IpcError::new("coordinate_out_of_range", error.to_string())
                .with_data(json!({"x": x, "y": y, "width": width, "height": height}))
        })
    }

    fn at_optional_position(
        &mut self,
        x: Option<u32>,
        y: Option<u32>,
    ) -> Result<(u32, u32), IpcError> {
        match (x, y) {
            (Some(x), Some(y)) => self.move_pointer(x, y),
            (None, None) => self.host.last_pointer().ok_or_else(|| {
                IpcError::new(
                    "invalid_params",
                    "no prior pointer position; supply both x and y",
                )
            }),
            _ => Err(IpcError::new(
                "invalid_params",
                "x and y must be supplied together",
            )),
        }
    }

    fn tick(&mut self) -> io::Result<()> {
        let now = Instant::now();
        self.tick_presenter();
        self.subscribers
            .retain(|subscriber| subscriber.sink.is_alive());
        self.tick_watch();
        self.tick_capture_health();
        let mut index = 0;
        while index < self.key_holds.len() {
            if self.key_holds[index].deadline > now {
                index += 1;
                continue;
            }
            let hold = self.key_holds.swap_remove(index);
            complete(
                hold.response,
                self.host
                    .release_key(&hold.stroke)
                    .map(|()| json!({}))
                    .map_err(input_error),
            );
        }

        let mut index = 0;
        while index < self.drags.len() {
            if self.drags[index].next_at > now {
                index += 1;
                continue;
            }
            let drag = &self.drags[index];
            let step = u32::from(drag.next_step);
            let steps = u32::from(drag.steps);
            let x = interpolate(drag.from.0, drag.to.0, step, steps);
            let y = interpolate(drag.from.1, drag.to.1, step, steps);
            let move_result = self.host.pointer_move(x, y).map_err(input_error);
            if let Err(error) = move_result {
                let drag = self.drags.swap_remove(index);
                let _ = self.host.pointer_button(drag.button, false);
                drag.response.error(error);
                continue;
            }
            if self.drags[index].next_step == self.drags[index].steps {
                let drag = self.drags.swap_remove(index);
                complete(
                    drag.response,
                    self.host
                        .pointer_button(drag.button, false)
                        .map(|()| json!({"x": drag.to.0, "y": drag.to.1}))
                        .map_err(input_error),
                );
            } else {
                self.drags[index].next_step += 1;
                self.drags[index].next_at = now + self.drags[index].interval;
                index += 1;
            }
        }

        let mut index = 0;
        while index < self.exit_waiters.len() {
            if self.exit_waiters[index].deadline > now {
                index += 1;
            } else {
                self.exit_waiters
                    .swap_remove(index)
                    .response
                    .error(IpcError::new("timeout", "wait_exit timed out"));
            }
        }

        self.input_live = self.host.input_status().is_ok();
        if let Some(status) = self.host.liveness()? {
            self.emit("compositor_exited", exit_result(status));
            self.exit_status = Some(status);
            for waiter in self.exit_waiters.drain(..) {
                waiter.response.success(exit_result(status));
            }
        }
        Ok(())
    }

    fn tick_presenter(&mut self) {
        let Some(mut presenter) = self.presenter.take() else {
            return;
        };
        while let Ok(call) = presenter.input.try_recv() {
            let result = self
                .apply_presenter_input(call.action)
                .map_err(|error| io::Error::other(error.message));
            let _ = call.reply.send(result);
        }
        if !presenter.connection_alive.load(Ordering::Acquire) {
            presenter.running.store(false, Ordering::Release);
            let _ = self.host.input().release_all();
        }
        if presenter.response.is_some() {
            match presenter.ready.try_recv() {
                Ok(Ok(value)) => {
                    presenter
                        .response
                        .take()
                        .expect("checked above")
                        .success(value.clone());
                    presenter.announced = true;
                    self.emit(
                        "presenter_attached",
                        json!({
                            "desktop_target": value["desktop_target"],
                        }),
                    );
                }
                Ok(Err(error)) => {
                    presenter
                        .response
                        .take()
                        .expect("checked above")
                        .error(IpcError::new("invalid_state", error.to_string()));
                    presenter.running.store(false, Ordering::Release);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    presenter
                        .response
                        .take()
                        .expect("checked above")
                        .error(IpcError::new(
                            "invalid_state",
                            "presenter stopped during attach",
                        ));
                    presenter.running.store(false, Ordering::Release);
                }
            }
        }
        let finished = match presenter.done.try_recv() {
            Ok(result) => {
                if let Err(error) = result {
                    (*lock(&presenter.state))["error"] = Value::String(error);
                }
                true
            }
            Err(TryRecvError::Disconnected) => true,
            Err(TryRecvError::Empty) => false,
        };
        if finished {
            if let Some(response) = presenter.response.take() {
                response.error(IpcError::new(
                    "invalid_state",
                    "presenter stopped during attach",
                ));
            }
            if let Some(join) = presenter.join.take() {
                let _ = join.join();
            }
            let _ = self.host.input().release_all();
            if presenter.announced {
                self.emit("presenter_detached", json!({}));
            }
        } else {
            self.presenter = Some(presenter);
        }
    }

    fn stop_presenter(&mut self) {
        let Some(mut presenter) = self.presenter.take() else {
            return;
        };
        presenter.running.store(false, Ordering::Release);
        let _ = self.host.input().release_all();
        while presenter
            .join
            .as_ref()
            .is_some_and(|join| !join.is_finished())
        {
            while let Ok(call) = presenter.input.try_recv() {
                let result = self
                    .apply_presenter_input(call.action)
                    .map_err(|error| io::Error::other(error.message));
                let _ = call.reply.send(result);
            }
            thread::sleep(ACTOR_TICK);
        }
        if let Some(join) = presenter.join.take() {
            let _ = join.join();
        }
        let _ = self.host.input().release_all();
        if let Some(response) = presenter.response.take() {
            response.error(IpcError::new(
                "invalid_state",
                "presenter attach was replaced",
            ));
        }
        if presenter.announced {
            self.emit("presenter_detached", json!({}));
        }
    }

    fn finish_pending(&mut self) {
        self.stop_presenter();
        let error = || IpcError::new("compositor_exited", "desktop control actor stopped");
        for waiter in self.exit_waiters.drain(..) {
            waiter.response.error(error());
        }
        for hold in self.key_holds.drain(..) {
            let _ = self.host.release_key(&hold.stroke);
            hold.response.error(error());
        }
        for drag in self.drags.drain(..) {
            let _ = self.host.pointer_button(drag.button, false);
            drag.response.error(error());
        }
        self.subscribers.clear();
    }
}

fn complete(response: Responder, result: Result<Value, IpcError>) {
    match result {
        Ok(value) => response.success(value),
        Err(error) => response.error(error),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn methods_for(compositor: ResolvedCompositor) -> Vec<&'static str> {
    let mut methods = METHODS.to_vec();
    if compositor == ResolvedCompositor::Sway {
        methods.extend_from_slice(SWAY_METHODS);
    }
    methods
}

fn window_unsupported() -> IpcError {
    IpcError::new(
        "unsupported",
        "window enumeration is unavailable with Weston",
    )
    .with_data(json!({
        "compositor": "weston",
        "reason": "weston exposes no window-enumeration IPC",
    }))
}

fn window_query_error(error: io::Error) -> IpcError {
    IpcError::new(
        "invalid_state",
        format!("could not query the Sway window tree: {error}"),
    )
}

fn capability_limits(width: u32, height: u32) -> Value {
    json!({
        "request_frame_bytes": MAX_REQUEST_FRAME_BYTES,
        "reply_frame_bytes": MAX_REPLY_FRAME_BYTES,
        "connections": MAX_CONNECTIONS,
        "in_flight_requests": MAX_IN_FLIGHT_REQUESTS,
        "subscriptions": MAX_SUBSCRIPTIONS,
        "subscriber_events": MAX_SUBSCRIBER_EVENTS,
        "event_replay": EVENT_REPLAY,
        "input_bytes": MAX_INPUT_BYTES,
        "screenshot_inline_bytes": MAX_SCREENSHOT_INLINE_BYTES,
        "screenshot_quality": {"min": 1, "max": 100},
        "key_repeat": MAX_KEY_REPEAT,
        "chord_modifiers": MAX_CHORD_MODIFIERS,
        "drag_steps": MAX_DRAG_STEPS,
        "scroll": {
            "units_per_detent": SCROLL_UNITS_PER_DETENT,
            "max_units": MAX_SCROLL_UNITS,
            "positive_vertical": "up"
        },
        "hold_ms": MAX_HOLD_MS,
        "timeout_ms": {"min": MIN_TIMEOUT_MS, "max": MAX_TIMEOUT_MS},
        "capture_stall_ms": millis(CAPTURE_STALL),
        "pointer": {"width": width, "height": height}
    })
}

fn subscription_kinds(events: &[String]) -> HashSet<String> {
    if events.is_empty() {
        EVENT_KINDS
            .iter()
            .copied()
            .filter(|kind| *kind != "frame_captured")
            .map(str::to_owned)
            .collect()
    } else {
        events.iter().cloned().collect()
    }
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, IpcError> {
    serde_json::from_value(params)
        .map_err(|error| IpcError::new("invalid_params", error.to_string()))
}

fn expect_empty(params: &Value) -> Result<(), IpcError> {
    if params.as_object().is_some_and(serde_json::Map::is_empty) || params.is_null() {
        Ok(())
    } else {
        Err(IpcError::new(
            "invalid_params",
            "method takes no parameters",
        ))
    }
}

fn validate_timeout(timeout_ms: u64) -> Result<(), IpcError> {
    if (MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        Ok(())
    } else {
        Err(IpcError::new(
            "invalid_params",
            "timeout must be between 1 ms and 24 hours",
        ))
    }
}

fn invalid_params(error: io::Error) -> IpcError {
    IpcError::new("invalid_params", error.to_string())
}

fn input_error(error: io::Error) -> IpcError {
    IpcError::new("input_unavailable", error.to_string())
}

fn capture_error(error: io::Error) -> IpcError {
    IpcError::new("capture_stalled", error.to_string())
}

fn ensure_inline_size(bytes: usize) -> Result<(), IpcError> {
    if bytes <= MAX_SCREENSHOT_INLINE_BYTES {
        Ok(())
    } else {
        Err(inline_limit_error(bytes))
    }
}

fn inline_limit_error(bytes: usize) -> IpcError {
    IpcError::new(
        "limit_exceeded",
        "screenshot is too large for an inline control reply",
    )
    .with_data(json!({
        "bytes": bytes,
        "limit": MAX_SCREENSHOT_INLINE_BYTES,
        "hint": "use --output or --format png",
    }))
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn raw_format(format: crate::linux::video::RawPixelFormat) -> &'static str {
    match format {
        crate::linux::video::RawPixelFormat::Bgrx => "bgrx",
        crate::linux::video::RawPixelFormat::Rgbx => "rgbx",
        crate::linux::video::RawPixelFormat::Bgra => "bgra",
        crate::linux::video::RawPixelFormat::Rgba => "rgba",
    }
}

fn screenshot_format(format: ScreenshotFormat) -> &'static str {
    match format {
        ScreenshotFormat::Png => "png",
        ScreenshotFormat::Jpeg => "jpeg",
        ScreenshotFormat::Raw => "raw",
    }
}

fn screen_result(snapshot: ScreenSnapshot) -> Value {
    json!({
        "screen_sequence": snapshot.screen_sequence,
        "frame_serial": snapshot.frame_serial,
        "hash": snapshot.hash_hex(),
    })
}

fn exit_result(status: ExitStatus) -> Value {
    json!({"exit_status": {"code": status.code(), "signal": status.signal()}})
}

fn interpolate(from: u32, to: u32, step: u32, steps: u32) -> u32 {
    let from = i64::from(from);
    let delta = i64::from(to) - from;
    u32::try_from(from + delta * i64::from(step) / i64::from(steps)).unwrap_or(to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_contract_lists_exactly_the_core_methods() {
        assert_eq!(
            METHODS,
            [
                "hello",
                "ping",
                "capabilities",
                "inspect",
                "key",
                "typing",
                "mouse",
                "launch",
                "screenshot",
                "wait_frame",
                "wait_screen_change",
                "wait_screen_stable",
                "wait_exit",
                "subscribe",
                "shutdown",
            ]
        );
    }

    #[test]
    fn window_methods_are_advertised_only_for_sway() {
        assert_eq!(methods_for(ResolvedCompositor::Weston), METHODS);
        let sway = methods_for(ResolvedCompositor::Sway);
        assert_eq!(&sway[..METHODS.len()], METHODS);
        assert_eq!(&sway[METHODS.len()..], SWAY_METHODS);

        let unsupported = window_unsupported();
        assert_eq!(unsupported.code, "unsupported");
        assert_eq!(unsupported.data.unwrap()["compositor"], "weston");
    }

    #[test]
    fn drag_interpolation_includes_the_destination() {
        assert_eq!(interpolate(10, 110, 1, 4), 35);
        assert_eq!(interpolate(10, 110, 4, 4), 110);
        assert_eq!(interpolate(110, 10, 4, 4), 10);
    }

    #[test]
    fn capabilities_publish_every_enforced_numeric_limit() {
        let limits = capability_limits(1920, 1080);
        assert_eq!(limits["request_frame_bytes"], MAX_REQUEST_FRAME_BYTES);
        assert_eq!(limits["reply_frame_bytes"], MAX_REPLY_FRAME_BYTES);
        assert_eq!(limits["connections"], MAX_CONNECTIONS);
        assert_eq!(limits["in_flight_requests"], MAX_IN_FLIGHT_REQUESTS);
        assert_eq!(limits["subscriptions"], MAX_SUBSCRIPTIONS);
        assert_eq!(limits["subscriber_events"], MAX_SUBSCRIBER_EVENTS);
        assert_eq!(limits["event_replay"], EVENT_REPLAY);
        assert_eq!(limits["input_bytes"], MAX_INPUT_BYTES);
        assert_eq!(
            limits["screenshot_inline_bytes"],
            MAX_SCREENSHOT_INLINE_BYTES
        );
        assert_eq!(limits["key_repeat"], MAX_KEY_REPEAT);
        assert_eq!(limits["chord_modifiers"], MAX_CHORD_MODIFIERS);
        assert_eq!(limits["drag_steps"], MAX_DRAG_STEPS);
        assert_eq!(limits["scroll"]["max_units"], MAX_SCROLL_UNITS);
        assert_eq!(
            limits["scroll"]["units_per_detent"],
            SCROLL_UNITS_PER_DETENT
        );
        assert_eq!(limits["hold_ms"], MAX_HOLD_MS);
        assert_eq!(limits["timeout_ms"]["min"], MIN_TIMEOUT_MS);
        assert_eq!(limits["timeout_ms"]["max"], MAX_TIMEOUT_MS);
        assert_eq!(limits["capture_stall_ms"], millis(CAPTURE_STALL));
        assert_eq!(limits["pointer"], json!({"width": 1920, "height": 1080}));
    }

    #[test]
    fn default_subscription_refuses_only_the_high_rate_frame_event() {
        let kinds = subscription_kinds(&[]);
        assert!(!kinds.contains("frame_captured"));
        assert_eq!(kinds.len(), EVENT_KINDS.len() - 1);
        for kind in EVENT_KINDS {
            assert_eq!(kinds.contains(*kind), *kind != "frame_captured");
        }

        let explicit = subscription_kinds(&["frame_captured".into()]);
        assert_eq!(explicit, HashSet::from(["frame_captured".into()]));
    }

    #[test]
    fn four_k_raw_is_rejected_inline_with_an_output_hint() {
        let bytes = 3840 * 2160 * 4;
        let error = ensure_inline_size(bytes).unwrap_err();
        assert_eq!(error.code, "limit_exceeded");
        assert_eq!(error.data.unwrap()["bytes"], bytes);
    }
}
