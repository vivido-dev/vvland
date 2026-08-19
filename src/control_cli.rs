//! Cross-platform Unix-socket client and the shared NDJSON control envelopes.

use std::env;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cli::{MsgCommand, MsgOptions, MsgWaitCondition, ScreenshotFormat};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_REQUEST_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_REPLY_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CONNECTIONS: usize = 32;
pub const MAX_IN_FLIGHT_REQUESTS: usize = 64;
pub const MAX_SUBSCRIPTIONS: usize = 32;
pub const MAX_SUBSCRIBER_EVENTS: usize = 256;
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;
pub const MAX_SCREENSHOT_INLINE_BYTES: usize = 12 * 1024 * 1024;
pub const MAX_KEY_REPEAT: u16 = 1000;
pub const MAX_CHORD_MODIFIERS: usize = 8;
pub const MAX_DRAG_STEPS: u16 = 256;
pub const MAX_SCROLL_UNITS: i32 = 12_000;
pub const SCROLL_UNITS_PER_DETENT: i32 = 120;
pub const MAX_HOLD_MS: u64 = 5_000;
pub const MIN_TIMEOUT_MS: u64 = 1;
pub const MAX_TIMEOUT_MS: u64 = 86_400_000;
pub const EVENT_KINDS: &[&str] = &[
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
];

#[derive(Debug, Deserialize, Serialize)]
pub struct RequestEnvelope {
    pub version: u16,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct IpcError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl IpcError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ResponseEnvelope {
    pub version: u16,
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,
}

impl ResponseEnvelope {
    pub fn success(id: u64, result: Value) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: u64, error: IpcError) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[allow(dead_code)]
pub struct SubscriptionEventEnvelope {
    pub version: u16,
    pub subscription_id: u64,
    pub event_sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_id: Option<u64>,
    pub event: Value,
}

pub fn run(options: &MsgOptions) -> io::Result<()> {
    validate_message(&options.message)?;
    if matches!(
        &options.message,
        MsgCommand::Screenshot(params) if params.output.is_none()
    ) && io::stdout().is_terminal()
    {
        return invalid(
            "refusing to write screenshot bytes to a terminal; redirect stdout or pass --output",
        );
    }
    let socket = discover_socket(options)?;
    let mut stream = UnixStream::connect(&socket).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("could not connect to {}: {error}", socket.display()),
        )
    })?;
    require_peer_owner(&stream)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let hello = exchange(&mut stream, &mut reader, 1, "hello", json!({}))?;
    if matches!(options.message, MsgCommand::Capabilities) {
        print_json(&hello)?;
        return Ok(());
    }
    let (method, params, print) = message_request(&options.message)?;
    stream.set_read_timeout(Some(response_read_timeout(&options.message)))?;
    let result = exchange(&mut stream, &mut reader, 2, method, params)?;
    if print {
        write_cli_result(&options.message, &result)?;
    }
    if matches!(options.message, MsgCommand::Subscribe(_)) {
        stream.set_read_timeout(None)?;
        let mut stdout = io::stdout().lock();
        loop {
            let Some(frame) = read_frame(&mut reader)? else {
                return Ok(());
            };
            let event: SubscriptionEventEnvelope = serde_json::from_slice(&frame)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            serde_json::to_writer(&mut stdout, &event)?;
            stdout.write_all(b"\n")?;
        }
    }
    Ok(())
}

fn response_read_timeout(message: &MsgCommand) -> Duration {
    let timeout_ms = match message {
        MsgCommand::Screenshot(params) => params.timeout_ms,
        MsgCommand::Wait { condition } => match condition {
            MsgWaitCondition::Frame { timeout_ms, .. }
            | MsgWaitCondition::Exit { timeout_ms }
            | MsgWaitCondition::ScreenChange { timeout_ms, .. }
            | MsgWaitCondition::ScreenStable { timeout_ms, .. }
            | MsgWaitCondition::Window { timeout_ms, .. } => *timeout_ms,
        },
        _ => 30_000,
    };
    Duration::from_millis(timeout_ms).saturating_add(Duration::from_secs(5))
}

