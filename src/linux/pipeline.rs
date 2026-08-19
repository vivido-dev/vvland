use std::error::Error;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use std::io::Write;

use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use vivid_protocol::messages::PayloadMap;
use vivid_protocol::surface::{
    DesktopSurfaceParameters, SurfaceDefinition, SurfaceDescriptor, SurfaceRole,
};
use vivid_protocol::time::Monotonic;
use vivid_protocol::track::{
    AudioConfiguration, KindConfiguration, TrackConfiguration, TrackMode, VideoConfiguration,
};
use vivid_sdk::{
    AcceptFileDrop, CORE_CONTROL, ChannelEvent, CoordinateModel, DESKTOP_CONTENT, DESKTOP_INPUT,
    DESKTOP_SURFACE, EncodedPacket, FILE_DROP, FileDropBindingGuard, FileDropDestination,
    GENERIC_CONTENT, IncomingFileTransferRequest, InputLaneEvent, LIVE_MEDIA, MILESTONE_PRESENTED,
    OBSERVABILITY, ProducerAuthentication, ProducerConfig, RequestMetadata, Session, SessionEvent,
    SurfaceSlots, TERMINAL_SURFACE, Track, TrackSender, input_capability, recover_channel,
};
use zeroize::Zeroizing;

use crate::cli::Config;

use crate::producer::TerminalInjector;

use super::audio::{AudioPipeline, AudioQueue, PulseSink};
use super::compositor::{Compositor, ResolvedCompositor};
use super::control::{AttachParams, HostInputCall, PresenterInput};
use super::desktop_input::{self, DEFAULT_WATCHDOG_US, InputRuntime, REASON_SHUTDOWN};
use super::host::{DesktopPlan, RequestedSize};
use super::scene::{Placement, TerminalDisplay};
use super::terminal::{LocalCommand, TerminalGuard, TerminalInput};
use super::video::{CaptureSource, EncodedQueue, H264Encoder, VideoRateControl};

const PREBUFFER_US: u64 = 100_000;
const LIVE_MAX_LATENCY_US: u64 = 100_000;
// Audio is cheap enough to retain across ordinary SSH jitter. Video is bounded to the live latency
// window instead, and congestion is answered by lowering the encoder target rather than by
// discarding a GOP.
const AUDIO_RESERVE_US: u64 = 2_000_000;
// Encoded access units allowed to wait for the transport before the encoder skips a capture frame.
// One unit is in flight and one is staged behind it; anything past that is latency nobody asked
// for, and a latest-value capture can drop a frame without breaking a reference chain.
const ENCODER_BACKLOG_LIMIT: usize = 2;
// A recovery IDR costs many times a predicted frame. Overflow that is already being answered by a
// lower encoder target must not also mint key frames faster than this.
const OVERFLOW_RECOVERY_INTERVAL: Duration = Duration::from_millis(1_000);
const OPUS_PACKET_US: u64 = 20_000;
const WORKER_POLL: Duration = Duration::from_millis(40);
const MAIN_POLL: Duration = Duration::from_millis(50);
/// Milestone 5 (first presentation) is required before an activation is legal.
const SLOT_MILESTONE: u64 = 1 << 4;

enum WorkerNotice {
    Fatal(String),
    AudioLost(String),
    FileDropCommitted(vvreceive::CommittedFileDrop),
}

struct DesktopDropRuntime {
    binding: FileDropBindingGuard,
    directory: File,
}

struct PrefbufferState {
    audio_expected: bool,
    audio_horizon_us: Option<i64>,
}

struct Prefbuffer {
    state: Mutex<PrefbufferState>,
    changed: Condvar,
}

impl Prefbuffer {
    fn new(audio_expected: bool) -> Self {
        Self {
            state: Mutex::new(PrefbufferState {
                audio_expected,
                audio_horizon_us: None,
            }),
            changed: Condvar::new(),
        }
    }

    fn observe_audio(&self, end_pts_us: i64) {
        let mut state = lock(&self.state);
        state.audio_horizon_us = Some(
            state
                .audio_horizon_us
                .map_or(end_pts_us, |current| current.max(end_pts_us)),
        );
        self.changed.notify_all();
    }

    fn disable_audio(&self) {
        let mut state = lock(&self.state);
        state.audio_expected = false;
        state.audio_horizon_us = None;
        self.changed.notify_all();
    }

