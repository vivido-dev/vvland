//! Bounded listener and connection framing for the control plane.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
#[cfg(test)]
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};
use zeroize::{Zeroize, Zeroizing};

use crate::control_cli::{
    IpcError, MAX_CONNECTIONS, MAX_IN_FLIGHT_REQUESTS, MAX_REPLY_FRAME_BYTES,
    MAX_REQUEST_FRAME_BYTES, MAX_SUBSCRIBER_EVENTS, PROTOCOL_VERSION, ResponseEnvelope,
    require_peer_owner,
};
use crate::linux::host::DesktopHost;

use super::methods::Actor;
use super::{ActorRequest, AttachParams, ControlContext, OutputFrame, Responder};

const ACTOR_QUEUE: usize = MAX_CONNECTIONS * 2;
const OUTPUT_QUEUE: usize = MAX_IN_FLIGHT_REQUESTS + MAX_SUBSCRIBER_EVENTS;

pub struct BoundControl {
    listener: UnixListener,
}

impl BoundControl {
    pub fn bind(path: &Path) -> io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(path);
            return Err(error);
        }
        listener.set_nonblocking(true)?;
        Ok(Self { listener })
    }
}

pub fn run(
    bound: BoundControl,
    host: DesktopHost,
    context: ControlContext,
    stopping: Arc<AtomicBool>,
) -> io::Result<()> {
    let (actor_tx, actor_rx) = mpsc::sync_channel(ACTOR_QUEUE);
    let connections = Arc::new(AtomicUsize::new(0));
    let next_connection_id = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let accept_stopping = stopping.clone();
    let accept_thread = thread::Builder::new()
        .name("vvland-control-accept".into())
        .spawn(move || {
            accept_loop(
                bound.listener,
                actor_tx,
                connections,
                next_connection_id,
                accept_stopping,
            )
        })?;

    let result = Actor::new(host, context, actor_rx, stopping.clone()).run();
    stopping.store(true, Ordering::Release);
    let _ = accept_thread.join();
    result
}

