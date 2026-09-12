//! The compositor seam: one enum, three backends.
//!
//! Everything above this module — session connect, surfaces, track math, workers, prebuffer,
//! recovery, the status row — is shared. Everything below it is genuinely per-compositor: how the
//! compositor is launched and proven ready, how a program is launched inside it (Weston and
//! Hyprland spawn directly, Sway execs through its IPC), how frames are captured, and how input is
//! injected.
//!
//! The wlroots protocol surface is shared rather than duplicated: Sway and Hyprland both capture
//! through `capture` and inject through `wlr_input`, and only their launch, readiness and window
//! IPC differ.
//!
//! Enum dispatch rather than `Box<dyn CompositorSession>`: the sessions own different state,
//! the pipeline calls the same handful of methods, and the enum keeps each session's
//! `Drop`-kills-its-children behavior without object-safety contortions (plan D3).

pub mod capture;
pub mod hyprland;
pub mod pipewire;
pub mod protocols;
pub mod sway;
pub mod weston;
pub mod weston_input;
pub mod wlr_input;

use std::ffi::{OsStr, OsString};
use std::io;
use std::process::ExitStatus;
use std::time::Instant;

use crate::producer::{ProductIdentity, TerminalInjector};

use crate::cli::Config;
use crate::linux::app::AppLaunch;
use crate::linux::video::CaptureSource;

use capture::ScreencopyCapture;
use hyprland::HyprlandSession;
use pipewire::{PIPEWIRE_NODE, VideoCapture};
use sway::SwaySession;
use weston::{ActiveBackend, WestonSession};

pub use crate::cli::CompositorChoice;

/// A resolved compositor choice: what the rest of the run is actually driving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedCompositor {
    Weston,
    Sway,
    Hyprland,
}

impl ResolvedCompositor {
    /// The runtime product identity for this compositor (plan D2).
    ///
    /// The slug is always `vvland` — it names Pulse sinks, threads, and the private runtime
    /// directory. Only `compositor_name` varies, which is exactly what it exists for.
    pub fn identity(self) -> ProductIdentity {
        ProductIdentity {
            slug: "vvland",
            display_name: "Vvland",
            compositor_name: self.display_name(),
        }
    }

    /// The compositor's own name, as diagnostics and the status row spell it.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Weston => "Weston",
            Self::Sway => "Sway",
            Self::Hyprland => "Hyprland",
        }
    }

    /// The lowercase name used by the CLI, the session registry, and the control protocol.
    pub fn name(self) -> &'static str {
        match self {
            Self::Weston => "weston",
            Self::Sway => "sway",
            Self::Hyprland => "hyprland",
        }
    }

    /// The wire-visible producer name declared in HELLO.
    ///
    /// Weston and Sway keep their per-compositor names: those ride the wire, appear in WELCOME
    /// echoes, traces, and session labels, and the `veston`/`vvsway` wrappers exist so old
    /// invocations keep old behavior. Hyprland arrived after the consolidation and has no
    /// deprecated wrapper to be compatible with, so it announces the binary's own name
    /// (plan D2, risk R1).
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Weston => "veston",
            Self::Sway => "vvsway",
            Self::Hyprland => "vvland",
        }
    }
}

/// A window observed through a compositor's IPC, in the shape the control protocol publishes.
///
/// Sway's tree and Hyprland's client list carry the same facts under different names; normalizing
/// here is what lets `list_windows` and `wait_window` answer identically on both.
#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct CompositorWindow {
    pub id: u64,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub xwayland_class: Option<String>,
    pub pid: Option<u32>,
    pub rect: WindowRect,
    pub focused: bool,
    pub fullscreen: bool,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct WindowRect {
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
}

/// A live window-observation IPC endpoint, owned by the session that created it.
///
/// Carried by value so the control methods can query from a worker thread without holding the
/// host: both sockets stay valid for the life of the compositor process.
#[derive(Clone, Debug)]
pub enum WindowIpc {
    Sway(std::path::PathBuf),
    Hyprland(std::path::PathBuf),
}

impl WindowIpc {
    pub fn query(&self) -> io::Result<Vec<CompositorWindow>> {
        match self {
            Self::Sway(socket) => sway::query_windows(socket),
            Self::Hyprland(socket) => hyprland::query_windows(socket),
        }
    }

