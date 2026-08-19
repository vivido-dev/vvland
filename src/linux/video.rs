//! The renderer-independent half of the video path: the latest-value frame boundary, the x264
//! encoder, the decoder description, and the bounded encoded queue.
//!
//! Every compositor backend feeds this through [`CaptureSource`]; the capture implementations
//! themselves live next to their compositor (`compositor/pipewire.rs`, `compositor/capture.rs`).

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once};
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;
use vivid_sdk::SendPressure;

/// The packed 32-bit layouts the backends can deliver.
///
/// Weston's PipeWire output negotiates BGRx/RGBx; wlroots screencopy adds the alpha-carrying
/// variants. The encoder maps each one straight to its FFmpeg pixel format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawPixelFormat {
    Bgrx,
    Rgbx,
    Bgra,
    Rgba,
}

impl RawPixelFormat {
    pub(crate) fn ffmpeg(self) -> ffmpeg::format::Pixel {
        match self {
            Self::Bgrx => ffmpeg::format::Pixel::BGRZ,
            Self::Rgbx => ffmpeg::format::Pixel::RGBZ,
            Self::Bgra => ffmpeg::format::Pixel::BGRA,
            Self::Rgba => ffmpeg::format::Pixel::RGBA,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RawFrame {
    pub format: RawPixelFormat,
    pub width: u32,
    pub height: u32,
    pub pts_us: i64,
    pub data: Arc<[u8]>,
}

struct LatestState {
    serial: u64,
    frame: Option<RawFrame>,
    closed: Option<String>,
    /// When the current frame arrived, so a consumer can tell a live desktop from a stalled one.
    ///
    /// `RawFrame::pts_us` is the capture clock and cannot answer this: a screencopy backend only
    /// delivers on damage, so an idle desktop's newest frame keeps its original PTS however long
    /// ago that was. Wall-clock arrival is the only thing that distinguishes "nothing changed"
    /// from "capture died".
    updated: Option<Instant>,
}

pub struct LatestFrame {
    state: Mutex<LatestState>,
    changed: Condvar,
}

impl LatestFrame {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(LatestState {
                serial: 0,
                frame: None,
                closed: None,
                updated: None,
            }),
            changed: Condvar::new(),
        }
    }

    pub(crate) fn replace(&self, frame: RawFrame) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.serial = state.serial.saturating_add(1);
        state.frame = Some(frame);
        state.updated = Some(Instant::now());
        self.changed.notify_all();
    }

    pub(crate) fn close(&self, error: Option<String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .closed
            .get_or_insert_with(|| error.unwrap_or_else(|| "desktop capture stopped".into()));
        self.changed.notify_all();
    }

    pub fn wait_next(
        &self,
        last_serial: &mut u64,
        timeout: Duration,
    ) -> io::Result<Option<RawFrame>> {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if state.serial != *last_serial {
                *last_serial = state.serial;
                return Ok(state.frame.clone());
            }
            if let Some(error) = &state.closed {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.clone()));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            state = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    /// Return the most recently captured frame even when the compositor produced no new damage.
    /// Live recovery needs this snapshot to generate an IDR immediately after a presenter asks
    /// for a keyframe; waiting only for a new damage-driven frame can leave a quiet desktop black.
    pub fn snapshot(&self) -> io::Result<Option<(u64, RawFrame)>> {
        Ok(self
            .snapshot_with_age()?
            .map(|(serial, frame, _)| (serial, frame)))
    }

    /// [`LatestFrame::snapshot`] plus how long ago the frame arrived.
    ///
    /// A consumer that has to decide whether a still is worth serving needs the age, not just the
    /// pixels: a damage-driven backend produces no frames at all while the desktop is idle, so a
    /// large age means "nothing has changed" at least as often as it means "capture stalled", and
    /// only the caller knows which of those it can tolerate. Reported rather than judged here.
    pub fn snapshot_with_age(&self) -> io::Result<Option<(u64, RawFrame, Duration)>> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(error) = &state.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.clone()));
        }
        let age = state.updated.map_or(Duration::ZERO, |at| at.elapsed());
        Ok(state.frame.clone().map(|frame| (state.serial, frame, age)))
    }
}

/// The one thing the pipeline needs from a capture backend.
///
/// Both `VideoCapture` (Weston/PipeWire) and `ScreencopyCapture` (Sway/wlroots) already published
/// exactly this; naming it lets the shared encoder worker start without knowing the compositor.
pub trait CaptureSource {
    fn latest(&self) -> Arc<LatestFrame>;
}

pub struct EncodedVideo {
    pub pts_us: i64,
    pub dts_us: i64,
    pub duration_us: u64,
    pub key: bool,
    packet: ffmpeg::Packet,
}

impl EncodedVideo {
    pub fn data(&self) -> &[u8] {
        self.packet.data().unwrap_or_default()
    }

    fn data_len(&self) -> usize {
        self.packet.size()
    }
}

/// libswscale's `SwsContext` is created and used only inside the single encoder thread, but
/// ffmpeg-next does not mark the scaling context `Send` (unlike its codec/format/resampling
/// contexts). This wrapper lets the owning `H264Encoder` move into that thread.
struct SendScaler(ffmpeg::software::scaling::Context);

// SAFETY: the wrapped scaling context never leaves the encoder thread and is never shared or used
// concurrently, so moving it across the thread boundary is sound.
unsafe impl Send for SendScaler {}

