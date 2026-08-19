use std::collections::VecDeque;
use std::ffi::{CString, OsStr, OsString, c_void};
use std::io;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;
use vivid_protocol::media::{self, AUDIO_PACKETIZATION_OPUS};

const AUDIO_RATE: u32 = 48_000;
const AUDIO_CHANNELS: u16 = 2;

/// The Opus track parameters the 1.5 `AudioConfiguration` is built from.
///
/// The Vivid 1.1 `AudioSourceSpec` moved into the immutable track configuration; this local
/// struct keeps the producer-side encoder wiring independent of the wire types.
#[derive(Debug, Clone)]
pub struct AudioTrackSpec {
    pub extradata: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u8,
    pub max_access_unit_bytes: u32,
}
const OPUS_SAMPLES: usize = 960;
const OPUS_FRAME_SAMPLES: usize = OPUS_SAMPLES * AUDIO_CHANNELS as usize;
const AUDIO_DELAY_US: u64 = 100_000;
const OPUS_FRAME_US: u64 = 20_000;
const MAX_CATCHUP_FRAMES: usize = 12;
const CAPTURE_BUFFER_CAP_SAMPLES: usize = 48_000 * 2 * 2 / 5;
pub const MAX_AUDIO_ACCESS_UNIT: u32 = 65_536;
const PACTL: &str = "pactl";

pub struct PulseSink {
    server: OsString,
    pactl: OsString,
    module_id: u32,
    sink_name: OsString,
    monitor_name: String,
}

impl PulseSink {
    pub fn create(server: &OsStr, identity: &crate::producer::ProductIdentity) -> io::Result<Self> {
        Self::create_with_pactl(server, OsStr::new(PACTL), identity)
    }

    fn create_with_pactl(
        server: &OsStr,
        pactl: &OsStr,
        identity: &crate::producer::ProductIdentity,
    ) -> io::Result<Self> {
        let sink_name = format!("{}_{}", identity.slug, std::process::id());
        let mut command = Command::new(pactl);
        let output = command
            .arg("--server")
            .arg(server)
            .args([
                "load-module",
                "module-null-sink",
                &format!("sink_name={sink_name}"),
                "rate=48000",
                "channels=2",
                "channel_map=front-left,front-right",
                &format!(
                    "sink_properties=device.description={}",
                    identity.display_name
                ),
            ])
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "pactl could not create the private audio sink: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let module_id = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid pactl module ID"))?;
        Ok(Self {
            server: server.to_owned(),
            pactl: pactl.to_owned(),
            module_id,
            sink_name: OsString::from(&sink_name),
            monitor_name: format!("{sink_name}.monitor"),
        })
    }

    pub fn server(&self) -> &OsStr {
        &self.server
    }

    pub fn sink_name(&self) -> &OsStr {
        &self.sink_name
    }

    pub fn monitor_name(&self) -> &str {
        &self.monitor_name
    }
}

impl Drop for PulseSink {
    fn drop(&mut self) {
        let mut command = Command::new(&self.pactl);
        let _ = command
            .arg("--server")
            .arg(&self.server)
            .args(["unload-module", &self.module_id.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

pub fn resolve_pulse_server(override_server: Option<&str>) -> io::Result<OsString> {
    resolve_pulse_server_with_pactl(override_server, OsStr::new(PACTL))
}

fn resolve_pulse_server_with_pactl(
    override_server: Option<&str>,
    pactl: &OsStr,
) -> io::Result<OsString> {
    let mut command = Command::new(pactl);
    if let Some(server) = override_server {
        command.arg("--server").arg(server);
    }
    let output = command
        .arg("info")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        return Err(io::Error::other(if detail.is_empty() {
            "pactl could not reach the Pulse compatibility server".to_owned()
        } else {
            format!("pactl could not reach the Pulse compatibility server: {detail}")
        }));
    }
    if let Some(server) = override_server {
        return Ok(OsString::from(server));
    }
    parse_pulse_server_info(&output.stdout).map(OsString::from)
}

fn parse_pulse_server_info(output: &[u8]) -> io::Result<&str> {
    let output = std::str::from_utf8(output).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "pactl info returned non-UTF-8 server information",
        )
    })?;
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Server String:").map(str::trim))
        .filter(|server| !server.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "pactl info did not report a Pulse server string",
            )
        })
}

pub struct EncodedAudio {
    pub pts_us: i64,
    pub dts_us: i64,
    pub duration_us: u64,
    packet: ffmpeg::Packet,
}

impl EncodedAudio {
    pub fn data(&self) -> &[u8] {
        self.packet.data().unwrap_or_default()
    }

    fn data_len(&self) -> usize {
        self.packet.size()
    }
}

