//! Provisioning and ownership for one isolated desktop.
//!
//! This module is the boundary between creating the desktop (compositor, capture, private audio
//! sink, and input keymap) and driving it.  In particular, it has no presenter/session concerns:
//! the legacy streaming path can connect first when terminal geometry requires it, while the
//! future headless server can provision directly from configured geometry.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Instant;

use crate::producer::TerminalInjector;
use crate::producer::audio::{PulseSink, resolve_pulse_server};
use crate::producer::keysynth::{KeyStroke, KeySynth};

use crate::cli::{Backend, Config};

use super::app::{self, AppLaunch};
use super::compositor::weston::{ActiveBackend, drm_native_mode};
use super::compositor::{
    AppWindow, Compositor, CompositorEnvironment, LiveInput, ResolvedCompositor,
    resolve as resolve_compositor,
};
use super::video::{CaptureSource, LatestFrame};

const DEFAULT_DESKTOP_WIDTH: u32 = 1920;
const DEFAULT_DESKTOP_HEIGHT: u32 = 1080;

/// How the capture geometry for a desktop is selected.
// `Configured` and the one-shot entry point become reachable when `serve` lands in H5. Keeping
// them here is the point of cutting the provisioning seam in H3.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedSize {
    /// Use `--width`/`--height`, or the default desktop size when neither was supplied.
    Configured,
    /// Use geometry negotiated by the caller (the terminal streaming path).
    Exact(u32, u32),
}

/// The resolution-only half of provisioning.
///
/// The streaming path needs the resolved compositor identity in its Vivid handshake before it can
/// learn terminal geometry.  Keeping that answer and the prepared app launch together lets the
/// subsequent provisioning step use the exact same resolution, rather than probing twice.
pub struct DesktopPlan {
    resolved: ResolvedCompositor,
    app_launch: Option<AppLaunch>,
    app_window: Option<AppWindow>,
}

impl DesktopPlan {
    pub fn resolve(config: &Config) -> io::Result<Self> {
        // Single-app mode resolves first so the profile can steer the compositor probe and its
        // window rule can be baked into the generated compositor configuration.
        let app = config.app.as_deref().map(app::profile).transpose()?;
        let resolved = resolve_compositor(
            config.compositor,
            config,
            app.map(|profile| profile.compositor),
        )?;
        let app_launch = app
            .map(|profile| profile.launch(&config.program))
            .transpose()?;
        let app_window = app_launch.as_ref().map(|launch| AppWindow {
            app_id: launch.app_id,
            fullscreen: launch.fullscreen,
        });
        if resolved == ResolvedCompositor::Weston {
            // Only Weston captures through PipeWire. Keep this in the provisioning seam so the
            // one-shot headless path gets the same prerequisite check as legacy streaming.
            super::doctor::require_pipewire_server()?;
        }
        Ok(Self {
            resolved,
            app_launch,
            app_window,
        })
    }

    pub fn resolved(&self) -> ResolvedCompositor {
        self.resolved
    }

    pub fn provision(self, config: &Config, size: RequestedSize) -> io::Result<DesktopHost> {
        DesktopHost::provision_plan(config, size, self)
    }
}

/// Everything that provisions and owns an isolated desktop.
///
/// It contains no presenter session, encoder, or terminal state.  The current streaming pipeline
/// eventually consumes it into [`StreamingDesktopParts`]; the headless actor will instead keep it
/// intact for the daemon lifetime.
#[allow(dead_code)]
pub struct DesktopHost {
    compositor: Compositor,
    capture: Box<dyn CaptureSource + Send + Sync>,
    latest: Arc<LatestFrame>,
    pulse: Option<PulseSink>,
    dimensions: (u32, u32),
    resolved: ResolvedCompositor,
    origin: Instant,
    started_at: Instant,
    keys: KeySynth,
    last_pointer: Option<(u32, u32)>,
    pressed_keys: HashSet<u32>,
    pressed_buttons: HashSet<u32>,
    launches: Vec<LaunchRecord>,
    initial_app: Option<AppLaunch>,
}

/// One successfully acknowledged program launch.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LaunchRecord {
    pub program: Vec<OsString>,
    pub started_at: Instant,
}

