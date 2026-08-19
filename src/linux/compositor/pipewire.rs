//! PipeWire capture for the Weston backend.
//!
//! Weston renders a mirrored PipeWire output (`weston.pipewire`). Node names are not unique, so
//! capture first follows each candidate's `client.id` to the kernel-attested `pipewire.sec.pid`,
//! selects the node owned by this vvland's Weston child, and targets its `object.serial`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::rc::Rc;
use std::sync::{Arc, Once, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;

use crate::linux::video::{CaptureSource, LatestFrame, RawFrame, RawPixelFormat};

pub const PIPEWIRE_NODE: &str = "weston.pipewire";

const PIPEWIRE_READY_TIMEOUT: Duration = Duration::from_secs(5);
const PIPEWIRE_TARGET_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const MAX_CAPTURE_BYTES: usize = 8192 * 8192 * 4;
static PIPEWIRE_INIT: Once = Once::new();

pub struct VideoCapture {
    latest: Arc<LatestFrame>,
    stop: pw::channel::Sender<()>,
    join: Option<thread::JoinHandle<()>>,
}

impl VideoCapture {
    pub fn start(
        node_name: &str,
        owner_pid: u32,
        width: u32,
        height: u32,
        fps: u32,
        origin: Instant,
    ) -> io::Result<Self> {
        retry_missing_pipewire_target(
            node_name,
            PIPEWIRE_READY_TIMEOUT,
            PIPEWIRE_TARGET_RETRY_INTERVAL,
            |remaining| {
                let resolution_started = Instant::now();
                let target = resolve_pipewire_target(node_name, owner_pid, remaining)?;
                let remaining = remaining.saturating_sub(resolution_started.elapsed());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("PipeWire target {node_name} resolution timed out"),
                    ));
                }
                Self::start_once(node_name, &target, width, height, fps, origin, remaining)
            },
        )
    }

    fn start_once(
        node_name: &str,
        target: &str,
        width: u32,
        height: u32,
        fps: u32,
        origin: Instant,
        ready_timeout: Duration,
    ) -> io::Result<Self> {
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let (stop, stop_rx) = pw::channel::channel();
        let latest = Arc::new(LatestFrame::new());
        let thread_latest = latest.clone();
        let thread_target = target.to_owned();
        let join = thread::Builder::new()
            .name("vvland-pipewire-video".into())
            .spawn(move || {
                let result = capture_thread(
                    &thread_target,
                    width,
                    height,
                    fps,
                    origin,
                    thread_latest.clone(),
                    ready_tx.clone(),
                    stop_rx,
                );
                if let Err(error) = &result {
                    let _ = ready_tx.try_send(Err(error.to_string()));
                }
                thread_latest.close(result.err().map(|error| error.to_string()));
            })?;

        match ready_rx.recv_timeout(ready_timeout) {
            Ok(Ok(())) => Ok(Self {
                latest,
                stop,
                join: Some(join),
            }),
            Ok(Err(error)) => {
                let _ = stop.send(());
                let _ = join.join();
                Err(io::Error::other(error))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = stop.send(());
                let _ = join.join();
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{node_name} did not negotiate a video format"),
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = join.join();
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "PipeWire capture exited before negotiation",
                ))
            }
        }
    }
}

fn pipewire_target_not_found(error: &io::Error) -> bool {
    error
        .to_string()
        .to_ascii_lowercase()
        .contains("target not found")
}

fn retry_missing_pipewire_target<T>(
    node_name: &str,
    timeout: Duration,
    retry_interval: Duration,
    mut attempt: impl FnMut(Duration) -> io::Result<T>,
) -> io::Result<T> {
    let deadline = Instant::now() + timeout;
    let mut last_target_error = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let error = last_target_error.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("PipeWire target {node_name} was not ready"),
                )
            });
            return Err(pipewire_target_timeout(node_name, timeout, error));
        }
        match attempt(remaining) {
            Ok(value) => return Ok(value),
            Err(error) if pipewire_target_not_found(&error) => {
                last_target_error = Some(error);
                thread::sleep(
                    retry_interval.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(error) => return Err(error),
        }
    }
}