fn message_request(message: &MsgCommand) -> io::Result<(&'static str, Value, bool)> {
    Ok(match message {
        MsgCommand::Ping => ("ping", json!({}), false),
        MsgCommand::Capabilities => unreachable!("handled after hello"),
        MsgCommand::Inspect => ("inspect", json!({}), true),
        MsgCommand::Key(params) => ("key", serde_json::to_value(params)?, false),
        MsgCommand::Typing(params) => ("typing", serde_json::to_value(params)?, false),
        MsgCommand::Mouse { action } => (
            "mouse",
            json!({"action": serde_json::to_value(action)?}),
            false,
        ),
        MsgCommand::Launch(params) => ("launch", serde_json::to_value(params)?, false),
        MsgCommand::ListWindows => ("list_windows", json!({}), true),
        MsgCommand::Screenshot(params) => ("screenshot", serde_json::to_value(params)?, true),
        MsgCommand::Wait { condition } => match condition {
            MsgWaitCondition::Frame {
                after_frame,
                timeout_ms,
            } => (
                "wait_frame",
                json!({"after_frame": after_frame, "timeout_ms": timeout_ms}),
                true,
            ),
            MsgWaitCondition::Exit { timeout_ms } => {
                ("wait_exit", json!({"timeout_ms": timeout_ms}), true)
            }
            MsgWaitCondition::ScreenChange {
                after_screen,
                exact,
                timeout_ms,
            } => (
                "wait_screen_change",
                json!({
                    "after_screen": after_screen,
                    "exact": exact,
                    "timeout_ms": timeout_ms,
                }),
                true,
            ),
            MsgWaitCondition::ScreenStable {
                quiet_ms,
                after_screen,
                exact,
                timeout_ms,
            } => (
                "wait_screen_stable",
                json!({
                    "quiet_ms": quiet_ms,
                    "after_screen": after_screen,
                    "exact": exact,
                    "timeout_ms": timeout_ms,
                }),
                true,
            ),
            MsgWaitCondition::Window { app_id, timeout_ms } => (
                "wait_window",
                json!({"app_id": app_id, "timeout_ms": timeout_ms}),
                true,
            ),
        },
        MsgCommand::Subscribe(params) => ("subscribe", serde_json::to_value(params)?, true),
        MsgCommand::Shutdown => ("shutdown", json!({}), false),
    })
}

pub fn validate_message(message: &MsgCommand) -> io::Result<()> {
    match message {
        MsgCommand::Typing(params) if params.text.len() > MAX_INPUT_BYTES => {
            invalid("typing text exceeds the 1 MiB input limit")
        }
        MsgCommand::Key(params) => {
            if !(1..=MAX_KEY_REPEAT).contains(&params.repeat) {
                return invalid("key repeat must be between 1 and 1000");
            }
            if params.mods.len() > MAX_CHORD_MODIFIERS {
                return invalid("a key chord may contain at most 8 modifiers");
            }
            if params.hold_ms.is_some() && params.repeat > 1 {
                return invalid("--hold-ms cannot be combined with --repeat greater than 1");
            }
            if params
                .hold_ms
                .is_some_and(|hold| !(1..=MAX_HOLD_MS).contains(&hold))
            {
                return invalid("key hold must be between 1 and 5000 ms");
            }
            Ok(())
        }
        MsgCommand::Mouse { action } => validate_mouse(action),
        MsgCommand::Launch(params) => {
            if params.program.is_empty() {
                return invalid("launch requires a program");
            }
            if params
                .program
                .iter()
                .any(|part| part.as_bytes().contains(&0))
            {
                return invalid("launch arguments cannot contain NUL bytes");
            }
            if params.shell && params.program.len() != 1 {
                return invalid("--shell requires exactly one command string");
            }
            Ok(())
        }
        MsgCommand::Screenshot(params) => {
            if !(1..=100).contains(&params.quality) {
                return invalid("screenshot quality must be between 1 and 100");
            }
            if params.format != ScreenshotFormat::Jpeg && params.quality != 85 {
                return invalid("--quality applies only to JPEG screenshots");
            }
            if params.max_age_ms == Some(0) {
                return invalid("screenshot max age must be at least 1 ms");
            }
            if params
                .max_age_ms
                .is_some_and(|duration| duration > MAX_TIMEOUT_MS)
            {
                return invalid("screenshot max age may not exceed 24 hours");
            }
            if let Some(path) = &params.output
                && !path.is_absolute()
            {
                return invalid("screenshot output path must be absolute");
            }
            validate_timeout(params.timeout_ms)
        }
        MsgCommand::Wait { condition } => {
            let timeout = match condition {
                MsgWaitCondition::Frame { timeout_ms, .. }
                | MsgWaitCondition::Exit { timeout_ms }
                | MsgWaitCondition::ScreenChange { timeout_ms, .. }
                | MsgWaitCondition::ScreenStable { timeout_ms, .. }
                | MsgWaitCondition::Window { timeout_ms, .. } => *timeout_ms,
            };
            validate_timeout(timeout)?;
            if let MsgWaitCondition::ScreenStable { quiet_ms, .. } = condition {
                validate_timeout(*quiet_ms)?;
            }
            if let MsgWaitCondition::Window { app_id, .. } = condition
                && app_id.is_empty()
            {
                return invalid("window app-id cannot be empty");
            }
            Ok(())
        }
        MsgCommand::Subscribe(params) => {
            for event in &params.events {
                if !EVENT_KINDS.contains(&event.as_str()) {
                    return invalid("subscription contains an unknown event kind");
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_mouse(action: &crate::cli::MsgMouseAction) -> io::Result<()> {
    use crate::cli::MsgMouseAction;
    match action {
        MsgMouseAction::Click { count, .. } if !(1..=3).contains(count) => {
            invalid("click count must be between 1 and 3")
        }
        MsgMouseAction::Drag { steps, step_ms, .. } => {
            if !(1..=MAX_DRAG_STEPS).contains(steps) {
                return invalid("drag steps must be between 1 and 256");
            }
            if *step_ms > 100 || u64::from(*steps) * u64::from(*step_ms) > MAX_HOLD_MS {
                return invalid("drag timing must be at most 100 ms per step and 5 seconds total");
            }
            Ok(())
        }
        MsgMouseAction::Scroll {
            vertical,
            horizontal,
            ..
        } => {
            let max_detents = MAX_SCROLL_UNITS / SCROLL_UNITS_PER_DETENT;
            if vertical.unsigned_abs() > max_detents as u32
                || horizontal.unsigned_abs() > max_detents as u32
            {
                return invalid("scroll distance may not exceed 100 detents per axis");
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_timeout(timeout_ms: u64) -> io::Result<()> {
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return invalid("timeout must be between 1 ms and 24 hours");
    }
    Ok(())
}

fn invalid<T>(message: &str) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidInput, message))
}

fn exchange(
    stream: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    id: u64,
    method: &str,
    params: Value,
) -> io::Result<Value> {
    let request = RequestEnvelope {
        version: PROTOCOL_VERSION,
        id,
        method: method.to_owned(),
        params,
    };
    let mut frame = serde_json::to_vec(&request)?;
    if frame.len() > MAX_REQUEST_FRAME_BYTES {
        return invalid("request exceeds the 1 MiB frame limit");
    }
    frame.push(b'\n');
    stream.write_all(&frame)?;
    let response = read_response(reader)?;
    if response.version != PROTOCOL_VERSION || response.id != id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control server returned an uncorrelated response",
        ));
    }
    if response.ok {
        response.result.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "successful reply omitted result",
            )
        })
    } else {
        let error = response.error.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "failed reply omitted error")
        })?;
        Err(io::Error::other(format!(
            "{}: {}",
            error.code, error.message
        )))
    }
}

