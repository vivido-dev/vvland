use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use crate::linux::video::{CaptureSource, LatestFrame, RawFrame, RawPixelFormat};

const INITIAL_CAPTURE_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_DIMENSION: u32 = 8192;
const BYTES_PER_PIXEL: u32 = 4;

pub struct ScreencopyCapture {
    latest: Arc<LatestFrame>,
    stop: Arc<AtomicBool>,
    shutdown: UnixStream,
    thread: Option<thread::JoinHandle<()>>,
}

impl ScreencopyCapture {
    pub fn start(
        socket: &Path,
        width: u32,
        height: u32,
        fps: u32,
        origin: Instant,
    ) -> io::Result<Self> {
        validate_dimensions(width, height, fps)?;
        let stream = UnixStream::connect(socket)?;
        let shutdown = stream.try_clone()?;
        let latest = Arc::new(LatestFrame::new());
        let stop = Arc::new(AtomicBool::new(false));
        let thread_latest = Arc::clone(&latest);
        let thread_stop = Arc::clone(&stop);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("vvland-screencopy".into())
            .spawn(move || {
                let mut ready = Some(ready_tx);
                let outcome = capture_loop(
                    stream,
                    width,
                    height,
                    fps,
                    origin,
                    &thread_latest,
                    &thread_stop,
                    &mut ready,
                );
                if let Err(error) = outcome {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Err(io::Error::new(error.kind(), error.to_string())));
                    }
                    if !thread_stop.load(Ordering::Acquire) {
                        thread_latest.close(Some(error.to_string()));
                    }
                } else {
                    thread_latest.close(None);
                }
            })?;

        match ready_rx.recv_timeout(INITIAL_CAPTURE_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                latest,
                stop,
                shutdown,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                stop.store(true, Ordering::Release);
                let _ = shutdown.shutdown(std::net::Shutdown::Both);
                let _ = thread.join();
                Err(error)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                stop.store(true, Ordering::Release);
                let _ = shutdown.shutdown(std::net::Shutdown::Both);
                let _ = thread.join();
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Sway did not produce an initial screencopy frame",
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                stop.store(true, Ordering::Release);
                let _ = shutdown.shutdown(std::net::Shutdown::Both);
                let _ = thread.join();
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Sway screencopy worker exited before its first frame",
                ))
            }
        }
    }
}

impl CaptureSource for ScreencopyCapture {
    fn latest(&self) -> Arc<LatestFrame> {
        Arc::clone(&self.latest)
    }
}

impl Drop for ScreencopyCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.shutdown.shutdown(std::net::Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BufferInfo {
    format: wl_shm::Format,
    pixel_format: RawPixelFormat,
    width: u32,
    height: u32,
    stride: u32,
    size: usize,
}

#[derive(Default)]
struct FrameState {
    info: Option<BufferInfo>,
    buffer_done: bool,
    copied: bool,
    ready: bool,
    failed: Option<String>,
    y_invert: bool,
}

struct CaptureState {
    shm: wl_shm::WlShm,
    mapped: Option<MappedBuffer>,
    frame: FrameState,
    with_damage: bool,
    expected_width: u32,
    expected_height: u32,
}

impl CaptureState {
    fn begin_frame(&mut self, with_damage: bool) {
        self.frame = FrameState::default();
        self.with_damage = with_damage;
    }

    fn prepare_copy(
        &mut self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        qh: &QueueHandle<Self>,
    ) -> io::Result<()> {
        let info = self.frame.info.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "Sway screencopy did not offer a wl_shm buffer",
            )
        })?;
        if let Some(mapped) = &self.mapped {
            if mapped.info != info {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Sway changed screencopy buffer geometry or format",
                ));
            }
        } else {
            self.mapped = Some(MappedBuffer::new(&self.shm, info, qh)?);
        }
        let buffer = &self
            .mapped
            .as_ref()
            .expect("mapped screencopy buffer was created")
            .buffer;
        if self.with_damage {
            frame.copy_with_damage(buffer);
        } else {
            frame.copy(buffer);
        }
        self.frame.copied = true;
        Ok(())
    }

    fn packed_frame(&self, pts_us: i64) -> io::Result<RawFrame> {
        let mapped = self
            .mapped
            .as_ref()
            .ok_or_else(|| io::Error::other("screencopy completed without a buffer"))?;
        let data = copy_packed_pixels(mapped.bytes(), mapped.info, self.frame.y_invert)?;
        Ok(RawFrame {
            format: mapped.info.pixel_format,
            width: mapped.info.width,
            height: mapped.info.height,
            pts_us,
            data: Arc::from(data),
        })
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for CaptureState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer {
                format,
                width,
                height,
                stride,
            } => match buffer_info(format, width, height, stride) {
                Ok(info)
                    if info.width == state.expected_width
                        && info.height == state.expected_height =>
                {
                    if state.frame.info.replace(info).is_some() {
                        state.frame.failed =
                            Some("Sway sent more than one wl_shm screencopy format".into());
                    }
                }
                Ok(info) => {
                    state.frame.failed = Some(format!(
                        "Sway offered {}x{} screencopy for the expected {}x{} output",
                        info.width, info.height, state.expected_width, state.expected_height
                    ));
                }
                Err(error) => state.frame.failed = Some(error.to_string()),
            },
            Event::BufferDone => {
                state.frame.buffer_done = true;
                if let Err(error) = state.prepare_copy(frame, qh) {
                    state.frame.failed = Some(error.to_string());
                }
            }
            Event::Flags { flags } => {
                state.frame.y_invert = match flags {
                    WEnum::Value(flags) => flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
                    WEnum::Unknown(raw) => raw & 1 != 0,
                };
            }
            Event::Ready { .. } => state.frame.ready = true,
            Event::Failed => state.frame.failed = Some("Sway screencopy frame failed".into()),
            Event::Damage { .. } | Event::LinuxDmabuf { .. } => {}
            _ => {}
        }
    }
}