impl std::ops::Deref for SendScaler {
    type Target = ffmpeg::software::scaling::Context;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for SendScaler {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub struct H264Encoder {
    encoder: ffmpeg::encoder::Video,
    decoder_description: H264DecoderDescription,
    scaler: Option<(RawPixelFormat, SendScaler)>,
    source: Option<(RawPixelFormat, ffmpeg::frame::Video)>,
    yuv: ffmpeg::frame::Video,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u64,
    gop_seconds: u32,
    frame_duration_us: u64,
    max_access_unit_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264DecoderDescription {
    pub profile: i32,
    pub level: i32,
    pub codec_string: String,
    pub extradata: Vec<u8>,
    pub decoder_config: Vec<u8>,
}

impl H264Encoder {
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u64,
        gop_seconds: u32,
        max_access_unit_bytes: u32,
    ) -> io::Result<Self> {
        static FFMPEG_INIT: Once = Once::new();
        FFMPEG_INIT.call_once(|| {
            let _ = ffmpeg::init();
            ffmpeg::log::set_level(ffmpeg::log::Level::Warning);
        });
        // A zero or absurd rate would leave libx264 to derive its rate from the microsecond time
        // base (one million frames per second, past every level limit) and would divide by zero
        // when deriving the frame duration. Reject it here rather than at the panic site.
        if !(1..=240).contains(&fps) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "H.264 frame rate must be between 1 and 240",
            ));
        }
        if width % 2 != 0 || height % 2 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "H.264 YUV420P dimensions must be even",
            ));
        }
        let codec = ffmpeg::encoder::find_by_name("libx264").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "FFmpeg libx264 encoder is unavailable",
            )
        })?;
        let mut encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .map_err(ffmpeg_error)?;
        encoder.set_width(width);
        encoder.set_height(height);
        encoder.set_format(ffmpeg::format::Pixel::YUV420P);
        encoder.set_time_base(ffmpeg::Rational(1, 1_000_000));
        encoder.set_frame_rate(Some(ffmpeg::Rational(
            i32::try_from(fps).unwrap_or(i32::MAX),
            1,
        )));
        encoder.set_bit_rate(usize::try_from(bitrate).unwrap_or(usize::MAX));
        encoder.set_gop(fps.saturating_mul(gop_seconds));
        encoder.set_max_b_frames(0);
        encoder.set_colorspace(ffmpeg::color::Space::BT709);
        encoder.set_color_range(ffmpeg::color::Range::MPEG);
        encoder.set_flags(ffmpeg::codec::Flags::GLOBAL_HEADER);
        let mut options = ffmpeg::Dictionary::new();
        options.set("preset", "veryfast");
        options.set("tune", "zerolatency");
        options.set("profile", "high");
        options.set("level", "4.1");
        options.set("forced-idr", "true");
        let bitrate_kbps = (bitrate.saturating_add(999) / 1_000).max(1);
        // Rate-control headroom, in milliseconds of the target. This is what a hard frame is
        // allowed to borrow against, so too small a buffer shows up directly as blocking and blur
        // whenever the picture moves. It was a tenth of a second, which for 1080p motion is not
        // enough to encode a frame properly; the encoder's own transport pacing, not a starved
        // VBV, is what keeps latency bounded.
        let vbv_buffer_kbits = (bitrate_kbps.saturating_mul(VBV_BUFFER_MS) / 1_000).max(1);
        // FFmpeg's libx264 wrapper re-reads `rc_max_rate` and `rc_buffer_size` on every frame and
        // calls `x264_encoder_reconfig` whenever they disagree with the parameters x264 is holding.
        // Setting the VBV only through `x264-params` therefore leaves the context at zero and lets
        // the very first frame reconfigure the encoder with *no* VBV at all. Declare the same
        // numbers on both sides so the burst limit that is asked for is the one that runs.
        encoder.set_max_bit_rate(usize::try_from(bitrate_kbps.saturating_mul(1_000)).unwrap_or(0));
        // SAFETY: the context is owned here and not yet opened; `rc_buffer_size` is a plain field
        // of the same `AVCodecContext` the safe setters above write.
        unsafe {
            (*encoder.as_mut_ptr()).rc_buffer_size =
                i32::try_from(vbv_buffer_kbits.saturating_mul(1_000)).unwrap_or(i32::MAX);
        }
        let x264_params = format!(
            "annexb=1:repeat-headers=1:aud=1:scenecut=0:rc-lookahead=0:sync-lookahead=0:\
             vbv-maxrate={bitrate_kbps}:vbv-bufsize={vbv_buffer_kbits}:vbv-init=0.9",
        );
        options.set("x264-params", &x264_params);
        let encoder = encoder.open_with(options).map_err(ffmpeg_error)?;
        let decoder_description = h264_decoder_description(encoder_extradata(&encoder)?)?;
        Ok(Self {
            encoder,
            decoder_description,
            scaler: None,
            source: None,
            yuv: ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, width, height),
            width,
            height,
            fps,
            bitrate,
            gop_seconds,
            frame_duration_us: 1_000_000_u64 / u64::from(fps),
            max_access_unit_bytes: usize::try_from(max_access_unit_bytes).unwrap_or(usize::MAX),
        })
    }

    pub fn decoder_description(&self) -> &H264DecoderDescription {
        &self.decoder_description
    }

    pub fn bitrate(&self) -> u64 {
        self.bitrate
    }

    /// Re-open the encoder at `bitrate`, keeping every immutable track property identical.
    ///
    /// Congestion has to cost bits, not key frames: an encoder that keeps offering the configured
    /// rate to a link that cannot carry it can only ever be recovered by discarding a GOP, and the
    /// IDR that recovery needs is larger than the frames that caused the overflow.
    ///
    /// `coded_width`, `coded_height`, `profile`, `level`, and the decoder configuration are all
    /// immutable in the negotiated track, so the replacement is adopted only when it publishes a
    /// byte-identical decoder description. Anything else keeps the encoder that is already
    /// streaming; a slower stream is always better than an undecodable one.
    pub fn retarget(&mut self, bitrate: u64) -> io::Result<bool> {
        if bitrate == self.bitrate {
            return Ok(false);
        }
        let replacement = Self::new(
            self.width,
            self.height,
            self.fps,
            bitrate,
            self.gop_seconds,
            u32::try_from(self.max_access_unit_bytes).unwrap_or(u32::MAX),
        )?;
        if replacement.decoder_description != self.decoder_description {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "re-targeted x264 encoder changed the immutable decoder description",
            ));
        }
        *self = replacement;
        Ok(true)
    }

    pub fn encode(&mut self, raw: RawFrame, force_keyframe: bool) -> io::Result<Vec<EncodedVideo>> {
        if raw.width != self.width || raw.height != self.height {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "captured frame dimensions changed after source creation",
            ));
        }
        let expected = usize::try_from(self.width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .and_then(|stride| {
                usize::try_from(self.height)
                    .ok()
                    .and_then(|height| stride.checked_mul(height))
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "raw frame size overflow"))?;
        if raw.data.len() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "captured frame length does not match its dimensions",
            ));
        }
        if self.scaler.as_ref().map(|(format, _)| *format) != Some(raw.format) {
            self.scaler = Some((
                raw.format,
                SendScaler(
                    ffmpeg::software::scaling::Context::get(
                        raw.format.ffmpeg(),
                        self.width,
                        self.height,
                        ffmpeg::format::Pixel::YUV420P,
                        self.width,
                        self.height,
                        ffmpeg::software::scaling::Flags::BILINEAR,
                    )
                    .map_err(ffmpeg_error)?,
                ),
            ));
        }
        if self.source.as_ref().map(|(format, _)| *format) != Some(raw.format) {
            self.source = Some((
                raw.format,
                ffmpeg::frame::Video::new(raw.format.ffmpeg(), self.width, self.height),
            ));
        }
        let source = &mut self.source.as_mut().expect("source initialized").1;
        let row_bytes = usize::try_from(self.width).unwrap_or(0).saturating_mul(4);
        let source_stride = source.stride(0);
        for row in 0..usize::try_from(self.height).unwrap_or(0) {
            let input_start = row * row_bytes;
            let output_start = row * source_stride;
            source.data_mut(0)[output_start..output_start + row_bytes]
                .copy_from_slice(&raw.data[input_start..input_start + row_bytes]);
        }
        self.scaler
            .as_mut()
            .expect("scaler initialized")
            .1
            .run(source, &mut self.yuv)
            .map_err(ffmpeg_error)?;
        self.yuv.set_pts(Some(raw.pts_us));
        self.yuv.set_kind(if force_keyframe {
            ffmpeg::picture::Type::I
        } else {
            ffmpeg::picture::Type::None
        });
        self.yuv.set_color_space(ffmpeg::color::Space::BT709);
        self.yuv.set_color_range(ffmpeg::color::Range::MPEG);
        self.yuv
            .set_color_primaries(ffmpeg::color::Primaries::BT709);
        self.yuv
            .set_color_transfer_characteristic(ffmpeg::color::TransferCharacteristic::BT709);
        self.encoder.send_frame(&self.yuv).map_err(ffmpeg_error)?;

        let mut output = Vec::new();
        loop {
            let mut packet = ffmpeg::Packet::empty();
            if self.encoder.receive_packet(&mut packet).is_err() {
                break;
            }
            let data = packet.data().unwrap_or_default();
            if data.is_empty() || data.len() > self.max_access_unit_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "libx264 emitted an empty or oversized access unit",
                ));
            }
            let bitstream_key = vivid_protocol::media::access_unit_is_key("h264", data)?;
            if bitstream_key != packet.is_key() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "libx264 packet flag disagrees with Annex-B keyframe classification",
                ));
            }
            output.push(EncodedVideo {
                pts_us: packet.pts().unwrap_or(raw.pts_us),
                dts_us: packet.dts().unwrap_or(raw.pts_us),
                duration_us: u64::try_from(packet.duration())
                    .ok()
                    .filter(|duration| *duration > 0)
                    .unwrap_or(self.frame_duration_us),
                key: bitstream_key,
                packet,
            });
        }
        Ok(output)
    }
}

