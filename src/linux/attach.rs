//! Foreground presenter client for a persistent vvland desktop.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::cli::Config;
use crate::control_cli::{
    MAX_REPLY_FRAME_BYTES, PROTOCOL_VERSION, ResponseEnvelope, require_peer_owner,
};
use crate::linux::control::{AttachParams, PresenterInput, VividCredentials};
use crate::linux::runtime::RuntimePaths;
use crate::producer::TerminalInjector;
use crate::producer::scene::{Placement, TerminalDisplay};
use crate::producer::terminal::{LocalCommand, TerminalGuard, TerminalInput};

const CONTROL_POLL: Duration = Duration::from_millis(50);
const PRESENTER_PING: Duration = Duration::from_secs(1);

pub fn run(config: &Config, session_name: &str) -> io::Result<()> {
    let paths = RuntimePaths::for_session(session_name)?;
    let mut launched = false;
    let stream = match connect(&paths.socket) {
        Ok(stream) => {
            if config.width.is_some() || config.height.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--width/--height cannot change an existing session's geometry",
                ));
            }
            stream
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            super::serve::launch_daemon(config, session_name)?;
            launched = true;
            connect(&paths.socket)?
        }
        Err(error) => return Err(error),
    };
    attach(config, session_name, stream, launched)
}

fn connect(path: &std::path::Path) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("could not connect to {}: {error}", path.display()),
        )
    })?;
    require_peer_owner(&stream)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

fn attach(
    config: &Config,
    session_name: &str,
    mut stream: UnixStream,
    _launched: bool,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    exchange(&mut stream, &mut reader, 1, "hello", &json!({}))?;

    let root_secret = Zeroizing::new(std::env::var("VIVID_ROOT_SECRET").map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "VIVID_ROOT_SECRET is not set",
        )
    })?);
    let endpoint_control = config.endpoint_control.clone().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "VIVID_ENDPOINT_CONTROL is not set")
    })?;
    let params = AttachParams {
        replace: config.replace,
        vivid: Zeroizing::new(VividCredentials {
            endpoint_control,
            endpoint_interactive: config.endpoint_interactive.clone(),
            endpoint_realtime: config.endpoint_realtime.clone(),
            endpoint_bulk: config.endpoint_bulk.clone(),
            root_secret,
        }),
        desktop_target: config.desktop_target,
        bitrate: config.bitrate,
        fps: config.fps,
        secure_input: config.secure_input,
    };
    let attached = exchange(&mut stream, &mut reader, 2, "attach", &params)?;
    let mut next_id = 3;
    let dimensions = response_dimensions(&attached)?;
    let mut placement = if config.desktop_target {
        None
    } else {
        let marker = attached
            .get("marker")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "attach reply omitted anchor marker",
                )
            })?;
        io::stdout().write_all(marker.as_bytes())?;
        io::stdout().flush()?;
        Some(Placement::calculate(
            response_display(&attached)?,
            dimensions.0,
            dimensions.1,
        )?)
    };

    let terminated = Arc::new(AtomicBool::new(false));
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        signal_hook::flag::register(signal, terminated.clone())?;
    }
    let mut terminal = TerminalGuard::enter()?;
    let compositor_name = "nested desktop";
    let mut input = TerminalInput::new(
        config.xkb_model.as_deref(),
        &config.xkb_layout,
        config.xkb_variant.as_deref(),
        config.xkb_options.clone(),
        compositor_name,
    )?
    .with_detach();
    if config.desktop_target {
        input = input.with_leader_only();
    }
    let mut remote = RemoteInjector {
        stream: &mut stream,
        reader: &mut reader,
        next_id: &mut next_id,
    };
    let mut last_ping = Instant::now();
    let mut quit_daemon = false;
    loop {
        if terminated.load(Ordering::Acquire) {
            break;
        }
        let command = if config.desktop_target {
            input.poll_leader_only(CONTROL_POLL, None)?
        } else {
            input.poll(
                CONTROL_POLL,
                &mut remote,
                placement.expect("terminal target has placement"),
                None,
            )?
        };
        match command {
            Some(LocalCommand::Detach) => {
                break;
            }
            Some(LocalCommand::Quit) => {
                let _ = remote.release_all();
                remote.request("shutdown", &json!({}))?;
                quit_daemon = true;
                break;
            }
            Some(LocalCommand::Run(command)) => {
                remote.request("presenter_run", &json!({"command": command}))?;
            }
            Some(LocalCommand::Resize) | None => {}
        }
        if last_ping.elapsed() >= PRESENTER_PING {
            let state = remote.request("presenter_ping", &json!({}))?;
            if !config.desktop_target {
                placement = Some(Placement::calculate(
                    response_display(&state)?,
                    dimensions.0,
                    dimensions.1,
                )?);
            }
            last_ping = Instant::now();
        }
        terminal.status(&format!(
            "vvland session {session_name} {}x{} | {}",
            dimensions.0,
            dimensions.1,
            input.status()
        ))?;
    }
    drop(terminal);
    drop(stream);
    drop(reader);
    if !quit_daemon {
        eprintln!(
            "vvland: detached from session '{session_name}'. Reattach with: vvland --session {session_name}"
        );
    }
    Ok(())
}