    fn wait_for_audio(
        &self,
        required_end_pts_us: i64,
        running: &AtomicBool,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.state);
        loop {
            if !state.audio_expected
                || state
                    .audio_horizon_us
                    .is_some_and(|horizon| horizon >= required_end_pts_us)
            {
                return true;
            }
            if !running.load(Ordering::Acquire) {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            state = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }
}

pub fn run(config: Config) -> Result<(), Box<dyn Error>> {
    // Resolve exactly once before HELLO: `producer_name` is declared in the handshake. The plan
    // retains that answer until terminal geometry is known and provisioning can finish.
    let desktop_plan = DesktopPlan::resolve(&config)?;
    let compositor = desktop_plan.resolved();
    let product = compositor.identity();
    let mut session = Session::connect(producer_config(&config, compositor))
        .map_err(|error| desktop_target_hint(&config, error))?;
    let desktop_mode = config.desktop_target;
    let headless_size = if desktop_mode {
        configured_dimensions(&config)?
    } else {
        headless_size(&config, terminal_display(&session)?)?
    };
    let mut host = desktop_plan.provision(
        &config,
        RequestedSize::Exact(headless_size.0, headless_size.1),
    )?;
    let dimensions = host.dimensions();
    let origin = host.origin();

    let encoder = H264Encoder::new(
        dimensions.0,
        dimensions.1,
        config.fps,
        config.bitrate,
        config.gop_seconds,
        config.max_access_unit_bytes,
    )?;
    let mut audio_pipeline = start_audio_pipeline(&config, host.pulse(), origin, &product)?;
    if audio_pipeline.is_some() && !session.supports(LIVE_MEDIA) {
        if config.require_audio {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "presenter lacks Vivid live-media-v1",
            )
            .into());
        }
        eprintln!("vvland: presenter lacks Vivid audio support; continuing video-only");
        audio_pipeline = None;
    }

    // Object IDs are allocated up front so the track configurations, the surface
    // definition, and the audio probe all name the same complete identity.
    let context_id = session.info().root_context_id;
    let surface_id = session.allocate_id()?;
    let video_track_id = session.allocate_id()?;
    let mut video_cfg =
        video_track_config(&session, dimensions, &config, encoder.decoder_description())?;
    video_cfg.context_id = context_id;
    video_cfg.surface_id = surface_id;
    video_cfg.track_id = video_track_id;
    let mut audio_cfg = audio_pipeline.as_ref().map(|audio| {
        let mut cfg = audio_track_config(&session, &audio.spec);
        cfg.context_id = context_id;
        cfg.surface_id = surface_id;
        cfg.track_id = session.allocate_id().expect("audio track ID allocation");
        cfg
    });
    // The presenter decides whether audio is usable: probe first and degrade to
    // video-only exactly as the 1.1 path did when a presenter rejected the audio source.
    // A probe has no track identity yet, so the track ID must be zero.
    if let Some(cfg) = &audio_cfg {
        let mut probe = cfg.clone();
        probe.track_id = 0;
        match session.probe_track(&probe) {
            Ok(_) => {}
            Err(error) if config.require_audio => return Err(error.into()),
            Err(error) => {
                eprintln!("vvland: presenter rejected audio ({error}); continuing video-only");
                audio_pipeline = None;
                audio_cfg = None;
            }
        }
    }

    host.launch_initial(&config.program)?;
    let desktop = host.into_streaming_parts();
    let session_compositor = desktop.compositor;
    let capture = desktop.capture;
    let _pulse_sink = desktop.pulse;
    debug_assert_eq!(desktop.dimensions, dimensions);
    debug_assert_eq!(desktop.resolved, compositor);
    debug_assert_eq!(desktop.origin, origin);

    let running = Arc::new(AtomicBool::new(true));
    let terminated = Arc::new(AtomicBool::new(false));
    for signal in [SIGTERM, SIGINT, SIGHUP] {
        signal_hook::flag::register(signal, terminated.clone())?;
    }
    let prebuffer = Arc::new(Prefbuffer::new(audio_cfg.is_some()));
    let audio_send_gate = Arc::new(Mutex::new(()));
    let audio_runtime = AudioRuntime {
        queue: audio_pipeline.as_ref().map(|audio| audio.queue.clone()),
        gain: audio_pipeline.as_ref().map(|audio| audio.gain.clone()),
        prebuffer: prebuffer.clone(),
        disabled_reason: Mutex::new(None),
    };
    let (notice_tx, notice_rx) = mpsc::channel();

    if desktop_mode {
        run_desktop(
            config,
            compositor,
            session,
            dimensions,
            origin,
            session_compositor,
            capture,
            encoder,
            audio_pipeline,
            audio_cfg,
            video_cfg,
            running,
            terminated,
            prebuffer,
            audio_runtime,
            audio_send_gate,
            notice_tx,
            notice_rx,
        )
    } else {
        run_terminal(
            config,
            compositor,
            session,
            dimensions,
            origin,
            session_compositor,
            capture,
            encoder,
            audio_pipeline,
            audio_cfg,
            video_cfg,
            running,
            terminated,
            prebuffer,
            audio_runtime,
            audio_send_gate,
            notice_tx,
            notice_rx,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn run_desktop(
    config: Config,
    compositor: ResolvedCompositor,
    session: Session,
    dimensions: (u32, u32),
    origin: Instant,
    mut compositor_session: Compositor,
    capture: Box<dyn CaptureSource + Send + Sync>,
    encoder: H264Encoder,
    audio_pipeline: Option<AudioPipeline>,
    audio_cfg: Option<TrackConfiguration>,
    video_cfg: TrackConfiguration,
    running: Arc<AtomicBool>,
    terminated: Arc<AtomicBool>,
    prebuffer: Arc<Prefbuffer>,
    audio_runtime: AudioRuntime,
    audio_send_gate: Arc<Mutex<()>>,
    notice_tx: mpsc::Sender<WorkerNotice>,
    notice_rx: mpsc::Receiver<WorkerNotice>,
) -> Result<(), Box<dyn Error>> {
    let product = compositor.identity();
    let surface_def = desktop_surface_def(
        session.info().root_context_id,
        video_cfg.surface_id,
        dimensions,
        config.secure_input,
        compositor.wire_name(),
    );
    let desktop = Arc::new(Mutex::new(vivid_sdk::DesktopSession::establish(
        session,
        surface_def,
        video_cfg,
        audio_cfg,
    )?));

    let video_sender = lock(&desktop).video_sender().clone();
    let audio_sender = lock(&desktop).audio_sender().cloned();
    let encoded_queue = Arc::new(EncodedQueue::new(
        video_queue_packets(&config),
        video_queue_bytes(video_sender.channel().track()),
        Duration::from_micros(LIVE_MAX_LATENCY_US),
    ));
    let rate = Arc::new(VideoRateControl::new(config.bitrate));
    if let (Some(audio), Some(sender)) = (audio_pipeline.as_ref(), audio_sender.as_ref()) {
        audio.queue.configure_limits(
            audio_queue_packets(sender.channel().track()),
            audio_queue_bytes(sender.channel().track()),
        );
    }

    let force_keyframe = Arc::new(AtomicBool::new(true));
    let audio_queue = audio_pipeline.as_ref().map(|audio| audio.queue.clone());
    let encoder_join = spawn_encoder(
        capture.latest(),
        encoder,
        origin,
        0,
        encoded_queue.clone(),
        rate.clone(),
        force_keyframe.clone(),
        running.clone(),
        notice_tx.clone(),
    )?;
    let recovery = {
        let desktop = desktop.clone();
        Arc::new(move |key_unit: &[u8]| -> io::Result<TrackSender> {
            let mut desktop = lock(&desktop);
            let track = desktop.video_track().clone();
            recover_channel(desktop.session_mut(), &track, key_unit)
        })
    };
    let video_join = spawn_video_sender(
        video_sender,
        recovery,
        encoded_queue.clone(),
        audio_queue.clone(),
        rate.clone(),
        force_keyframe.clone(),
        prebuffer.clone(),
        running.clone(),
        notice_tx.clone(),
        config.require_audio,
    )?;
    let audio_join = audio_sender
        .zip(audio_queue.clone())
        .map(|(sender, queue)| {
            spawn_audio_sender(
                sender,
                queue,
                prebuffer.clone(),
                audio_send_gate,
                running.clone(),
                notice_tx.clone(),
            )
        })
        .transpose()?;

    let context_id = lock(&desktop).session().info().root_context_id;
    let surface_id = lock(&desktop).desktop_surface().id();
    let mut file_drops = {
        let mut desktop_session = lock(&desktop);
        enable_desktop_file_drops(&mut desktop_session).ok()
    };
    // The producer terminal is the control surface; live keys are forwarded by
    // the presenter lane, so only the leader commands are read here.
    let mut terminal = Some(TerminalGuard::enter()?);
    let mut leader_input = Some(
        TerminalInput::new(
            config.xkb_model.as_deref(),
            &config.xkb_layout,
            config.xkb_variant.as_deref(),
            config.xkb_options.clone(),
            product.compositor_name,
        )?
        .with_leader_only(),
    );

    // The presenter requires decoded-output readiness (milestone 4) before slot
    // activation; the media workers stream while this interruptible wait runs,
    // and Ctrl+B q aborts the establishment cleanly.
    let established = {
        let mut desktop = lock(&desktop);
        let video_track = desktop.video_track().clone();
        let video_generation = desktop.video_sender().generation();
        let audio_pair = desktop
            .audio_track()
            .cloned()
            .zip(desktop.audio_sender().map(|sender| sender.generation()));
        let leader = leader_input
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal input is unavailable"))?;
        let terminal = terminal
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal display is unavailable"))?;
        let gain = audio_runtime.active_gain();
        let mut ready = super::terminal::wait_for_milestone(
            desktop.session_mut(),
            &video_track,
            SLOT_MILESTONE,
            "waiting for the presenter to decode the first frame | C-b q quits",
            leader,
            terminal,
            gain,
        )?;
        if ready {
            if let Some((track, _)) = &audio_pair {
                ready = super::terminal::wait_for_milestone(
                    desktop.session_mut(),
                    track,
                    SLOT_MILESTONE,
                    "waiting for the presenter to decode audio | C-b q quits",
                    leader,
                    terminal,
                    gain,
                )?;
            }
        }
        if ready {
            let mut slots = SurfaceSlots::new(desktop.desktop_surface().inner());
            slots.require(1, &video_track, video_generation, SLOT_MILESTONE)?;
            if let (Some(track), Some(sender)) = (&desktop.audio_track(), &desktop.audio_sender()) {
                slots.require(2, track, sender.generation(), SLOT_MILESTONE)?;
            }
            slots.activate(desktop.session_mut())?;
        }
        ready
    };

    // The 1.5 input model: binding lifecycle, watchdog, and the final injection gate.
    let presented = if established {
        let mut desktop = lock(&desktop);
        let track = desktop.video_track().clone();
        let leader = leader_input
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal input is unavailable"))?;
        let terminal = terminal
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal display is unavailable"))?;
        super::terminal::wait_for_milestone(
            desktop.session_mut(),
            &track,
            MILESTONE_PRESENTED,
            "waiting for the presenter to present the first frame | C-b q quits",
            leader,
            terminal,
            audio_runtime.active_gain(),
        )?
    } else {
        false
    };
    let mut input = {
        let desktop_session = lock(&desktop);
        let generation = desktop_session.desktop_surface().generation();
        let capability_mask = desktop_session.desktop_surface().input_capabilities()?;
        let mut runtime = InputRuntime::new(
            desktop_session.session().info().root_context_id,
            desktop_session.desktop_surface().id(),
            capability_mask,
            DEFAULT_WATCHDOG_US,
        );
        runtime.set_surface_state(generation, capability_mask, &mut || {});
        runtime.set_presented(presented);
        runtime
    };
    let loop_result = if established && presented {
        session_loop_desktop(
            &config,
            &product,
            &desktop,
            &mut compositor_session,
            &mut terminal,
            &mut leader_input,
            &audio_runtime,
            &notice_tx,
            &notice_rx,
            &terminated,
            &mut input,
            &origin,
            context_id,
            surface_id,
            dimensions,
            &mut file_drops,
        )
    } else {
        Ok(())
    };

    running.store(false, Ordering::Release);
    prebuffer.disable_audio();
    encoded_queue.close();
    if let Some(queue) = &audio_queue {
        queue.clear();
    }
    let _ = compositor_session.input_mut().release_all();
    if let Some(file_drops) = &mut file_drops {
        let mut desktop_session = lock(&desktop);
        if let Ok(binding) = file_drops.binding.disable() {
            let _ = desktop_session
                .session()
                .set_file_drop_binding(&binding, &RequestMetadata::default());
        }
    }
    // Wake any sender blocked on channel flow (a stalled presenter stops
    // replenishing) so the joins below complete; the lifecycle close is local.
    let _ = lock(&desktop).session_mut().abort();
    drop(capture);
    drop(audio_pipeline);
    join_worker(encoder_join);
    join_worker(video_join);
    if let Some(join) = audio_join {
        join_worker(join);
    }
    let desktop_session = lock(&desktop);
    if let Some(lane) = desktop_session.lane() {
        input.disable(REASON_SHUTDOWN, lane, &mut || {
            let _ = compositor_session.input_mut().release_all();
        });
    }
    drop(desktop_session);
    drop(desktop);
    drop(terminal);
    if let Some(reason) = audio_runtime.disabled_reason() {
        eprintln!("vvland: audio disabled ({reason})");
    }
    loop_result.map_err(Into::into)
}

#[allow(clippy::too_many_arguments)]
fn run_terminal(
    config: Config,
    compositor: ResolvedCompositor,
    mut session: Session,
    dimensions: (u32, u32),
    origin: Instant,
    mut compositor_session: Compositor,
    capture: Box<dyn CaptureSource + Send + Sync>,
    encoder: H264Encoder,
    audio_pipeline: Option<AudioPipeline>,
    audio_cfg: Option<TrackConfiguration>,
    video_cfg: TrackConfiguration,
    running: Arc<AtomicBool>,
    terminated: Arc<AtomicBool>,
    prebuffer: Arc<Prefbuffer>,
    audio_runtime: AudioRuntime,
    audio_send_gate: Arc<Mutex<()>>,
    notice_tx: mpsc::Sender<WorkerNotice>,
    notice_rx: mpsc::Receiver<WorkerNotice>,
) -> Result<(), Box<dyn Error>> {
    let product = compositor.identity();
    let context_id = session.info().root_context_id;
    let surface_id = video_cfg.surface_id;
    let node_id = session.allocate_id()?;
    let surface = session.create_surface(
        terminal_surface_def(
            context_id,
            surface_id,
            dimensions,
            config.secure_input,
            compositor.wire_name(),
        ),
        &RequestMetadata::default(),
    )?;
    let video_track = session.create_track(video_cfg, &RequestMetadata::default())?;
    let video_channel = session.open_track_channel(&video_track)?;
    let video_sender = TrackSender::new(video_channel);
    let (audio_track, audio_sender) = if let Some(cfg) = audio_cfg {
        match session.create_track(cfg, &RequestMetadata::default()) {
            Ok(track) => {
                let channel = session.open_track_channel(&track)?;
                (Some(track), Some(TrackSender::new(channel)))
            }
            Err(error) if config.require_audio => return Err(error.into()),
            Err(error) => {
                eprintln!("vvland: audio track rejected ({error}); continuing video-only");
                (None, None)
            }
        }
    } else {
        (None, None)
    };
    // Slot activation waits for decoded-output readiness (milestone 4), so the
    // media workers stream first; keep the channel generations for the bindings.
    let video_generation = video_sender.generation();
    let audio_generation = audio_sender.as_ref().map(|sender| sender.generation());

    let display = terminal_display(&session)?;
    let mut placement = Placement::calculate(display, dimensions.0, dimensions.1)?;
    let node = placement.node(node_id, context_id, surface_id);
    session.create_node(&node, &RequestMetadata::default())?;

    // The bounded authenticated anchor marker is the only content on the PTY; the terminal
    // target verifies it and reports ANCHOR_READY.
    let anchor_id = session.allocate_id()?;
    let marker = session.anchor_marker(context_id, anchor_id)?;
    print!("{marker}");
    io::stdout().flush()?;

    let session = Arc::new(Mutex::new(session));
    let mut terminal = Some(TerminalGuard::enter()?);
    let mut input = Some(TerminalInput::new(
        config.xkb_model.as_deref(),
        &config.xkb_layout,
        config.xkb_variant.as_deref(),
        config.xkb_options.clone(),
        product.compositor_name,
    )?);
    let encoded_queue = Arc::new(EncodedQueue::new(
        video_queue_packets(&config),
        video_queue_bytes(&video_track),
        Duration::from_micros(LIVE_MAX_LATENCY_US),
    ));
    let rate = Arc::new(VideoRateControl::new(config.bitrate));
    if let (Some(audio), Some(sender)) = (audio_pipeline.as_ref(), audio_sender.as_ref()) {
        audio.queue.configure_limits(
            audio_queue_packets(sender.channel().track()),
            audio_queue_bytes(sender.channel().track()),
        );
    }

    let force_keyframe = Arc::new(AtomicBool::new(true));
    let audio_queue = audio_pipeline.as_ref().map(|audio| audio.queue.clone());
    let encoder_join = spawn_encoder(
        capture.latest(),
        encoder,
        origin,
        0,
        encoded_queue.clone(),
        rate.clone(),
        force_keyframe.clone(),
        running.clone(),
        notice_tx.clone(),
    )?;
    let recovery = {
        let session = session.clone();
        let track = video_track.clone();
        Arc::new(move |key_unit: &[u8]| -> io::Result<TrackSender> {
            recover_channel(&mut lock(&session), &track, key_unit)
        })
    };
    let video_join = spawn_video_sender(
        video_sender,
        recovery,
        encoded_queue.clone(),
        audio_queue.clone(),
        rate.clone(),
        force_keyframe.clone(),
        prebuffer.clone(),
        running.clone(),
        notice_tx.clone(),
        config.require_audio,
    )?;
    let audio_join = audio_sender
        .zip(audio_queue.clone())
        .map(|(sender, queue)| {
            spawn_audio_sender(
                sender,
                queue,
                prebuffer.clone(),
                audio_send_gate,
                running.clone(),
                notice_tx,
            )
        })
        .transpose()?;

    // The presenter requires decoded-output readiness (milestone 4) before slot
    // activation; the media workers stream while this interruptible wait runs,
    // and Ctrl+B q aborts the establishment cleanly.
    let established = {
        let mut session = lock(&session);
        let leader = input
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal input is unavailable"))?;
        let terminal = terminal
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal display is unavailable"))?;
        let gain = audio_runtime.active_gain();
        let mut ready = super::terminal::wait_for_milestone(
            &mut session,
            &video_track,
            SLOT_MILESTONE,
            "waiting for the presenter to decode the first frame | C-b q quits",
            leader,
            terminal,
            gain,
        )?;
        if ready {
            if let Some(track) = &audio_track {
                ready = super::terminal::wait_for_milestone(
                    &mut session,
                    track,
                    SLOT_MILESTONE,
                    "waiting for the presenter to decode audio | C-b q quits",
                    leader,
                    terminal,
                    gain,
                )?;
            }
        }
        if ready {
            let mut slots = SurfaceSlots::new(&surface);
            slots.require(1, &video_track, video_generation, SLOT_MILESTONE)?;
            if let (Some(track), Some(sender)) = (&audio_track, &audio_generation) {
                slots.require(2, track, *sender, SLOT_MILESTONE)?;
            }
            slots.activate(&mut session)?;
        }
        ready
    };

    let backend_name = compositor_session.backend_name();
    let loop_result = if established {
        session_loop_terminal(
            &config,
            &product,
            &session,
            &surface,
            &video_track,
            node_id,
            &mut placement,
            &mut compositor_session,
            &mut terminal,
            &mut input,
            &audio_runtime,
            &notice_rx,
            &terminated,
            dimensions,
            backend_name,
        )
    } else {
        Ok(())
    };

    running.store(false, Ordering::Release);
    prebuffer.disable_audio();
    encoded_queue.close();
    if let Some(queue) = &audio_queue {
        queue.clear();
    }
    let _ = compositor_session.input_mut().release_all();
    // Wake any sender blocked on channel flow (a stalled presenter stops
    // replenishing) so the joins below complete; the lifecycle close is local.
    let _ = lock(&session).abort();
    drop(capture);
    drop(audio_pipeline);
    join_worker(encoder_join);
    join_worker(video_join);
    if let Some(join) = audio_join {
        join_worker(join);
    }
    // Ordered terminal teardown: node, tracks, surface, then the session.
    {
        let mut session = lock(&session);
        let _ = session.delete_node(context_id, node_id, &RequestMetadata::default());
        let _ = session.destroy_track(&video_track, &RequestMetadata::default());
        if let Some(track) = &audio_track {
            let _ = session.destroy_track(track, &RequestMetadata::default());
        }
        let _ = session.destroy_surface(&surface, &RequestMetadata::default());
    }
    // The recovery closure was dropped when its worker joined, so the session is ours again.
    if let Ok(mutex) = Arc::try_unwrap(session) {
        if let Ok(session) = mutex.into_inner() {
            let _ = session.close();
        }
    }
    drop(terminal);
    if let Some(reason) = audio_runtime.disabled_reason() {
        eprintln!("vvland: audio disabled ({reason})");
    }
    loop_result.map_err(Into::into)
}

/// A monotonic clock in the producer's origin for the step-driven input runtime.
fn monotonic_now(origin: &Instant) -> Monotonic {
    Monotonic::from_micros(u64::try_from(origin.elapsed().as_micros()).unwrap_or(u64::MAX))
}

#[allow(clippy::too_many_arguments)]
fn session_loop_desktop(
    config: &Config,
    product: &crate::producer::ProductIdentity,
    desktop: &Arc<Mutex<vivid_sdk::DesktopSession>>,
    compositor_session: &mut Compositor,
    terminal: &mut Option<TerminalGuard>,
    leader_input: &mut Option<TerminalInput>,
    audio: &AudioRuntime,
    notice_sender: &mpsc::Sender<WorkerNotice>,
    notices: &mpsc::Receiver<WorkerNotice>,
    terminated: &AtomicBool,
    input: &mut InputRuntime,
    origin: &Instant,
    context_id: u64,
    surface_id: u64,
    dimensions: (u32, u32),
    file_drops: &mut Option<DesktopDropRuntime>,
) -> io::Result<()> {
    let mut last_reported_rejections = 0_u64;
    loop {
        if terminated.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut committed_drops = Vec::new();
        while let Ok(notice) = notices.try_recv() {
            match notice {
                WorkerNotice::Fatal(error) => return Err(io::Error::other(error)),
                WorkerNotice::AudioLost(error) => {
                    if config.require_audio {
                        return Err(io::Error::other(error));
                    }
                    audio.disable(&error);
                }
                WorkerNotice::FileDropCommitted(committed) => committed_drops.push(committed),
            }
        }
        if let Some(status) = compositor_session.try_wait()? {
            return Err(io::Error::other(format!(
                "{} exited with {status}",
                product.compositor_name
            )));
        }
        compositor_session.input_mut().check_status()?;

        let mut desktop_session = lock(desktop);
        for committed in committed_drops {
            vvreceive::reconcile_committed(desktop_session.session(), committed)?;
        }
        // Session control events: the desktop surface is producer-defined on a fixed headless
        // output, so only target and connection changes matter; flow (b) geometry changes do
        // not apply here.
        while let Some(event) = desktop_session.session().take_event()? {
            match event {
                SessionEvent::TargetChanged(payload) => {
                    desktop_session
                        .session_mut()
                        .apply_target_changed(&payload)?;
                }
                SessionEvent::ConnectionClosed { diagnostic } => {
                    // Flow (d): a root session is intentionally non-resumable; surface the
                    // loss and exit so the caller can reconnect.
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        format!("Vivid control connection closed: {diagnostic}"),
                    ));
                }
                SessionEvent::TrackLost { object_id, .. }
                    if object_id == desktop_session.video_track().id() =>
                {
                    return Err(io::Error::other(
                        "the presenter lost the video track; reconnecting is required",
                    ));
                }
                SessionEvent::FileDropOffered(offer) => {
                    if let Some(runtime) = file_drops.as_ref() {
                        let transfer_id = desktop_session.session().allocate_id()?;
                        let maximum_record_body = runtime
                            .binding
                            .grant()
                            .ok_or_else(|| io::Error::other("file-drop grant is not active"))?
                            .maximum_record_body;
                        let accepted = AcceptFileDrop {
                            binding: offer.binding,
                            transfer_id,
                            transfer_generation: vivid_sdk::FileTransferGeneration::ONE,
                            maximum_record_body,
                            initial_maximum_body_bytes: 16 * 1024 * 1024,
                            initial_maximum_records: 32,
                        };
                        desktop_session
                            .session()
                            .accept_file_drop(accepted, &RequestMetadata::default())?;
                        let channel = desktop_session.session().open_incoming_file_transfer(
                            IncomingFileTransferRequest {
                                context_id: offer.binding.context_id,
                                surface_id: offer.binding.surface_id,
                                producer_epoch: offer.binding.producer_epoch,
                                grant_generation: offer.binding.grant_generation,
                                surface_generation: offer.binding.surface_generation,
                                drop_id: offer.binding.drop_id,
                                transfer_id,
                                transfer_generation: vivid_sdk::FileTransferGeneration::ONE,
                                resume_offset: 0,
                                declared_length: offer.declared_length,
                                maximum_record_body,
                                maximum_body_bytes: 16 * 1024 * 1024,
                                maximum_records: 32,
                            },
                        )?;
                        let directory = runtime.directory.try_clone()?;
                        let completion = notice_sender.clone();
                        thread::Builder::new()
                            .name("vvland-file-drop".into())
                            .spawn(move || {
                                if let Ok(committed) =
                                    vvreceive::receive_accepted(channel, offer, directory)
                                {
                                    let _ =
                                        completion.send(WorkerNotice::FileDropCommitted(committed));
                                }
                            })?;
                    }
                }
                _ => {}
            }
        }
        // The 1.5 input model (desktop §4–§8): the surface state, the lane, the watchdog,
        // and the binding lifecycle all flow through the shared runtime.
        let now = monotonic_now(origin);
        let surface = desktop_session.desktop_surface();
        input.set_surface_state(
            surface.generation(),
            surface.input_capabilities()?,
            &mut || {
                let _ = compositor_session.input_mut().release_all();
            },
        );
        input.set_lane_live(desktop_session.lane().is_some());
        if input.watchdog_expired(now) {
            input.on_watchdog_expiry(&mut || {
                let _ = compositor_session.input_mut().release_all();
            });
        }
        while let Some(event) = desktop_session
            .lane()
            .and_then(|lane| lane.take_event().ok())
            .flatten()
        {
            match event {
                InputLaneEvent::Input { .. } => {
                    if let Ok(input_event) =
                        event.decode_input(u64::from(dimensions.0), u64::from(dimensions.1))
                    {
                        input.push(input_event);
                    }
                }
                other => {
                    // Lane loss and presenter errors revoke input and release held state but
                    // leave the stream view-only; the control session survives.
                    if let Err(error) = input.observe(other, now, &mut || {
                        let _ = compositor_session.input_mut().release_all();
                    }) {
                        eprintln!("vvland: input lane: {error}");
                    }
                }
            }
        }
        if let Some(lane) = desktop_session.lane() {
            input.enable_if_ready(lane)?;
        }
        drop(desktop_session);
        if input.overflowed() {
            input.release_overflow(&mut || {
                let _ = compositor_session.input_mut().release_all();
            });
        }
        input.drain(now, |event| {
            desktop_input::apply(
                event,
                &mut compositor_session.input_mut(),
                context_id,
                surface_id,
                dimensions,
            )
        })?;
        let rejections = input.rejections();
        if rejections > last_reported_rejections {
            last_reported_rejections = rejections;
            eprintln!("vvland: {rejections} stale input events rejected at the final gate");
        }
        // The producer terminal carries only the leader commands; live keys flow
        // through the presenter lane into the input runtime above.
        let leader_input = leader_input
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal input is unavailable"))?;
        let command = leader_input.poll_leader_only(MAIN_POLL, audio.active_gain())?;
        match command {
            Some(LocalCommand::Quit) => return Ok(()),
            Some(LocalCommand::Detach) => return Ok(()),
            Some(LocalCommand::Run(command)) => {
                compositor_session.launch_shell_command(&command)?
            }
            Some(LocalCommand::Resize) | None => {}
        }
        let audio_status = audio.active_gain().map_or_else(
            || {
                audio.disabled_reason().map_or_else(
                    || "audio off".to_owned(),
                    |reason| format!("audio off: {reason}"),
                )
            },
            |gain| {
                let (muted, volume) = gain.status();
                if muted {
                    "audio muted".into()
                } else {
                    format!("audio {:.0}%", volume * 100.0)
                }
            },
        );
        terminal
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal display is unavailable"))?
            .status(&format!(
                "vvland {} desktop {}x{} {} | {}",
                compositor_label(product),
                dimensions.0,
                dimensions.1,
                audio_status,
                leader_input.status()
            ))?;
    }
}