    pub fn compositor(&self) -> &'static str {
        match self {
            Self::Sway(_) => "sway",
            Self::Hyprland(_) => "hyprland",
        }
    }
}

/// The single application a session is dedicated to, in `--app` mode.
///
/// The compositor is configured for it up front — Sway gets a `for_window` rule, Weston drops its
/// shell panel — so the window owns the whole output from its first frame rather than being
/// resized after it appears (plan D4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppWindow {
    pub app_id: &'static str,
    pub fullscreen: bool,
}

/// What a compositor needs from the surrounding session to start: the capture geometry, the
/// private Pulse routing its clients should inherit, and the single-app window rule if any.
pub struct CompositorEnvironment<'a> {
    pub width: u32,
    pub height: u32,
    pub pulse_server: Option<&'a OsStr>,
    pub pulse_sink: Option<&'a OsStr>,
    pub app_window: Option<AppWindow>,
}

/// The injector surface the pipeline sees.
///
/// Enum rather than `&mut dyn`: the shared translation layers take `&mut impl TerminalInjector`,
/// which is `Sized`, and the two transports are known statically anyway (plan D3).
pub enum LiveInput<'a> {
    Weston(&'a mut weston_input::InputChannel),
    Wlr(&'a mut wlr_input::InputChannel),
}

impl LiveInput<'_> {
    /// Fail if the injection transport died — the Weston module's channel, or Sway's globals.
    pub fn check_status(&mut self) -> io::Result<()> {
        match self {
            Self::Weston(input) => input.check_status(),
            Self::Wlr(input) => input.check_status(),
        }
    }
}

/// Reject an absolute pointer position outside the captured output.
///
/// Both transports call this rather than each carrying its own copy, because they used to
/// disagree: Weston rejected and Sway clamped, so the same out-of-range click was an error on one
/// compositor and a plausible-looking wrong click on the other. Rejecting is the honest answer —
/// a clamp is indistinguishable from success to the caller — and sharing the predicate is what
/// stops the two from drifting apart again.
///
/// The message carries the position and the extent because a caller that got this wrong needs to
/// know what the bounds actually were.
pub fn check_pointer_bounds(
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    compositor: &str,
) -> io::Result<()> {
    if x >= width || y >= height {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "absolute pointer position ({x}, {y}) is outside the {compositor} output \
                 ({width}x{height})"
            ),
        ));
    }
    Ok(())
}

impl TerminalInjector for LiveInput<'_> {
    fn key(&mut self, code: u32, pressed: bool) -> io::Result<()> {
        match self {
            Self::Weston(input) => input.key(code, pressed),
            Self::Wlr(input) => input.key(code, pressed),
        }
    }

    fn pointer_absolute(&mut self, x: u32, y: u32) -> io::Result<()> {
        match self {
            Self::Weston(input) => input.pointer_absolute(x, y),
            Self::Wlr(input) => input.pointer_absolute(x, y),
        }
    }

    fn pointer_button(&mut self, button: u32, pressed: bool) -> io::Result<()> {
        match self {
            Self::Weston(input) => input.pointer_button(button, pressed),
            Self::Wlr(input) => input.pointer_button(button, pressed),
        }
    }

    fn pointer_axis(&mut self, axis: u32, delta: i32) -> io::Result<()> {
        match self {
            Self::Weston(input) => input.pointer_axis(axis, delta),
            Self::Wlr(input) => input.pointer_axis(axis, delta),
        }
    }

    fn release_all(&mut self) -> io::Result<()> {
        match self {
            Self::Weston(input) => input.release_all(),
            Self::Wlr(input) => input.release_all(),
        }
    }
}

pub enum Compositor {
    Weston(WestonSession),
    Sway(SwaySession),
    Hyprland(HyprlandSession),
}

impl Compositor {
    pub fn pid(&self) -> u32 {
        match self {
            Self::Weston(session) => session.pid(),
            Self::Sway(session) => session.pid(),
            Self::Hyprland(session) => session.pid(),
        }
    }

    pub fn start(
        compositor: ResolvedCompositor,
        config: &Config,
        environment: CompositorEnvironment<'_>,
    ) -> io::Result<Self> {
        match compositor {
            ResolvedCompositor::Weston => {
                WestonSession::start(config, environment).map(Self::Weston)
            }
            ResolvedCompositor::Sway => SwaySession::start(config, environment).map(Self::Sway),
            ResolvedCompositor::Hyprland => {
                HyprlandSession::start(config, environment).map(Self::Hyprland)
            }
        }
    }