delegate_noop!(CaptureState: ignore wl_output::WlOutput);
delegate_noop!(CaptureState: ignore wl_shm::WlShm);
delegate_noop!(CaptureState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(CaptureState: ignore wl_buffer::WlBuffer);
delegate_noop!(CaptureState: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);

#[allow(clippy::too_many_arguments)]
fn capture_loop(
    stream: UnixStream,
    width: u32,
    height: u32,
    fps: u32,
    origin: Instant,
    latest: &LatestFrame,
    stop: &AtomicBool,
    ready: &mut Option<mpsc::SyncSender<io::Result<()>>>,
) -> io::Result<()> {
    let connection = Connection::from_socket(stream).map_err(wayland_error)?;
    let (globals, mut queue): (_, EventQueue<CaptureState>) =
        registry_queue_init(&connection).map_err(wayland_error)?;
    let qh = queue.handle();
    let manager: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1 =
        globals.bind(&qh, 3..=3, ()).map_err(wayland_error)?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).map_err(wayland_error)?;
    let output_global = globals
        .contents()
        .clone_list()
        .into_iter()
        .find(|global| global.interface == wl_output::WlOutput::interface().name)
        .ok_or_else(|| missing_global("wl_output"))?;
    let output: wl_output::WlOutput = globals.registry().bind(
        output_global.name,
        output_global
            .version
            .min(wl_output::WlOutput::interface().version),
        &qh,
        (),
    );
    let mut state = CaptureState {
        shm,
        mapped: None,
        frame: FrameState::default(),
        with_damage: false,
        expected_width: width,
        expected_height: height,
    };
    let frame_period = Duration::from_micros(1_000_000 / u64::from(fps));
    let mut first = true;

    while !stop.load(Ordering::Acquire) {
        let started = Instant::now();
        state.begin_frame(capture_waits_for_damage(first));
        let frame = manager.capture_output(1, &output, &qh, ());
        connection.flush().map_err(wayland_error)?;
        while !state.frame.ready && state.frame.failed.is_none() {
            queue.blocking_dispatch(&mut state).map_err(wayland_error)?;
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
        }
        if let Err(error) = validate_frame_completion(&state.frame) {
            frame.destroy();
            return Err(error);
        }
        let pts_us = i64::try_from(origin.elapsed().as_micros()).unwrap_or(i64::MAX);
        latest.replace(state.packed_frame(pts_us)?);
        frame.destroy();
        if let Some(sender) = ready.take() {
            let _ = sender.send(Ok(()));
        }
        first = false;

        let remaining = frame_limit_delay(frame_period, started.elapsed());
        if !remaining.is_zero() {
            thread::sleep(remaining);
        }
    }
    Ok(())
}