fn enable_desktop_file_drops(
    desktop: &mut vivid_sdk::DesktopSession,
) -> io::Result<DesktopDropRuntime> {
    if !desktop.session().supports(FILE_DROP) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file-drop-v1 was not negotiated",
        ));
    }
    let directory = vvreceive::open_xdg_desktop()?;
    let mut binding = FileDropBindingGuard::new();
    let request = binding.enable(
        desktop.desktop_surface().context_id(),
        desktop.desktop_surface().id(),
        desktop.desktop_surface().generation(),
        FileDropDestination::DesktopFolder,
        1 << 40,
        vivid_protocol::file_drop::DEFAULT_PENDING_FILE_DROPS,
        vivid_protocol::file_drop::DEFAULT_ACTIVE_FILE_TRANSFERS,
        1024 * 1024,
        vivid_protocol::file_drop::DEFAULT_FILE_DROP_ACCEPTANCE_US,
        vivid_protocol::file_drop::DEFAULT_FILE_TRANSFER_IDLE_US,
    )?;
    let grant = desktop
        .session()
        .set_file_drop_binding(&request, &RequestMetadata::default())?;
    binding.handle_bound(grant)?;
    Ok(DesktopDropRuntime { binding, directory })
}

#[allow(clippy::too_many_arguments)]
fn session_loop_terminal(
    config: &Config,
    product: &crate::producer::ProductIdentity,
    session: &Arc<Mutex<Session>>,
    surface: &vivid_sdk::Surface,
    video_track: &Track,
    node_id: u64,
    placement: &mut Placement,
    compositor_session: &mut Compositor,
    terminal: &mut Option<TerminalGuard>,
    input: &mut Option<TerminalInput>,
    audio: &AudioRuntime,
    notices: &mpsc::Receiver<WorkerNotice>,
    terminated: &AtomicBool,
    dimensions: (u32, u32),
    backend_name: &str,
) -> io::Result<()> {
    loop {
        if terminated.load(Ordering::Acquire) {
            return Ok(());
        }
        while let Ok(notice) = notices.try_recv() {
            match notice {
                WorkerNotice::Fatal(error) => return Err(io::Error::other(error)),
                WorkerNotice::AudioLost(error) => {
                    if config.require_audio {
                        return Err(io::Error::other(error));
                    }
                    audio.disable(&error);
                }
                WorkerNotice::FileDropCommitted(_) => {}
            }
        }
        if let Some(status) = compositor_session.try_wait()? {
            return Err(io::Error::other(format!(
                "{} exited with {status}",
                product.compositor_name
            )));
        }
        compositor_session.input_mut().check_status()?;

        let mut session_guard = lock(session);
        let mut target_changed = false;
        while let Some(event) = session_guard.take_event()? {
            match event {
                SessionEvent::TargetChanged(payload) => {
                    session_guard.apply_target_changed(&payload)?;
                    target_changed = true;
                }
                SessionEvent::ConnectionClosed { diagnostic } => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        format!("Vivid control connection closed: {diagnostic}"),
                    ));
                }
                SessionEvent::TrackLost { object_id, .. } if object_id == video_track.id() => {
                    return Err(io::Error::other(
                        "the presenter lost the video track; reconnecting is required",
                    ));
                }
                SessionEvent::AnchorReady { .. } | SessionEvent::AnchorGone { .. } => {}
                _ => {}
            }
        }
        if target_changed && session_guard.info().target_settled()? {
            // Flow (b): the terminal target settled on a new grid; recompute the letterboxed
            // placement and move the node, leaving the surface and tracks untouched.
            let display = terminal_display(&session_guard)?;
            *placement = Placement::calculate(display, dimensions.0, dimensions.1)?;
            let node = placement.node(node_id, session_guard.info().root_context_id, surface.id());
            session_guard.update_node(&node, &RequestMetadata::default())?;
        }
        drop(session_guard);

        let input = input
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal input is unavailable"))?;
        let command = input.poll(
            MAIN_POLL,
            &mut compositor_session.input_mut(),
            *placement,
            audio.active_gain(),
        )?;
        match command {
            Some(LocalCommand::Quit) => return Ok(()),
            Some(LocalCommand::Detach) => return Ok(()),
            Some(LocalCommand::Run(command)) => {
                compositor_session.launch_shell_command(&command)?
            }
            Some(LocalCommand::Resize) | None => {}
        }
        let audio_status = audio.active_gain().map_or_else(
            || {
                audio.disabled_reason().map_or_else(
                    || "audio off".to_owned(),
                    |reason| format!("audio off: {reason}"),
                )
            },
            |gain| {
                let (muted, volume) = gain.status();
                if muted {
                    "audio muted".into()
                } else {
                    format!("audio {:.0}%", volume * 100.0)
                }
            },
        );
        terminal
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal display is unavailable"))?
            .status(&format!(
                "vvland {} {backend_name} {}x{} {} | {}",
                compositor_label(product),
                dimensions.0,
                dimensions.1,
                audio_status,
                input.status()
            ))?;
    }
}

fn configured_dimensions(config: &Config) -> io::Result<(u32, u32)> {
    super::host::configured_dimensions(config)
}

/// Grid metrics of the negotiated `terminal-surface-v1` target.
///
/// Keys are the validated terminal target descriptor: 2/3 columns/rows, 4/5 cell size.
fn terminal_display(session: &Session) -> io::Result<TerminalDisplay> {
    if session.info().target_profile != TERMINAL_SURFACE {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "presenter did not negotiate the terminal target",
        ));
    }
    let descriptor = &session.info().target_descriptor;
    let unsigned = |key: u64| -> io::Result<u64> {
        descriptor
            .iter()
            .find(|(k, _)| *k == key)
            .and_then(|(_, v)| v.as_u64())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "terminal target descriptor is missing a grid key",
                )
            })
    };
    Ok(TerminalDisplay {
        grid_columns: u32::try_from(unsigned(2)?).unwrap_or(0),
        grid_rows: u32::try_from(unsigned(3)?).unwrap_or(0),
        cell_width: u32::try_from(unsigned(4)?).unwrap_or(0),
        cell_height: u32::try_from(unsigned(5)?).unwrap_or(0),
    })
}

/// A presenter window presenting the terminal target rejects a desktop HELLO before
/// `WELCOME`; surface the required `--desktop-target` flag instead of the bare error.
/// The presenter diagnostic is display-only, so matching it for a hint is safe.
fn desktop_target_hint(config: &Config, error: io::Error) -> io::Error {
    let mismatch = config.desktop_target
        && error
            .to_string()
            .contains("this window presents a different target profile");
    if mismatch {
        io::Error::new(
            error.kind(),
            format!(
                "{error}\nhint: start the presenter with --desktop-target so its window \
                 presents the desktop-surface-v1 target"
            ),
        )
    } else {
        error
    }
}

fn producer_config(config: &Config, compositor: ResolvedCompositor) -> ProducerConfig {
    let target = if config.desktop_target {
        DESKTOP_SURFACE
    } else {
        TERMINAL_SURFACE
    };
    let mut required_profiles = vec![
        CORE_CONTROL.to_owned(),
        target.to_owned(),
        LIVE_MEDIA.to_owned(),
    ];
    if config.desktop_target {
        required_profiles.push(DESKTOP_INPUT.to_owned());
    }
    // The SDK requires ascending-sorted, unique profile lists in HELLO.
    required_profiles.sort();
    ProducerConfig {
        endpoint_control: config
            .endpoint_control
            .as_ref()
            .map(|value| (**value).clone()),
        endpoint_interactive: config
            .endpoint_interactive
            .as_ref()
            .map(|value| (**value).clone()),
        endpoint_realtime: config
            .endpoint_realtime
            .as_ref()
            .map(|value| (**value).clone()),
        endpoint_bulk: config.endpoint_bulk.as_ref().map(|value| (**value).clone()),
        authentication: ProducerAuthentication::RootFromEnvironment,
        producer_name: compositor.wire_name().into(),
        producer_version: env!("CARGO_PKG_VERSION").into(),
        target_profile: target.to_owned(),
        required_profiles,
        optional_profiles: {
            let mut profiles = vec![FILE_DROP.to_owned(), OBSERVABILITY.to_owned()];
            profiles.sort();
            profiles
        },
        maximum_control_body: 262_144,
        dry_run: false,
        trace_dir: None,
    }
}