/// Legacy streaming ownership after provisioning is complete.
pub struct StreamingDesktopParts {
    pub compositor: Compositor,
    pub capture: Box<dyn CaptureSource + Send + Sync>,
    pub pulse: Option<PulseSink>,
    pub dimensions: (u32, u32),
    pub resolved: ResolvedCompositor,
    pub origin: Instant,
}

#[allow(dead_code)]
impl DesktopHost {
    /// Resolve and provision a desktop in one call.
    ///
    /// This is the headless-server entry point.  The legacy terminal streaming path uses
    /// [`DesktopPlan`] because its exact size is learned between resolution and provisioning.
    pub fn provision(config: &Config, size: RequestedSize) -> io::Result<Self> {
        DesktopPlan::resolve(config)?.provision(config, size)
    }

    fn provision_plan(config: &Config, size: RequestedSize, plan: DesktopPlan) -> io::Result<Self> {
        let headless_size = match size {
            RequestedSize::Configured => configured_dimensions(config)?,
            RequestedSize::Exact(width, height) => (width, height),
        };
        let (mut launch_config, initial_size) = initial_size(config, plan.resolved, headless_size)?;
        let started_at = Instant::now();
        let origin = started_at;
        let product = plan.resolved.identity();

        let pulse = if config.no_audio {
            None
        } else {
            match resolve_pulse_server(config.audio_capture_server.as_deref())
                .and_then(|server| PulseSink::create(&server, &product))
            {
                Ok(sink) => Some(sink),
                Err(error) if config.require_audio => return Err(error),
                Err(error) => {
                    eprintln!("vvland: audio unavailable ({error}); continuing video-only");
                    None
                }
            }
        };

        let mut dimensions = initial_size;
        let mut compositor = Compositor::start(
            plan.resolved,
            &launch_config,
            compositor_environment(dimensions, pulse.as_ref(), plan.app_window),
        )?;
        // Weston's DRM sizing dance moves as one unit: if startup selected headless after the
        // optimistic native-mode size, restart at the requested headless size.  Sway has no DRM
        // leg and cannot take this path.
        if compositor.weston_backend() == Some(ActiveBackend::Headless)
            && dimensions != headless_size
        {
            drop(compositor);
            launch_config.backend = Backend::Headless;
            dimensions = headless_size;
            compositor = Compositor::start(
                plan.resolved,
                &launch_config,
                compositor_environment(dimensions, pulse.as_ref(), plan.app_window),
            )?;
        }

        let capture = match compositor.start_capture(dimensions.0, dimensions.1, config.fps, origin)
        {
            Ok(capture) => capture,
            // Weston only: a DRM session whose mirrored PipeWire output never appeared is
            // recoverable by restarting headless, but only when the backend was not pinned.
            Err(_)
                if config.backend == Backend::Auto
                    && compositor.weston_backend() == Some(ActiveBackend::Drm) =>
            {
                drop(compositor);
                launch_config.backend = Backend::Headless;
                dimensions = headless_size;
                compositor = Compositor::start(
                    plan.resolved,
                    &launch_config,
                    compositor_environment(dimensions, pulse.as_ref(), plan.app_window),
                )?;
                compositor.start_capture(dimensions.0, dimensions.1, config.fps, origin)?
            }
            Err(error) => return Err(error),
        };
        let latest = capture.latest();
        let keys = KeySynth::new(
            config.xkb_model.as_deref(),
            &config.xkb_layout,
            config.xkb_variant.as_deref(),
            config.xkb_options.clone(),
        )?;

        Ok(Self {
            compositor,
            capture,
            latest,
            pulse,
            dimensions,
            resolved: plan.resolved,
            origin,
            started_at,
            keys,
            last_pointer: None,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
            launches: Vec::new(),
            initial_app: plan.app_launch,
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        self.dimensions
    }

    pub fn resolved(&self) -> ResolvedCompositor {
        self.resolved
    }

    pub fn origin(&self) -> Instant {
        self.origin
    }

    pub fn latest(&self) -> &Arc<LatestFrame> {
        &self.latest
    }

    pub fn pulse(&self) -> Option<&PulseSink> {
        self.pulse.as_ref()
    }

    pub fn input(&mut self) -> LiveInput<'_> {
        self.compositor.input_mut()
    }

    pub fn liveness(&mut self) -> io::Result<Option<ExitStatus>> {
        self.compositor.try_wait()
    }

    pub fn compositor_pid(&self) -> u32 {
        self.compositor.pid()
    }

    pub fn backend_name(&self) -> &'static str {
        self.compositor.backend_name()
    }