struct MappedBuffer {
    info: BufferInfo,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    _file: File,
    pointer: NonNull<u8>,
}

impl MappedBuffer {
    fn new(
        shm: &wl_shm::WlShm,
        info: BufferInfo,
        qh: &QueueHandle<CaptureState>,
    ) -> io::Result<Self> {
        let file = anonymous_file()?;
        file.set_len(u64::try_from(info.size).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "screencopy size exceeds u64")
        })?)?;
        // SAFETY: the fd is valid, size is checked and non-zero, and the mapping is unmapped in Drop.
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                info.size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_fd().as_raw_fd(),
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let pointer = NonNull::new(raw.cast::<u8>())
            .ok_or_else(|| io::Error::other("mmap returned a null screencopy buffer"))?;
        let pool_size = i32::try_from(info.size)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "wl_shm pool is too large"))?;
        let pool = shm.create_pool(file.as_fd(), pool_size, qh, ());
        let buffer = pool.create_buffer(
            0,
            i32::try_from(info.width).expect("validated screencopy width"),
            i32::try_from(info.height).expect("validated screencopy height"),
            i32::try_from(info.stride).expect("validated screencopy stride"),
            info.format,
            qh,
            (),
        );
        Ok(Self {
            info,
            pool,
            buffer,
            _file: file,
            pointer,
        })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping remains valid for self's lifetime and has exactly info.size bytes.
        unsafe { std::slice::from_raw_parts(self.pointer.as_ptr(), self.info.size) }
    }
}

impl Drop for MappedBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        // SAFETY: this pointer and length are exactly those returned to us by mmap.
        unsafe {
            libc::munmap(self.pointer.as_ptr().cast(), self.info.size);
        }
    }
}

fn validate_dimensions(width: u32, height: u32, fps: u32) -> io::Result<()> {
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || !(1..=240).contains(&fps)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Sway capture dimensions or frame rate",
        ));
    }
    Ok(())
}

fn capture_waits_for_damage(first: bool) -> bool {
    !first
}

fn frame_limit_delay(frame_period: Duration, elapsed: Duration) -> Duration {
    frame_period.saturating_sub(elapsed)
}

fn validate_frame_completion(frame: &FrameState) -> io::Result<()> {
    if let Some(error) = &frame.failed {
        return Err(io::Error::new(io::ErrorKind::InvalidData, error.clone()));
    }
    if !frame.ready || !frame.buffer_done || !frame.copied {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Sway completed screencopy without buffer negotiation",
        ));
    }
    Ok(())
}

fn buffer_info(
    format: WEnum<wl_shm::Format>,
    width: u32,
    height: u32,
    stride: u32,
) -> io::Result<BufferInfo> {
    validate_dimensions(width, height, 1)?;
    let format = match format {
        WEnum::Value(format) => format,
        WEnum::Unknown(format) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported wl_shm format 0x{format:08x}"),
            ));
        }
    };
    let pixel_format = match format {
        wl_shm::Format::Xrgb8888 => RawPixelFormat::Bgrx,
        wl_shm::Format::Argb8888 => RawPixelFormat::Bgra,
        wl_shm::Format::Xbgr8888 => RawPixelFormat::Rgbx,
        wl_shm::Format::Abgr8888 => RawPixelFormat::Rgba,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported Sway wl_shm format {format:?}"),
            ));
        }
    };
    let row_bytes = width
        .checked_mul(BYTES_PER_PIXEL)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame row size overflow"))?;
    if stride < row_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "screencopy stride is shorter than one pixel row",
        ));
    }
    let size = usize::try_from(stride)
        .ok()
        .and_then(|stride| {
            usize::try_from(height)
                .ok()
                .and_then(|height| stride.checked_mul(height))
        })
        .filter(|size| *size <= i32::MAX as usize)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame size overflow"))?;
    Ok(BufferInfo {
        format,
        pixel_format,
        width,
        height,
        stride,
        size,
    })
}