fn response_dimensions(value: &Value) -> io::Result<(u32, u32)> {
    let field = |name| {
        value
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "attach reply omitted geometry")
            })
    };
    Ok((field("width")?, field("height")?))
}

fn response_display(value: &Value) -> io::Result<TerminalDisplay> {
    let display = value.get("display").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "presenter state omitted terminal display",
        )
    })?;
    let field = |name| {
        display
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid terminal display"))
    };
    Ok(TerminalDisplay {
        grid_columns: field("grid_columns")?,
        grid_rows: field("grid_rows")?,
        cell_width: field("cell_width")?,
        cell_height: field("cell_height")?,
    })
}

struct RemoteInjector<'a> {
    stream: &'a mut UnixStream,
    reader: &'a mut BufReader<UnixStream>,
    next_id: &'a mut u64,
}

impl RemoteInjector<'_> {
    fn request<T: Serialize>(&mut self, method: &str, params: &T) -> io::Result<Value> {
        let id = *self.next_id;
        *self.next_id = self.next_id.saturating_add(1);
        exchange(self.stream, self.reader, id, method, params)
    }
}

impl TerminalInjector for RemoteInjector<'_> {
    fn key(&mut self, code: u32, pressed: bool) -> io::Result<()> {
        self.request("presenter_input", &PresenterInput::Key { code, pressed })?;
        Ok(())
    }

    fn pointer_absolute(&mut self, x: u32, y: u32) -> io::Result<()> {
        self.request("presenter_input", &PresenterInput::PointerAbsolute { x, y })?;
        Ok(())
    }

    fn pointer_button(&mut self, button: u32, pressed: bool) -> io::Result<()> {
        self.request(
            "presenter_input",
            &PresenterInput::PointerButton { button, pressed },
        )?;
        Ok(())
    }

    fn pointer_axis(&mut self, axis: u32, delta: i32) -> io::Result<()> {
        self.request(
            "presenter_input",
            &PresenterInput::PointerAxis { axis, delta },
        )?;
        Ok(())
    }

    fn release_all(&mut self) -> io::Result<()> {
        self.request("presenter_input", &PresenterInput::ReleaseAll)?;
        Ok(())
    }
}

fn exchange<T: Serialize>(
    stream: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    id: u64,
    method: &str,
    params: &T,
) -> io::Result<Value> {
    #[derive(Serialize)]
    struct Request<'a, T> {
        version: u16,
        id: u64,
        method: &'a str,
        params: &'a T,
    }
    serde_json::to_writer(
        &mut *stream,
        &Request {
            version: PROTOCOL_VERSION,
            id,
            method,
            params,
        },
    )?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut frame = Vec::new();
    let bytes = reader
        .by_ref()
        .take((MAX_REPLY_FRAME_BYTES + 2) as u64)
        .read_until(b'\n', &mut frame)?;
    if bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "control server closed",
        ));
    }
    if frame.len() > MAX_REPLY_FRAME_BYTES + 1 || !frame.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control reply exceeds limit",
        ));
    }
    let response: ResponseEnvelope = serde_json::from_slice(&frame)?;
    if response.id != id || response.version != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mismatched control reply",
        ));
    }
    if response.ok {
        response
            .result
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "reply omitted result"))
    } else {
        let error = response
            .error
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "reply omitted error"))?;
        Err(io::Error::other(format!(
            "{}: {}",
            error.code, error.message
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_geometry_requires_all_fields() {
        assert_eq!(
            response_dimensions(&json!({"width": 1280, "height": 720})).unwrap(),
            (1280, 720)
        );
        assert!(response_dimensions(&json!({"width": 1280})).is_err());
    }
}