    /// Start this compositor's capture backend at the negotiated size.
    pub fn start_capture(
        &self,
        width: u32,
        height: u32,
        fps: u32,
        origin: Instant,
    ) -> io::Result<Box<dyn CaptureSource + Send + Sync>> {
        match self {
            Self::Weston(session) => {
                VideoCapture::start(PIPEWIRE_NODE, session.pid(), width, height, fps, origin)
                    .map(|capture| Box::new(capture) as Box<dyn CaptureSource + Send + Sync>)
            }
            Self::Sway(session) => ScreencopyCapture::start(
                session.wayland_socket(),
                "Sway",
                width,
                height,
                fps,
                origin,
            )
            .map(|capture| Box::new(capture) as Box<dyn CaptureSource + Send + Sync>),
            Self::Hyprland(session) => ScreencopyCapture::start(
                session.wayland_socket(),
                "Hyprland",
                width,
                height,
                fps,
                origin,
            )
            .map(|capture| Box::new(capture) as Box<dyn CaptureSource + Send + Sync>),
        }
    }

    /// The active backend name for the status row: Weston's `drm`/`headless`, and the `headless`
    /// output the wlroots-protocol compositors always render to.
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Weston(session) => session.backend().name(),
            Self::Sway(session) => session.backend_name(),
            Self::Hyprland(session) => session.backend_name(),
        }
    }

    /// Weston only: whether the DRM leg is live, which drives the capture-fallback restart.
    pub fn weston_backend(&self) -> Option<ActiveBackend> {
        match self {
            Self::Weston(session) => Some(session.backend()),
            Self::Sway(_) | Self::Hyprland(_) => None,
        }
    }

    pub fn input_mut(&mut self) -> LiveInput<'_> {
        match self {
            Self::Weston(session) => LiveInput::Weston(session.input_mut()),
            Self::Sway(session) => LiveInput::Wlr(session.input_mut()),
            Self::Hyprland(session) => LiveInput::Wlr(session.input_mut()),
        }
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self {
            Self::Weston(session) => session.try_wait(),
            Self::Sway(session) => session.try_wait(),
            Self::Hyprland(session) => session.try_wait(),
        }
    }

    pub fn launch_program(&mut self, program: &[OsString]) -> io::Result<()> {
        match self {
            Self::Weston(session) => session.launch_program(program),
            Self::Sway(session) => session.launch_program(program),
            Self::Hyprland(session) => session.launch_program(program),
        }
    }

    /// Launch the single application of `--app` mode with its profile environment.
    ///
    /// Weston and Hyprland spawn it directly, so the profile environment goes on the `Command`
    /// and the child gets the kitweb liveness probe. Sway execs through its IPC, where there is no
    /// child handle to probe: the environment is baked into the launcher script and a recorded
    /// exit status stands in for the probe. All three then leave the window alone — it was already
    /// made fullscreen by the generated compositor configuration.
    pub fn launch_app(&mut self, launch: &AppLaunch) -> io::Result<()> {
        match self {
            Self::Weston(session) => session.launch_app(launch),
            Self::Sway(session) => session.launch_app(launch),
            Self::Hyprland(session) => session.launch_app(launch),
        }
    }

    pub fn launch_shell_command(&mut self, command_text: &str) -> io::Result<()> {
        match self {
            Self::Weston(session) => session.launch_shell_command(command_text),
            Self::Sway(session) => session.launch_shell_command(command_text),
            Self::Hyprland(session) => session.launch_shell_command(command_text),
        }
    }

    /// The window-observation endpoint, for the compositors that expose one.
    pub fn window_ipc(&self) -> Option<WindowIpc> {
        match self {
            // Weston exposes no window-enumeration IPC at all.
            Self::Weston(_) => None,
            Self::Sway(session) => Some(WindowIpc::Sway(session.ipc_socket().to_owned())),
            Self::Hyprland(session) => Some(WindowIpc::Hyprland(session.ipc_socket().to_owned())),
        }
    }
}