fn pipewire_target_timeout(node_name: &str, timeout: Duration, error: io::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "PipeWire target {node_name} did not appear within {} ms ({error})",
            timeout.as_millis()
        ),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NodeCandidate {
    client_id: u32,
    serial: u64,
}

#[derive(Default)]
struct TargetCatalog {
    clients: HashMap<u32, u32>,
    nodes: Vec<NodeCandidate>,
}

impl TargetCatalog {
    fn observe(
        &mut self,
        object: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
        node_name: &str,
    ) {
        let Some(properties) = object.props else {
            return;
        };
        match object.type_ {
            pw::types::ObjectType::Client => {
                if let Some(pid) = properties
                    .get(*pw::keys::SEC_PID)
                    .and_then(|value| value.parse::<u32>().ok())
                {
                    self.clients.insert(object.id, pid);
                }
            }
            pw::types::ObjectType::Node
                if properties.get(*pw::keys::NODE_NAME) == Some(node_name) =>
            {
                let candidate = properties
                    .get(*pw::keys::CLIENT_ID)
                    .and_then(|value| value.parse::<u32>().ok())
                    .zip(
                        properties
                            .get(*pw::keys::OBJECT_SERIAL)
                            .and_then(|value| value.parse::<u64>().ok()),
                    )
                    .map(|(client_id, serial)| NodeCandidate { client_id, serial });
                if let Some(candidate) = candidate {
                    self.nodes.push(candidate);
                }
            }
            _ => {}
        }
    }

    fn target_for(&self, owner_pid: u32) -> io::Result<String> {
        let mut matches = self
            .nodes
            .iter()
            .filter(|node| self.clients.get(&node.client_id).copied() == Some(owner_pid));
        let Some(target) = matches.next() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "PipeWire target not found: {PIPEWIRE_NODE} owned by Weston PID {owner_pid}"
                ),
            ));
        };
        if matches.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "multiple {PIPEWIRE_NODE} nodes belong to Weston PID {owner_pid}; refusing ambiguous capture"
                ),
            ));
        }
        Ok(target.serial.to_string())
    }
}

fn resolve_pipewire_target(
    node_name: &str,
    owner_pid: u32,
    timeout: Duration,
) -> io::Result<String> {
    init_pipewire();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pw_error)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(pw_error)?;
    let core = context.connect_rc(None).map_err(pw_error)?;
    let registry = core.get_registry_rc().map_err(pw_error)?;
    let catalog = Rc::new(RefCell::new(TargetCatalog::default()));
    let listener_catalog = Rc::clone(&catalog);
    let listener_node_name = node_name.to_owned();
    let registry_listener = registry
        .add_listener_local()
        .global(move |object| {
            listener_catalog
                .borrow_mut()
                .observe(object, &listener_node_name);
        })
        .register();

    let expected_sequence = Rc::new(Cell::new(None));
    let listener_sequence = Rc::clone(&expected_sequence);
    let done_loop = mainloop.clone();
    let core_error = Rc::new(RefCell::new(None::<String>));
    let listener_error = Rc::clone(&core_error);
    let error_loop = mainloop.clone();
    let core_listener = core
        .add_listener_local()
        .done(move |id, sequence| {
            if id == pw::core::PW_ID_CORE && listener_sequence.get() == Some(sequence.seq()) {
                done_loop.quit();
            }
        })
        .error(move |_, _, result, message| {
            *listener_error.borrow_mut() =
                Some(format!("PipeWire registry error {result}: {message}"));
            error_loop.quit();
        })
        .register();
    let sequence = core.sync(0).map_err(pw_error)?;
    expected_sequence.set(Some(sequence.seq()));

    let timed_out = Rc::new(Cell::new(false));
    let timer_timed_out = Rc::clone(&timed_out);
    let timer_loop = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        timer_timed_out.set(true);
        timer_loop.quit();
    });
    timer
        .update_timer(Some(timeout), None)
        .into_result()
        .map_err(pw_error)?;

    mainloop.run();
    drop(timer);
    drop(core_listener);
    drop(registry_listener);
    if timed_out.get() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("PipeWire registry did not answer while resolving {node_name}"),
        ));
    }
    if let Some(error) = core_error.borrow_mut().take() {
        return Err(io::Error::other(error));
    }
    catalog.borrow().target_for(owner_pid)
}