    pub fn uptime(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    pub fn launches(&self) -> usize {
        self.launches.len()
    }

    pub fn last_pointer(&self) -> Option<(u32, u32)> {
        self.last_pointer
    }

    pub fn pressed_counts(&self) -> (usize, usize) {
        (self.pressed_keys.len(), self.pressed_buttons.len())
    }

    pub fn input_status(&mut self) -> io::Result<()> {
        self.compositor.input_mut().check_status()
    }

    pub fn resolve_key(&self, key: &str, modifiers: &[String]) -> io::Result<KeyStroke> {
        let stroke = self.keys.resolve(key).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("key {key:?} is not in the keymap"),
            )
        })?;
        let mut codes = Vec::with_capacity(modifiers.len());
        for modifier in modifiers {
            codes.push(KeySynth::modifier(modifier).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown modifier {modifier:?}"),
                )
            })?);
        }
        Ok(stroke.with_modifiers(codes))
    }

    pub fn tap_key(&mut self, stroke: &KeyStroke) -> io::Result<()> {
        let keys = &self.keys;
        let mut input = self.compositor.input_mut();
        input.check_status()?;
        keys.tap(&mut input, stroke)
    }

    pub fn press_key(&mut self, stroke: &KeyStroke) -> io::Result<()> {
        let keys = &self.keys;
        let mut input = self.compositor.input_mut();
        input.check_status()?;
        keys.press(&mut input, stroke)?;
        self.pressed_keys.insert(stroke.code);
        self.pressed_keys.extend(stroke.modifiers.iter().copied());
        Ok(())
    }

    pub fn release_key(&mut self, stroke: &KeyStroke) -> io::Result<()> {
        let keys = &self.keys;
        let mut input = self.compositor.input_mut();
        input.check_status()?;
        keys.release(&mut input, stroke)?;
        self.pressed_keys.remove(&stroke.code);
        for modifier in &stroke.modifiers {
            self.pressed_keys.remove(modifier);
        }
        Ok(())
    }

    pub fn type_text(&mut self, text: &str) -> io::Result<()> {
        // Planning happens inside `type_text` before its first event, preserving all-or-nothing
        // behavior for unmappable characters.
        let keys = &self.keys;
        let mut input = self.compositor.input_mut();
        input.check_status()?;
        keys.type_text(&mut input, text)
    }

    pub fn pointer_move(&mut self, x: u32, y: u32) -> io::Result<()> {
        let mut input = self.compositor.input_mut();
        input.check_status()?;
        input.pointer_absolute(x, y)?;
        self.last_pointer = Some((x, y));
        Ok(())
    }

    pub fn pointer_button(&mut self, button: u32, pressed: bool) -> io::Result<()> {
        let mut input = self.compositor.input_mut();
        input.check_status()?;
        input.pointer_button(button, pressed)?;
        if pressed {
            self.pressed_buttons.insert(button);
        } else {
            self.pressed_buttons.remove(&button);
        }
        Ok(())
    }

    pub fn pointer_axis(&mut self, axis: u32, delta: i32) -> io::Result<()> {
        let mut input = self.compositor.input_mut();
        input.check_status()?;
        input.pointer_axis(axis, delta)
    }

    pub fn launch(&mut self, program: &[OsString], shell: bool) -> io::Result<()> {
        if shell {
            let command = program
                .first()
                .and_then(|value| value.to_str())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid shell command")
                })?;
            self.compositor.launch_shell_command(command)?;
        } else {
            self.compositor.launch_program(program)?;
        }
        self.launches.push(LaunchRecord {
            program: program.to_vec(),
            started_at: Instant::now(),
        });
        Ok(())
    }

    pub fn audio_sink_name(&self) -> Option<&OsStr> {
        self.pulse.as_ref().map(PulseSink::sink_name)
    }

    pub fn audio_enabled(&self) -> bool {
        self.pulse.is_some()
    }

    pub fn sway_ipc_socket(&self) -> Option<std::path::PathBuf> {
        self.compositor.sway_ipc_socket().map(ToOwned::to_owned)
    }

    /// Launch the program selected by `--app`, or the trailing program for a normal desktop.
    ///
    /// This remains a separate step from provisioning to preserve the streaming path's existing
    /// ordering: track negotiation completes before the initial program is started.
    pub fn launch_initial(&mut self, program: &[OsString]) -> io::Result<()> {
        let launched_program = match self.initial_app.take() {
            Some(launch) => {
                if launch.snap_confined {
                    eprintln!(
                        "vvland: {} {}",
                        launch.binary.display(),
                        app::SNAP_WAYLAND_GAP
                    );
                }
                self.compositor.launch_app(&launch)?;
                launch.program
            }
            None => {
                self.compositor.launch_program(program)?;
                if program.is_empty() {
                    return Ok(());
                }
                program.to_vec()
            }
        };
        self.launches.push(LaunchRecord {
            program: launched_program,
            started_at: Instant::now(),
        });
        Ok(())
    }

    pub fn into_streaming_parts(self) -> StreamingDesktopParts {
        StreamingDesktopParts {
            compositor: self.compositor,
            capture: self.capture,
            pulse: self.pulse,
            dimensions: self.dimensions,
            resolved: self.resolved,
            origin: self.origin,
        }
    }
}