fn desktop_surface_def(
    context_id: u64,
    surface_id: u64,
    dimensions: (u32, u32),
    secure_input: bool,
    wire_name: &str,
) -> SurfaceDefinition {
    SurfaceDefinition {
        context_id,
        surface_id,
        semantic_profile: DESKTOP_CONTENT.into(),
        coordinate_model: CoordinateModel::DesktopLogicalPixels,
        logical_width: u64::from(dimensions.0),
        logical_height: u64::from(dimensions.1),
        scale_numerator: 1,
        scale_denominator: 1,
        rotation: 0,
        descriptor: SurfaceDescriptor {
            role: SurfaceRole::Desktop,
            title: format!("{wire_name} nested desktop"),
            semantic_content_revision: 1,
            semantic_availability: 0,
            locator_hint: proposed_control_locator(wire_name),
        },
        policy: capture_policy(secure_input),
        profile_parameters: DesktopSurfaceParameters {
            captured_origin_x: 0,
            captured_origin_y: 0,
            topology: vec![],
            semantic_generation: 1,
            input_capabilities: input_capability::KEYBOARD
                | input_capability::POINTER_MOTION
                | input_capability::POINTER_BUTTON
                | input_capability::POINTER_AXIS,
        }
        .encode(),
    }
}

fn terminal_surface_def(
    context_id: u64,
    surface_id: u64,
    dimensions: (u32, u32),
    secure_input: bool,
    wire_name: &str,
) -> SurfaceDefinition {
    SurfaceDefinition {
        context_id,
        surface_id,
        semantic_profile: GENERIC_CONTENT.into(),
        coordinate_model: CoordinateModel::DesktopLogicalPixels,
        logical_width: u64::from(dimensions.0),
        logical_height: u64::from(dimensions.1),
        scale_numerator: 1,
        scale_denominator: 1,
        rotation: 0,
        descriptor: SurfaceDescriptor {
            role: SurfaceRole::Desktop,
            title: format!("{wire_name} nested desktop"),
            semantic_content_revision: 1,
            semantic_availability: 0,
            locator_hint: proposed_control_locator(wire_name),
        },
        policy: capture_policy(secure_input),
        profile_parameters: Vec::new(),
    }
}

fn capture_policy(secure_input: bool) -> u64 {
    if secure_input {
        vivid_sdk::POLICY_DENY_CAPTURE
    } else {
        0
    }
}

fn proposed_control_locator(producer: &str) -> String {
    std::env::var("XDG_RUNTIME_DIR").map_or_else(
        |_| String::new(),
        |directory| {
            format!(
                "{producer}+unix://{directory}/{producer}-{}.control",
                std::process::id()
            )
        },
    )
}

fn video_track_config(
    session: &Session,
    dimensions: (u32, u32),
    config: &Config,
    decoder_description: &super::video::H264DecoderDescription,
) -> io::Result<TrackConfiguration> {
    let coded_pixels = u64::from(dimensions.0)
        .checked_mul(u64::from(dimensions.1))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "coded pixels overflow"))?;
    let mut cfg = TrackConfiguration {
        context_id: session.info().root_context_id,
        surface_id: 0, // filled by the caller
        track_id: 0,   // filled by the caller
        slot: 1,
        mode: TrackMode::Live,
        lane: vivid_sdk::LaneClass::Bulk,
        maximum_record_body: config.max_access_unit_bytes.saturating_add(48),
        maximum_rate_millihertz: u64::from(config.fps).saturating_mul(1000),
        maximum_encoded_bits_per_second: config.bitrate,
        maximum_records_per_second: u64::from(config.fps).saturating_add(4),
        maximum_inflight_body_bytes: u64::from(config.max_access_unit_bytes)
            .saturating_mul(16)
            .max(1 << 20),
        kind: KindConfiguration::Video(VideoConfiguration {
            codec: "h264".into(),
            packetization: "h264-annexb-au-v1".into(),
            // Parameter sets ride in every key frame (libx264 repeat-headers), so the
            // immutable extradata stays empty, exactly as the 1.1 producer sent it.
            extradata: Vec::new(),
            coded_width: dimensions.0,
            coded_height: dimensions.1,
            profile: decoder_description.profile,
            level: decoder_description.level,
            maximum_reorder_depth: 0,
            color_primaries: 1,
            transfer: 1,
            matrix: 1,
            signal_range: 1,
            aspect_numerator: 1,
            aspect_denominator: 1,
            maximum_access_unit_bytes: config.max_access_unit_bytes,
            codec_string: Some(decoder_description.codec_string.clone()),
            decoder_configuration: Some(decoder_description.decoder_config.clone()),
        }),
        target_latency_us: 33_000,
        maximum_latency_us: 100_000,
        retained_pixel_charge: coded_pixels,
    };
    bound_track_claims(&mut cfg, &session.info().resource_contract)?;
    Ok(cfg)
}

fn audio_track_config(
    session: &Session,
    spec: &super::audio::AudioTrackSpec,
) -> TrackConfiguration {
    let mut cfg = TrackConfiguration {
        context_id: session.info().root_context_id,
        surface_id: 0, // filled by the caller
        track_id: 0,   // filled by the caller
        slot: 2,
        mode: TrackMode::Live,
        lane: vivid_sdk::LaneClass::Realtime,
        maximum_record_body: spec.max_access_unit_bytes.saturating_add(48),
        maximum_rate_millihertz: 50_000,
        maximum_encoded_bits_per_second: 128_000,
        maximum_records_per_second: 100,
        maximum_inflight_body_bytes: 1 << 20,
        kind: KindConfiguration::Audio(AudioConfiguration {
            codec: "opus".into(),
            packetization: "opus-packet-v1".into(),
            extradata: spec.extradata.clone(),
            sample_rate: spec.sample_rate,
            channels: spec.channels,
            channel_mask: 3,
            maximum_access_unit_bytes: spec.max_access_unit_bytes,
            codec_string: Some("opus".into()),
        }),
        target_latency_us: 33_000,
        maximum_latency_us: 100_000,
        retained_pixel_charge: 0,
    };
    let _ = bound_track_claims(&mut cfg, &session.info().resource_contract);
    cfg
}

/// Clamp every worst-case claim to the negotiated resource contract.
///
/// The 1.5 contract is a ceiling, not a hint: the producer must never claim more than the
/// presenter reserved, and the encoder is configured from the same CLI bounds the claims derive
/// from, so the clamped values stay consistent with what actually runs.
fn bound_track_claims(
    cfg: &mut TrackConfiguration,
    contract: &vivid_protocol::resource::ResourceContract,
) -> io::Result<()> {
    use vivid_protocol::resource::Resource;
    let clamp = |claim: u64, resource: Resource| -> io::Result<u64> {
        let ceiling = contract.get(resource);
        if ceiling == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("resource contract denies {}", resource_label(resource)),
            ));
        }
        if claim > ceiling {
            eprintln!(
                "vvland: {} claim {} exceeds the negotiated ceiling {}; clamping",
                resource_label(resource),
                claim,
                ceiling
            );
        }
        Ok(claim.min(ceiling))
    };
    cfg.maximum_encoded_bits_per_second = clamp(
        cfg.maximum_encoded_bits_per_second,
        Resource::EncodedBitsPerSecond,
    )?;
    cfg.maximum_records_per_second = clamp(
        cfg.maximum_records_per_second,
        Resource::MediaRecordsPerSecond,
    )?;
    cfg.maximum_record_body = u32::try_from(clamp(
        u64::from(cfg.maximum_record_body),
        Resource::MediaRecordBody,
    )?)
    .unwrap_or(u32::MAX);
    cfg.maximum_inflight_body_bytes = clamp(
        cfg.maximum_inflight_body_bytes,
        Resource::InflightMediaBytes,
    )?;
    cfg.retained_pixel_charge = clamp(cfg.retained_pixel_charge, Resource::RetainedPixels)?;
    if cfg.maximum_record_body == 0
        || cfg.maximum_encoded_bits_per_second == 0
        || cfg.maximum_records_per_second == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the negotiated resource contract cannot carry the stream",
        ));
    }
    Ok(())
}

fn resource_label(resource: vivid_protocol::resource::Resource) -> &'static str {
    use vivid_protocol::resource::Resource;
    match resource {
        Resource::EncodedBitsPerSecond => "encoded-bps",
        Resource::MediaRecordsPerSecond => "media-records-per-second",
        Resource::MediaRecordBody => "media-record-body",
        Resource::InflightMediaBytes => "inflight-media-bytes",
        Resource::RetainedPixels => "retained-pixels",
        _ => "resource",
    }
}

fn video_queue_packets(config: &Config) -> usize {
    usize::try_from(
        u64::from(config.fps)
            .saturating_mul(LIVE_MAX_LATENCY_US)
            .div_ceil(1_000_000),
    )
    .unwrap_or(usize::MAX)
}

fn video_queue_bytes(track: &Track) -> usize {
    track
        .configuration()
        .map(|cfg| {
            let latency_bytes = cfg
                .maximum_encoded_bits_per_second
                .saturating_mul(LIVE_MAX_LATENCY_US)
                .div_ceil(8_000_000);
            let bound = u64::from(cfg.maximum_record_body)
                .saturating_add(latency_bytes)
                .min(cfg.maximum_inflight_body_bytes)
                .max(u64::from(cfg.maximum_record_body));
            usize::try_from(bound).unwrap_or(usize::MAX)
        })
        .unwrap_or(1 << 20)
}

fn audio_queue_packets(track: &Track) -> usize {
    track
        .configuration()
        .map(|_| usize::try_from(AUDIO_RESERVE_US.div_ceil(OPUS_PACKET_US)).unwrap_or(usize::MAX))
        .unwrap_or(100)
}

fn audio_queue_bytes(track: &Track) -> usize {
    track
        .configuration()
        .map(|cfg| {
            let latency_bytes = cfg
                .maximum_encoded_bits_per_second
                .saturating_mul(AUDIO_RESERVE_US)
                .div_ceil(8_000_000);
            let bound = u64::from(cfg.maximum_record_body)
                .saturating_add(latency_bytes)
                .min(cfg.maximum_inflight_body_bytes)
                .max(u64::from(cfg.maximum_record_body));
            usize::try_from(bound).unwrap_or(usize::MAX)
        })
        .unwrap_or(1 << 20)
}

fn headless_size(config: &Config, display: TerminalDisplay) -> io::Result<(u32, u32)> {
    let (width, height) = match (config.width, config.height) {
        (Some(width), Some(height)) => (width, height),
        (None, None) => {
            let grid_width = display.grid_columns.saturating_mul(display.cell_width);
            let reserved_rows = 1;
            let grid_height = display
                .grid_rows
                .saturating_sub(reserved_rows)
                .saturating_mul(display.cell_height);
            (
                display_grid_dimension(grid_width, "grid width"),
                display_grid_dimension(grid_height, "grid height"),
            )
        }
        _ => unreachable!("CLI validation requires paired dimensions"),
    };
    let width = width.min(1920);
    let height = height.min(1080);
    let width = width & !1;
    let height = height & !1;
    if width < 64 || height < 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "streaming area must be at least 64x64 after reserving the status row",
        ));
    }
    Ok((width, height))
}

fn display_grid_dimension(grid: u32, label: &str) -> u32 {
    if grid == 0 {
        eprintln!(
            "vvland: terminal target reported zero {label}; using the default streaming size"
        );
        1920
    } else {
        grid
    }
}

/// The lowercase compositor name used in the status row.
fn compositor_label(product: &crate::producer::ProductIdentity) -> &'static str {
    match product.compositor_name {
        "Sway" => "sway",
        _ => "weston",
    }
}