struct AudioQueueState {
    packets: VecDeque<EncodedAudio>,
    bytes: usize,
    max_packets: usize,
    max_bytes: usize,
    closed: Option<String>,
    dropped_packets: u64,
    peak_packets: usize,
    peak_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioQueueSnapshot {
    pub packets: usize,
    pub bytes: usize,
    pub queued_duration_us: u64,
    pub dropped_packets: u64,
    pub peak_packets: usize,
    pub peak_bytes: usize,
}

pub struct AudioQueue {
    state: Mutex<AudioQueueState>,
    changed: Condvar,
}

impl AudioQueue {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(AudioQueueState {
                packets: VecDeque::new(),
                bytes: 0,
                max_packets: 1,
                max_bytes: usize::try_from(MAX_AUDIO_ACCESS_UNIT).unwrap_or(4096),
                closed: None,
                dropped_packets: 0,
                peak_packets: 0,
                peak_bytes: 0,
            }),
            changed: Condvar::new(),
        }
    }

    pub fn configure_limits(&self, max_packets: usize, max_bytes: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.max_packets = max_packets.max(1);
        state.max_bytes = max_bytes.max(1);
        while state.packets.len() > state.max_packets || state.bytes > state.max_bytes {
            let Some(old) = state.packets.pop_front() else {
                break;
            };
            state.bytes = state.bytes.saturating_sub(old.data_len());
            state.dropped_packets = state.dropped_packets.saturating_add(1);
        }
    }

    fn push(&self, packet: EncodedAudio) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while state.packets.len() >= state.max_packets
            || state.bytes.saturating_add(packet.data_len()) > state.max_bytes
        {
            let Some(old) = state.packets.pop_front() else {
                break;
            };
            state.bytes = state.bytes.saturating_sub(old.data_len());
            state.dropped_packets = state.dropped_packets.saturating_add(1);
        }
        state.bytes = state.bytes.saturating_add(packet.data_len());
        state.packets.push_back(packet);
        state.peak_packets = state.peak_packets.max(state.packets.len());
        state.peak_bytes = state.peak_bytes.max(state.bytes);
        self.changed.notify_one();
    }

    fn close(&self, error: Option<String>) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .closed = Some(error.unwrap_or_else(|| "audio capture stopped".into()));
        self.changed.notify_all();
    }

    pub fn pop(&self, timeout: Duration) -> io::Result<Option<EncodedAudio>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.packets.is_empty() && state.closed.is_none() {
            state = self
                .changed
                .wait_timeout(state, timeout)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        if let Some(packet) = state.packets.pop_front() {
            state.bytes = state.bytes.saturating_sub(packet.data_len());
            return Ok(Some(packet));
        }
        if let Some(error) = &state.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.clone()));
        }
        Ok(None)
    }

    pub fn clear(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.dropped_packets = state
            .dropped_packets
            .saturating_add(u64::try_from(state.packets.len()).unwrap_or(u64::MAX));
        state.packets.clear();
        state.bytes = 0;
    }

    pub fn snapshot(&self) -> AudioQueueSnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let queued_duration_us = match (state.packets.front(), state.packets.back()) {
            (Some(first), Some(last)) => last
                .pts_us
                .saturating_add(i64::try_from(last.duration_us).unwrap_or(i64::MAX))
                .saturating_sub(first.pts_us)
                .try_into()
                .unwrap_or(0),
            _ => 0,
        };
        AudioQueueSnapshot {
            packets: state.packets.len(),
            bytes: state.bytes,
            queued_duration_us,
            dropped_packets: state.dropped_packets,
            peak_packets: state.peak_packets,
            peak_bytes: state.peak_bytes,
        }
    }
}

struct CaptureBufferState {
    samples: VecDeque<f32>,
    closed: Option<String>,
}

struct CaptureBuffer {
    state: Mutex<CaptureBufferState>,
    changed: Condvar,
}

impl CaptureBuffer {
    fn new() -> Self {
        Self {
            state: Mutex::new(CaptureBufferState {
                samples: VecDeque::new(),
                closed: None,
            }),
            changed: Condvar::new(),
        }
    }

    fn push_samples(&self, samples: &[f32]) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let retained = samples.len().min(CAPTURE_BUFFER_CAP_SAMPLES);
        let incoming = &samples[samples.len().saturating_sub(retained)..];
        let overflow = state
            .samples
            .len()
            .saturating_add(incoming.len())
            .saturating_sub(CAPTURE_BUFFER_CAP_SAMPLES);
        let discard = overflow.min(state.samples.len());
        state.samples.drain(..discard);
        state.samples.extend(incoming.iter().copied());
        self.changed.notify_one();
    }

    fn take_up_to(&self, count: usize) -> Vec<f32> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let count = count.min(state.samples.len());
        state.samples.drain(..count).collect()
    }

    fn close(&self, reason: impl Into<String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed.is_none() {
            state.closed = Some(reason.into());
        }
        self.changed.notify_all();
    }

    fn closed(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .closed
            .clone()
    }
}

#[derive(Default)]
struct GainState {
    muted: bool,
    volume: f32,
}

#[derive(Clone)]
pub struct AudioGain(Arc<Mutex<GainState>>);

impl AudioGain {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(GainState {
            muted: false,
            volume: 1.0,
        })))
    }

    pub fn toggle_mute(&self) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.muted = !state.muted;
        state.muted
    }

    pub fn adjust(&self, delta: f32) -> f32 {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.volume = (state.volume + delta).clamp(0.0, 2.0);
        state.volume
    }

    pub fn status(&self) -> (bool, f32) {
        let state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (state.muted, state.volume)
    }
}

pub struct AudioPipeline {
    pub spec: AudioTrackSpec,
    pub queue: Arc<AudioQueue>,
    pub gain: AudioGain,
    stop: Arc<AtomicBool>,
    joins: Vec<thread::JoinHandle<()>>,
}