fn encoder_extradata(encoder: &ffmpeg::encoder::Video) -> io::Result<&[u8]> {
    // SAFETY: the opened encoder owns this allocation for at least the returned borrow, and the
    // FFmpeg context is not mutated while the slice is inspected during construction.
    let context = unsafe { &*encoder.as_ptr() };
    let length = usize::try_from(context.extradata_size)
        .ok()
        .filter(|length| *length <= 4096)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid x264 extradata size"))?;
    if length == 0 || context.extradata.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "x264 did not publish SPS/PPS decoder configuration",
        ));
    }
    // SAFETY: FFmpeg reports `extradata_size` readable bytes at `extradata`; the bounds above cap
    // the allocation before forming the slice.
    Ok(unsafe { std::slice::from_raw_parts(context.extradata, length) })
}

fn h264_decoder_description(extradata: &[u8]) -> io::Result<H264DecoderDescription> {
    let avcc = if extradata.first() == Some(&1) {
        if extradata.len() < 7 || extradata.len() > 4096 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "x264 avcC decoder configuration is invalid",
            ));
        }
        extradata.to_vec()
    } else {
        avcc_from_annexb(extradata)?
    };
    let profile = i32::from(avcc[1]);
    let level = i32::from(avcc[3]);
    let portable_extradata = annexb_from_avcc(&avcc)?;
    Ok(H264DecoderDescription {
        profile,
        level,
        codec_string: format!("avc1.{:02X}{:02X}{:02X}", avcc[1], avcc[2], avcc[3]),
        extradata: portable_extradata,
        decoder_config: avcc,
    })
}

fn annexb_from_avcc(avcc: &[u8]) -> io::Result<Vec<u8>> {
    if avcc.len() < 7 || avcc[0] != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "x264 avcC decoder configuration is invalid",
        ));
    }
    let mut cursor = 6;
    let sps_count = usize::from(avcc[5] & 0x1f);
    if sps_count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "x264 avcC decoder configuration has no SPS",
        ));
    }
    let mut parameter_sets = Vec::with_capacity(sps_count.saturating_add(1));
    for _ in 0..sps_count {
        let sps = take_avcc_nal(avcc, &mut cursor)?;
        if sps.first().is_none_or(|header| header & 0x1f != 7) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "x264 avcC decoder configuration has an invalid SPS",
            ));
        }
        parameter_sets.push(sps);
    }
    let pps_count = avcc.get(cursor).copied().map(usize::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "x264 avcC decoder configuration has no PPS count",
        )
    })?;
    cursor += 1;
    if pps_count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "x264 avcC decoder configuration has no PPS",
        ));
    }
    for _ in 0..pps_count {
        let pps = take_avcc_nal(avcc, &mut cursor)?;
        if pps.first().is_none_or(|header| header & 0x1f != 8) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "x264 avcC decoder configuration has an invalid PPS",
            ));
        }
        parameter_sets.push(pps);
    }
    let capacity = parameter_sets
        .iter()
        .try_fold(0_usize, |total, parameter_set| {
            total.checked_add(4)?.checked_add(parameter_set.len())
        });
    let mut annexb = Vec::with_capacity(capacity.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "x264 Annex-B decoder initialization is oversized",
        )
    })?);
    for parameter_set in parameter_sets {
        annexb.extend_from_slice(&[0, 0, 0, 1]);
        annexb.extend_from_slice(parameter_set);
    }
    Ok(annexb)
}