impl CaptureSource for VideoCapture {
    fn latest(&self) -> Arc<LatestFrame> {
        Arc::clone(&self.latest)
    }
}

impl Drop for VideoCapture {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct CaptureData {
    format: spa::param::video::VideoInfoRaw,
    negotiated: Option<RawPixelFormat>,
    expected_width: u32,
    expected_height: u32,
    origin: Instant,
    latest: Arc<LatestFrame>,
    ready: Option<mpsc::SyncSender<Result<(), String>>>,
    quit: pw::main_loop::MainLoopRc,
}

#[allow(clippy::too_many_arguments)]
fn capture_thread(
    node_name: &str,
    width: u32,
    height: u32,
    fps: u32,
    origin: Instant,
    latest: Arc<LatestFrame>,
    ready: mpsc::SyncSender<Result<(), String>>,
    stop: pw::channel::Receiver<()>,
) -> io::Result<()> {
    init_pipewire();

    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pw_error)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(pw_error)?;
    let core = context.connect_rc(None).map_err(pw_error)?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "vvland-video-capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            *pw::keys::TARGET_OBJECT => node_name,
        },
    )
    .map_err(pw_error)?;

    let data = CaptureData {
        format: Default::default(),
        negotiated: None,
        expected_width: width,
        expected_height: height,
        origin,
        latest,
        ready: Some(ready),
        quit: mainloop.clone(),
    };
    let listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, data, _, new| {
            if let pw::stream::StreamState::Error(error) = new {
                if let Some(ready) = data.ready.take() {
                    let _ = ready.try_send(Err(format!("PipeWire stream error: {error}")));
                }
                data.latest
                    .close(Some(format!("PipeWire stream error: {error}")));
                data.quit.quit();
            }
        })
        .param_changed(|_, data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let valid = (|| -> Result<RawPixelFormat, String> {
                let (media_type, subtype) = spa::param::format_utils::parse_format(param)
                    .map_err(|error| format!("invalid PipeWire format: {error:?}"))?;
                if media_type != spa::param::format::MediaType::Video
                    || subtype != spa::param::format::MediaSubtype::Raw
                {
                    return Err("PipeWire node did not provide raw video".into());
                }
                data.format
                    .parse(param)
                    .map_err(|error| format!("invalid raw video format: {error:?}"))?;
                let size = data.format.size();
                if size.width != data.expected_width || size.height != data.expected_height {
                    return Err(format!(
                        "PipeWire negotiated {}x{}, expected {}x{}",
                        size.width, size.height, data.expected_width, data.expected_height
                    ));
                }
                match data.format.format() {
                    spa::param::video::VideoFormat::BGRx => Ok(RawPixelFormat::Bgrx),
                    spa::param::video::VideoFormat::RGBx => Ok(RawPixelFormat::Rgbx),
                    other => Err(format!("unsupported PipeWire pixel format {other:?}")),
                }
            })();
            match valid {
                Ok(format) => {
                    data.negotiated = Some(format);
                    if let Some(ready) = data.ready.take() {
                        let _ = ready.try_send(Ok(()));
                    }
                }
                Err(error) => {
                    if let Some(ready) = data.ready.take() {
                        let _ = ready.try_send(Err(error.clone()));
                    }
                    data.latest.close(Some(error));
                    data.quit.quit();
                }
            }
        })
        .process(|stream, data| {
            let Some(format) = data.negotiated else {
                return;
            };
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            match copy_frame(&mut buffer, data, format) {
                Ok(frame) => data.latest.replace(frame),
                Err(error) => {
                    data.latest.close(Some(error.to_string()));
                    data.quit.quit();
                }
            }
        })
        .register()
        .map_err(pw_error)?;

    let quit_loop = mainloop.clone();
    let attached_stop = stop.attach(mainloop.loop_(), move |()| quit_loop.quit());
    let values = capture_format(width, height, fps)?;
    let mut params = [Pod::from_bytes(&values).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid serialized PipeWire format",
        )
    })?];
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::DONT_RECONNECT,
            &mut params,
        )
        .map_err(pw_error)?;
    mainloop.run();
    drop(attached_stop);
    drop(listener);
    Ok(())
}