fn read_response(reader: &mut BufReader<UnixStream>) -> io::Result<ResponseEnvelope> {
    let frame = read_frame(reader)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "control server closed without a reply",
        )
    })?;
    serde_json::from_slice(&frame)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn read_frame(reader: &mut BufReader<UnixStream>) -> io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    let bytes = reader
        .by_ref()
        .take((MAX_REPLY_FRAME_BYTES + 2) as u64)
        .read_until(b'\n', &mut frame)?;
    if bytes == 0 {
        return Ok(None);
    }
    if frame.len() > MAX_REPLY_FRAME_BYTES + 1 || !frame.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control reply exceeds the 16 MiB frame limit",
        ));
    }
    frame.pop();
    Ok(Some(frame))
}

fn print_json(value: &Value) -> io::Result<()> {
    serde_json::to_writer_pretty(io::stdout().lock(), value)?;
    println!();
    Ok(())
}

fn write_cli_result(message: &MsgCommand, result: &Value) -> io::Result<()> {
    match message {
        MsgCommand::Screenshot(params) => {
            if params.output.is_some() {
                let path = result.get("path").and_then(Value::as_str).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "screenshot reply omitted path")
                })?;
                println!("{path}");
                return Ok(());
            }
            let encoded = result.get("data").and_then(Value::as_str).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "screenshot reply omitted data")
            })?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            io::stdout().lock().write_all(&bytes)
        }
        MsgCommand::Subscribe(_) => {
            serde_json::to_writer(io::stdout().lock(), result)?;
            println!();
            Ok(())
        }
        _ => print_json(result),
    }
}

fn discover_socket(options: &MsgOptions) -> io::Result<PathBuf> {
    if let Some(socket) = &options.socket {
        return Ok(socket.clone());
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(target) = options
            .target
            .clone()
            .or_else(|| env::var("VVLAND_SESSION").ok())
        {
            return Ok(crate::linux::runtime::RuntimePaths::for_session(&target)?.socket);
        }
        let sessions = crate::linux::runtime::list_registries()?;
        match sessions.as_slice() {
            [one] => Ok(one.socket.clone()),
            [] => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no live vvland session; start one with `vvland serve --session NAME`",
            )),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "more than one vvland session is live; select one with `-t NAME` (see `vvland list`)",
            )),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = env::var_os("VVLAND_SESSION");
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "session discovery is currently supported only on Linux; pass --socket",
        ))
    }
}