fn start_audio_pipeline(
    config: &Config,
    pulse: Option<&PulseSink>,
    origin: Instant,
    product: &crate::producer::ProductIdentity,
) -> io::Result<Option<AudioPipeline>> {
    let Some(pulse) = pulse else {
        return Ok(None);
    };
    match AudioPipeline::start(pulse.monitor_name(), pulse.server(), origin, product) {
        Ok(audio) => Ok(Some(audio)),
        Err(error) if config.require_audio => Err(error),
        Err(error) => {
            eprintln!("vvland: Pulse/Opus capture failed ({error}); continuing video-only");
            Ok(None)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_encoder(
    latest: Arc<super::video::LatestFrame>,
    mut encoder: H264Encoder,
    origin: Instant,
    maximum_fps: u32,
    queue: Arc<EncodedQueue>,
    rate: Arc<VideoRateControl>,
    force_keyframe: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    notices: mpsc::Sender<WorkerNotice>,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("vvland-video-encoder".into())
        .spawn(move || {
            let mut serial = 0;
            let minimum_interval_us = if maximum_fps == 0 {
                0
            } else {
                1_000_000_i64 / i64::from(maximum_fps)
            };
            let mut last_encoded_pts = None;
            let mut retarget_failed = false;
            while running.load(Ordering::Acquire) {
                let raw = match latest.wait_next(&mut serial, Duration::from_millis(100)) {
                    Ok(Some(frame)) => frame,
                    Ok(None) if force_keyframe.load(Ordering::Acquire) => {
                        match latest.snapshot() {
                            Ok(Some((snapshot_serial, mut frame))) => {
                                serial = snapshot_serial;
                                // Screencopy is damage-driven and may be idle precisely when a
                                // hidden pane is projected again. Re-stamp the retained desktop
                                // snapshot on the live clock so the forced IDR rebases to the
                                // resume position rather than the last damaged frame.
                                frame.pts_us =
                                    i64::try_from(origin.elapsed().as_micros()).unwrap_or(i64::MAX);
                                frame
                            }
                            Ok(None) => continue,
                            Err(error) => {
                                if running.load(Ordering::Acquire) {
                                    let _ = notices.send(WorkerNotice::Fatal(error.to_string()));
                                }
                                break;
                            }
                        }
                    }
                    Ok(None) => continue,
                    Err(error) => {
                        if running.load(Ordering::Acquire) {
                            let _ = notices.send(WorkerNotice::Fatal(error.to_string()));
                        }
                        break;
                    }
                };
                let force = force_keyframe.load(Ordering::Acquire);
                if !frame_due(last_encoded_pts, raw.pts_us, minimum_interval_us, force) {
                    continue;
                }
                // Transport pacing. The capture is a latest-value source, so a frame that the link
                // has no room for is skipped before it costs an encode, a queue slot, and the GOP
                // that a later overflow would have to discard. A forced IDR is never skipped: it is
                // the recovery unit something is already waiting for.
                if !force && queue.staged() >= ENCODER_BACKLOG_LIMIT {
                    continue;
                }
                // Congestion is answered here, by offering fewer bits, rather than in the sender by
                // discarding frames and minting the key frame that made the link worse.
                if !retarget_failed && encoder.bitrate() != rate.target() {
                    match encoder.retarget(rate.target()) {
                        Ok(true) => force_keyframe.store(true, Ordering::Release),
                        Ok(false) => {}
                        Err(error) => {
                            // Streaming on at the previous rate is always better than stopping.
                            retarget_failed = true;
                            eprintln!("vvland: keeping the current encoder bitrate ({error})");
                        }
                    }
                }
                let force = force_keyframe.swap(false, Ordering::AcqRel);
                let encoded_pts = raw.pts_us;
                match encoder.encode(raw, force) {
                    Ok(packets) => {
                        if !packets.is_empty() {
                            last_encoded_pts = Some(encoded_pts);
                        }
                        for packet in packets {
                            // Overflow recovery belongs to the sender, which rate-limits the key
                            // frame it costs. Forcing one here as well doubles the IDRs that a
                            // congested link is asked to carry.
                            let _ = queue.push(packet);
                        }
                    }
                    Err(error) => {
                        let _ = notices.send(WorkerNotice::Fatal(format!(
                            "video encoder failed: {error}"
                        )));
                        break;
                    }
                }
            }
            queue.close();
        })
}

fn frame_due(last: Option<i64>, pts_us: i64, minimum_interval_us: i64, force: bool) -> bool {
    force || last.is_none_or(|last| pts_us.saturating_sub(last) >= minimum_interval_us)
}

type VideoRecovery = Arc<dyn Fn(&[u8]) -> io::Result<TrackSender> + Send + Sync>;

/// Wait for the forced key frame the encoder produces after `force_keyframe`.
///
/// The queue was cleared before this is called; the IDR is the independently decodable recovery
/// unit flow (c) needs, and stale pre-IDR frames are dropped.
fn wait_for_keyframe(
    queue: &EncodedQueue,
    force_keyframe: &AtomicBool,
    running: &AtomicBool,
) -> io::Result<super::video::EncodedVideo> {
    let deadline = Instant::now() + Duration::from_secs(2);
    force_keyframe.store(true, Ordering::Release);
    while running.load(Ordering::Acquire) {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no key frame produced for channel recovery",
            ));
        }
        match queue.pop(Duration::from_millis(20)) {
            Some(packet) if packet.key => return Ok(packet),
            Some(_) => {}
            None => continue,
        }
    }
    Err(io::Error::new(
        io::ErrorKind::Interrupted,
        "shutting down while waiting for a recovery key frame",
    ))
}

#[allow(clippy::too_many_arguments)]
fn spawn_video_sender(
    mut sender: TrackSender,
    recovery: VideoRecovery,
    queue: Arc<EncodedQueue>,
    audio_queue: Option<Arc<AudioQueue>>,
    rate: Arc<VideoRateControl>,
    force_keyframe: Arc<AtomicBool>,
    prebuffer: Arc<Prefbuffer>,
    running: Arc<AtomicBool>,
    notices: mpsc::Sender<WorkerNotice>,
    require_audio: bool,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("vvland-vivid-video".into())
        .spawn(move || {
            let mut waiting_keyframe = true;
            let mut last_overflow_recovery: Option<Instant> = None;
            loop {
                // The quit path joins this thread; an idle worker must observe the
                // running flag instead of idling on an empty queue forever.
                if !running.load(Ordering::Acquire) {
                    return;
                }
                for event in sender.drain_events() {
                    match event {
                        ChannelEvent::NeedKeyframe(payload) => {
                            // The presenter requests a new epoch: recover with a forced IDR at
                            // an epoch above both the current one and the requested minimum.
                            let target = sender
                                .current_epoch()
                                .saturating_add(1)
                                .max(minimum_recovery_epoch(&payload));
                            while sender.current_epoch() < target {
                                sender.bump_epoch();
                            }
                            clear_for_recovery(&queue);
                            force_keyframe.store(true, Ordering::Release);
                            waiting_keyframe = true;
                        }
                        ChannelEvent::NeedFullFrame(_) => {
                            force_keyframe.store(true, Ordering::Release);
                            waiting_keyframe = true;
                        }
                        ChannelEvent::Error(error) => {
                            if running.load(Ordering::Acquire) {
                                let _ = notices.send(WorkerNotice::Fatal(format!(
                                    "video channel error: {error}"
                                )));
                            }
                            return;
                        }
                    }
                }
                // Audio backing up on a two-orders-of-magnitude cheaper track means this session is
                // congested. That is an input to the encoder target, not a reason to stop video and
                // mint an IDR: the previous design paused video, forced a key frame every ten
                // milliseconds while it waited, and then resumed into the same saturated link.
                if let Some(audio_queue) = audio_queue.as_deref() {
                    rate.observe_audio_backlog(audio_queue.snapshot().queued_duration_us);
                }
                // Closing the window publishes any new target for the encoder to pick up. The
                // change is reported through the streaming diagnostics rather than stderr: this
                // producer owns a terminal UI, and a congested link would otherwise write to it
                // every second.
                let _ = rate.poll();
                if queue.take_overflow() {
                    // Source-scoped backpressure: a slow presenter must not grow the queue. The
                    // stale GOP is already gone, but the IDR that makes the stream decodable again
                    // is the most expensive frame there is, so it is rate limited. Until one is
                    // due, hold delivery rather than sending frames whose references were dropped;
                    // the GOP boundary produces a key frame on its own.
                    let due = last_overflow_recovery
                        .is_none_or(|at| at.elapsed() >= OVERFLOW_RECOVERY_INTERVAL);
                    waiting_keyframe = true;
                    if due {
                        last_overflow_recovery = Some(Instant::now());
                        if sender.current_epoch() != u32::MAX {
                            sender.bump_epoch();
                        }
                        clear_for_recovery(&queue);
                        force_keyframe.store(true, Ordering::Release);
                    }
                    continue;
                }
                let Some(packet) = queue.pop(WORKER_POLL) else {
                    continue;
                };
                if waiting_keyframe && !packet.key {
                    continue;
                }
                if waiting_keyframe {
                    waiting_keyframe = false;
                    // Linked pre-roll: hold the first key frame until audio has reached the
                    // bounded horizon, so the presenter never starts video ahead of audio.
                    let minimum_buffer = playback_buffer_us(packet.duration_us);
                    let required_end = packet
                        .pts_us
                        .saturating_add(i64::try_from(minimum_buffer).unwrap_or(i64::MAX));
                    if !prebuffer.wait_for_audio(required_end, &running, Duration::from_millis(750))
                    {
                        if require_audio {
                            let _ = notices.send(WorkerNotice::Fatal(
                                "audio did not reach the linked pre-roll deadline".into(),
                            ));
                            return;
                        }
                        let _ = notices.send(WorkerNotice::AudioLost(
                            "audio did not reach the linked pre-roll deadline".into(),
                        ));
                    }
                }
                let body_length = packet.data().len();
                let send_started = Instant::now();
                let send_result = sender.send(&EncodedPacket::Video(vivid_sdk::VideoPacketData {
                    epoch: sender.current_epoch(),
                    packet_id: sender.next_packet_id(),
                    pts_us: packet.pts_us,
                    dts_us: packet.dts_us,
                    duration_us: packet.duration_us,
                    key: packet.key,
                    data: packet.data().to_vec(),
                }));
                queue.record_send_duration(send_started.elapsed());
                // Total blocked time is a diagnostic, not a control signal: it lumps this
                // producer's own rate limiter and the presenter's channel-flow window in with the
                // transport. The controller needs to know which of the three actually waited.
                rate.observe_send(body_length, sender.channel().take_send_pressure());
                match send_result {
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error)
                        if error.kind() == io::ErrorKind::BrokenPipe
                            || error.kind() == io::ErrorKind::ConnectionAborted =>
                    {
                        // Flow (c): the channel generation died. Advance, reopen, and prime
                        // the new channel with a forced IDR; the surface, node, and other
                        // track are untouched.
                        match wait_for_keyframe(&queue, &force_keyframe, &running)
                            .and_then(|key| recovery(key.data()))
                        {
                            Ok(recovered) => {
                                sender = recovered;
                                waiting_keyframe = false;
                            }
                            Err(_error) if !running.load(Ordering::Acquire) => return,
                            Err(error) => {
                                let _ = notices.send(WorkerNotice::Fatal(format!(
                                    "video channel recovery failed: {error}"
                                )));
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        if running.load(Ordering::Acquire) {
                            let _ = notices.send(WorkerNotice::Fatal(format!(
                                "video media channel failed: {error}"
                            )));
                        }
                        return;
                    }
                }
            }
        })
}

fn minimum_recovery_epoch(payload: &PayloadMap) -> u32 {
    payload
        .iter()
        .find(|(key, _)| *key == 4)
        .and_then(|(_, value)| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0)
}

fn clear_for_recovery(video_queue: &EncodedQueue) {
    video_queue.clear();
}

fn playback_buffer_us(keyframe_duration_us: u64) -> u64 {
    // wlroots screencopy can remain idle indefinitely after one unchanged frame. Requiring a
    // multi-frame video horizon would delay the linked pre-roll indefinitely for a static
    // desktop, so the independently decodable IDR is the complete bounded video pre-roll.
    // Linked audio still has to reach this exact end PTS before the first key frame is sent.
    keyframe_duration_us.min(PREBUFFER_US)
}

fn spawn_audio_sender(
    sender: TrackSender,
    queue: Arc<AudioQueue>,
    prebuffer: Arc<Prefbuffer>,
    send_gate: Arc<Mutex<()>>,
    running: Arc<AtomicBool>,
    notices: mpsc::Sender<WorkerNotice>,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("vvland-vivid-audio".into())
        .spawn(move || {
            loop {
                // The quit path joins this thread; an idle worker must observe the
                // running flag instead of idling on an empty queue forever.
                if !running.load(Ordering::Acquire) {
                    return;
                }
                for event in sender.drain_events() {
                    match event {
                        ChannelEvent::Error(error) => {
                            prebuffer.disable_audio();
                            if running.load(Ordering::Acquire) {
                                let _ = notices.send(WorkerNotice::AudioLost(error.to_string()));
                            }
                            return;
                        }
                        ChannelEvent::NeedKeyframe(_) | ChannelEvent::NeedFullFrame(_) => {}
                    }
                }
                let packet = match queue.pop(WORKER_POLL) {
                    Ok(Some(packet)) => packet,
                    Ok(None) => continue,
                    Err(error) => {
                        prebuffer.disable_audio();
                        if running.load(Ordering::Acquire) {
                            let _ = notices.send(WorkerNotice::AudioLost(error.to_string()));
                        }
                        return;
                    }
                };
                let send_guard = lock(&send_gate);
                let result = sender.send(&EncodedPacket::Audio(vivid_sdk::AudioPacketData {
                    epoch: sender.current_epoch(),
                    packet_id: sender.next_packet_id(),
                    pts_us: packet.pts_us,
                    dts_us: packet.dts_us,
                    duration_us: packet.duration_us,
                    data: packet.data().to_vec(),
                }));
                drop(send_guard);
                match result {
                    Ok(_) => {
                        let end = packet
                            .pts_us
                            .saturating_add(i64::try_from(packet.duration_us).unwrap_or(i64::MAX));
                        prebuffer.observe_audio(end);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        prebuffer.disable_audio();
                        if running.load(Ordering::Acquire) {
                            let _ = notices.send(WorkerNotice::AudioLost(error.to_string()));
                        }
                        return;
                    }
                }
            }
        })
}

struct AudioRuntime {
    queue: Option<Arc<AudioQueue>>,
    gain: Option<super::audio::AudioGain>,
    prebuffer: Arc<Prefbuffer>,
    disabled_reason: Mutex<Option<String>>,
}

impl AudioRuntime {
    fn active_gain(&self) -> Option<&super::audio::AudioGain> {
        self.gain.as_ref()
    }

    fn disabled_reason(&self) -> Option<String> {
        lock(&self.disabled_reason).clone()
    }

    fn disable(&self, reason: &str) {
        *lock(&self.disabled_reason) = Some(safe_audio_reason(reason));
        self.prebuffer.disable_audio();
        if let Some(queue) = &self.queue {
            queue.clear();
        }
    }
}

fn safe_audio_reason(reason: &str) -> String {
    reason
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(160)
        .collect()
}

fn join_worker(join: thread::JoinHandle<()>) {
    let _ = join.join();
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The daemon-owned resources a presenter may borrow without taking ownership of the desktop.
pub(super) struct PresenterSource {
    pub latest: Arc<super::video::LatestFrame>,
    pub pulse: Option<(String, OsString)>,
    pub dimensions: (u32, u32),
    pub compositor: ResolvedCompositor,
    pub origin: Instant,
}

/// State shared with the control actor while an attach worker is alive.
pub(super) struct PresenterSpawn {
    pub running: Arc<AtomicBool>,
    pub ready: mpsc::Receiver<io::Result<serde_json::Value>>,
    pub done: mpsc::Receiver<Result<(), String>>,
    pub state: Arc<Mutex<serde_json::Value>>,
    pub join: thread::JoinHandle<()>,
}

/// Start one presentation without moving the compositor, capture source, or Pulse sink out of
/// the daemon. All credentials move into this worker and are cleared when it exits.
pub(super) fn spawn_presenter(
    mut config: Config,
    params: AttachParams,
    source: PresenterSource,
    input: mpsc::SyncSender<HostInputCall>,
) -> io::Result<PresenterSpawn> {
    config.desktop_target = params.desktop_target;
    config.secure_input = params.secure_input;
    config.bitrate = params.bitrate;
    config.fps = params.fps;
    config.width = Some(source.dimensions.0);
    config.height = Some(source.dimensions.1);
    let running = Arc::new(AtomicBool::new(true));
    let state = Arc::new(Mutex::new(json!({
        "attached": true,
        "ready": false,
        "desktop_target": config.desktop_target,
        "bitrate": config.bitrate,
        "fps": config.fps,
    })));
    let (ready_tx, ready) = mpsc::sync_channel(1);
    let (done_tx, done) = mpsc::sync_channel(1);
    let worker_running = running.clone();
    let worker_state = state.clone();
    let join = thread::Builder::new()
        .name("vvland-presenter".into())
        .spawn(move || {
            let mut ready_tx = Some(ready_tx);
            let result = run_presenter(
                config,
                params,
                source,
                input,
                worker_running.clone(),
                worker_state,
                &mut ready_tx,
            );
            worker_running.store(false, Ordering::Release);
            if let Some(sender) = ready_tx {
                let error = result.as_ref().err().map_or_else(
                    || io::Error::other("presenter stopped during attach"),
                    clone_io,
                );
                let _ = sender.send(Err(error));
            }
            let _ = done_tx.send(result.map_err(|error| safe_audio_reason(&error.to_string())));
        })?;
    Ok(PresenterSpawn {
        running,
        ready,
        done,
        state,
        join,
    })
}

fn clone_io(error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), error.to_string())
}