fn copy_packed_pixels(source: &[u8], info: BufferInfo, y_invert: bool) -> io::Result<Vec<u8>> {
    if source.len() != info.size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mapped screencopy length changed",
        ));
    }
    let row_bytes = usize::try_from(info.width)
        .ok()
        .and_then(|width| width.checked_mul(BYTES_PER_PIXEL as usize))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "packed row size overflow"))?;
    let height = usize::try_from(info.height).expect("validated height");
    let stride = usize::try_from(info.stride).expect("validated stride");
    let output_size = row_bytes
        .checked_mul(height)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "packed frame size overflow"))?;
    let mut output = vec![0; output_size];
    for destination_row in 0..height {
        let source_row = if y_invert {
            height - 1 - destination_row
        } else {
            destination_row
        };
        let source_start = source_row * stride;
        let destination_start = destination_row * row_bytes;
        output[destination_start..destination_start + row_bytes]
            .copy_from_slice(&source[source_start..source_start + row_bytes]);
    }
    Ok(output)
}

fn anonymous_file() -> io::Result<File> {
    let mut template = CString::new("/tmp/vvland-screencopy-XXXXXX")
        .expect("static screencopy temporary-file template has no NUL")
        .into_bytes_with_nul();
    // SAFETY: mkstemp receives a writable NUL-terminated XXXXXX template and returns an owned fd.
    let fd = unsafe { libc::mkstemp(template.as_mut_ptr().cast()) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the generated path stays NUL-terminated; unlink leaves the descriptor valid.
    unsafe {
        libc::unlink(template.as_ptr().cast());
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    // SAFETY: fd was returned as a new owned descriptor above.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn missing_global(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("Sway does not advertise required Wayland global {name}"),
    )
}

fn wayland_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(stride: u32) -> BufferInfo {
        buffer_info(WEnum::Value(wl_shm::Format::Xrgb8888), 2, 2, stride).unwrap()
    }

    #[test]
    fn rejects_short_stride_and_unknown_format() {
        assert!(buffer_info(WEnum::Value(wl_shm::Format::Xrgb8888), 2, 2, 7).is_err());
        assert!(buffer_info(WEnum::Unknown(0xdead_beef), 2, 2, 8).is_err());
    }

    #[test]
    fn accepts_all_required_formats() {
        for (format, pixel) in [
            (wl_shm::Format::Xrgb8888, RawPixelFormat::Bgrx),
            (wl_shm::Format::Argb8888, RawPixelFormat::Bgra),
            (wl_shm::Format::Xbgr8888, RawPixelFormat::Rgbx),
            (wl_shm::Format::Abgr8888, RawPixelFormat::Rgba),
        ] {
            assert_eq!(
                buffer_info(WEnum::Value(format), 4, 3, 16)
                    .unwrap()
                    .pixel_format,
                pixel
            );
        }
    }

    #[test]
    fn strips_stride_padding_and_inverts_rows() {
        let source = [
            1, 2, 3, 4, 5, 6, 7, 8, 99, 99, 99, 99, 9, 10, 11, 12, 13, 14, 15, 16, 88, 88, 88, 88,
        ];
        assert_eq!(
            copy_packed_pixels(&source, info(12), false).unwrap(),
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
        assert_eq!(
            copy_packed_pixels(&source, info(12), true).unwrap(),
            [9, 10, 11, 12, 13, 14, 15, 16, 1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    #[test]
    fn validates_capture_limits() {
        assert!(validate_dimensions(1920, 1080, 60).is_ok());
        assert!(validate_dimensions(0, 1080, 60).is_err());
        assert!(validate_dimensions(8193, 1080, 60).is_err());
        assert!(validate_dimensions(1920, 1080, 241).is_err());
    }

    #[test]
    fn only_the_initial_capture_is_immediate() {
        assert!(!capture_waits_for_damage(true));
        assert!(capture_waits_for_damage(false));
    }

    #[test]
    fn frame_rate_limit_waits_only_for_remaining_period() {
        let period = Duration::from_millis(20);
        assert_eq!(
            frame_limit_delay(period, Duration::from_millis(7)),
            Duration::from_millis(13)
        );
        assert_eq!(
            frame_limit_delay(period, Duration::from_millis(25)),
            Duration::ZERO
        );
    }

    #[test]
    fn capture_failure_is_terminal_for_the_current_frame() {
        assert!(validate_frame_completion(&FrameState::default()).is_err());
        assert!(
            validate_frame_completion(&FrameState {
                ready: true,
                buffer_done: true,
                copied: true,
                ..FrameState::default()
            })
            .is_ok()
        );
        let failed = FrameState {
            failed: Some("capture failed".into()),
            ..FrameState::default()
        };
        assert_eq!(
            validate_frame_completion(&failed).unwrap_err().to_string(),
            "capture failed"
        );
    }
}