pub fn require_peer_owner(stream: &UnixStream) -> io::Result<()> {
    let credential_uid = peer_uid(stream)?;
    // SAFETY: geteuid has no preconditions.
    let owner = unsafe { libc::geteuid() };
    if credential_uid != owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("control socket peer uid {credential_uid} is not owner uid {owner}"),
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> io::Result<libc::uid_t> {
    let mut credential = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `credential` and `length` are valid writable buffers for getsockopt.
    let result = unsafe {
        libc::getsockopt(
            std::os::fd::AsRawFd::as_raw_fd(stream),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credential).cast(),
            &raw mut length,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(credential.uid)
}

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
fn peer_uid(stream: &UnixStream) -> io::Result<libc::uid_t> {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: uid and gid are valid output pointers and the stream owns a live descriptor.
    let result =
        unsafe { libc::getpeereid(std::os::fd::AsRawFd::as_raw_fd(stream), &mut uid, &mut gid) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{
        MsgKey, MsgMouseAction, MsgScreenshot, MsgTyping, ScreenshotFormat, ScreenshotScale,
    };

    #[test]
    fn envelope_round_trip_preserves_correlation_and_error_data() {
        let request = RequestEnvelope {
            version: 1,
            id: 42,
            method: "ping".into(),
            params: json!({}),
        };
        let encoded = serde_json::to_vec(&request).unwrap();
        let decoded: RequestEnvelope = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.id, 42);
        assert_eq!(decoded.method, "ping");

        let response = ResponseEnvelope::error(
            42,
            IpcError::new("invalid_params", "bad").with_data(json!({"field": "x"})),
        );
        let decoded: ResponseEnvelope =
            serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
        assert_eq!(decoded.error.unwrap().data, Some(json!({"field": "x"})));
    }

    #[test]
    fn client_validation_enforces_input_and_key_limits() {
        assert!(
            validate_message(&MsgCommand::Typing(MsgTyping {
                text: "x".repeat(MAX_INPUT_BYTES + 1),
            }))
            .is_err()
        );
        for repeat in [0, MAX_KEY_REPEAT + 1] {
            assert!(
                validate_message(&MsgCommand::Key(MsgKey {
                    key: "a".into(),
                    mods: vec![],
                    repeat,
                    hold_ms: None,
                }))
                .is_err()
            );
        }
        assert!(
            validate_message(&MsgCommand::Key(MsgKey {
                key: "a".into(),
                mods: vec!["ctrl".into(); MAX_CHORD_MODIFIERS + 1],
                repeat: 1,
                hold_ms: None,
            }))
            .is_err()
        );
    }

    #[test]
    fn client_validation_enforces_mouse_and_wait_limits() {
        let invalid_drag = MsgCommand::Mouse {
            action: MsgMouseAction::Drag {
                button: crate::cli::Button::Left,
                from_x: 0,
                from_y: 0,
                to_x: 1,
                to_y: 1,
                steps: MAX_DRAG_STEPS + 1,
                step_ms: 8,
            },
        };
        assert!(validate_message(&invalid_drag).is_err());
        for timeout_ms in [0, MAX_TIMEOUT_MS + 1] {
            assert!(
                validate_message(&MsgCommand::Wait {
                    condition: MsgWaitCondition::Exit { timeout_ms },
                })
                .is_err()
            );
        }

        assert!(
            validate_message(&MsgCommand::Wait {
                condition: MsgWaitCondition::ScreenStable {
                    quiet_ms: 0,
                    after_screen: None,
                    exact: false,
                    timeout_ms: 1,
                },
            })
            .is_err()
        );
        assert!(
            validate_message(&MsgCommand::Wait {
                condition: MsgWaitCondition::Window {
                    app_id: String::new(),
                    timeout_ms: 1,
                },
            })
            .is_err()
        );
    }

    #[test]
    fn client_validation_enforces_screenshot_quality_and_inline_options() {
        for quality in [0, 101] {
            assert!(
                validate_message(&MsgCommand::Screenshot(MsgScreenshot {
                    format: ScreenshotFormat::Jpeg,
                    output: None,
                    scale: ScreenshotScale::Full,
                    quality,
                    max_age_ms: None,
                    fresh: false,
                    timeout_ms: 1,
                }))
                .is_err()
            );
        }
        assert!(
            validate_message(&MsgCommand::Screenshot(MsgScreenshot {
                format: ScreenshotFormat::Png,
                output: None,
                scale: ScreenshotScale::Full,
                quality: 50,
                max_age_ms: None,
                fresh: false,
                timeout_ms: 1,
            }))
            .is_err()
        );
    }

    #[test]
    fn source_never_names_vivid_credentials() {
        let source = include_str!("control_cli.rs");
        let forbidden = ["VIVID", "_ROOT", "_SECRET"].concat();
        assert!(!source.contains(&forbidden));
    }
}