fn take_avcc_nal<'a>(avcc: &'a [u8], cursor: &mut usize) -> io::Result<&'a [u8]> {
    let length_end = cursor
        .checked_add(2)
        .filter(|end| *end <= avcc.len())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "x264 avcC parameter-set length is truncated",
            )
        })?;
    let length = usize::from(u16::from_be_bytes(
        avcc[*cursor..length_end].try_into().unwrap(),
    ));
    *cursor = length_end;
    let end = cursor
        .checked_add(length)
        .filter(|end| *end <= avcc.len())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "x264 avcC parameter set is truncated",
            )
        })?;
    let nal = &avcc[*cursor..end];
    *cursor = end;
    if nal.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "x264 avcC parameter set is empty",
        ));
    }
    Ok(nal)
}

fn avcc_from_annexb(extradata: &[u8]) -> io::Result<Vec<u8>> {
    let nals = annexb_nals(extradata);
    let sps = nals
        .iter()
        .copied()
        .find(|nal| nal.first().is_some_and(|header| header & 0x1f == 7))
        .filter(|sps| sps.len() >= 4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "x264 extradata has no SPS"))?;
    let pps = nals
        .iter()
        .copied()
        .find(|nal| nal.first().is_some_and(|header| header & 0x1f == 8))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "x264 extradata has no PPS"))?;
    let sps_length = u16::try_from(sps.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "x264 SPS is oversized"))?;
    let pps_length = u16::try_from(pps.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "x264 PPS is oversized"))?;
    let capacity = 11_usize
        .checked_add(sps.len())
        .and_then(|length| length.checked_add(pps.len()))
        .filter(|length| *length <= 4096)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "x264 decoder configuration is oversized",
            )
        })?;
    let mut avcc = Vec::with_capacity(capacity);
    avcc.extend_from_slice(&[1, sps[1], sps[2], sps[3], 0xff, 0xe1]);
    avcc.extend_from_slice(&sps_length.to_be_bytes());
    avcc.extend_from_slice(sps);
    avcc.push(1);
    avcc.extend_from_slice(&pps_length.to_be_bytes());
    avcc.extend_from_slice(pps);
    Ok(avcc)
}

fn annexb_nals(mut data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    while let Some((start, prefix)) = find_start_code(data) {
        data = &data[start + prefix..];
        let end = find_start_code(data).map_or(data.len(), |(index, _)| index);
        let mut nal = &data[..end];
        while nal.last() == Some(&0) {
            nal = &nal[..nal.len() - 1];
        }
        if !nal.is_empty() {
            nals.push(nal);
        }
        data = &data[end..];
    }
    nals
}

fn find_start_code(data: &[u8]) -> Option<(usize, usize)> {
    (0..data.len().saturating_sub(2)).find_map(|index| {
        if data[index..].starts_with(&[0, 0, 0, 1]) {
            Some((index, 4))
        } else if data[index..].starts_with(&[0, 0, 1]) {
            Some((index, 3))
        } else {
            None
        }
    })
}

pub(crate) fn ffmpeg_error(error: ffmpeg::Error) -> io::Error {
    io::Error::other(format!("FFmpeg: {error}"))
}

/// How much of the target rate the x264 VBV may hold, in milliseconds.
const VBV_BUFFER_MS: u64 = 300;
/// Encoder targets move in whole steps so ordinary jitter cannot cause a re-open.
const RATE_STEP_BITS_PER_SECOND: u64 = 250_000;
/// Below this the picture is no longer worth the bits; the frame rate absorbs the rest.
const MINIMUM_TARGET_BITS_PER_SECOND: u64 = 400_000;
/// One decision per window. Shorter windows chase individual key frames.
const RATE_WINDOW: Duration = Duration::from_millis(1_000);
/// The share of a window spent inside the transport write that counts as congestion.
const CONGESTED_TRANSPORT_PERCENT: u64 = 10;
/// Below this the link is carrying everything offered and the target may grow again.
const UNCONGESTED_TRANSPORT_PERCENT: u64 = 2;
/// Audio backlog that means the session is congested even if video happens not to be blocked.
const CONGESTED_AUDIO_BACKLOG_US: u64 = 250_000;
/// Delivering this share of the target means the target is not what is limiting the stream, so
/// lowering it cannot help. Without this the loop has no fixed point and decays to the floor.
const DELIVERED_TARGET_PERCENT: u64 = 90;

/// Closed-loop control of what the encoder is allowed to offer the transport.
///
/// The producer cannot measure the link directly, so it measures where its sends actually wait.
/// Only [`SendPressure::transport`] means the link will not take the bytes; the declared-rate
/// limiter is self-imposed, and channel-flow waiting means the presenter is behind on *records*,
/// which fewer bits per frame would not help — that is the frame rate's job, and the encoder
/// already paces itself against the queue for it.
///
/// Getting this wrong is not a matter of tuning. If the bitrate answers a frame-rate limit, the
/// bytes delivered fall in proportion to the target, the next window sees an even lower delivered
/// rate, and the loop decays to the floor with no fixed point: the stream degrades to a blurry,
/// pixelated picture over a few seconds while the link sits idle. Hence both the transport-only
/// signal and the delivered-target guard below.
pub struct VideoRateControl {
    configured_bits_per_second: u64,
    target_bits_per_second: AtomicU64,
    inner: Mutex<RateWindow>,
}

struct RateWindow {
    started: Instant,
    bytes: u64,
    pressure: SendPressure,
    audio_backlog_us: u64,
    adjustments: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoRateSnapshot {
    pub configured_bits_per_second: u64,
    pub target_bits_per_second: u64,
    pub adjustments: u64,
    pub rate_limited: Duration,
    pub flow_limited: Duration,
    pub transport: Duration,
}

impl VideoRateControl {
    pub fn new(configured_bits_per_second: u64) -> Self {
        let configured = configured_bits_per_second.max(MINIMUM_TARGET_BITS_PER_SECOND);
        Self {
            configured_bits_per_second: configured,
            target_bits_per_second: AtomicU64::new(configured),
            inner: Mutex::new(RateWindow {
                started: Instant::now(),
                bytes: 0,
                pressure: SendPressure::default(),
                audio_backlog_us: 0,
                adjustments: 0,
            }),
        }
    }