pub fn configured_dimensions(config: &Config) -> io::Result<(u32, u32)> {
    let (width, height) = match (config.width, config.height) {
        (Some(width), Some(height)) => (width, height),
        (None, None) => (DEFAULT_DESKTOP_WIDTH, DEFAULT_DESKTOP_HEIGHT),
        _ => unreachable!("CLI validation requires paired dimensions"),
    };
    let width = width & !1;
    let height = height & !1;
    if width < 64 || height < 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "streaming area must be at least 64x64",
        ));
    }
    Ok((width, height))
}

fn compositor_environment(
    dimensions: (u32, u32),
    pulse: Option<&PulseSink>,
    app_window: Option<AppWindow>,
) -> CompositorEnvironment<'_> {
    CompositorEnvironment {
        width: dimensions.0,
        height: dimensions.1,
        app_window,
        pulse_server: pulse.map(PulseSink::server),
        pulse_sink: pulse.map(PulseSink::sink_name),
    }
}

/// The size the compositor should start at.
///
/// Weston can drive a physical connector, so it starts at that connector's native mode when one
/// is available and falls back to the headless size otherwise. Sway is headless-only and always
/// uses the headless size.
fn initial_size(
    config: &Config,
    compositor: ResolvedCompositor,
    headless: (u32, u32),
) -> io::Result<(Config, (u32, u32))> {
    if compositor == ResolvedCompositor::Sway || config.backend == Backend::Headless {
        return Ok((config.clone(), headless));
    }
    let output = config.drm_output.clone().or_else(|| {
        super::compositor::weston::connected_drm_outputs(std::path::Path::new("/sys/class/drm"))
            .into_iter()
            .next()
    });
    if let Some(output) = output {
        if let Some((width, height)) =
            drm_native_mode(std::path::Path::new("/sys/class/drm"), &output)
        {
            let dimensions = (width & !1, height & !1);
            if dimensions.0 >= 64 && dimensions.1 >= 64 {
                return Ok((config.clone(), dimensions));
            }
        }
    }
    if config.backend == Backend::Drm {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the selected DRM connector has no readable native mode",
        ));
    }
    let mut headless_config = config.clone();
    headless_config.backend = Backend::Headless;
    Ok((headless_config, headless))
}

#[cfg(test)]
mod tests {
    #[test]
    fn desktop_host_has_no_presenter_sdk_dependency() {
        let source = include_str!("host.rs");
        let forbidden = ["use ", "vivid", "_sdk"].concat();
        assert!(!source.contains(&forbidden));
    }
}