impl AudioPipeline {
    pub fn start(
        monitor: &str,
        server: &OsStr,
        origin: Instant,
        identity: &crate::producer::ProductIdentity,
    ) -> io::Result<Self> {
        initialize_ffmpeg();
        let (opus, spec) = create_opus_source()?;
        let queue = Arc::new(AudioQueue::new());
        let gain = AudioGain::new();
        let stop = Arc::new(AtomicBool::new(false));
        let buffer = Arc::new(CaptureBuffer::new());

        let monitor = monitor.to_owned();
        let server = server.to_owned();
        let capture_buffer = buffer.clone();
        let capture_stop = stop.clone();
        let capture_join = thread::Builder::new()
            .name(format!("{}-pulse-capture", identity.slug))
            .spawn(move || {
                if let Err(error) =
                    capture_thread(&monitor, &server, &capture_buffer, &capture_stop)
                {
                    if !capture_stop.load(Ordering::Acquire) {
                        capture_buffer.close(error.to_string());
                    }
                }
            })?;

        let pacer_buffer = buffer;
        let pacer_queue = queue.clone();
        let pacer_gain = gain.clone();
        let pacer_stop = stop.clone();
        let pacer_join = match thread::Builder::new()
            .name(format!("{}-opus-pacer", identity.slug))
            .spawn(move || {
                if let Err(error) = pacer_thread(
                    origin,
                    opus,
                    &pacer_buffer,
                    &pacer_queue,
                    &pacer_gain,
                    &pacer_stop,
                ) {
                    pacer_queue.close(Some(error.to_string()));
                }
            }) {
            Ok(join) => join,
            Err(error) => {
                stop.store(true, Ordering::Release);
                let _ = capture_join.join();
                return Err(error);
            }
        };

        Ok(Self {
            spec,
            queue,
            gain,
            stop,
            joins: vec![capture_join, pacer_join],
        })
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for join in self.joins.drain(..) {
            let _ = join.join();
        }
    }
}

fn initialize_ffmpeg() {
    static FFMPEG_INIT: Once = Once::new();
    FFMPEG_INIT.call_once(|| {
        let _ = ffmpeg::init();
        ffmpeg::log::set_level(ffmpeg::log::Level::Warning);
    });
}

fn create_opus_source() -> io::Result<(ffmpeg::encoder::Audio, AudioTrackSpec)> {
    let opus = create_opus_encoder()?;
    // SAFETY: the opened encoder context lives for the duration of this copy.
    let pre_skip = unsafe {
        u16::try_from((*opus.as_ptr()).initial_padding)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid Opus pre-skip"))?
    };
    let extradata = opus_head(pre_skip);
    media::validate_audio_initialization(
        "opus",
        AUDIO_PACKETIZATION_OPUS,
        &extradata,
        AUDIO_RATE,
        AUDIO_CHANNELS,
    )?;
    let spec = AudioTrackSpec {
        extradata,
        sample_rate: AUDIO_RATE,
        channels: u8::try_from(AUDIO_CHANNELS).expect("two audio channels"),
        max_access_unit_bytes: MAX_AUDIO_ACCESS_UNIT,
    };
    Ok((opus, spec))
}

fn capture_thread(
    monitor: &str,
    server: &OsStr,
    buffer: &CaptureBuffer,
    stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    let (pulse_format, options) = pulse_input(server)?;
    let interrupt = Arc::new(PulseInterrupt::stopped_by(stop.clone()));
    let mut input = open_pulse_with_interrupt(monitor, &pulse_format, options, &interrupt)?;
    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Audio)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "Pulse input has no audio stream")
        })?;
    let stream_index = stream.index();
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(ffmpeg_error)?
        .decoder()
        .audio()
        .map_err(ffmpeg_error)?;
    let decoder_layout = decoder.channel_layout();
    let fallback_input_layout = if decoder_layout.is_empty() {
        match decoder.channels() {
            1 => ffmpeg::ChannelLayout::MONO,
            _ => ffmpeg::ChannelLayout::STEREO,
        }
    } else {
        decoder_layout
    };
    let fallback_input_rate = match decoder.rate() {
        0 => AUDIO_RATE,
        rate => rate,
    };
    let mut resampler = None;
    while !stop.load(Ordering::Acquire) {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {}
            Err(_) if stop.load(Ordering::Acquire) => return Ok(()),
            Err(ffmpeg::Error::Other {
                errno: ffmpeg::error::EAGAIN | ffmpeg::error::EINTR,
            }) => continue,
            Err(ffmpeg::Error::Eof) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Pulse monitor capture ended",
                ));
            }
            Err(error) => return Err(ffmpeg_error(error)),
        }
        if packet.stream() != stream_index {
            continue;
        }
        decoder.send_packet(&packet).map_err(ffmpeg_error)?;
        loop {
            let mut decoded = ffmpeg::frame::Audio::empty();
            match decoder.receive_frame(&mut decoded) {
                Ok(()) => {}
                Err(ffmpeg::Error::Other {
                    errno: ffmpeg::error::EAGAIN,
                })
                | Err(ffmpeg::Error::Eof) => break,
                Err(error) => return Err(ffmpeg_error(error)),
            }
            let converted = resample_audio_frame(
                &mut resampler,
                &mut decoded,
                fallback_input_layout,
                fallback_input_rate,
            )?;
            if converted.samples() == 0 || converted.planes() != 1 {
                continue;
            }
            let samples = converted
                .plane::<(f32, f32)>(0)
                .iter()
                .flat_map(|&(left, right)| [left, right])
                .collect::<Vec<_>>();
            buffer.push_samples(&samples);
        }
    }
    Ok(())
}

struct PacerState {
    schedule_base_us: u64,
    pts_base_us: i64,
    sample_cursor: i64,
}

