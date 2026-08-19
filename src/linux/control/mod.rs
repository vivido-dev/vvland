//! Owner-only, versioned NDJSON control plane for one desktop host.

pub mod methods;
pub mod screenshot;
pub mod server;
pub mod watch;

use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::{Zeroize, Zeroizing};

use crate::control_cli::{
    IpcError, MAX_SUBSCRIBER_EVENTS, ResponseEnvelope, SubscriptionEventEnvelope,
};

pub const METHODS: &[&str] = &[
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
];

pub const SWAY_METHODS: &[&str] = &["list_windows", "wait_window"];

pub use crate::control_cli::EVENT_KINDS;

pub enum OutputFrame {
    Response(ResponseEnvelope),
    Event(SubscriptionEventEnvelope, EventQueueSlot),
}

pub struct EventQueueSlot(Arc<AtomicUsize>);

impl Drop for EventQueueSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone)]
pub struct EventSink {
    output: SyncSender<OutputFrame>,
    queued: Arc<AtomicUsize>,
    alive: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSendError {
    Full,
    Closed,
}

impl EventSink {
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub fn send(&self, event: SubscriptionEventEnvelope) -> Result<(), EventSendError> {
        if !self.is_alive() {
            return Err(EventSendError::Closed);
        }
        if self
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                (queued < MAX_SUBSCRIBER_EVENTS).then_some(queued + 1)
            })
            .is_err()
        {
            return Err(EventSendError::Full);
        }
        let slot = EventQueueSlot(self.queued.clone());
        match self.output.try_send(OutputFrame::Event(event, slot)) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => Err(EventSendError::Full),
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(EventSendError::Closed),
        }
    }
}

pub const ERROR_CODES: &[&str] = &[
    "unsupported_version",
    "invalid_request",
    "invalid_params",
    "duplicate_request_id",
    "limit_exceeded",
    "unsupported",
    "timeout",
    "invalid_state",
    "subscription_overflow",
    "desktop_not_ready",
    "capture_stalled",
    "input_unavailable",
    "compositor_exited",
    "key_not_in_keymap",
    "coordinate_out_of_range",
    "launch_failed",
    "encode_failed",
];

#[derive(Clone)]
pub struct Responder {
    id: u64,
    connection_id: u64,
    output: SyncSender<OutputFrame>,
    in_flight: Arc<Mutex<HashSet<u64>>>,
    alive: Arc<AtomicBool>,
}

impl Responder {
    pub fn new(
        id: u64,
        connection_id: u64,
        output: SyncSender<OutputFrame>,
        in_flight: Arc<Mutex<HashSet<u64>>>,
        alive: Arc<AtomicBool>,
    ) -> Self {
        Self {
            id,
            connection_id,
            output,
            in_flight,
            alive,
        }
    }

    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }

    pub fn connection_alive(&self) -> Arc<AtomicBool> {
        self.alive.clone()
    }

    pub fn success(self, result: Value) {
        let id = self.id;
        self.complete(ResponseEnvelope::success(id, result));
    }

    pub fn error(self, error: IpcError) {
        let id = self.id;
        self.complete(ResponseEnvelope::error(id, error));
    }

    pub fn event_sink(&self) -> EventSink {
        EventSink {
            output: self.output.clone(),
            queued: Arc::new(AtomicUsize::new(0)),
            alive: self.alive.clone(),
        }
    }

    fn complete(self, envelope: ResponseEnvelope) {
        self.in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.id);
        let _ = self.output.try_send(OutputFrame::Response(envelope));
    }
}

pub struct ActorRequest {
    pub method: String,
    pub params: Value,
    pub attach: Option<AttachParams>,
    pub response: Responder,
}

/// Credential material accepted only by the private `attach` method.
///
/// Neither this type nor its enclosing request implements `Debug`; dropping the request clears
/// every string, including endpoints whose query components may themselves carry capabilities.
#[derive(Deserialize, Serialize, Zeroize)]
#[zeroize(drop)]
#[serde(deny_unknown_fields)]
pub struct VividCredentials {
    pub endpoint_control: Zeroizing<String>,
    #[serde(default)]
    pub endpoint_interactive: Option<Zeroizing<String>>,
    #[serde(default)]
    pub endpoint_realtime: Option<Zeroizing<String>>,
    #[serde(default)]
    pub endpoint_bulk: Option<Zeroizing<String>>,
    pub root_secret: Zeroizing<String>,
}

#[derive(Deserialize, Serialize, Zeroize)]
#[zeroize(drop)]
#[serde(deny_unknown_fields)]
pub struct AttachParams {
    #[serde(default)]
    pub replace: bool,
    pub vivid: Zeroizing<VividCredentials>,
    #[serde(default)]
    pub desktop_target: bool,
    pub bitrate: u64,
    pub fps: u32,
    #[serde(default)]
    pub secure_input: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PresenterInput {
    Key { code: u32, pressed: bool },
    PointerAbsolute { x: u32, y: u32 },
    PointerButton { button: u32, pressed: bool },
    PointerAxis { axis: u32, delta: i32 },
    ReleaseAll,
}

pub struct HostInputCall {
    pub action: PresenterInput,
    pub reply: SyncSender<std::io::Result<()>>,
}

impl fmt::Debug for ActorRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActorRequest")
            .field("method", &self.method)
            .field("params", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use serde_json::json;

    use super::*;
    use crate::control_cli::PROTOCOL_VERSION;

    fn event(sequence: u64) -> SubscriptionEventEnvelope {
        SubscriptionEventEnvelope {
            version: PROTOCOL_VERSION,
            subscription_id: 1,
            event_sequence: sequence,
            window_id: None,
            event: json!({"type": "screen_changed", "data": {}}),
        }
    }

    #[test]
    fn event_sink_enforces_its_per_subscription_queue_bound() {
        let (output, received) = mpsc::sync_channel(MAX_SUBSCRIBER_EVENTS + 1);
        let alive = Arc::new(AtomicBool::new(true));
        let sink = EventSink {
            output,
            queued: Arc::new(AtomicUsize::new(0)),
            alive: alive.clone(),
        };

        for sequence in 0..MAX_SUBSCRIBER_EVENTS {
            sink.send(event(sequence as u64)).unwrap();
        }
        assert_eq!(
            sink.send(event(MAX_SUBSCRIBER_EVENTS as u64)),
            Err(EventSendError::Full)
        );

        drop(received.recv().unwrap());
        sink.send(event(MAX_SUBSCRIBER_EVENTS as u64)).unwrap();
        alive.store(false, Ordering::Release);
        assert_eq!(sink.send(event(u64::MAX)), Err(EventSendError::Closed));
    }

    #[test]
    fn event_kind_registry_matches_the_control_contract() {
        assert_eq!(
            EVENT_KINDS,
            [
                "screen_changed",
                "frame_captured",
                "capture_stalled",
                "capture_resumed",
                "compositor_exited",
                "launch_started",
                "launch_failed",
                "audio_disabled",
                "presenter_attached",
                "presenter_detached",
                "overflow",
            ]
        );
    }
}

#[derive(Clone)]
pub struct ControlContext {
    pub session: String,
    pub fps: u32,
    pub app: Option<String>,
    pub xkb_model: Option<String>,
    pub xkb_layout: String,
    pub xkb_variant: Option<String>,
    pub xkb_options: Option<String>,
    pub config: crate::cli::Config,
}