fn accept_loop(
    listener: UnixListener,
    actor: SyncSender<ActorRequest>,
    connections: Arc<AtomicUsize>,
    next_connection_id: Arc<std::sync::atomic::AtomicU64>,
    stopping: Arc<AtomicBool>,
) {
    while !stopping.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if connections
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                        (current < MAX_CONNECTIONS).then_some(current + 1)
                    })
                    .is_err()
                {
                    let _ = send_immediate(
                        stream,
                        ResponseEnvelope::error(
                            0,
                            IpcError::new("limit_exceeded", "too many control connections"),
                        ),
                    );
                    continue;
                }
                let actor = actor.clone();
                let connections = connections.clone();
                let connection_id = next_connection_id.fetch_add(1, Ordering::Relaxed);
                let _ = thread::Builder::new()
                    .name("vvland-control-connection".into())
                    .spawn(move || {
                        let _guard = ConnectionGuard(connections);
                        if let Err(error) = connection_loop(stream, actor, connection_id) {
                            eprintln!("vvland: control connection failed: {error}");
                        }
                    });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                eprintln!("vvland: control accept failed: {error}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

struct ConnectionGuard(Arc<AtomicUsize>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn connection_loop(
    stream: UnixStream,
    actor: SyncSender<ActorRequest>,
    connection_id: u64,
) -> io::Result<()> {
    require_peer_owner(&stream)?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let reader_stream = stream.try_clone()?;
    let writer_stream = stream;
    let (output_tx, output_rx) = mpsc::sync_channel(OUTPUT_QUEUE);
    let alive = Arc::new(AtomicBool::new(true));
    let writer_alive = alive.clone();
    let writer = thread::Builder::new()
        .name("vvland-control-writer".into())
        .spawn(move || writer_loop(writer_stream, output_rx, writer_alive))?;
    let in_flight = Arc::new(Mutex::new(HashSet::new()));
    let mut reader = SecureFrameReader::new(reader_stream);
    let mut hello_seen = false;

    loop {
        let frame = match reader.read_request_frame() {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                let _ = output_tx.try_send(OutputFrame::Response(ResponseEnvelope::error(
                    0,
                    IpcError::new("limit_exceeded", error.to_string()),
                )));
                break;
            }
            Err(error) => return Err(error),
        };
        let request: BorrowedRequest<'_> = match serde_json::from_slice(&frame) {
            Ok(request) => request,
            Err(error) => {
                let _ = output_tx.try_send(OutputFrame::Response(ResponseEnvelope::error(
                    0,
                    IpcError::new("invalid_request", format!("invalid request JSON: {error}")),
                )));
                continue;
            }
        };
        if request.version != PROTOCOL_VERSION {
            let _ = output_tx.try_send(OutputFrame::Response(ResponseEnvelope::error(
                request.id,
                IpcError::new(
                    "unsupported_version",
                    format!("control protocol version {} is required", PROTOCOL_VERSION),
                )
                .with_data(json!({"supported": [PROTOCOL_VERSION]})),
            )));
            break;
        }
        if !hello_seen && request.method != "hello" {
            let _ = output_tx.try_send(OutputFrame::Response(ResponseEnvelope::error(
                request.id,
                IpcError::new("invalid_state", "hello must be the first request"),
            )));
            break;
        }
        if hello_seen && request.method == "hello" {
            let _ = output_tx.try_send(OutputFrame::Response(ResponseEnvelope::error(
                request.id,
                IpcError::new("invalid_state", "hello may be sent only once"),
            )));
            continue;
        }
        hello_seen = true;
        let attach = if request.method == "attach" {
            let Some(params) = request.params else {
                let _ = output_tx.try_send(OutputFrame::Response(ResponseEnvelope::error(
                    request.id,
                    IpcError::new("invalid_params", "attach parameters are required"),
                )));
                continue;
            };
            match serde_json::from_str::<AttachParams>(params.get()) {
                Ok(params) => Some(params),
                Err(error) => {
                    let _ = output_tx.try_send(OutputFrame::Response(ResponseEnvelope::error(
                        request.id,
                        IpcError::new("invalid_params", error.to_string()),
                    )));
                    continue;
                }
            }
        } else {
            None
        };
        let params = if request.method == "attach" {
            Value::Null
        } else {
            match request.params {
                Some(params) => match serde_json::from_str(params.get()) {
                    Ok(params) => params,
                    Err(error) => {
                        let _ = output_tx.try_send(OutputFrame::Response(ResponseEnvelope::error(
                            request.id,
                            IpcError::new(
                                "invalid_request",
                                format!("invalid params JSON: {error}"),
                            ),
                        )));
                        continue;
                    }
                },
                None => Value::Null,
            }
        };
        {
            let mut ids = in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if ids.contains(&request.id) {
                let _ = output_tx.try_send(OutputFrame::Response(ResponseEnvelope::error(
                    request.id,
                    IpcError::new("duplicate_request_id", "request id is already in flight"),
                )));
                continue;
            }
            if ids.len() >= MAX_IN_FLIGHT_REQUESTS {
                let _ = output_tx.try_send(OutputFrame::Response(ResponseEnvelope::error(
                    request.id,
                    IpcError::new("limit_exceeded", "too many in-flight requests"),
                )));
                continue;
            }
            ids.insert(request.id);
        }
        let response = Responder::new(
            request.id,
            connection_id,
            output_tx.clone(),
            in_flight.clone(),
            alive.clone(),
        );
        match actor.try_send(ActorRequest {
            method: request.method.to_owned(),
            params,
            attach,
            response,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(request)) => request.response.error(IpcError::new(
                "limit_exceeded",
                "control actor queue is full",
            )),
            Err(TrySendError::Disconnected(request)) => request.response.error(IpcError::new(
                "compositor_exited",
                "desktop control actor stopped",
            )),
        }
    }
    alive.store(false, Ordering::Release);
    drop(output_tx);
    let _ = writer.join();
    Ok(())
}

#[derive(Deserialize)]
struct BorrowedRequest<'a> {
    version: u16,
    id: u64,
    method: &'a str,
    #[serde(default, borrow)]
    params: Option<&'a RawValue>,
}

struct SecureFrameReader {
    stream: UnixStream,
    pending: Zeroizing<Vec<u8>>,
}