struct PacedFrame {
    pts_samples: i64,
    samples: Vec<f32>,
}

fn due_frames(
    now_us: u64,
    buffer: &CaptureBuffer,
    gain: &AudioGain,
    state: &mut PacerState,
) -> Vec<PacedFrame> {
    let target_us = now_us.saturating_sub(AUDIO_DELAY_US);
    let cursor_us = u64::try_from(samples_to_us(state.sample_cursor)).unwrap_or(u64::MAX);
    let next_end_us = state
        .schedule_base_us
        .saturating_add(cursor_us)
        .saturating_add(OPUS_FRAME_US);
    if target_us < next_end_us {
        return Vec::new();
    }
    let due = 1_u64.saturating_add((target_us - next_end_us) / OPUS_FRAME_US);
    let due = usize::try_from(due).unwrap_or(usize::MAX);
    if due > MAX_CATCHUP_FRAMES {
        let skipped = due - MAX_CATCHUP_FRAMES;
        state.schedule_base_us = state.schedule_base_us.saturating_add(
            u64::try_from(skipped)
                .unwrap_or(u64::MAX)
                .saturating_mul(OPUS_FRAME_US),
        );
    }
    let count = due.min(MAX_CATCHUP_FRAMES);
    let (muted, volume) = gain.status();
    let scale = if muted { 0.0 } else { volume };
    (0..count)
        .map(|_| {
            let mut samples = buffer.take_up_to(OPUS_FRAME_SAMPLES);
            samples.resize(OPUS_FRAME_SAMPLES, 0.0);
            if scale != 1.0 {
                for sample in &mut samples {
                    *sample *= scale;
                }
            }
            let frame = PacedFrame {
                pts_samples: state.sample_cursor,
                samples,
            };
            state.sample_cursor = state.sample_cursor.saturating_add(OPUS_SAMPLES as i64);
            frame
        })
        .collect()
}

fn encode_paced_frame(
    paced: PacedFrame,
    encoder: &mut ffmpeg::encoder::Audio,
    queue: &AudioQueue,
    base_pts_us: i64,
) -> io::Result<()> {
    let mut frame = ffmpeg::frame::Audio::new(
        ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
        OPUS_SAMPLES,
        ffmpeg::ChannelLayout::STEREO,
    );
    frame.set_rate(AUDIO_RATE);
    frame.set_pts(Some(paced.pts_samples));
    for (output, samples) in frame
        .plane_mut::<(f32, f32)>(0)
        .iter_mut()
        .zip(paced.samples.chunks_exact(2))
    {
        *output = (samples[0], samples[1]);
    }
    encoder.send_frame(&frame).map_err(ffmpeg_error)?;
    receive_opus(encoder, base_pts_us, queue)
}

fn pace(
    now_us: u64,
    buffer: &CaptureBuffer,
    gain: &AudioGain,
    encoder: &mut ffmpeg::encoder::Audio,
    queue: &AudioQueue,
    state: &mut PacerState,
) -> io::Result<usize> {
    let frames = due_frames(now_us, buffer, gain, state);
    let count = frames.len();
    for frame in frames {
        encode_paced_frame(frame, encoder, queue, state.pts_base_us)?;
    }
    Ok(count)
}

fn pacer_thread(
    origin: Instant,
    mut opus: ffmpeg::encoder::Audio,
    buffer: &CaptureBuffer,
    queue: &AudioQueue,
    gain: &AudioGain,
    stop: &AtomicBool,
) -> io::Result<()> {
    let base_us = u64::try_from(origin.elapsed().as_micros()).unwrap_or(u64::MAX);
    let mut state = PacerState {
        schedule_base_us: base_us,
        pts_base_us: i64::try_from(base_us).unwrap_or(i64::MAX),
        sample_cursor: 0,
    };
    while !stop.load(Ordering::Acquire) {
        if let Some(error) = buffer.closed() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, error));
        }
        let now_us = u64::try_from(origin.elapsed().as_micros()).unwrap_or(u64::MAX);
        if pace(now_us, buffer, gain, &mut opus, queue, &mut state)? == 0 {
            thread::sleep(Duration::from_millis(5));
        }
    }
    opus.send_eof().map_err(ffmpeg_error)?;
    receive_opus(&mut opus, state.pts_base_us, queue)
}

fn resample_audio_frame(
    resampler: &mut Option<ffmpeg::software::resampling::Context>,
    input: &mut ffmpeg::frame::Audio,
    fallback_layout: ffmpeg::ChannelLayout,
    fallback_rate: u32,
) -> io::Result<ffmpeg::frame::Audio> {
    let format = input.format();
    if format == ffmpeg::format::Sample::None {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Pulse decoder produced an audio frame without a sample format",
        ));
    }
    let mut layout = input.channel_layout();
    if layout.is_empty() {
        layout = match input.channels() {
            1 => ffmpeg::ChannelLayout::MONO,
            2 => ffmpeg::ChannelLayout::STEREO,
            _ if !fallback_layout.is_empty() => fallback_layout,
            channels => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Pulse decoder produced an audio frame with {channels} channels and no layout"
                    ),
                ));
            }
        };
        input.set_channel_layout(layout);
    }
    let rate = match input.rate() {
        0 if fallback_rate != 0 => {
            input.set_rate(fallback_rate);
            fallback_rate
        }
        0 => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Pulse decoder produced an audio frame without a sample rate",
            ));
        }
        rate => rate,
    };
    let input_changed = resampler.as_ref().is_none_or(|resampler| {
        let current = resampler.input();
        current.format != format || current.channel_layout != layout || current.rate != rate
    });
    if input_changed {
        *resampler = Some(
            ffmpeg::software::resampling::Context::get(
                format,
                layout,
                rate,
                ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
                ffmpeg::ChannelLayout::STEREO,
                AUDIO_RATE,
            )
            .map_err(ffmpeg_error)?,
        );
    }
    let mut output = ffmpeg::frame::Audio::empty();
    resampler
        .as_mut()
        .expect("audio resampler was initialized")
        .run(input, &mut output)
        .map_err(ffmpeg_error)?;
    Ok(output)
}