fn init_pipewire() {
    PIPEWIRE_INIT.call_once(pw::init);
}

fn capture_format(width: u32, height: u32, fps: u32) -> io::Result<Vec<u8>> {
    let object = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::RGBx,
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Rectangle,
            pw::spa::utils::Rectangle { width, height }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Fraction,
            pw::spa::utils::Fraction { num: 0, denom: 1 }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoMaxFramerate,
            Fraction,
            pw::spa::utils::Fraction { num: fps, denom: 1 }
        ),
    );
    Ok(pw::spa::pod::serialize::PodSerializer::serialize(
        io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(object),
    )
    .map_err(|error| io::Error::other(format!("PipeWire format serialization failed: {error:?}")))?
    .0
    .into_inner())
}

fn copy_frame(
    buffer: &mut pw::buffer::Buffer<'_>,
    data: &CaptureData,
    format: RawPixelFormat,
) -> io::Result<RawFrame> {
    let Some(plane) = buffer.datas_mut().first_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PipeWire video buffer has no plane",
        ));
    };
    let data_type = plane.type_();
    if data_type != spa::buffer::DataType::MemFd && data_type != spa::buffer::DataType::MemPtr {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported PipeWire video buffer type {data_type:?}"),
        ));
    }
    if plane
        .chunk()
        .flags()
        .contains(spa::buffer::ChunkFlags::CORRUPTED)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PipeWire video buffer is marked corrupted",
        ));
    }
    let offset = usize::try_from(plane.chunk().offset()).unwrap_or(usize::MAX);
    let size = usize::try_from(plane.chunk().size()).unwrap_or(usize::MAX);
    let stride = plane.chunk().stride();
    if stride <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PipeWire video buffer has a non-positive stride",
        ));
    }
    let stride = usize::try_from(stride).unwrap_or(usize::MAX);
    let Some(mapped) = plane.data() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PipeWire memfd buffer is not mapped",
        ));
    };
    let (row_bytes, height, output_len) = validate_plane_layout(
        offset,
        size,
        stride,
        data.expected_width,
        data.expected_height,
        mapped.len(),
    )?;
    let mut pixels = vec![0_u8; output_len];
    for row in 0..height {
        let source_start = offset + row * stride;
        let target_start = row * row_bytes;
        pixels[target_start..target_start + row_bytes]
            .copy_from_slice(&mapped[source_start..source_start + row_bytes]);
    }
    let pts_us = i64::try_from(data.origin.elapsed().as_micros()).unwrap_or(i64::MAX);
    Ok(RawFrame {
        format,
        width: data.expected_width,
        height: data.expected_height,
        pts_us,
        data: Arc::from(pixels),
    })
}