impl SecureFrameReader {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            pending: Zeroizing::new(Vec::new()),
        }
    }

    fn read_request_frame(&mut self) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
        loop {
            if let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
                if newline > MAX_REQUEST_FRAME_BYTES {
                    return Err(frame_too_large());
                }
                let frame = Zeroizing::new(self.pending[..newline].to_vec());
                let remaining = Zeroizing::new(self.pending[newline + 1..].to_vec());
                self.pending.zeroize();
                self.pending = remaining;
                return Ok(Some(frame));
            }
            if self.pending.len() > MAX_REQUEST_FRAME_BYTES {
                return Err(frame_too_large());
            }
            let mut chunk = Zeroizing::new([0_u8; 8192]);
            let bytes = self.stream.read(&mut *chunk)?;
            if bytes == 0 {
                if self.pending.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "control request ended before its newline",
                ));
            }
            self.pending.extend_from_slice(&chunk[..bytes]);
        }
    }
}

fn frame_too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "control request exceeds the 1 MiB frame limit",
    )
}

fn writer_loop(
    mut stream: UnixStream,
    output: mpsc::Receiver<OutputFrame>,
    alive: Arc<AtomicBool>,
) -> io::Result<()> {
    let result = (|| {
        while let Ok(output) = output.recv() {
            let mut event_slot = None;
            let mut frame = match output {
                OutputFrame::Response(envelope) => {
                    let mut frame = serde_json::to_vec(&envelope)?;
                    if frame.len() > MAX_REPLY_FRAME_BYTES {
                        frame = serde_json::to_vec(&ResponseEnvelope::error(
                            envelope.id,
                            IpcError::new(
                                "limit_exceeded",
                                "control reply exceeds the 16 MiB limit",
                            ),
                        ))?;
                    }
                    frame
                }
                OutputFrame::Event(event, slot) => {
                    event_slot = Some(slot);
                    let frame = serde_json::to_vec(&event)?;
                    if frame.len() > MAX_REPLY_FRAME_BYTES {
                        continue;
                    }
                    frame
                }
            };
            frame.push(b'\n');
            stream.write_all(&frame)?;
            drop(event_slot);
        }
        Ok(())
    })();
    alive.store(false, Ordering::Release);
    result
}