fn run_presenter(
    config: Config,
    mut params: AttachParams,
    source: PresenterSource,
    input: mpsc::SyncSender<HostInputCall>,
    running: Arc<AtomicBool>,
    state: Arc<Mutex<serde_json::Value>>,
    ready: &mut Option<mpsc::SyncSender<io::Result<serde_json::Value>>>,
) -> io::Result<()> {
    let vivid = &mut *params.vivid;
    let authentication = ProducerAuthentication::root_hex(&vivid.root_secret)?;
    let mut producer = producer_config(&config, source.compositor);
    producer.endpoint_control = Some(std::mem::take(&mut *vivid.endpoint_control));
    producer.endpoint_interactive = vivid
        .endpoint_interactive
        .as_mut()
        .map(|value| std::mem::take(&mut **value));
    producer.endpoint_realtime = vivid
        .endpoint_realtime
        .as_mut()
        .map(|value| std::mem::take(&mut **value));
    producer.endpoint_bulk = vivid
        .endpoint_bulk
        .as_mut()
        .map(|value| std::mem::take(&mut **value));
    producer.authentication = authentication;
    let mut session =
        Session::connect(producer).map_err(|error| desktop_target_hint(&config, error))?;

    let encoder = H264Encoder::new(
        source.dimensions.0,
        source.dimensions.1,
        config.fps,
        config.bitrate,
        config.gop_seconds,
        config.max_access_unit_bytes,
    )?;
    let product = source.compositor.identity();
    let mut audio_pipeline = match source.pulse.as_ref() {
        Some((monitor, server)) => {
            match AudioPipeline::start(monitor, server, source.origin, &product) {
                Ok(audio) => Some(audio),
                Err(error) if config.require_audio => return Err(error),
                Err(_) => None,
            }
        }
        None => None,
    };
    if audio_pipeline.is_some() && !session.supports(LIVE_MEDIA) {
        if config.require_audio {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "presenter lacks Vivid live-media-v1",
            ));
        }
        audio_pipeline = None;
    }
    let context_id = session.info().root_context_id;
    let surface_id = session.allocate_id()?;
    let mut video_cfg = video_track_config(
        &session,
        source.dimensions,
        &config,
        encoder.decoder_description(),
    )?;
    video_cfg.context_id = context_id;
    video_cfg.surface_id = surface_id;
    video_cfg.track_id = session.allocate_id()?;
    let mut audio_cfg = audio_pipeline.as_ref().map(|audio| {
        let mut cfg = audio_track_config(&session, &audio.spec);
        cfg.context_id = context_id;
        cfg.surface_id = surface_id;
        cfg.track_id = session.allocate_id().expect("audio track ID allocation");
        cfg
    });
    if let Some(cfg) = &audio_cfg {
        let mut probe = cfg.clone();
        probe.track_id = 0;
        if let Err(error) = session.probe_track(&probe) {
            if config.require_audio {
                return Err(error);
            }
            audio_pipeline = None;
            audio_cfg = None;
        }
    }
    let prebuffer = Arc::new(Prefbuffer::new(audio_cfg.is_some()));
    let audio_runtime = AudioRuntime {
        queue: audio_pipeline.as_ref().map(|audio| audio.queue.clone()),
        gain: audio_pipeline.as_ref().map(|audio| audio.gain.clone()),
        prebuffer: prebuffer.clone(),
        disabled_reason: Mutex::new(None),
    };
    let (notice_tx, notice_rx) = mpsc::channel();
    if config.desktop_target {
        run_attached_desktop(
            &config,
            source,
            session,
            encoder,
            audio_pipeline,
            audio_cfg,
            video_cfg,
            running,
            state,
            input,
            prebuffer,
            audio_runtime,
            notice_tx,
            notice_rx,
            ready,
        )
    } else {
        run_attached_terminal(
            &config,
            source,
            session,
            encoder,
            audio_pipeline,
            audio_cfg,
            video_cfg,
            running,
            state,
            prebuffer,
            audio_runtime,
            notice_tx,
            notice_rx,
            ready,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn run_attached_terminal(
    config: &Config,
    source: PresenterSource,
    mut session: Session,
    encoder: H264Encoder,
    audio_pipeline: Option<AudioPipeline>,
    audio_cfg: Option<TrackConfiguration>,
    video_cfg: TrackConfiguration,
    running: Arc<AtomicBool>,
    state: Arc<Mutex<serde_json::Value>>,
    prebuffer: Arc<Prefbuffer>,
    audio_runtime: AudioRuntime,
    notice_tx: mpsc::Sender<WorkerNotice>,
    notice_rx: mpsc::Receiver<WorkerNotice>,
    ready: &mut Option<mpsc::SyncSender<io::Result<serde_json::Value>>>,
) -> io::Result<()> {
    let context_id = session.info().root_context_id;
    let surface_id = video_cfg.surface_id;
    let node_id = session.allocate_id()?;
    let surface = session.create_surface(
        terminal_surface_def(
            context_id,
            surface_id,
            source.dimensions,
            config.secure_input,
            source.compositor.wire_name(),
        ),
        &RequestMetadata::default(),
    )?;
    let video_track = session.create_track(video_cfg, &RequestMetadata::default())?;
    let video_sender = TrackSender::new(session.open_track_channel(&video_track)?);
    let (audio_track, audio_sender) = create_attached_audio(config, &mut session, audio_cfg)?;
    let video_generation = video_sender.generation();
    let audio_generation = audio_sender.as_ref().map(TrackSender::generation);
    let display = terminal_display(&session)?;
    let mut placement = Placement::calculate(display, source.dimensions.0, source.dimensions.1)?;
    session.create_node(
        &placement.node(node_id, context_id, surface_id),
        &RequestMetadata::default(),
    )?;
    let anchor_id = session.allocate_id()?;
    let marker = Zeroizing::new(session.anchor_marker(context_id, anchor_id)?);

    let session = Arc::new(Mutex::new(session));
    let encoded_queue = Arc::new(EncodedQueue::new(
        video_queue_packets(config),
        video_queue_bytes(&video_track),
        Duration::from_micros(LIVE_MAX_LATENCY_US),
    ));
    let rate = Arc::new(VideoRateControl::new(config.bitrate));
    configure_audio_queue(audio_pipeline.as_ref(), audio_sender.as_ref());
    let force_keyframe = Arc::new(AtomicBool::new(true));
    let audio_queue = audio_pipeline.as_ref().map(|audio| audio.queue.clone());
    let encoder_join = spawn_encoder(
        source.latest,
        encoder,
        source.origin,
        config.fps,
        encoded_queue.clone(),
        rate.clone(),
        force_keyframe.clone(),
        running.clone(),
        notice_tx.clone(),
    )?;
    let recovery = {
        let session = session.clone();
        let track = video_track.clone();
        Arc::new(move |key_unit: &[u8]| recover_channel(&mut lock(&session), &track, key_unit))
    };
    let video_join = spawn_video_sender(
        video_sender,
        recovery,
        encoded_queue.clone(),
        audio_queue.clone(),
        rate.clone(),
        force_keyframe,
        prebuffer.clone(),
        running.clone(),
        notice_tx.clone(),
        config.require_audio,
    )?;
    let audio_join = spawn_optional_audio(
        audio_sender,
        audio_queue.clone(),
        prebuffer.clone(),
        running.clone(),
        notice_tx,
    )?;
    send_attach_ready(
        ready,
        &state,
        json!({
            "desktop_target": false,
            "marker": &*marker,
            "width": source.dimensions.0,
            "height": source.dimensions.1,
            "display": display_json(display),
        }),
    );

    let established = {
        let mut guard = lock(&session);
        let video_ready =
            wait_attached_milestone(&mut guard, &video_track, SLOT_MILESTONE, &running)?;
        let audio_ready = if video_ready {
            match &audio_track {
                Some(track) => {
                    wait_attached_milestone(&mut guard, track, SLOT_MILESTONE, &running)?
                }
                None => true,
            }
        } else {
            false
        };
        if video_ready && audio_ready {
            let mut slots = SurfaceSlots::new(&surface);
            slots.require(1, &video_track, video_generation, SLOT_MILESTONE)?;
            if let (Some(track), Some(generation)) = (&audio_track, audio_generation) {
                slots.require(2, track, generation, SLOT_MILESTONE)?;
            }
            slots.activate(&mut guard)?;
            true
        } else {
            false
        }
    };

    while established && running.load(Ordering::Acquire) {
        drain_notices(config, &audio_runtime, &notice_rx)?;
        let mut guard = lock(&session);
        let mut changed = false;
        while let Some(event) = guard.take_event()? {
            match event {
                SessionEvent::TargetChanged(payload) => {
                    guard.apply_target_changed(&payload)?;
                    changed = true;
                }
                SessionEvent::ConnectionClosed { diagnostic } => {
                    return Err(io::Error::new(io::ErrorKind::ConnectionAborted, diagnostic));
                }
                SessionEvent::TrackLost { object_id, .. } if object_id == video_track.id() => {
                    return Err(io::Error::other("the presenter lost the video track"));
                }
                _ => {}
            }
        }
        if changed && guard.info().target_settled()? {
            let display = terminal_display(&guard)?;
            placement = Placement::calculate(display, source.dimensions.0, source.dimensions.1)?;
            guard.update_node(
                &placement.node(node_id, context_id, surface_id),
                &RequestMetadata::default(),
            )?;
            (*lock(&state))["display"] = display_json(display);
        }
        drop(guard);
        update_streaming_state(&state, &encoded_queue, audio_queue.as_deref(), &rate);
        thread::sleep(MAIN_POLL);
    }

    stop_attached_media(
        &running,
        &prebuffer,
        &encoded_queue,
        audio_queue.as_ref(),
        &session,
        audio_pipeline,
        encoder_join,
        video_join,
        audio_join,
    );
    // `abort` above is the detach boundary: it wakes blocked media flow and the presenter drops
    // every object scoped to this root session. Sending ordered deletes after abort would wait on
    // a connection that was deliberately closed and turn a local detach into multi-second timeouts.
    drop(session);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_attached_desktop(
    config: &Config,
    source: PresenterSource,
    session: Session,
    encoder: H264Encoder,
    audio_pipeline: Option<AudioPipeline>,
    audio_cfg: Option<TrackConfiguration>,
    video_cfg: TrackConfiguration,
    running: Arc<AtomicBool>,
    state: Arc<Mutex<serde_json::Value>>,
    input_sender: mpsc::SyncSender<HostInputCall>,
    prebuffer: Arc<Prefbuffer>,
    audio_runtime: AudioRuntime,
    notice_tx: mpsc::Sender<WorkerNotice>,
    notice_rx: mpsc::Receiver<WorkerNotice>,
    ready: &mut Option<mpsc::SyncSender<io::Result<serde_json::Value>>>,
) -> io::Result<()> {
    let surface_def = desktop_surface_def(
        session.info().root_context_id,
        video_cfg.surface_id,
        source.dimensions,
        config.secure_input,
        source.compositor.wire_name(),
    );
    let desktop = Arc::new(Mutex::new(vivid_sdk::DesktopSession::establish(
        session,
        surface_def,
        video_cfg,
        audio_cfg,
    )?));
    let video_sender = lock(&desktop).video_sender().clone();
    let audio_sender = lock(&desktop).audio_sender().cloned();
    let video_track = lock(&desktop).video_track().clone();
    let audio_track = lock(&desktop).audio_track().cloned();
    let encoded_queue = Arc::new(EncodedQueue::new(
        video_queue_packets(config),
        video_queue_bytes(&video_track),
        Duration::from_micros(LIVE_MAX_LATENCY_US),
    ));
    let rate = Arc::new(VideoRateControl::new(config.bitrate));
    configure_audio_queue(audio_pipeline.as_ref(), audio_sender.as_ref());
    let force_keyframe = Arc::new(AtomicBool::new(true));
    let audio_queue = audio_pipeline.as_ref().map(|audio| audio.queue.clone());
    let encoder_join = spawn_encoder(
        source.latest,
        encoder,
        source.origin,
        config.fps,
        encoded_queue.clone(),
        rate.clone(),
        force_keyframe.clone(),
        running.clone(),
        notice_tx.clone(),
    )?;
    let recovery = {
        let desktop = desktop.clone();
        Arc::new(move |key_unit: &[u8]| {
            let mut guard = lock(&desktop);
            let track = guard.video_track().clone();
            recover_channel(guard.session_mut(), &track, key_unit)
        })
    };
    let video_join = spawn_video_sender(
        video_sender.clone(),
        recovery,
        encoded_queue.clone(),
        audio_queue.clone(),
        rate.clone(),
        force_keyframe,
        prebuffer.clone(),
        running.clone(),
        notice_tx.clone(),
        config.require_audio,
    )?;
    let audio_join = spawn_optional_audio(
        audio_sender,
        audio_queue.clone(),
        prebuffer.clone(),
        running.clone(),
        notice_tx,
    )?;
    send_attach_ready(
        ready,
        &state,
        json!({
            "desktop_target": true,
            "width": source.dimensions.0,
            "height": source.dimensions.1,
        }),
    );
    let presented = {
        let mut guard = lock(&desktop);
        let video_ready =
            wait_attached_milestone(guard.session_mut(), &video_track, SLOT_MILESTONE, &running)?;
        let audio_ready = if video_ready {
            match &audio_track {
                Some(track) => {
                    wait_attached_milestone(guard.session_mut(), track, SLOT_MILESTONE, &running)?
                }
                None => true,
            }
        } else {
            false
        };
        if video_ready && audio_ready {
            let mut slots = SurfaceSlots::new(guard.desktop_surface().inner());
            slots.require(1, &video_track, video_sender.generation(), SLOT_MILESTONE)?;
            if let (Some(track), Some(sender)) = (&audio_track, guard.audio_sender()) {
                slots.require(2, track, sender.generation(), SLOT_MILESTONE)?;
            }
            slots.activate(guard.session_mut())?;
            wait_attached_milestone(
                guard.session_mut(),
                &video_track,
                MILESTONE_PRESENTED,
                &running,
            )?
        } else {
            false
        }
    };
    let (context_id, surface_id, generation, capabilities) = {
        let guard = lock(&desktop);
        (
            guard.session().info().root_context_id,
            guard.desktop_surface().id(),
            guard.desktop_surface().generation(),
            guard.desktop_surface().input_capabilities()?,
        )
    };
    let mut input = InputRuntime::new(context_id, surface_id, capabilities, DEFAULT_WATCHDOG_US);
    input.set_surface_state(generation, capabilities, &mut || {});
    input.set_presented(presented);
    let mut proxy = ActorInputProxy {
        sender: input_sender,
    };
    while presented && running.load(Ordering::Acquire) {
        drain_notices(config, &audio_runtime, &notice_rx)?;
        let mut guard = lock(&desktop);
        while let Some(event) = guard.session().take_event()? {
            match event {
                SessionEvent::TargetChanged(payload) => {
                    guard.session_mut().apply_target_changed(&payload)?;
                }
                SessionEvent::ConnectionClosed { diagnostic } => {
                    return Err(io::Error::new(io::ErrorKind::ConnectionAborted, diagnostic));
                }
                SessionEvent::TrackLost { object_id, .. } if object_id == video_track.id() => {
                    return Err(io::Error::other("the presenter lost the video track"));
                }
                _ => {}
            }
        }
        let now = monotonic_now(&source.origin);
        let surface = guard.desktop_surface();
        input.set_surface_state(
            surface.generation(),
            surface.input_capabilities()?,
            &mut || {
                let _ = proxy.release_all();
            },
        );
        input.set_lane_live(guard.lane().is_some());
        if input.watchdog_expired(now) {
            input.on_watchdog_expiry(&mut || {
                let _ = proxy.release_all();
            });
        }
        while let Some(event) = guard
            .lane()
            .and_then(|lane| lane.take_event().ok())
            .flatten()
        {
            match event {
                InputLaneEvent::Input { .. } => {
                    if let Ok(decoded) = event.decode_input(
                        u64::from(source.dimensions.0),
                        u64::from(source.dimensions.1),
                    ) {
                        input.push(decoded);
                    }
                }
                other => {
                    let _ = input.observe(other, now, &mut || {
                        let _ = proxy.release_all();
                    });
                }
            }
        }
        if let Some(lane) = guard.lane() {
            input.enable_if_ready(lane)?;
        }
        drop(guard);
        if input.overflowed() {
            input.release_overflow(&mut || {
                let _ = proxy.release_all();
            });
        }
        input.drain(now, |event| {
            desktop_input::apply(event, &mut proxy, context_id, surface_id, source.dimensions)
        })?;
        update_streaming_state(&state, &encoded_queue, audio_queue.as_deref(), &rate);
        thread::sleep(MAIN_POLL);
    }
    let _ = proxy.release_all();
    running.store(false, Ordering::Release);
    prebuffer.disable_audio();
    encoded_queue.close();
    if let Some(queue) = &audio_queue {
        queue.clear();
    }
    let _ = lock(&desktop).session_mut().abort();
    drop(audio_pipeline);
    join_worker(encoder_join);
    join_worker(video_join);
    if let Some(join) = audio_join {
        join_worker(join);
    }
    drop(desktop);
    Ok(())
}

fn create_attached_audio(
    config: &Config,
    session: &mut Session,
    audio_cfg: Option<TrackConfiguration>,
) -> io::Result<(Option<Track>, Option<TrackSender>)> {
    let Some(cfg) = audio_cfg else {
        return Ok((None, None));
    };
    match session.create_track(cfg, &RequestMetadata::default()) {
        Ok(track) => {
            let sender = TrackSender::new(session.open_track_channel(&track)?);
            Ok((Some(track), Some(sender)))
        }
        Err(error) if config.require_audio => Err(error),
        Err(_) => Ok((None, None)),
    }
}

fn configure_audio_queue(audio: Option<&AudioPipeline>, sender: Option<&TrackSender>) {
    if let (Some(audio), Some(sender)) = (audio, sender) {
        audio.queue.configure_limits(
            audio_queue_packets(sender.channel().track()),
            audio_queue_bytes(sender.channel().track()),
        );
    }
}

fn spawn_optional_audio(
    sender: Option<TrackSender>,
    queue: Option<Arc<AudioQueue>>,
    prebuffer: Arc<Prefbuffer>,
    running: Arc<AtomicBool>,
    notices: mpsc::Sender<WorkerNotice>,
) -> io::Result<Option<thread::JoinHandle<()>>> {
    sender
        .zip(queue)
        .map(|(sender, queue)| {
            spawn_audio_sender(
                sender,
                queue,
                prebuffer,
                Arc::new(Mutex::new(())),
                running,
                notices,
            )
        })
        .transpose()
}

fn send_attach_ready(
    ready: &mut Option<mpsc::SyncSender<io::Result<serde_json::Value>>>,
    state: &Mutex<serde_json::Value>,
    mut value: serde_json::Value,
) {
    value["attached"] = serde_json::Value::Bool(true);
    value["ready"] = serde_json::Value::Bool(true);
    let mut public = value.clone();
    if let Some(object) = public.as_object_mut() {
        object.remove("marker");
    }
    *lock(state) = public;
    if let Some(sender) = ready.take() {
        let _ = sender.send(Ok(value));
    }
}

fn update_streaming_state(
    state: &Mutex<serde_json::Value>,
    video: &EncodedQueue,
    audio: Option<&AudioQueue>,
    rate: &VideoRateControl,
) {
    let video = video.snapshot();
    let audio = audio.map(AudioQueue::snapshot);
    let rate = rate.snapshot();
    lock(state)["streaming"] = json!({
        "video": {
            "configured_bits_per_second": rate.configured_bits_per_second,
            "target_bits_per_second": rate.target_bits_per_second,
            "rate_adjustments": rate.adjustments,
            // Which limit the session is actually hitting. Only transport pressure means the link
            // is full; flow pressure means the presenter is behind on records, which costs frame
            // rate rather than picture quality.
            "rate_limited_ms": rate.rate_limited.as_millis(),
            "flow_limited_ms": rate.flow_limited.as_millis(),
            "transport_blocked_ms": rate.transport.as_millis(),
            "queued_packets": video.packets,
            "queued_bytes": video.bytes,
            "oldest_queue_age_ms": video.oldest_age.as_millis(),
            "overflow_events": video.overflow_events,
            "dropped_packets": video.dropped_packets,
            "peak_packets": video.peak_packets,
            "peak_bytes": video.peak_bytes,
            "encoded_packets": video.pushed_packets,
            "sent_packets": video.popped_packets,
            "cumulative_send_blocked_ms": video.cumulative_send_blocked.as_millis(),
            "maximum_send_blocked_ms": video.maximum_send_blocked.as_millis(),
        },
        "audio": audio.map(|audio| json!({
            "queued_packets": audio.packets,
            "queued_bytes": audio.bytes,
            "queued_duration_us": audio.queued_duration_us,
            "dropped_packets": audio.dropped_packets,
            "peak_packets": audio.peak_packets,
            "peak_bytes": audio.peak_bytes,
        })),
    });
}

fn display_json(display: TerminalDisplay) -> serde_json::Value {
    json!({
        "grid_columns": display.grid_columns,
        "grid_rows": display.grid_rows,
        "cell_width": display.cell_width,
        "cell_height": display.cell_height,
    })
}

fn wait_attached_milestone(
    session: &mut Session,
    track: &Track,
    milestone: u64,
    running: &AtomicBool,
) -> io::Result<bool> {
    while running.load(Ordering::Acquire) {
        match session.wait_track(
            track,
            vivid_sdk::TrackWaitCondition::MilestoneSet,
            Some(milestone),
            200_000,
        ) {
            Ok(satisfied) => return Ok(satisfied.observed_value.is_some()),
            Err(error) if error.to_string().contains("track wait timed out") => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn drain_notices(
    config: &Config,
    audio: &AudioRuntime,
    notices: &mpsc::Receiver<WorkerNotice>,
) -> io::Result<()> {
    while let Ok(notice) = notices.try_recv() {
        match notice {
            WorkerNotice::Fatal(error) => return Err(io::Error::other(error)),
            WorkerNotice::AudioLost(error) if config.require_audio => {
                return Err(io::Error::other(error));
            }
            WorkerNotice::AudioLost(error) => audio.disable(&error),
            WorkerNotice::FileDropCommitted(_) => {}
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn stop_attached_media(
    running: &AtomicBool,
    prebuffer: &Prefbuffer,
    encoded: &EncodedQueue,
    audio_queue: Option<&Arc<AudioQueue>>,
    session: &Mutex<Session>,
    audio_pipeline: Option<AudioPipeline>,
    encoder: thread::JoinHandle<()>,
    video: thread::JoinHandle<()>,
    audio: Option<thread::JoinHandle<()>>,
) {
    running.store(false, Ordering::Release);
    prebuffer.disable_audio();
    encoded.close();
    if let Some(queue) = audio_queue {
        queue.clear();
    }
    let _ = lock(session).abort();
    drop(audio_pipeline);
    join_worker(encoder);
    join_worker(video);
    if let Some(join) = audio {
        join_worker(join);
    }
}

struct ActorInputProxy {
    sender: mpsc::SyncSender<HostInputCall>,
}

impl ActorInputProxy {
    fn call(&self, action: PresenterInput) -> io::Result<()> {
        let (reply, received) = mpsc::sync_channel(1);
        self.sender
            .send(HostInputCall { action, reply })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "desktop actor stopped"))?;
        received
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "desktop actor stopped"))?
    }
}

impl TerminalInjector for ActorInputProxy {
    fn key(&mut self, code: u32, pressed: bool) -> io::Result<()> {
        self.call(PresenterInput::Key { code, pressed })
    }

    fn pointer_absolute(&mut self, x: u32, y: u32) -> io::Result<()> {
        self.call(PresenterInput::PointerAbsolute { x, y })
    }

    fn pointer_button(&mut self, button: u32, pressed: bool) -> io::Result<()> {
        self.call(PresenterInput::PointerButton { button, pressed })
    }

    fn pointer_axis(&mut self, axis: u32, delta: i32) -> io::Result<()> {
        self.call(PresenterInput::PointerAxis { axis, delta })
    }

    fn release_all(&mut self) -> io::Result<()> {
        self.call(PresenterInput::ReleaseAll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn config() -> Config {
        Config::try_parse_from(["vvland", "--doctor"]).unwrap()
    }

    fn display(columns: u32, rows: u32) -> TerminalDisplay {
        TerminalDisplay {
            grid_columns: columns,
            grid_rows: rows,
            cell_width: 10,
            cell_height: 20,
        }
    }

    #[test]
    fn headless_size_excludes_status_row_and_caps_defaults() {
        assert_eq!(
            headless_size(&config(), display(256, 72)).unwrap(),
            (1920, 1080)
        );
        assert_eq!(
            headless_size(&config(), display(80, 30)).unwrap(),
            (800, 580)
        );
        assert_eq!(
            headless_size(&config(), display(0, 0)).unwrap(),
            (1920, 1080)
        );
    }

    #[test]
    fn prebuffer_waits_for_linked_audio_horizon() {
        let prebuffer = Prefbuffer::new(true);
        let running = AtomicBool::new(true);
        prebuffer.observe_audio(99_999);
        assert!(!prebuffer.wait_for_audio(100_000, &running, Duration::ZERO));
        prebuffer.observe_audio(100_000);
        assert!(prebuffer.wait_for_audio(100_000, &running, Duration::ZERO));
    }

    #[test]
    fn recovery_can_play_from_one_damage_driven_keyframe() {
        assert_eq!(playback_buffer_us(33_333), 33_333);
        assert_eq!(playback_buffer_us(0), 0);
        assert_eq!(playback_buffer_us(250_000), PREBUFFER_US);
    }

    #[test]
    fn audio_failure_status_is_bounded_and_terminal_safe() {
        let reason = format!("bad\r\n\x1b[31m{}", "x".repeat(200));
        let reason = safe_audio_reason(&reason);
        assert_eq!(reason.chars().count(), 160);
        assert!(!reason.chars().any(char::is_control));
    }

    #[test]
    fn need_keyframe_minimum_epoch_is_read_from_key_four() {
        assert_eq!(minimum_recovery_epoch(&vec![]), 0);
        assert_eq!(
            minimum_recovery_epoch(&vec![(0, vivid_protocol::cbor::Value::Unsigned(1))]),
            0
        );
        assert_eq!(
            minimum_recovery_epoch(&vec![(4, vivid_protocol::cbor::Value::Unsigned(7))]),
            7
        );
    }

    #[test]
    fn video_queue_is_bounded_by_the_live_latency_window() {
        let mut bounded = config();
        bounded.fps = 30;
        bounded.gop_seconds = 2;
        assert_eq!(video_queue_packets(&bounded), 3);
    }

    #[test]
    fn an_audio_backlog_lowers_the_video_target_instead_of_pausing_the_track() {
        // Regression: a backlogged audio track used to stop video delivery outright and force an
        // IDR every ten milliseconds until the backlog drained, which is the one response that
        // makes a congested link worse. It now only moves the encoder target.
        let rate = VideoRateControl::new(8_000_000);
        rate.observe_audio_backlog(400_000);
        rate.observe_send(4_096, vivid_sdk::SendPressure::default());
        thread::sleep(Duration::from_millis(1_050));

        let lowered = rate.poll().expect("an audio backlog lowers the target");
        assert!(lowered < 8_000_000, "{lowered}");
        assert_eq!(rate.target(), lowered);
    }

    #[test]
    fn attach_frame_rate_drops_early_frames_but_never_a_forced_idr() {
        assert!(frame_due(None, 0, 100_000, false));
        assert!(!frame_due(Some(0), 99_999, 100_000, false));
        assert!(frame_due(Some(0), 100_000, 100_000, false));
        assert!(frame_due(Some(0), 1, 100_000, true));
    }

    #[test]
    fn plain_terminal_flow_establishes_against_a_terminal_presenter() {
        // The default vivido window is a terminal emulator; plain vvland must
        // negotiate it without any option and complete the establishment order:
        // probe (no track identity), surface, track, channel, first key unit,
        // decoded-output readiness, then slot activation.
        let _env_guard = crate::cli::tests::TEST_ENV_LOCK.lock().unwrap();
        let presenter = vivid_sdk::testing::TestPresenter::start(80, 24).unwrap();
        unsafe {
            std::env::set_var("VIVID_ENDPOINT_CONTROL", presenter.endpoint());
            std::env::set_var("VIVID_ENDPOINT_INTERACTIVE", presenter.endpoint());
            std::env::set_var("VIVID_ENDPOINT_REALTIME", presenter.endpoint());
            std::env::set_var("VIVID_ENDPOINT_BULK", presenter.endpoint());
            std::env::set_var("VIVID_ROOT_SECRET", vivid_sdk::testing::ROOT_SECRET_HEX);
        }
        let config = Config::try_parse_from(["vvland"]).unwrap();
        assert!(config.validate().is_ok());
        let mut session =
            Session::connect(producer_config(&config, ResolvedCompositor::Weston)).unwrap();
        assert_eq!(session.info().target_profile, TERMINAL_SURFACE);
        let dimensions = headless_size(&config, terminal_display(&session).unwrap()).unwrap();

        // The plain audio probe has no track identity: track_id must be zero.
        let spec = crate::producer::audio::AudioTrackSpec {
            extradata: vec![
                b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd', 1, 2, 0x38, 0x01, 0x80, 0xbb, 0x00,
                0x00, 0, 0, 0,
            ],
            sample_rate: 48_000,
            channels: 2,
            max_access_unit_bytes: 1_500,
        };
        let mut audio_cfg = audio_track_config(&session, &spec);
        audio_cfg.context_id = session.info().root_context_id;
        audio_cfg.surface_id = 1;
        let mut probe = audio_cfg.clone();
        probe.track_id = 0;
        assert!(session.probe_track(&probe).unwrap().supported);

        let context_id = session.info().root_context_id;
        let surface = session
            .create_surface(
                terminal_surface_def(context_id, 1, dimensions, false, "veston"),
                &RequestMetadata::default(),
            )
            .unwrap();
        let encoder = H264Encoder::new(
            dimensions.0,
            dimensions.1,
            config.fps,
            config.bitrate,
            config.gop_seconds,
            config.max_access_unit_bytes,
        )
        .unwrap();
        let mut video_cfg =
            video_track_config(&session, dimensions, &config, encoder.decoder_description())
                .unwrap();
        video_cfg.context_id = context_id;
        video_cfg.surface_id = surface.id();
        video_cfg.track_id = session.allocate_id().unwrap();
        let video_track = session
            .create_track(video_cfg, &RequestMetadata::default())
            .unwrap();
        let video_sender = TrackSender::new(session.open_track_channel(&video_track).unwrap());
        let generation = video_sender.generation();
        video_sender
            .send(&EncodedPacket::Video(vivid_sdk::VideoPacketData {
                epoch: 1,
                packet_id: 1,
                pts_us: 0,
                dts_us: 0,
                duration_us: 33_333,
                key: true,
                data: vec![0, 0, 0, 1, 0x67],
            }))
            .unwrap();
        // The presenter requires decoded-output readiness (milestone 4) before
        // ACTIVATE_TRACK; the plain flow waits for it first.
        let satisfied = session
            .wait_track(
                &video_track,
                vivid_sdk::TrackWaitCondition::MilestoneSet,
                Some(SLOT_MILESTONE),
                30_000_000,
            )
            .unwrap();
        assert_eq!(satisfied.observed_value, Some(SLOT_MILESTONE));
        let mut slots = SurfaceSlots::new(&surface);
        slots
            .require(1, &video_track, generation, SLOT_MILESTONE)
            .unwrap();
        slots.activate(&mut session).unwrap();
    }

    #[test]
    fn attached_presenter_stops_without_consuming_the_desktop_capture() {
        let _env_guard = crate::cli::tests::TEST_ENV_LOCK.lock().unwrap();
        let presenter = vivid_sdk::testing::TestPresenter::start(80, 24).unwrap();
        let latest = Arc::new(super::super::video::LatestFrame::new());
        latest.replace(super::super::video::RawFrame {
            format: super::super::video::RawPixelFormat::Bgrx,
            width: 64,
            height: 64,
            pts_us: 0,
            data: vec![0_u8; 64 * 64 * 4].into(),
        });
        let mut config = config();
        config.no_audio = true;
        config.fps = 5;
        let params = AttachParams {
            replace: false,
            vivid: Zeroizing::new(super::super::control::VividCredentials {
                endpoint_control: Zeroizing::new(presenter.endpoint().to_owned()),
                endpoint_interactive: Some(Zeroizing::new(presenter.endpoint().to_owned())),
                endpoint_realtime: Some(Zeroizing::new(presenter.endpoint().to_owned())),
                endpoint_bulk: Some(Zeroizing::new(presenter.endpoint().to_owned())),
                root_secret: Zeroizing::new(vivid_sdk::testing::ROOT_SECRET_HEX.to_owned()),
            }),
            desktop_target: false,
            bitrate: 1_000_000,
            fps: 5,
            secure_input: false,
        };
        let (input, _requests) = mpsc::sync_channel(8);
        let spawn = spawn_presenter(
            config,
            params,
            PresenterSource {
                latest: latest.clone(),
                pulse: None,
                dimensions: (64, 64),
                compositor: ResolvedCompositor::Weston,
                origin: Instant::now(),
            },
            input,
        )
        .unwrap();
        let ready = spawn
            .ready
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(ready["width"], 64);
        assert!(
            ready["marker"]
                .as_str()
                .is_some_and(|marker| !marker.is_empty())
        );
        assert!(lock(&spawn.state).get("marker").is_none());
        let detach_started = Instant::now();
        spawn.running.store(false, Ordering::Release);
        assert!(
            spawn
                .done
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .is_ok()
        );
        spawn.join.join().unwrap();
        assert!(detach_started.elapsed() < Duration::from_secs(2));
        assert!(latest.snapshot().unwrap().is_some());
    }

    #[test]
    fn desktop_target_hint_suggests_the_presenter_flag() {
        let mut desktop = config();
        desktop.desktop_target = true;
        let error = io::Error::other(
            "Vivid presenter rejected request 1 with error 3: this window presents \
             a different target profile",
        );
        let hinted = desktop_target_hint(&desktop, error);
        assert!(hinted.to_string().contains("--desktop-target"));
    }

    #[test]
    fn desktop_target_hint_leaves_unrelated_errors_untouched() {
        let mut desktop = config();
        desktop.desktop_target = true;
        let error = io::Error::other("unrelated failure");
        let hinted = desktop_target_hint(&desktop, error);
        assert!(!hinted.to_string().contains("--desktop-target"));
        assert!(hinted.to_string().contains("unrelated failure"));
    }

    #[test]
    fn desktop_target_requires_desktop_input_and_the_desktop_profile() {
        let mut desktop = config();
        desktop.desktop_target = true;
        let producer = producer_config(&desktop, ResolvedCompositor::Weston);
        assert_eq!(producer.target_profile, DESKTOP_SURFACE);
        assert!(
            producer
                .required_profiles
                .contains(&DESKTOP_INPUT.to_owned())
        );
        assert!(
            producer
                .required_profiles
                .contains(&DESKTOP_SURFACE.to_owned())
        );
        assert!(producer.required_profiles.contains(&LIVE_MEDIA.to_owned()));
        assert!(
            producer
                .required_profiles
                .contains(&CORE_CONTROL.to_owned())
        );
    }

    #[test]
    fn terminal_target_never_requires_desktop_input() {
        let producer = producer_config(&config(), ResolvedCompositor::Weston);
        assert_eq!(producer.target_profile, TERMINAL_SURFACE);
        assert!(
            !producer
                .required_profiles
                .contains(&DESKTOP_INPUT.to_owned())
        );
        assert!(
            producer
                .required_profiles
                .contains(&TERMINAL_SURFACE.to_owned())
        );
    }

    #[test]
    fn track_claims_are_clamped_to_the_negotiated_contract() {
        use vivid_protocol::resource::{Resource, ResourceContract};
        let mut contract = ResourceContract::new([1_000_000_000; 33]);
        contract.set(Resource::EncodedBitsPerSecond, 4_000_000);
        contract.set(Resource::MediaRecordBody, 65536);
        let mut cfg = TrackConfiguration {
            context_id: 1,
            surface_id: 1,
            track_id: 1,
            slot: 1,
            mode: TrackMode::Live,
            lane: vivid_sdk::LaneClass::Bulk,
            maximum_record_body: 4_194_304,
            maximum_rate_millihertz: 30_000,
            maximum_encoded_bits_per_second: 8_000_000,
            maximum_records_per_second: 30,
            maximum_inflight_body_bytes: 1 << 20,
            kind: KindConfiguration::Video(VideoConfiguration {
                codec: "h264".into(),
                packetization: "h264-annexb-au-v1".into(),
                extradata: vec![],
                coded_width: 1920,
                coded_height: 1080,
                profile: 0,
                level: 0,
                maximum_reorder_depth: 0,
                color_primaries: 1,
                transfer: 1,
                matrix: 1,
                signal_range: 1,
                aspect_numerator: 1,
                aspect_denominator: 1,
                maximum_access_unit_bytes: 4096,
                codec_string: None,
                decoder_configuration: None,
            }),
            target_latency_us: 33_000,
            maximum_latency_us: 100_000,
            retained_pixel_charge: 2_073_600,
        };
        bound_track_claims(&mut cfg, &contract).unwrap();
        assert_eq!(cfg.maximum_encoded_bits_per_second, 4_000_000);
        assert_eq!(cfg.maximum_record_body, 65536);
        assert_eq!(cfg.maximum_inflight_body_bytes, 1 << 20);

        let mut denied = ResourceContract::new([1_000_000_000; 33]);
        denied.set(Resource::EncodedBitsPerSecond, 0);
        assert!(bound_track_claims(&mut cfg, &denied).is_err());
    }

    #[test]
    fn configured_dimensions_default_to_full_hd_and_stay_even() {
        let base = config();
        assert_eq!(configured_dimensions(&base).unwrap(), (1920, 1080));
        let mut odd = base;
        odd.width = Some(1281);
        odd.height = Some(721);
        assert_eq!(configured_dimensions(&odd).unwrap(), (1280, 720));
    }
}