    pub fn target(&self) -> u64 {
        self.target_bits_per_second.load(Ordering::Acquire)
    }

    /// Account one completed media send: its body size and where the send waited.
    pub fn observe_send(&self, bytes: usize, pressure: SendPressure) {
        let mut window = self.lock();
        window.bytes = window
            .bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        window.pressure.rate_limited = window
            .pressure
            .rate_limited
            .saturating_add(pressure.rate_limited);
        window.pressure.flow_limited = window
            .pressure
            .flow_limited
            .saturating_add(pressure.flow_limited);
        window.pressure.transport = window.pressure.transport.saturating_add(pressure.transport);
        window.pressure.records = window.pressure.records.saturating_add(pressure.records);
    }

    /// Audio that cannot be handed to the transport is congestion the video target has to answer
    /// for: audio is two orders of magnitude cheaper, so if it is backing up, video is the cause.
    pub fn observe_audio_backlog(&self, queued_duration_us: u64) {
        let mut window = self.lock();
        window.audio_backlog_us = window.audio_backlog_us.max(queued_duration_us);
    }

    /// Close the window if it is due and return a changed target.
    pub fn poll(&self) -> Option<u64> {
        let mut window = self.lock();
        let elapsed = window.started.elapsed();
        if elapsed < RATE_WINDOW {
            return None;
        }
        let current = self.target_bits_per_second.load(Ordering::Acquire);
        let next = next_target(
            current,
            self.configured_bits_per_second,
            achieved_bits_per_second(window.bytes, elapsed),
            blocked_percent(window.pressure.transport, elapsed),
            window.audio_backlog_us,
        );
        window.started = Instant::now();
        window.bytes = 0;
        window.pressure = SendPressure::default();
        window.audio_backlog_us = 0;
        if next == current {
            return None;
        }
        window.adjustments = window.adjustments.saturating_add(1);
        self.target_bits_per_second.store(next, Ordering::Release);
        Some(next)
    }