/// Probe the host and pick a compositor (plan D5).
///
/// Deterministic and evaluated exactly once per run: `producer_name` is declared in HELLO before
/// anything else happens, so the answer must not change mid-session.
///
/// `preferred` is the app profile's preference in `--app` mode. It only reorders the probe: a
/// preference that is not usable on this host still falls through to the normal order, and an
/// explicit `--compositor` or a Weston-only flag always wins over it.
pub fn resolve(
    choice: CompositorChoice,
    config: &Config,
    preferred: Option<CompositorChoice>,
) -> io::Result<ResolvedCompositor> {
    match choice {
        CompositorChoice::Weston => Ok(ResolvedCompositor::Weston),
        CompositorChoice::Sway => Ok(ResolvedCompositor::Sway),
        CompositorChoice::Hyprland => Ok(ResolvedCompositor::Hyprland),
        CompositorChoice::Auto => {
            // A Weston-only flag is an explicit request for the DRM-capable backend.
            if weston_flags_requested(config) {
                return Ok(ResolvedCompositor::Weston);
            }
            // App mode prefers Sway where the profile asks for it: it can dedicate the output to
            // one window over IPC and draws no panel.
            if preferred == Some(CompositorChoice::Sway) && probe_sway(config).is_ok() {
                return Ok(ResolvedCompositor::Sway);
            }
            if preferred == Some(CompositorChoice::Hyprland) && probe_hyprland(config).is_ok() {
                return Ok(ResolvedCompositor::Hyprland);
            }
            let weston = probe_weston(config);
            if weston.is_ok() {
                return Ok(ResolvedCompositor::Weston);
            }
            let sway = probe_sway(config);
            if sway.is_ok() {
                return Ok(ResolvedCompositor::Sway);
            }
            // Hyprland comes last under `auto`: its version probe cannot tell whether this host
            // will give aquamarine a GPU to allocate from, so a host that can run either of the
            // other two should.
            let hyprland = probe_hyprland(config);
            if hyprland.is_ok() {
                return Ok(ResolvedCompositor::Hyprland);
            }
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no usable compositor: weston ({}); sway ({}); hyprland ({})",
                    weston.unwrap_err(),
                    sway.unwrap_err(),
                    hyprland.unwrap_err()
                ),
            ))
        }
    }
}

/// Whether the invocation names a Weston-only capability.
pub fn weston_flags_requested(config: &Config) -> bool {
    config.backend == crate::cli::Backend::Drm
        || config.drm_device.is_some()
        || config.drm_output.is_some()
}

/// Weston is usable when the binary is a supported release, advertises its PipeWire backend, and
/// this build embedded the libweston input module.
pub fn probe_weston(config: &Config) -> Result<String, String> {
    if !weston::weston_input_compiled_in() {
        return Err("built without libweston input support".into());
    }
    let version = crate::linux::doctor::weston_version(&config.weston)
        .map_err(|error| format!("could not execute weston: {error}"))?;
    if !crate::linux::doctor::weston_supported(&version) {
        return Err(format!(
            "weston 13-16 is required; found {}",
            version.trim()
        ));
    }
    if !crate::linux::doctor::weston_advertises_pipewire(&config.weston) {
        return Err("weston does not advertise its PipeWire backend".into());
    }
    Ok(version)
}

/// Hyprland is usable when the binary reports a release whose configuration this build can
/// generate. Whether the host can actually give aquamarine an allocator only shows at startup.
pub fn probe_hyprland(config: &Config) -> Result<String, String> {
    let version = hyprland::hyprland_version(&config.hyprland)
        .map_err(|error| format!("could not execute Hyprland: {error}"))?;
    if hyprland::hyprland_supported(&version) {
        Ok(version)
    } else {
        Err(format!(
            "hyprland 0.53 or newer is required; found {}",
            version.trim()
        ))
    }
}