pub(super) struct PulseInterrupt {
    stop: Option<Arc<AtomicBool>>,
    deadline: Option<Instant>,
}

impl PulseInterrupt {
    fn stopped_by(stop: Arc<AtomicBool>) -> Self {
        Self {
            stop: Some(stop),
            deadline: None,
        }
    }

    fn at_deadline(deadline: Instant) -> Self {
        Self {
            stop: None,
            deadline: Some(deadline),
        }
    }

    fn interrupted(&self) -> bool {
        self.stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::Acquire))
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }
}

extern "C" fn interrupt_audio(opaque: *mut c_void) -> i32 {
    if opaque.is_null() {
        return 1;
    }
    // SAFETY: opaque points to the PulseInterrupt held by the caller's Arc for the whole format
    // context lifetime. Its fields permit concurrent reads.
    i32::from(unsafe { &*(opaque.cast::<PulseInterrupt>()) }.interrupted())
}

pub(super) fn open_pulse_with_interrupt(
    monitor: &str,
    format: &ffmpeg::format::Input,
    options: ffmpeg::Dictionary<'_>,
    interrupt: &Arc<PulseInterrupt>,
) -> io::Result<ffmpeg::format::context::Input> {
    let path = CString::new(monitor)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Pulse monitor contains NUL"))?;
    // SAFETY: all pointers are owned by this scope or FFmpeg. The interrupt opaque points into an
    // Arc that outlives the returned context, and FFmpeg takes ownership of the allocated context
    // on success. Leftover options are reclaimed immediately.
    unsafe {
        let mut context = ffmpeg::ffi::avformat_alloc_context();
        if context.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "FFmpeg could not allocate a Pulse input context",
            ));
        }
        (*context).interrupt_callback = ffmpeg::ffi::AVIOInterruptCB {
            callback: Some(interrupt_audio),
            opaque: Arc::as_ptr(interrupt).cast_mut().cast(),
        };
        let mut raw_options = options.disown();
        let open_result = ffmpeg::ffi::avformat_open_input(
            &mut context,
            path.as_ptr(),
            format.as_ptr().cast_mut(),
            &mut raw_options,
        );
        drop(ffmpeg::Dictionary::own(raw_options));
        if open_result < 0 {
            return Err(ffmpeg_error(ffmpeg::Error::from(open_result)));
        }
        let info_result = ffmpeg::ffi::avformat_find_stream_info(context, std::ptr::null_mut());
        if info_result < 0 {
            ffmpeg::ffi::avformat_close_input(&mut context);
            return Err(ffmpeg_error(ffmpeg::Error::from(info_result)));
        }
        Ok(ffmpeg::format::context::Input::wrap(context))
    }
}

fn pulse_input(server: &OsStr) -> io::Result<(ffmpeg::format::Input, ffmpeg::Dictionary<'static>)> {
    let mut options = ffmpeg::Dictionary::new();
    options.set("sample_rate", "48000");
    options.set("channels", "2");
    options.set("fragment_size", "3840");
    options.set("wallclock", "1");
    let server = server.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Pulse server string is not valid UTF-8",
        )
    })?;
    options.set("server", server);
    let pulse_name = CString::new("pulse").expect("literal has no NUL");
    // SAFETY: FFmpeg returns a static input-format descriptor, which is wrapped without ownership.
    let pulse_format = unsafe {
        let pointer = ffmpeg::ffi::av_find_input_format(pulse_name.as_ptr());
        if pointer.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "FFmpeg Pulse input device is unavailable",
            ));
        }
        ffmpeg::format::format::Input::wrap(pointer.cast_mut())
    };
    Ok((pulse_format, options))
}

/// Probe whether the Pulse monitor produces data, for `--doctor`.
pub fn idle_monitor_produces_data(
    monitor: &str,
    server: &OsStr,
    timeout: Duration,
) -> io::Result<bool> {
    initialize_ffmpeg();
    let deadline = Instant::now() + timeout;
    let (pulse_format, options) = pulse_input(server)?;
    let interrupt = Arc::new(PulseInterrupt::at_deadline(deadline));
    let mut input = match open_pulse_with_interrupt(monitor, &pulse_format, options, &interrupt) {
        Ok(input) => input,
        Err(_) if Instant::now() >= deadline => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut packet = ffmpeg::Packet::empty();
    match packet.read(&mut input) {
        Ok(()) => Ok(true),
        Err(_) if Instant::now() >= deadline => Ok(false),
        Err(ffmpeg::Error::Eof) => Ok(false),
        Err(error) => Err(ffmpeg_error(error)),
    }
}

fn create_opus_encoder() -> io::Result<ffmpeg::encoder::Audio> {
    let codec = ffmpeg::encoder::find_by_name("libopus").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "FFmpeg libopus encoder is unavailable",
        )
    })?;
    let mut encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
        .encoder()
        .audio()
        .map_err(ffmpeg_error)?;
    encoder.set_rate(AUDIO_RATE as i32);
    encoder.set_channel_layout(ffmpeg::ChannelLayout::STEREO);
    encoder.set_format(ffmpeg::format::Sample::F32(
        ffmpeg::format::sample::Type::Packed,
    ));
    encoder.set_time_base(ffmpeg::Rational(1, AUDIO_RATE as i32));
    encoder.set_bit_rate(128_000);
    let mut options = ffmpeg::Dictionary::new();
    options.set("application", "lowdelay");
    options.set("frame_duration", "20");
    options.set("vbr", "on");
    encoder.open_with(options).map_err(ffmpeg_error)
}