fn validate_plane_layout(
    offset: usize,
    size: usize,
    stride: usize,
    width: u32,
    height: u32,
    mapped_len: usize,
) -> io::Result<(usize, usize, usize)> {
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame row size overflow"))?;
    let height = usize::try_from(height)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame height exceeds usize"))?;
    let required = height
        .checked_sub(1)
        .and_then(|rows| rows.checked_mul(stride))
        .and_then(|bytes| bytes.checked_add(row_bytes))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame plane size overflow"))?;
    let end = offset
        .checked_add(required)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame plane end overflow"))?;
    let output_len = row_bytes
        .checked_mul(height)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame copy size overflow"))?;
    if width == 0
        || height == 0
        || stride < row_bytes
        || required > size
        || required > MAX_CAPTURE_BYTES
        || output_len > MAX_CAPTURE_BYTES
        || end > mapped_len
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PipeWire video plane has invalid bounds or stride",
        ));
    }
    Ok((row_bytes, height, output_len))
}

fn pw_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("PipeWire: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Config;
    use crate::linux::compositor::CompositorEnvironment;
    use crate::linux::compositor::weston::WestonSession;
    use clap::Parser;

    #[test]
    fn retries_only_missing_pipewire_targets() {
        let missing = io::Error::other("PipeWire stream error: target not found");
        let denied = io::Error::other("PipeWire stream error: permission denied");
        assert!(pipewire_target_not_found(&missing));
        assert!(!pipewire_target_not_found(&denied));

        let timeout = pipewire_target_timeout("weston.pipewire", Duration::from_secs(5), missing);
        assert_eq!(timeout.kind(), io::ErrorKind::TimedOut);
        assert!(timeout.to_string().contains("weston.pipewire"));
        assert!(timeout.to_string().contains("target not found"));
    }

    #[test]
    fn capture_format_matches_weston_variable_framerate_contract() {
        let values = capture_format(1920, 1080, 30).unwrap();
        let pod = Pod::from_bytes(&values).unwrap();
        let mut format = spa::param::video::VideoInfoRaw::default();
        format.parse(pod).unwrap();

        assert_eq!(format.size().width, 1920);
        assert_eq!(format.size().height, 1080);
        assert_eq!(format.framerate().num, 0);
        assert_eq!(format.framerate().denom, 1);
        assert_eq!(format.max_framerate().num, 30);
        assert_eq!(format.max_framerate().denom, 1);
    }

    #[test]
    fn waits_for_delayed_pipewire_target_registration() {
        let mut attempts = 0;
        let value = retry_missing_pipewire_target(
            "weston.pipewire",
            Duration::from_secs(1),
            Duration::ZERO,
            |_| {
                attempts += 1;
                if attempts < 3 {
                    Err(io::Error::other("PipeWire stream error: target not found"))
                } else {
                    Ok(42)
                }
            },
        )
        .unwrap();
        assert_eq!(value, 42);
        assert_eq!(attempts, 3);

        let mut attempts = 0;
        let error = retry_missing_pipewire_target(
            "weston.pipewire",
            Duration::from_secs(1),
            Duration::ZERO,
            |_| {
                attempts += 1;
                Err::<(), _>(io::Error::other("PipeWire stream error: permission denied"))
            },
        )
        .unwrap_err();
        assert_eq!(attempts, 1);
        assert_eq!(error.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn selects_weston_nodes_by_kernel_attested_client_pid() {
        let catalog = TargetCatalog {
            clients: HashMap::from([(95, 1_008_670), (102, 1_008_569)]),
            nodes: vec![
                NodeCandidate {
                    client_id: 102,
                    serial: 1692,
                },
                NodeCandidate {
                    client_id: 95,
                    serial: 1698,
                },
            ],
        };
        assert_eq!(catalog.target_for(1_008_569).unwrap(), "1692");
        assert_eq!(catalog.target_for(1_008_670).unwrap(), "1698");
        assert!(
            catalog
                .target_for(42)
                .unwrap_err()
                .to_string()
                .contains("target not found")
        );

        let ambiguous = TargetCatalog {
            clients: HashMap::from([(95, 1_008_670)]),
            nodes: vec![
                NodeCandidate {
                    client_id: 95,
                    serial: 1698,
                },
                NodeCandidate {
                    client_id: 95,
                    serial: 1700,
                },
            ],
        };
        assert_eq!(
            ambiguous.target_for(1_008_670).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn validates_pipewire_stride_offset_and_plane_size() {
        assert_eq!(
            validate_plane_layout(8, 24, 12, 2, 2, 32).unwrap(),
            (8, 2, 16)
        );
        assert!(validate_plane_layout(8, 15, 12, 2, 2, 32).is_err());
        assert!(validate_plane_layout(30, 24, 12, 2, 2, 32).is_err());
        assert!(validate_plane_layout(0, 16, 4, 2, 2, 16).is_err());
    }

    /// Start a headless Weston at the given size, for tests that need a live PipeWire node.
    #[cfg(test)]
    fn headless_weston(width: u32, height: u32) -> WestonSession {
        let config = Config::parse_from([
            "vvland",
            "--compositor=weston",
            "--backend=headless",
            "--renderer=gl",
            "--xwayland=off",
            "--no-audio",
        ]);
        WestonSession::start(
            &config,
            CompositorEnvironment {
                width,
                height,
                pulse_server: None,
                pulse_sink: None,
                app_window: None,
            },
        )
        .unwrap()
    }

    #[test]
    #[ignore = "requires live Weston and PipeWire services"]
    fn captures_live_weston_pipewire_output() {
        // Owns its Weston so the test does not silently depend on a sibling test, or on an
        // externally running compositor, to provide the node.
        let weston = headless_weston(1920, 1080);
        let capture = VideoCapture::start(
            "weston.pipewire",
            weston.pid(),
            1920,
            1080,
            30,
            Instant::now(),
        )
        .unwrap();
        let mut serial = 0;
        let frame = capture
            .latest()
            .wait_next(&mut serial, Duration::from_secs(2))
            .unwrap();
        assert!(frame.is_some());
    }

    #[test]
    #[ignore = "requires live Weston and PipeWire services"]
    fn weston_session_captures_visible_desktop() {
        let mut weston = headless_weston(1920, 1080);
        weston.launch_program(&["weston-terminal".into()]).unwrap();
        let capture = VideoCapture::start(
            "weston.pipewire",
            weston.pid(),
            1920,
            1080,
            30,
            Instant::now(),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut serial = 0;
        let mut visible = false;
        let mut first_pts_us = None;
        let mut buffered_us = 0;
        let latest = capture.latest();
        while Instant::now() < deadline {
            if let Some(frame) = latest
                .wait_next(&mut serial, Duration::from_millis(500))
                .unwrap()
            {
                let first_pts_us = *first_pts_us.get_or_insert(frame.pts_us);
                buffered_us = frame.pts_us.saturating_sub(first_pts_us);
                visible |= frame
                    .data
                    .chunks_exact(4)
                    .any(|pixel| pixel[..3] != [0, 0, 0]);
                if visible && buffered_us >= 100_000 {
                    break;
                }
            }
        }
        assert!(
            visible,
            "Weston's mirrored desktop remained completely black"
        );
        assert!(
            buffered_us >= 100_000,
            "Weston's mirrored desktop did not produce enough startup frames"
        );
    }

    #[test]
    #[ignore = "requires two live Weston sessions and PipeWire services"]
    fn concurrent_weston_sessions_capture_their_own_pipewire_nodes() {
        let weston_a = headless_weston(800, 600);
        let weston_b = headless_weston(1024, 768);
        let capture_a =
            VideoCapture::start(PIPEWIRE_NODE, weston_a.pid(), 800, 600, 30, Instant::now())
                .unwrap();
        let capture_b =
            VideoCapture::start(PIPEWIRE_NODE, weston_b.pid(), 1024, 768, 30, Instant::now())
                .unwrap();

        let frame_a = capture_a
            .latest()
            .wait_next(&mut 0, Duration::from_secs(2))
            .unwrap()
            .unwrap();
        let frame_b = capture_b
            .latest()
            .wait_next(&mut 0, Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!((frame_a.width, frame_a.height), (800, 600));
        assert_eq!((frame_b.width, frame_b.height), (1024, 768));
    }
}