/// Sway is usable when the binary reports 1.9 or newer.
pub fn probe_sway(config: &Config) -> Result<String, String> {
    let version = sway::sway_version(&config.sway)
        .map_err(|error| format!("could not execute sway: {error}"))?;
    if sway::sway_supported(&version) {
        Ok(version)
    } else {
        Err(format!(
            "sway 1.9 or newer is required; found {}",
            version.trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_vvland_with_a_per_compositor_display() {
        let weston = ResolvedCompositor::Weston.identity();
        assert_eq!(weston.slug, "vvland");
        assert_eq!(weston.display_name, "Vvland");
        assert_eq!(weston.compositor_name, "Weston");
        assert_eq!(ResolvedCompositor::Sway.identity().compositor_name, "Sway");
        assert_eq!(ResolvedCompositor::Sway.identity().slug, "vvland");
        let hyprland = ResolvedCompositor::Hyprland.identity();
        assert_eq!(hyprland.slug, "vvland");
        assert_eq!(hyprland.compositor_name, "Hyprland");
    }

    #[test]
    fn wire_identity_stays_per_compositor() {
        // Wire-visible in HELLO/WELCOME and in operator traces: consolidating the binary must not
        // silently rename the producer (plan D2, risk R1).
        assert_eq!(ResolvedCompositor::Weston.wire_name(), "veston");
        assert_eq!(ResolvedCompositor::Sway.wire_name(), "vvsway");
        // Hyprland has no deprecated wrapper to stay compatible with, so it announces vvland.
        assert_eq!(ResolvedCompositor::Hyprland.wire_name(), "vvland");
        for compositor in [
            ResolvedCompositor::Weston,
            ResolvedCompositor::Sway,
            ResolvedCompositor::Hyprland,
        ] {
            assert_eq!(compositor.name(), compositor.display_name().to_lowercase());
        }
    }

    #[test]
    fn every_compositor_rejects_the_same_out_of_range_pointer_positions() {
        // The parity that matters: the last valid pixel is accepted and the first invalid one is
        // rejected, identically for every transport, because they all run this one predicate. Sway
        // used to clamp here, which made an out-of-range click look like a successful click on a
        // neighbouring pixel.
        for compositor in ["Weston", "Sway", "Hyprland"] {
            assert!(check_pointer_bounds(1919, 1079, 1920, 1080, compositor).is_ok());
            assert!(check_pointer_bounds(0, 0, 1920, 1080, compositor).is_ok());
            for (x, y) in [(1920, 0), (0, 1080), (1920, 1080), (u32::MAX, 0)] {
                let error = check_pointer_bounds(x, y, 1920, 1080, compositor).unwrap_err();
                assert_eq!(
                    error.kind(),
                    io::ErrorKind::InvalidInput,
                    "{compositor} ({x}, {y})"
                );
                let message = error.to_string();
                assert!(message.contains(compositor), "{message}");
                assert!(message.contains("1920x1080"), "{message}");
                assert!(message.contains(&format!("({x}, {y})")), "{message}");
            }
        }
    }

    #[test]
    fn a_zero_sized_output_admits_no_pointer_position() {
        // A degenerate extent must reject rather than underflow: the old Sway path computed
        // `width.saturating_sub(1)`, which mapped every position onto pixel 0.
        assert!(check_pointer_bounds(0, 0, 0, 0, "Sway").is_err());
        assert!(check_pointer_bounds(0, 0, 1920, 0, "Weston").is_err());
    }

    #[test]
    fn explicit_choices_never_probe() {
        let config = crate::cli::tests::parse(["vvland", "--doctor"]);
        assert_eq!(
            resolve(CompositorChoice::Weston, &config, None).unwrap(),
            ResolvedCompositor::Weston
        );
        assert_eq!(
            resolve(CompositorChoice::Sway, &config, None).unwrap(),
            ResolvedCompositor::Sway
        );
        assert_eq!(
            resolve(CompositorChoice::Hyprland, &config, None).unwrap(),
            ResolvedCompositor::Hyprland
        );
    }

    #[test]
    fn drm_flags_force_weston_under_auto() {
        let plain = crate::cli::tests::parse(["vvland", "--doctor"]);
        assert!(!weston_flags_requested(&plain));

        for flags in [
            vec!["vvland", "--doctor", "--backend=drm"],
            vec!["vvland", "--doctor", "--drm-device=/dev/dri/card0"],
            vec!["vvland", "--doctor", "--drm-output=HDMI-A-1"],
        ] {
            let config = crate::cli::tests::parse(flags.clone());
            assert!(weston_flags_requested(&config), "{flags:?}");
            assert_eq!(
                resolve(CompositorChoice::Auto, &config, None).unwrap(),
                ResolvedCompositor::Weston,
                "{flags:?}"
            );
        }
    }
}