fn receive_opus(
    encoder: &mut ffmpeg::encoder::Audio,
    base_pts_us: i64,
    queue: &AudioQueue,
) -> io::Result<()> {
    loop {
        let mut packet = ffmpeg::Packet::empty();
        if encoder.receive_packet(&mut packet).is_err() {
            break;
        }
        let data = packet.data().unwrap_or_default();
        if data.is_empty() || data.len() > MAX_AUDIO_ACCESS_UNIT as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "libopus emitted an empty or oversized packet",
            ));
        }
        let pts = packet.pts().unwrap_or(0);
        let dts = packet.dts().unwrap_or(pts);
        queue.push(EncodedAudio {
            pts_us: base_pts_us.saturating_add(samples_to_us(pts)),
            dts_us: base_pts_us.saturating_add(samples_to_us(dts)),
            duration_us: 20_000,
            packet,
        });
    }
    Ok(())
}

fn samples_to_us(samples: i64) -> i64 {
    i64::try_from(i128::from(samples) * 1_000_000 / i128::from(AUDIO_RATE)).unwrap_or_else(|_| {
        if samples.is_negative() {
            i64::MIN
        } else {
            i64::MAX
        }
    })
}

fn opus_head(pre_skip: u16) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1);
    head.push(AUDIO_CHANNELS as u8);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&AUDIO_RATE.to_le_bytes());
    head.extend_from_slice(&0_i16.to_le_bytes());
    head.push(0);
    head
}