    pub fn snapshot(&self) -> VideoRateSnapshot {
        let window = self.lock();
        VideoRateSnapshot {
            configured_bits_per_second: self.configured_bits_per_second,
            target_bits_per_second: self.target_bits_per_second.load(Ordering::Acquire),
            adjustments: window.adjustments,
            rate_limited: window.pressure.rate_limited,
            flow_limited: window.pressure.flow_limited,
            transport: window.pressure.transport,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RateWindow> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn achieved_bits_per_second(bytes: u64, elapsed: Duration) -> u64 {
    let micros = elapsed.as_micros().max(1);
    u64::try_from(u128::from(bytes).saturating_mul(8_000_000) / micros).unwrap_or(u64::MAX)
}

fn blocked_percent(blocked: Duration, elapsed: Duration) -> u64 {
    let micros = elapsed.as_micros().max(1);
    u64::try_from(blocked.as_micros().saturating_mul(100) / micros)
        .unwrap_or(100)
        .min(100)
}

/// The control law, split out so the decision is testable without a transport.
///
/// `transport_percent` is the share of the window spent inside the transport write — deliberately
/// not the total time `send` blocked, which also covers this producer's own rate limiter and the
/// presenter's channel-flow window. Neither of those is answered by encoding at a lower rate.
fn next_target(
    current: u64,
    configured: u64,
    achieved_bits_per_second: u64,
    transport_percent: u64,
    audio_backlog_us: u64,
) -> u64 {
    // If the target is reaching the presenter, the target is not the constraint. Whatever else is
    // slow — the encoder, the compositor, the presenter's decode — lowering the bitrate only
    // spends the same frames worse, and each lower target would make the next window look worse
    // still. This is the guard that gives the loop a fixed point.
    let delivering = achieved_bits_per_second.saturating_mul(100)
        >= current.saturating_mul(DELIVERED_TARGET_PERCENT);
    let link_full = transport_percent >= CONGESTED_TRANSPORT_PERCENT && !delivering;
    // Audio is two orders of magnitude cheaper than video. If it cannot reach the transport, the
    // session is over-subscribed whatever the video sends look like.
    let over_subscribed = audio_backlog_us >= CONGESTED_AUDIO_BACKLOG_US;
    let candidate = if link_full || over_subscribed {
        // Back off from whatever the link actually carried, not from what was asked for: a target
        // that only ever halves itself takes far too long to reach a link an order of magnitude
        // slower than the configured ceiling.
        let reference = if achieved_bits_per_second > 0 {
            current.min(achieved_bits_per_second)
        } else {
            current
        };
        reference.saturating_mul(7) / 8
    } else if transport_percent <= UNCONGESTED_TRANSPORT_PERCENT
        && audio_backlog_us < CONGESTED_AUDIO_BACKLOG_US
    {
        current.saturating_add(configured / 8)
    } else {
        current
    };
    quantize_target(candidate, configured)
}

fn quantize_target(candidate: u64, configured: u64) -> u64 {
    let clamped = candidate.clamp(MINIMUM_TARGET_BITS_PER_SECOND, configured);
    if clamped >= configured {
        return configured;
    }
    (clamped / RATE_STEP_BITS_PER_SECOND)
        .saturating_mul(RATE_STEP_BITS_PER_SECOND)
        .max(MINIMUM_TARGET_BITS_PER_SECOND)
}

pub struct EncodedQueue {
    inner: Mutex<EncodedQueueState>,
    changed: Condvar,
    max_packets: usize,
    max_bytes: usize,
    max_age: Duration,
}

struct EncodedQueueState {
    packets: VecDeque<QueuedVideo>,
    bytes: usize,
    closed: bool,
    overflowed: bool,
    overflow_events: u64,
    dropped_packets: u64,
    peak_packets: usize,
    peak_bytes: usize,
    pushed_packets: u64,
    popped_packets: u64,
    cumulative_send_blocked: Duration,
    maximum_send_blocked: Duration,
}

struct QueuedVideo {
    packet: EncodedVideo,
    queued_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodedQueueSnapshot {
    pub packets: usize,
    pub bytes: usize,
    pub oldest_age: Duration,
    pub overflow_events: u64,
    pub dropped_packets: u64,
    pub peak_packets: usize,
    pub peak_bytes: usize,
    pub pushed_packets: u64,
    pub popped_packets: u64,
    pub cumulative_send_blocked: Duration,
    pub maximum_send_blocked: Duration,
}

impl EncodedQueue {
    pub fn new(max_packets: usize, max_bytes: usize, max_age: Duration) -> Self {
        Self {
            inner: Mutex::new(EncodedQueueState {
                packets: VecDeque::new(),
                bytes: 0,
                closed: false,
                overflowed: false,
                overflow_events: 0,
                dropped_packets: 0,
                peak_packets: 0,
                peak_bytes: 0,
                pushed_packets: 0,
                popped_packets: 0,
                cumulative_send_blocked: Duration::ZERO,
                maximum_send_blocked: Duration::ZERO,
            }),
            changed: Condvar::new(),
            max_packets: max_packets.max(1),
            max_bytes: max_bytes.max(1),
            max_age,
        }
    }

    /// Returns true when the old GOP was discarded and the encoder must force a new keyframe.
    pub fn push(&self, packet: EncodedVideo) -> bool {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        let age_overflow = state
            .packets
            .front()
            .is_some_and(|queued| now.saturating_duration_since(queued.queued_at) >= self.max_age);
        let pts_overflow = state.packets.front().is_some_and(|queued| {
            let span_us = packet.pts_us.saturating_sub(queued.packet.pts_us);
            span_us >= i64::try_from(self.max_age.as_micros()).unwrap_or(i64::MAX)
        });
        let would_overflow = state.packets.len() >= self.max_packets
            || state.bytes.saturating_add(packet.data_len()) > self.max_bytes
            || age_overflow
            || pts_overflow;
        if would_overflow {
            state.dropped_packets = state
                .dropped_packets
                .saturating_add(u64::try_from(state.packets.len()).unwrap_or(u64::MAX));
            state.packets.clear();
            state.bytes = 0;
            state.overflowed = true;
            state.overflow_events = state.overflow_events.saturating_add(1);
        }
        if !would_overflow || packet.key {
            state.bytes = state.bytes.saturating_add(packet.data_len());
            state.packets.push_back(QueuedVideo {
                packet,
                queued_at: now,
            });
            state.peak_packets = state.peak_packets.max(state.packets.len());
            state.peak_bytes = state.peak_bytes.max(state.bytes);
            state.pushed_packets = state.pushed_packets.saturating_add(1);
            self.changed.notify_one();
        } else {
            state.dropped_packets = state.dropped_packets.saturating_add(1);
        }
        would_overflow
    }

    pub fn pop(&self, timeout: Duration) -> Option<EncodedVideo> {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.packets.is_empty() && !state.closed {
            state = self
                .changed
                .wait_timeout(state, timeout)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        let queued = state.packets.pop_front();
        if let Some(queued) = &queued {
            state.bytes = state.bytes.saturating_sub(queued.packet.data_len());
            state.popped_packets = state.popped_packets.saturating_add(1);
        }
        queued.map(|queued| queued.packet)
    }

    /// How many encoded access units are still waiting for the transport.
    ///
    /// The encoder uses this as its pacing signal. Frames that are produced faster than the link
    /// drains them are the whole source of the backlog, and a latest-value capture can skip one
    /// for free — whereas an encoded frame can only be discarded by breaking the GOP.
    pub fn staged(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .packets
            .len()
    }

    pub fn take_overflow(&self) -> bool {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut state.overflowed)
    }

    pub fn clear(&self) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.dropped_packets = state
            .dropped_packets
            .saturating_add(u64::try_from(state.packets.len()).unwrap_or(u64::MAX));
        state.packets.clear();
        state.bytes = 0;
    }

    pub fn snapshot(&self) -> EncodedQueueSnapshot {
        let state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        EncodedQueueSnapshot {
            packets: state.packets.len(),
            bytes: state.bytes,
            oldest_age: state
                .packets
                .front()
                .map_or(Duration::ZERO, |packet| packet.queued_at.elapsed()),
            overflow_events: state.overflow_events,
            dropped_packets: state.dropped_packets,
            peak_packets: state.peak_packets,
            peak_bytes: state.peak_bytes,
            pushed_packets: state.pushed_packets,
            popped_packets: state.popped_packets,
            cumulative_send_blocked: state.cumulative_send_blocked,
            maximum_send_blocked: state.maximum_send_blocked,
        }
    }

    pub fn record_send_duration(&self, duration: Duration) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.cumulative_send_blocked = state.cumulative_send_blocked.saturating_add(duration);
        state.maximum_send_blocked = state.maximum_send_blocked.max(duration);
    }

    pub fn close(&self) {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .closed = true;
        self.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(key: bool, bytes: usize) -> EncodedVideo {
        EncodedVideo {
            pts_us: 0,
            dts_us: 0,
            duration_us: 33_333,
            key,
            packet: ffmpeg::Packet::new(bytes),
        }
    }

    #[test]
    fn out_of_range_frame_rates_are_rejected_before_encoder_open() {
        // Regression: a zero rate used to reach `1_000_000 / fps` and panic, after libx264 had
        // already derived a million-frame-per-second rate from the microsecond time base.
        for fps in [0, 241] {
            let kind = H264Encoder::new(64, 64, fps, 100_000, 2, 1_048_576)
                .err()
                .map(|error| error.kind());
            assert_eq!(kind, Some(io::ErrorKind::InvalidInput), "fps {fps}");
        }
        assert!(H264Encoder::new(64, 64, 1, 100_000, 2, 1_048_576).is_ok());
        assert!(H264Encoder::new(64, 64, 240, 100_000, 2, 1_048_576).is_ok());
    }

    #[test]
    fn x264_encoder_emits_idr_for_forced_frame() {
        let mut encoder = H264Encoder::new(64, 64, 30, 100_000, 2, 1_048_576).unwrap();
        let description = encoder.decoder_description();
        assert_eq!(description.profile, 100);
        assert_eq!(description.level, 41);
        assert!(description.codec_string.starts_with("avc1.64"));
        let parameter_sets = annexb_nals(&description.extradata);
        assert!(parameter_sets.iter().any(|nal| nal[0] & 0x1f == 7));
        assert!(parameter_sets.iter().any(|nal| nal[0] & 0x1f == 8));
        assert_eq!(description.decoder_config.first(), Some(&1));
        let frame = RawFrame {
            format: RawPixelFormat::Bgrx,
            width: 64,
            height: 64,
            pts_us: 0,
            data: Arc::from([0_u8, 0, 0, 255].repeat(64 * 64)),
        };

        let packets = encoder.encode(frame, true).unwrap();
        assert_eq!(packets.len(), 1);
        assert!(packets[0].key);
        assert!(!packets[0].data().is_empty());
    }

    #[test]
    fn annex_b_parameter_sets_build_decoder_ready_avcc() {
        let annexb = [
            0, 0, 0, 1, 0x67, 0x64, 0x00, 0x29, 0xaa, 0, 0, 1, 0x68, 0xbb, 0xcc,
        ];
        let description = h264_decoder_description(&annexb).unwrap();
        assert_eq!(description.profile, 100);
        assert_eq!(description.level, 41);
        assert_eq!(description.codec_string, "avc1.640029");
        assert_eq!(
            description.extradata,
            [
                0, 0, 0, 1, 0x67, 0x64, 0x00, 0x29, 0xaa, 0, 0, 0, 1, 0x68, 0xbb, 0xcc
            ]
        );
        assert_eq!(
            &description.decoder_config[..6],
            &[1, 0x64, 0, 0x29, 0xff, 0xe1]
        );
        assert!(
            description
                .decoder_config
                .windows(3)
                .any(|bytes| bytes == [0x68, 0xbb, 0xcc])
        );
    }

    #[test]
    fn x264_encoder_emits_mid_gop_idr_for_forced_frame() {
        let mut encoder = H264Encoder::new(64, 64, 30, 100_000, 30, 1_048_576).unwrap();
        let frame = |pts_us| RawFrame {
            format: RawPixelFormat::Bgrx,
            width: 64,
            height: 64,
            pts_us,
            data: Arc::from([0_u8, 0, 0, 255].repeat(64 * 64)),
        };

        assert!(encoder.encode(frame(0), false).unwrap()[0].key);
        assert!(!encoder.encode(frame(33_333), false).unwrap()[0].key);
        assert!(encoder.encode(frame(66_666), true).unwrap()[0].key);
    }

    #[test]
    fn latest_frame_retains_a_snapshot_after_delivery() {
        let latest = LatestFrame::new();
        let frame = RawFrame {
            format: RawPixelFormat::Bgrx,
            width: 1,
            height: 1,
            pts_us: 7,
            data: Arc::from([1_u8, 2, 3, 4]),
        };
        latest.replace(frame);

        let mut serial = 0;
        let delivered = latest
            .wait_next(&mut serial, Duration::ZERO)
            .unwrap()
            .unwrap();
        let (snapshot_serial, retained) = latest.snapshot().unwrap().unwrap();
        assert_eq!(snapshot_serial, serial);
        assert_eq!(delivered.pts_us, 7);
        assert_eq!(retained.pts_us, 7);
        assert!(Arc::ptr_eq(&delivered.data, &retained.data));
    }

    #[test]
    fn frame_age_starts_near_zero_and_only_grows_until_the_next_frame() {
        let latest = LatestFrame::new();
        let frame = |pts| RawFrame {
            format: RawPixelFormat::Bgrx,
            width: 1,
            height: 1,
            pts_us: pts,
            data: Arc::from([1_u8, 2, 3, 4]),
        };

        // No frame yet: nothing to report an age for.
        assert!(latest.snapshot_with_age().unwrap().is_none());

        latest.replace(frame(7));
        let (_, _, first) = latest.snapshot_with_age().unwrap().unwrap();
        assert!(
            first < Duration::from_secs(1),
            "a just-captured frame should be fresh, got {first:?}"
        );

        // Age is wall-clock since arrival, so it only ever grows while the desktop is idle.
        let (_, _, later) = latest.snapshot_with_age().unwrap().unwrap();
        assert!(later >= first, "{later:?} < {first:?}");

        // A new frame resets it, and does so on arrival rather than on PTS: this frame carries an
        // *older* PTS than the last one, and the age must still reset.
        latest.replace(frame(1));
        let (serial, retained, reset) = latest.snapshot_with_age().unwrap().unwrap();
        assert_eq!(serial, 2);
        assert_eq!(retained.pts_us, 1);
        assert!(reset <= later, "a new frame must reset the age");
    }

    #[test]
    fn snapshot_and_snapshot_with_age_report_the_same_frame() {
        let latest = LatestFrame::new();
        latest.replace(RawFrame {
            format: RawPixelFormat::Bgrx,
            width: 1,
            height: 1,
            pts_us: 7,
            data: Arc::from([1_u8, 2, 3, 4]),
        });
        let (plain_serial, plain) = latest.snapshot().unwrap().unwrap();
        let (aged_serial, aged, _) = latest.snapshot_with_age().unwrap().unwrap();
        assert_eq!(plain_serial, aged_serial);
        assert_eq!(plain.pts_us, aged.pts_us);
        assert!(Arc::ptr_eq(&plain.data, &aged.data));

        // A closed capture is an error on both, not a stale frame with a large age.
        latest.close(Some("capture stopped".into()));
        assert_eq!(
            latest.snapshot().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            latest.snapshot_with_age().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn congestion_lowers_the_target_toward_what_the_link_actually_carried() {
        // A link carrying 2 Mbit/s while 8 Mbit/s is offered must converge on the link, not creep
        // down from the ceiling: the old design never lowered the offer at all and answered the
        // resulting overflow with key frames, which cost more than the frames it discarded.
        let mut target = 8_000_000;
        for _ in 0..8 {
            target = next_target(target, 8_000_000, 2_000_000, 60, 0);
        }
        assert!(
            (1_500_000..=2_500_000).contains(&target),
            "converged on {target}, which should sit near the 2 Mbit/s the link carried"
        );
    }

    #[test]
    fn a_frame_limited_session_keeps_its_full_quality_budget() {
        // The regression the pressure split exists for. When the presenter returns capacity for
        // only half the frames, the sender waits on channel flow rather than on the transport, and
        // the bytes delivered fall in proportion to the target. Answering that with a lower target
        // makes the next window look worse still: the previous law read 8 Mbit/s down to the
        // 400 kbit/s floor in four windows, which is a 1080p picture going visibly blurry and
        // pixelated while a link that was never full sat idle. Fewer frames must cost frame rate,
        // never bits per frame.
        let mut target = 8_000_000;
        for _ in 0..8 {
            // Half the target delivered, with no time at all inside the transport write.
            target = next_target(target, 8_000_000, target / 2, 0, 0);
        }
        assert_eq!(target, 8_000_000);
    }

    #[test]
    fn a_target_that_is_being_delivered_is_never_lowered() {
        // The loop's fixed point. Once the target is arriving, transport pressure cannot mean the
        // target is too high, so a saturated link settles at its capacity instead of passing
        // through it on the way to the floor.
        assert_eq!(
            next_target(2_000_000, 8_000_000, 2_000_000, 90, 0),
            2_000_000
        );
    }

    #[test]
    fn only_transport_pressure_reaches_the_control_law() {
        // Self-imposed pacing and channel-flow waiting are recorded for diagnostics but must not
        // move the target: a window full of both leaves a lowered target free to climb back.
        let rate = VideoRateControl::new(8_000_000);
        rate.target_bits_per_second
            .store(1_000_000, Ordering::Release);
        rate.observe_send(
            125_000,
            SendPressure {
                rate_limited: Duration::from_millis(400),
                flow_limited: Duration::from_millis(500),
                transport: Duration::ZERO,
                records: 30,
            },
        );
        std::thread::sleep(Duration::from_millis(1_050));

        assert_eq!(rate.poll(), Some(2_000_000));
    }

    #[test]
    fn an_uncongested_window_returns_the_target_to_the_configured_ceiling() {
        let mut target = 1_000_000;
        for _ in 0..16 {
            target = next_target(target, 8_000_000, 1_000_000, 0, 0);
        }
        assert_eq!(target, 8_000_000);
    }

    #[test]
    fn a_partly_blocked_window_holds_the_target_still() {
        // Between the two thresholds: neither congested enough to back off nor clear enough to
        // claim more.
        let held = next_target(4_000_000, 8_000_000, 1_000_000, 8, 0);
        assert_eq!(held, 4_000_000);
    }

    #[test]
    fn an_audio_backlog_counts_as_congestion_on_its_own() {
        // Audio is two orders of magnitude cheaper than video. If it cannot reach the transport,
        // video is what is filling the link, whether or not this window happened to block.
        let lowered = next_target(
            8_000_000,
            8_000_000,
            8_000_000,
            0,
            CONGESTED_AUDIO_BACKLOG_US,
        );
        assert!(lowered < 8_000_000, "{lowered}");
    }

    #[test]
    fn the_target_never_leaves_its_bounds_or_lands_off_the_ladder() {
        assert_eq!(next_target(400_000, 8_000_000, 1, 100, 0), 400_000);
        assert_eq!(
            next_target(8_000_000, 8_000_000, 8_000_000, 0, 0),
            8_000_000
        );
        let stepped = next_target(3_000_000, 8_000_000, 1_000_000, 50, 0);
        assert_eq!(stepped % RATE_STEP_BITS_PER_SECOND, 0, "{stepped}");
        // A configured ceiling under the floor still yields a usable, in-range target.
        assert_eq!(
            VideoRateControl::new(100_000).target(),
            MINIMUM_TARGET_BITS_PER_SECOND
        );
    }

    #[test]
    fn re_targeting_the_encoder_keeps_the_immutable_decoder_description() {
        // `coded_width`, `coded_height`, `profile`, `level`, and the decoder configuration are
        // immutable in the negotiated track, so adapting the rate must not disturb any of them.
        let mut encoder = H264Encoder::new(64, 64, 30, 8_000_000, 2, 1_048_576).unwrap();
        let original = encoder.decoder_description().clone();

        assert!(encoder.retarget(1_000_000).unwrap());
        assert_eq!(encoder.bitrate(), 1_000_000);
        assert_eq!(encoder.decoder_description(), &original);
        assert!(!encoder.retarget(1_000_000).unwrap());

        // The re-opened encoder still encodes, and still honours a forced IDR.
        let frame = RawFrame {
            format: RawPixelFormat::Bgrx,
            width: 64,
            height: 64,
            pts_us: 0,
            data: Arc::from([0_u8, 0, 0, 255].repeat(64 * 64)),
        };
        assert!(encoder.encode(frame, true).unwrap()[0].key);
    }

    #[test]
    fn staged_reports_what_the_transport_has_not_drained() {
        let queue = EncodedQueue::new(4, 1 << 20, Duration::from_millis(100));
        assert_eq!(queue.staged(), 0);
        queue.push(packet(true, 4));
        queue.push(packet(false, 4));
        assert_eq!(queue.staged(), 2);
        queue.pop(Duration::ZERO);
        assert_eq!(queue.staged(), 1);
    }

    #[test]
    fn bounded_queue_discards_old_gop_on_overflow() {
        let queue = EncodedQueue::new(2, 10, Duration::from_millis(100));
        assert!(!queue.push(packet(true, 4)));
        assert!(!queue.push(packet(false, 4)));
        assert!(queue.push(packet(false, 4)));
        assert!(queue.take_overflow());
        assert!(queue.pop(Duration::ZERO).is_none());
        assert!(!queue.push(packet(true, 4)));
        assert!(queue.pop(Duration::ZERO).unwrap().key);
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.overflow_events, 1);
        assert_eq!(snapshot.dropped_packets, 3);
        assert_eq!(snapshot.peak_packets, 2);
        assert_eq!(snapshot.peak_bytes, 8);
    }

    #[test]
    fn bounded_queue_uses_media_age_instead_of_waiting_for_a_gop_limit() {
        let queue = EncodedQueue::new(60, 1 << 20, Duration::from_millis(100));
        let mut first = packet(true, 4);
        first.pts_us = 1_000_000;
        assert!(!queue.push(first));
        let mut stale = packet(false, 4);
        stale.pts_us = 1_100_000;
        assert!(queue.push(stale));
        assert!(queue.pop(Duration::ZERO).is_none());
        assert_eq!(queue.snapshot().dropped_packets, 2);
    }
}