fn send_immediate(mut stream: UnixStream, response: ResponseEnvelope) -> io::Result<()> {
    let mut frame = serde_json::to_vec(&response)?;
    frame.push(b'\n');
    stream.write_all(&frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(stream: &mut UnixStream, value: serde_json::Value) {
        serde_json::to_writer(&mut *stream, &value).unwrap();
        stream.write_all(b"\n").unwrap();
    }

    fn receive(reader: &mut BufReader<UnixStream>) -> ResponseEnvelope {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn pair() -> (
        UnixStream,
        BufReader<UnixStream>,
        mpsc::Receiver<ActorRequest>,
        thread::JoinHandle<io::Result<()>>,
    ) {
        let (server, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let reader = BufReader::new(client.try_clone().unwrap());
        let (tx, rx) = mpsc::sync_channel(8);
        let handle = thread::spawn(move || connection_loop(server, tx, 1));
        // Keep this mutable binding exercised so a failed pair setup is caught before a test's
        // first request and not reported as a protocol failure.
        client.flush().unwrap();
        (client, reader, rx, handle)
    }

    #[test]
    fn hello_is_mandatory_first_and_version_mismatch_closes() {
        let (mut client, mut reader, _requests, handle) = pair();
        send(
            &mut client,
            json!({"version": PROTOCOL_VERSION, "id": 1, "method": "ping", "params": {}}),
        );
        let response = receive(&mut reader);
        assert_eq!(response.error.unwrap().code, "invalid_state");
        drop(client);
        drop(reader);
        assert!(handle.join().unwrap().is_ok());

        let (mut client, mut reader, _requests, handle) = pair();
        send(
            &mut client,
            json!({"version": PROTOCOL_VERSION + 1, "id": 9, "method": "hello", "params": {}}),
        );
        let response = receive(&mut reader);
        assert_eq!(response.id, 9);
        assert_eq!(response.error.unwrap().code, "unsupported_version");
        drop(client);
        drop(reader);
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn a_second_hello_and_duplicate_in_flight_id_are_rejected() {
        let (mut client, mut reader, requests, handle) = pair();
        send(
            &mut client,
            json!({"version": PROTOCOL_VERSION, "id": 1, "method": "hello", "params": {}}),
        );
        requests.recv().unwrap().response.success(json!({}));
        assert!(receive(&mut reader).ok);

        send(
            &mut client,
            json!({"version": PROTOCOL_VERSION, "id": 2, "method": "hello", "params": {}}),
        );
        assert_eq!(receive(&mut reader).error.unwrap().code, "invalid_state");

        send(
            &mut client,
            json!({"version": PROTOCOL_VERSION, "id": 3, "method": "wait_frame", "params": {}}),
        );
        let pending = requests.recv().unwrap();
        send(
            &mut client,
            json!({"version": PROTOCOL_VERSION, "id": 3, "method": "ping", "params": {}}),
        );
        assert_eq!(
            receive(&mut reader).error.unwrap().code,
            "duplicate_request_id"
        );
        pending.response.success(json!({}));
        assert!(receive(&mut reader).ok);
        drop(client);
        drop(reader);
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn oversized_request_gets_a_typed_bounded_error() {
        let (mut client, mut reader, _requests, handle) = pair();
        client
            .write_all(&vec![b'x'; MAX_REQUEST_FRAME_BYTES + 1])
            .unwrap();
        client.write_all(b"\n").unwrap();
        let response = receive(&mut reader);
        assert_eq!(response.error.unwrap().code, "limit_exceeded");
        drop(client);
        drop(reader);
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn an_oversized_reply_is_replaced_by_a_bounded_error() {
        let response =
            ResponseEnvelope::success(7, json!({"value": "x".repeat(MAX_REPLY_FRAME_BYTES)}));
        let encoded = serde_json::to_vec(&response).unwrap();
        assert!(encoded.len() > MAX_REPLY_FRAME_BYTES);
        let replacement = ResponseEnvelope::error(
            response.id,
            IpcError::new("limit_exceeded", "control reply exceeds the 16 MiB limit"),
        );
        assert!(serde_json::to_vec(&replacement).unwrap().len() < MAX_REPLY_FRAME_BYTES);
    }

    #[test]
    fn attach_is_parsed_into_the_zeroizing_request_path_only() {
        let (mut client, mut reader, requests, handle) = pair();
        send(
            &mut client,
            json!({"version": PROTOCOL_VERSION, "id": 1, "method": "hello", "params": {}}),
        );
        requests.recv().unwrap().response.success(json!({}));
        assert!(receive(&mut reader).ok);

        send(
            &mut client,
            json!({
                "version": PROTOCOL_VERSION,
                "id": 2,
                "method": "attach",
                "params": {
                    "replace": false,
                    "vivid": {
                        "endpoint_control": "unix:/secret-endpoint",
                        "root_secret": "secret-root"
                    },
                    "desktop_target": false,
                    "bitrate": 8000000,
                    "fps": 30,
                    "secure_input": false
                }
            }),
        );
        let request = requests.recv().unwrap();
        assert_eq!(request.method, "attach");
        assert!(request.params.is_null());
        let attach = request.attach.as_ref().expect("typed attach parameters");
        assert_eq!(&**attach.vivid.endpoint_control, "unix:/secret-endpoint");
        assert_eq!(&**attach.vivid.root_secret, "secret-root");
        assert!(!format!("{request:?}").contains("secret-root"));
        request.response.success(json!({}));
        assert!(receive(&mut reader).ok);
        drop(client);
        drop(reader);
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn secure_reader_preserves_pipelined_frames_without_a_generic_buffer() {
        let (server, mut client) = UnixStream::pair().unwrap();
        client.write_all(b"first\nsecond\n").unwrap();
        let mut reader = SecureFrameReader::new(server);
        assert_eq!(&*reader.read_request_frame().unwrap().unwrap(), b"first");
        assert_eq!(&*reader.read_request_frame().unwrap().unwrap(), b"second");
    }
}