fn ffmpeg_error(error: ffmpeg::Error) -> io::Error {
    io::Error::other(format!("FFmpeg: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_identity() -> crate::producer::ProductIdentity {
        crate::producer::ProductIdentity {
            slug: "testproduct",
            display_name: "Testproduct",
            compositor_name: "Testcompositor",
        }
    }
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    #[test]
    fn audio_queue_uses_presenter_advertised_limits() {
        let queue = AudioQueue::new();
        queue.configure_limits(7, 123_456);
        let state = queue
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!((state.max_packets, state.max_bytes), (7, 123_456));
    }

    #[test]
    fn audio_queue_reports_live_drops_and_retained_duration() {
        let queue = AudioQueue::new();
        queue.configure_limits(2, 1_024);
        for index in 0..3 {
            queue.push(EncodedAudio {
                pts_us: index * 20_000,
                dts_us: index * 20_000,
                duration_us: 20_000,
                packet: ffmpeg::Packet::new(4),
            });
        }

        assert_eq!(
            queue.snapshot(),
            AudioQueueSnapshot {
                packets: 2,
                bytes: 8,
                queued_duration_us: 40_000,
                dropped_packets: 1,
                peak_packets: 2,
                peak_bytes: 8,
            }
        );
    }

    fn pacer_state() -> PacerState {
        PacerState {
            schedule_base_us: 0,
            pts_base_us: 0,
            sample_cursor: 0,
        }
    }

    #[test]
    fn pacer_emits_continuous_silence_when_capture_is_idle() {
        let buffer = CaptureBuffer::new();
        let gain = AudioGain::new();
        let mut state = pacer_state();
        let mut frames = Vec::new();
        for now_us in (120_000..=600_000).step_by(OPUS_FRAME_US as usize) {
            frames.extend(due_frames(now_us, &buffer, &gain, &mut state));
        }
        assert_eq!(frames.len(), 25);
        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(frame.pts_samples, (index * OPUS_SAMPLES) as i64);
            assert!(frame.samples.iter().all(|sample| *sample == 0.0));
        }
    }

    #[test]
    fn paced_silence_produces_opus_packets_and_advances_horizon() {
        initialize_ffmpeg();
        let mut encoder = create_opus_encoder().unwrap();
        let buffer = CaptureBuffer::new();
        let gain = AudioGain::new();
        let mut state = pacer_state();
        let queue = AudioQueue::new();
        let mut packets = Vec::new();
        for now_us in (120_000..=600_000).step_by(OPUS_FRAME_US as usize) {
            pace(now_us, &buffer, &gain, &mut encoder, &queue, &mut state).unwrap();
            while let Some(packet) = queue.pop(Duration::ZERO).unwrap() {
                packets.push(packet);
            }
        }
        assert_eq!(packets.len(), 25);
        assert!(packets.windows(2).all(|packets| {
            packets[1].pts_us - packets[0].pts_us == OPUS_FRAME_US as i64
                && packets[1].pts_us + packets[1].duration_us as i64
                    > packets[0].pts_us + packets[0].duration_us as i64
        }));
        encoder.send_eof().unwrap();
        receive_opus(&mut encoder, state.pts_base_us, &queue).unwrap();
    }

    #[test]
    fn pacer_mixes_real_samples_and_pads_partial_frames() {
        let buffer = CaptureBuffer::new();
        let samples = (0..OPUS_FRAME_SAMPLES + OPUS_FRAME_SAMPLES / 2)
            .map(|sample| sample as f32)
            .collect::<Vec<_>>();
        buffer.push_samples(&samples);
        let frames = due_frames(140_000, &buffer, &AudioGain::new(), &mut pacer_state());
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].samples, samples[..OPUS_FRAME_SAMPLES]);
        assert_eq!(
            frames[1].samples[..OPUS_FRAME_SAMPLES / 2],
            samples[OPUS_FRAME_SAMPLES..]
        );
        assert!(
            frames[1].samples[OPUS_FRAME_SAMPLES / 2..]
                .iter()
                .all(|sample| *sample == 0.0)
        );
    }

    #[test]
    fn pacer_reanchors_after_long_stall_without_burst() {
        let buffer = CaptureBuffer::new();
        let gain = AudioGain::new();
        let mut state = pacer_state();
        let frames = due_frames(5_100_000, &buffer, &gain, &mut state);
        assert_eq!(frames.len(), MAX_CATCHUP_FRAMES);
        assert!(frames.windows(2).all(|frames| {
            frames[1].pts_samples - frames[0].pts_samples == OPUS_SAMPLES as i64
        }));
        assert!(due_frames(5_100_000, &buffer, &gain, &mut state).is_empty());
    }

    #[test]
    fn capture_close_propagates_to_queue() {
        initialize_ffmpeg();
        let (encoder, _) = create_opus_source().unwrap();
        let buffer = CaptureBuffer::new();
        buffer.close("x");
        let queue = AudioQueue::new();
        let stop = AtomicBool::new(false);
        let error = pacer_thread(
            Instant::now(),
            encoder,
            &buffer,
            &queue,
            &AudioGain::new(),
            &stop,
        )
        .unwrap_err();
        queue.close(Some(error.to_string()));
        let Err(closed) = queue.pop(Duration::ZERO) else {
            panic!("a closed queue must report the close reason");
        };
        assert_eq!(closed.to_string(), "x");
    }

    #[test]
    fn pipeline_start_returns_spec_without_pulse_data() {
        let pipeline = AudioPipeline::start(
            "missing.monitor",
            OsStr::new("unix:/testproduct-test-server-does-not-exist"),
            Instant::now(),
            &test_identity(),
        )
        .unwrap();
        media::validate_audio_initialization(
            "opus",
            AUDIO_PACKETIZATION_OPUS,
            &pipeline.spec.extradata,
            pipeline.spec.sample_rate,
            u16::from(pipeline.spec.channels),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match pipeline.queue.pop(Duration::from_millis(20)) {
                Err(_) => break,
                Ok(_) if Instant::now() < deadline => {}
                Ok(_) => panic!("failed Pulse capture did not close the audio queue"),
            }
        }
    }

    fn write_fake_pactl(root: &Path) -> (OsString, std::path::PathBuf) {
        let pactl = root.join("pactl");
        let calls = root.join("calls");
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{}"
case "$*" in
  "info")
    printf '%s\n' 'Server String: unix:/run/user/1000/pulse/native'
    ;;
  "--server tcp:override.example:4713 info")
    printf '%s\n' 'Server String: tcp:ignored.example:4713'
    ;;
  "--server unix:/run/user/1000/pulse/native load-module module-null-sink"*)
    printf '%s\n' '73'
    ;;
  "--server unix:/run/user/1000/pulse/native unload-module 73")
    ;;
  *)
    printf '%s\n' "unexpected pactl arguments: $*" >&2
    exit 2
    ;;
esac
"#,
            calls.display()
        );
        fs::write(&pactl, script).unwrap();
        fs::set_permissions(&pactl, fs::Permissions::from_mode(0o700)).unwrap();
        (pactl.into_os_string(), calls)
    }

    #[test]
    fn parses_pulse_server_from_pactl_info() {
        let server = parse_pulse_server_info(
            b"Server Name: PulseAudio (on PipeWire 1.0.5)\nServer String: /run/user/1000/pulse/native\n",
        )
        .unwrap();
        assert_eq!(server, "/run/user/1000/pulse/native");
        assert!(parse_pulse_server_info(b"Server Name: PulseAudio\n").is_err());
        assert!(parse_pulse_server_info(b"Server String:   \n").is_err());
    }

    #[test]
    fn explicit_pulse_server_overrides_discovered_server() {
        let root = std::env::temp_dir().join(format!(
            "testproduct-pactl-override-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let (pactl, _) = write_fake_pactl(&root);
        let server =
            resolve_pulse_server_with_pactl(Some("tcp:override.example:4713"), OsStr::new(&pactl))
                .unwrap();
        assert_eq!(server, OsStr::new("tcp:override.example:4713"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pulse_sink_uses_one_server_for_discovery_load_and_unload() {
        let root = std::env::temp_dir().join(format!(
            "testproduct-pactl-lifecycle-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let (pactl, calls) = write_fake_pactl(&root);

        let server = resolve_pulse_server_with_pactl(None, OsStr::new(&pactl)).unwrap();
        assert_eq!(server, OsStr::new("unix:/run/user/1000/pulse/native"));
        let sink =
            PulseSink::create_with_pactl(&server, OsStr::new(&pactl), &test_identity()).unwrap();
        assert_eq!(sink.server(), server.as_os_str());
        drop(sink);

        let calls = fs::read_to_string(calls).unwrap();
        let lines = calls.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], "info");
        assert!(
            lines[1].starts_with(
                "--server unix:/run/user/1000/pulse/native load-module module-null-sink"
            )
        );
        assert_eq!(
            lines[2],
            "--server unix:/run/user/1000/pulse/native unload-module 73"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generated_opus_head_is_canonical() {
        let head = opus_head(312);
        media::validate_audio_initialization(
            "opus",
            AUDIO_PACKETIZATION_OPUS,
            &head,
            AUDIO_RATE,
            AUDIO_CHANNELS,
        )
        .unwrap();
    }

    #[test]
    fn sample_clock_handles_encoder_delay() {
        assert_eq!(samples_to_us(960), 20_000);
        assert_eq!(samples_to_us(-312), -6_500);
    }

    #[test]
    fn libopus_encoder_accepts_packed_float_frame() {
        ffmpeg::init().unwrap();
        let mut encoder = create_opus_encoder().unwrap();
        assert_eq!(
            encoder.format(),
            ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed)
        );

        let mut frame = ffmpeg::frame::Audio::new(
            ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
            OPUS_SAMPLES,
            ffmpeg::ChannelLayout::STEREO,
        );
        frame.set_rate(AUDIO_RATE);
        frame.set_pts(Some(0));
        frame.plane_mut::<(f32, f32)>(0).fill((0.0, 0.0));
        encoder.send_frame(&frame).unwrap();
        encoder.send_eof().unwrap();

        let queue = AudioQueue::new();
        receive_opus(&mut encoder, 0, &queue).unwrap();
        let packet = queue.pop(Duration::ZERO).unwrap().unwrap();
        assert!(!packet.data().is_empty());
    }

    #[test]
    fn resampler_uses_decoded_frame_properties_and_repairs_missing_layout() {
        ffmpeg::init().unwrap();
        let mut input = ffmpeg::frame::Audio::new(
            ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
            OPUS_SAMPLES,
            ffmpeg::ChannelLayout::STEREO,
        );
        input.set_rate(AUDIO_RATE);
        input.plane_mut::<(f32, f32)>(0).fill((0.25, -0.25));
        input.set_channel_layout(ffmpeg::ChannelLayout::default(0));

        let mut resampler = Some(
            ffmpeg::software::resampling::Context::get(
                ffmpeg::format::Sample::I16(ffmpeg::format::sample::Type::Packed),
                ffmpeg::ChannelLayout::MONO,
                44_100,
                ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
                ffmpeg::ChannelLayout::STEREO,
                AUDIO_RATE,
            )
            .unwrap(),
        );
        let output = resample_audio_frame(
            &mut resampler,
            &mut input,
            ffmpeg::ChannelLayout::STEREO,
            AUDIO_RATE,
        )
        .unwrap();

        let definition = resampler.as_ref().unwrap().input();
        assert_eq!(
            definition.format,
            ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed)
        );
        assert_eq!(definition.channel_layout, ffmpeg::ChannelLayout::STEREO);
        assert_eq!(definition.rate, AUDIO_RATE);
        assert_eq!(output.samples(), OPUS_SAMPLES);
        assert_eq!(output.planes(), 1);
    }

    #[test]
    #[ignore = "requires live Pulse compatibility and FFmpeg pulse output"]
    fn isolated_client_reaches_private_sink_and_capture_pipeline() {
        let server = resolve_pulse_server(None).unwrap();
        let sink = PulseSink::create(&server, &test_identity()).unwrap();
        let pipeline = AudioPipeline::start(
            sink.monitor_name(),
            sink.server(),
            Instant::now(),
            &test_identity(),
        )
        .unwrap();
        let sinks = Command::new("pactl")
            .arg("--server")
            .arg(sink.server())
            .args(["list", "sinks", "short"])
            .output()
            .unwrap();
        assert!(sinks.status.success());
        let sink_index = String::from_utf8_lossy(&sinks.stdout)
            .lines()
            .find_map(|line| {
                let mut fields = line.split_whitespace();
                let index = fields.next()?;
                (fields.next()? == sink.sink_name().to_str()?).then(|| index.to_owned())
            })
            .expect("private Pulse sink was not listed");
        let runtime = std::env::temp_dir().join(format!(
            "testproduct-isolated-audio-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&runtime);
        fs::create_dir(&runtime).unwrap();

        let child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=1000:sample_rate=48000",
                "-t",
                "2",
                "-f",
                "pulse",
                "default",
            ])
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("PULSE_SERVER", sink.server())
            .env("PULSE_SINK", sink.sink_name())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let attached = loop {
            let inputs = Command::new("pactl")
                .arg("--server")
                .arg(sink.server())
                .args(["list", "sink-inputs", "short"])
                .output()
                .unwrap();
            assert!(inputs.status.success());
            if String::from_utf8_lossy(&inputs.stdout)
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(sink_index.as_str()))
            {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(20));
        };
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "isolated FFmpeg client could not play through Pulse: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            attached,
            "isolated FFmpeg client did not attach to the private sink"
        );
        assert!(
            pipeline
                .queue
                .pop(Duration::from_secs(1))
                .unwrap()
                .is_some(),
            "Pulse monitor produced no Opus packets"
        );
        fs::remove_dir_all(runtime).unwrap();
    }
}
