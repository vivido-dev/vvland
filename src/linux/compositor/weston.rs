use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::{Backend, Config, Renderer};
use crate::linux::app::{AppLaunch, is_unix_pulse_server};
use crate::linux::launcher::{
    RuntimeDirectory, child_output, confirm_started, pipe, sanitize_child_environment,
    set_pulse_environment, socketpair, start_bounded_log, startup_error, terminate_group,
    write_private_file, xwayland_enabled,
};

use super::CompositorEnvironment;
use super::weston_input::InputChannel;

/// The libweston input module, embedded at build time.
///
/// Absent when the host had no libweston-13..16 development files (plan D12); the Weston backend
/// then refuses to start and `--doctor` reports the gap, while `--compositor sway` is unaffected.
#[cfg(not(no_weston_input))]
const INPUT_MODULE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libveston-input.so"));

const READY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveBackend {
    Drm,
    Headless,
}

impl ActiveBackend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Drm => "drm",
            Self::Headless => "headless",
        }
    }
}

fn automatic_renderer_fallback(renderer: Renderer) -> Option<Renderer> {
    (renderer == Renderer::Auto).then_some(Renderer::Pixman)
}

pub struct WestonSession {
    runtime: RuntimeDirectory,
    child: Child,
    launched: Vec<Child>,
    input: InputChannel,
    backend: ActiveBackend,
    wayland_display: String,
    process_group: i32,
    pulse_server: Option<OsString>,
    pulse_sink: Option<OsString>,
    log_thread: Option<thread::JoinHandle<()>>,
}

impl WestonSession {
    pub fn start(config: &Config, environment: CompositorEnvironment<'_>) -> io::Result<Self> {
        // Refuse before the backend and renderer retries, so the typed refusal is not laundered
        // into a generic "failed with automatic renderer and Pixman" message (plan D12).
        weston_input_module()?;
        let connector = config.drm_output.clone().or_else(|| {
            connected_drm_outputs(Path::new("/sys/class/drm"))
                .into_iter()
                .next()
        });

        match config.backend {
            Backend::Drm => {
                let connector = connector.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "no connected DRM output")
                })?;
                Self::start_backend(config, &environment, ActiveBackend::Drm, connector)
            }
            Backend::Headless => Self::start_backend(
                config,
                &environment,
                ActiveBackend::Headless,
                "headless".into(),
            ),
            Backend::Auto => {
                if let Some(connector) = connector {
                    if let Ok(session) =
                        Self::start_backend(config, &environment, ActiveBackend::Drm, connector)
                    {
                        return Ok(session);
                    }
                }
                Self::start_backend(
                    config,
                    &environment,
                    ActiveBackend::Headless,
                    "headless".into(),
                )
            }
        }
    }

    /// PID of the owned Weston client connected to the host PipeWire daemon.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    fn start_backend(
        config: &Config,
        environment: &CompositorEnvironment<'_>,
        backend: ActiveBackend,
        output_name: String,
    ) -> io::Result<Self> {
        let first = match Self::start_once(
            config,
            environment,
            backend,
            output_name.clone(),
            config.renderer,
        ) {
            Ok(session) => return Ok(session),
            Err(error) => error,
        };
        let Some(fallback) = automatic_renderer_fallback(config.renderer) else {
            return Err(first);
        };
        Self::start_once(config, environment, backend, output_name, fallback).map_err(|second| {
            io::Error::other(format!(
                "{} Weston failed with automatic renderer ({first}) and Pixman ({second})",
                backend.name()
            ))
        })
    }

    fn start_once(
        config: &Config,
        environment: &CompositorEnvironment<'_>,
        backend: ActiveBackend,
        output_name: String,
        renderer: Renderer,
    ) -> io::Result<Self> {
        // Fail before anything is created when this build has no input module (plan D12).
        let input_module = weston_input_module()?;
        let runtime = RuntimeDirectory::create()?;
        let module_path = runtime.path.join("libveston-input.so");
        write_private_file(&module_path, input_module, 0o500)?;
        let wayland_display = "wayland-vvland".to_owned();
        let config_path = runtime.path.join("weston.ini");
        let generated = weston_config(
            backend,
            &output_name,
            environment.width,
            environment.height,
            config.fps,
            renderer,
            environment.app_window.is_some(),
            xwayland_enabled(config.xwayland),
            config.xkb_model.as_deref(),
            &config.xkb_layout,
            config.xkb_variant.as_deref(),
            config.xkb_options.as_deref(),
        );
        write_private_file(&config_path, generated.as_bytes(), 0o600)?;

        let (parent_fd, child_fd) = socketpair()?;
        let raw_child_fd = child_fd.as_raw_fd();
        let (log_read, log_write) = pipe()?;
        let log_write_clone = log_write.try_clone()?;

        let mut command = Command::new(&config.weston);
        let pipewire_runtime = std::env::var_os("PIPEWIRE_RUNTIME_DIR")
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"));
        command
            .arg(format!(
                "--backend={}",
                match backend {
                    // DRM renders the physical output while a PipeWire output mirrors it for
                    // capture. Headless has no physical output to mirror, and libweston never
                    // populates `native_mode_copy` for headless outputs, so `mirror-of` aborts
                    // Weston (`wet_output_compute_output_from_mirror` assertion). The PipeWire
                    // backend therefore renders the desktop directly and is captured.
                    ActiveBackend::Drm => "drm,pipewire",
                    ActiveBackend::Headless => "pipewire",
                }
            ))
            .arg(format!("--socket={wayland_display}"))
            .arg(format!("--config={}", config_path.display()))
            .arg(format!("--modules={}", module_path.display()))
            .arg(format!("--renderer={}", renderer.as_weston()))
            .env("VESTON_INPUT_FD", raw_child_fd.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_write_clone))
            .stderr(Stdio::from(log_write));
        set_runtime_environment(&mut command, &runtime.path, pipewire_runtime.as_deref());
        add_backend_arguments(
            &mut command,
            backend,
            environment.width,
            environment.height,
            config.drm_device.as_deref(),
        );
        if xwayland_enabled(config.xwayland) {
            command.arg("--xwayland");
        }
        // Sanitize before the explicit routing below: the superset filter strips PULSE_* and the
        // host display, and a later `env` call must win over the removal.
        sanitize_child_environment(&mut command);
        set_pulse_environment(
            &mut command,
            environment.pulse_server,
            environment.pulse_sink,
        );

        // SAFETY: only async-signal-safe libc calls are made after fork and before exec.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(raw_child_fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::setpgid(0, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command.spawn()?;
        // Command retains its configured stdio handles, so release the log-pipe writers before
        // any startup failure waits for the log reader to finish.
        drop(command);
        drop(child_fd);
        let process_group = i32::try_from(child.id())
            .map_err(|_| io::Error::other("Weston PID exceeds process-group range"))?;
        let log_path = runtime.path.join("weston.log");
        let log_thread = match start_bounded_log("vvland-weston-log", log_read, log_path.clone()) {
            Ok(thread) => thread,
            Err(error) => {
                terminate_group(process_group, &mut child);
                return Err(error);
            }
        };
        let input = InputChannel::new(parent_fd, environment.width, environment.height);

        let readiness_deadline = Instant::now() + READY_TIMEOUT;
        let ready = input.wait_ready(READY_TIMEOUT).and_then(|()| {
            wait_for_wayland_socket(
                &runtime.path.join(&wayland_display),
                &mut child,
                readiness_deadline.saturating_duration_since(Instant::now()),
            )
        });
        if let Err(error) = ready {
            terminate_group(process_group, &mut child);
            let _ = log_thread.join();
            return Err(weston_startup_error(backend, &error, &log_path));
        }

        Ok(Self {
            runtime,
            child,
            launched: Vec::new(),
            input,
            backend,
            wayland_display,
            process_group,
            pulse_server: environment.pulse_server.map(OsStr::to_owned),
            pulse_sink: environment.pulse_sink.map(OsStr::to_owned),
            log_thread: Some(log_thread),
        })
    }

    pub fn backend(&self) -> ActiveBackend {
        self.backend
    }

    pub fn input_mut(&mut self) -> &mut InputChannel {
        &mut self.input
    }

    pub fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    pub fn launch_program(&mut self, program: &[OsString]) -> io::Result<()> {
        if program.is_empty() {
            return Ok(());
        }
        let mut command = Command::new(&program[0]);
        command.args(&program[1..]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Sanitize first so the nested client's own display and Pulse routing survive.
        sanitize_child_environment(&mut command);
        set_client_environment(
            &mut command,
            &self.runtime.path,
            &self.wayland_display,
            self.pulse_server.as_deref(),
            self.pulse_sink.as_deref(),
        );
        let group = self.process_group;
        // SAFETY: setpgid is async-signal-safe and the group belongs to this session.
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, group) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        self.launched.push(command.spawn()?);
        Ok(())
    }

    pub fn launch_shell_command(&mut self, command_text: &str) -> io::Result<()> {
        let program = [
            OsString::from("/bin/sh"),
            OsString::from("-lc"),
            OsString::from(command_text),
        ];
        self.launch_program(&program)
    }

    /// Launch the single application of `--app` mode and prove it survived startup.
    pub fn launch_app(&mut self, launch: &AppLaunch) -> io::Result<()> {
        let Some(binary) = launch.program.first() else {
            return Ok(());
        };
        let mut command = Command::new(binary);
        command.args(&launch.program[1..]);
        let (stdout, stderr) = child_output();
        command.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
        sanitize_child_environment(&mut command);
        set_client_environment(
            &mut command,
            &self.runtime.path,
            &self.wayland_display,
            self.app_pulse_server(launch),
            self.pulse_sink.as_deref(),
        );
        for (name, value) in &launch.env {
            command.env(name, value);
        }
        let group = self.process_group;
        // SAFETY: setpgid is async-signal-safe and the group belongs to this session.
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, group) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let started = confirm_started(&launch.binary.to_string_lossy(), &mut child);
        self.launched.push(child);
        started
    }

    /// The Pulse server address this application should see.
    ///
    /// A snap-confined browser already runs behind snap's own Pulse mediation; a raw host unix
    /// socket can drop it to ALSA, while `PULSE_SINK` alone still selects the private sink
    /// (kitweb `browser.rs:258-275`).
    fn app_pulse_server(&self, launch: &AppLaunch) -> Option<&OsStr> {
        let server = self.pulse_server.as_deref()?;
        if launch.snap_confined && is_unix_pulse_server(server) {
            return None;
        }
        Some(server)
    }
}

fn add_backend_arguments(
    command: &mut Command,
    backend: ActiveBackend,
    width: u32,
    height: u32,
    drm_device: Option<&Path>,
) {
    match backend {
        ActiveBackend::Headless => {
            command
                .arg(format!("--width={width}"))
                .arg(format!("--height={height}"));
        }
        ActiveBackend::Drm => {
            command.arg("--continue-without-input");
            if let Some(device) = drm_device {
                let device = device.file_name().unwrap_or(device.as_os_str());
                command.arg(format!("--drm-device={}", device.to_string_lossy()));
            }
        }
    }
}

impl Drop for WestonSession {
    fn drop(&mut self) {
        let _ = self.input.shutdown();
        terminate_group(self.process_group, &mut self.child);
        if let Some(log_thread) = self.log_thread.take() {
            let _ = log_thread.join();
        }
        for child in &mut self.launched {
            let _ = child.try_wait();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn weston_config(
    backend: ActiveBackend,
    output_name: &str,
    width: u32,
    height: u32,
    fps: u32,
    renderer: Renderer,
    single_app: bool,
    xwayland: bool,
    xkb_model: Option<&str>,
    xkb_layout: &str,
    xkb_variant: Option<&str>,
    xkb_options: Option<&str>,
) -> String {
    let pipewire_mode = format!("{width}x{height}@{fps}");
    // Single-app mode drops the shell panel so the one window owns the whole output. Weston
    // validates this key and falls back to the default with a warning on any release that does
    // not know "none", so the worst case is a visible panel, never a failed start (plan D4).
    let panel_position = if single_app { "none" } else { "top" };
    let mut config = format!(
        "[core]\nidle-time=0\nrequire-input=false\nrequire-outputs=any\nrenderer={}\nxwayland={}\n\
         [shell]\nlocking=false\npanel-position={panel_position}\n\
         [keyboard]\nkeymap_rules=evdev\nkeymap_model={}\nkeymap_layout={}\n",
        renderer.as_weston(),
        xwayland,
        xkb_model.unwrap_or("pc105"),
        xkb_layout,
    );
    if let Some(variant) = xkb_variant {
        config.push_str(&format!("keymap_variant={variant}\n"));
    }
    if let Some(options) = xkb_options {
        config.push_str(&format!("keymap_options={options}\n"));
    }
    config.push_str("[pipewire]\nnum-outputs=1\n");
    match backend {
        // DRM outputs carry a native mode, so the physical output renders the desktop and a
        // mirrored PipeWire output reproduces it at the capture resolution.
        ActiveBackend::Drm => config.push_str(&format!(
            "[output]\nname={output_name}\nmode=preferred\n\
             [output]\nname=pipewire\nmode={pipewire_mode}\nmirror-of={output_name}\n",
        )),
        // Headless outputs have no native mode to mirror, so the PipeWire output is the sole
        // rendered output and is captured directly.
        ActiveBackend::Headless => {
            config.push_str(&format!("[output]\nname=pipewire\nmode={pipewire_mode}\n"))
        }
    }
    config
}

pub fn connected_drm_outputs(root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut outputs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.contains('-') {
            continue;
        }
        let status = fs::read_to_string(entry.path().join("status"));
        if status
            .as_deref()
            .is_ok_and(|value| value.trim() == "connected")
        {
            let weston_name = name.split_once('-').map_or(name.as_ref(), |(_, rest)| rest);
            outputs.push(weston_name.to_owned());
        }
    }
    outputs.sort();
    outputs
}

pub fn drm_device_for_output(
    sys_root: &Path,
    dev_root: &Path,
    output_name: &str,
) -> Option<PathBuf> {
    let mut devices = fs::read_dir(sys_root)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let (card, output) = name.split_once('-')?;
            if output != output_name
                || !card.strip_prefix("card").is_some_and(|index| {
                    !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
                })
                || !fs::read_to_string(entry.path().join("status"))
                    .is_ok_and(|value| value.trim() == "connected")
            {
                return None;
            }
            Some(dev_root.join(card))
        })
        .collect::<Vec<_>>();
    devices.sort();
    devices.into_iter().next()
}

pub fn drm_render_node(sys_root: &Path, dev_root: &Path, device: &Path) -> Option<PathBuf> {
    let card = device.file_name()?.to_str()?;
    if !card
        .strip_prefix("card")
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    let mut nodes = fs::read_dir(sys_root.join(card).join("device/drm"))
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            name.strip_prefix("renderD")
                .is_some_and(|index| {
                    !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
                })
                .then(|| dev_root.join(name))
        })
        .collect::<Vec<_>>();
    nodes.sort();
    nodes.into_iter().next()
}

pub fn drm_native_mode(root: &Path, output_name: &str) -> Option<(u32, u32)> {
    for entry in fs::read_dir(root).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let normalized = name.split_once('-').map_or(name.as_ref(), |(_, rest)| rest);
        if normalized != output_name {
            continue;
        }
        let modes = fs::read_to_string(entry.path().join("modes")).ok()?;
        for mode in modes.lines() {
            let Some((width, height)) = mode.trim().split_once('x') else {
                continue;
            };
            let (Ok(width), Ok(height)) = (width.parse::<u32>(), height.parse::<u32>()) else {
                continue;
            };
            if width > 0 && height > 0 {
                return Some((width, height));
            }
        }
    }
    None
}

fn set_runtime_environment(
    command: &mut Command,
    weston_runtime: &Path,
    pipewire_runtime: Option<&OsStr>,
) {
    command.env("XDG_RUNTIME_DIR", weston_runtime);
    if let Some(pipewire_runtime) = pipewire_runtime {
        command.env("PIPEWIRE_RUNTIME_DIR", pipewire_runtime);
    }
}

fn set_client_environment(
    command: &mut Command,
    weston_runtime: &Path,
    wayland_display: &str,
    pulse_server: Option<&OsStr>,
    pulse_sink: Option<&OsStr>,
) {
    command
        .env("XDG_RUNTIME_DIR", weston_runtime)
        .env("WAYLAND_DISPLAY", wayland_display);
    set_pulse_environment(command, pulse_server, pulse_sink);
}

fn weston_startup_error(backend: ActiveBackend, error: &io::Error, log_path: &Path) -> io::Error {
    startup_error(
        format!("{} Weston did not become ready: {error}", backend.name()),
        "Weston",
        log_path,
    )
}

/// The libweston input module, or the documented refusal when this build has none (plan D12).
///
/// Injecting input into libweston without a global or `/dev/uinput` requires the compiled-in
/// module, so a build without it can only run `--compositor sway`.
#[cfg(no_weston_input)]
fn weston_input_module() -> io::Result<&'static [u8]> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        WESTON_INPUT_MISSING,
    ))
}

#[cfg(not(no_weston_input))]
fn weston_input_module() -> io::Result<&'static [u8]> {
    Ok(INPUT_MODULE)
}

pub const WESTON_INPUT_MISSING: &str = "vvland was built without libweston input support (no libweston-13..16 dev files at build \
     time); rebuild with the libweston development package or use --compositor sway";

/// Whether this build embeds the libweston input module; reported by `--doctor`.
pub const fn weston_input_compiled_in() -> bool {
    cfg!(not(no_weston_input))
}

fn wait_for_wayland_socket(path: &Path, child: &mut Child, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!("Weston exited with {status}")));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Wayland socket was not created",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// D12: a Sway-only build must refuse the Weston backend with the documented message rather
    /// than starting a compositor it cannot inject input into.
    #[test]
    #[cfg(no_weston_input)]
    fn weston_mode_fails_fast_without_the_input_module() {
        let config = Config::try_parse_from(["vvland", "--doctor"]).unwrap();
        let Err(error) = WestonSession::start(
            &config,
            CompositorEnvironment {
                width: 640,
                height: 360,
                pulse_server: None,
                pulse_sink: None,
                app_window: None,
            },
        ) else {
            panic!("Weston started without an input module");
        };
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("libweston input support"));
    }

    #[test]
    fn weston_input_support_matches_the_build_configuration() {
        assert_eq!(weston_input_compiled_in(), cfg!(not(no_weston_input)));
        assert!(WESTON_INPUT_MISSING.contains("--compositor sway"));
    }

    #[test]
    fn automatic_renderer_retries_pixman_only_for_auto() {
        assert_eq!(
            automatic_renderer_fallback(Renderer::Auto),
            Some(Renderer::Pixman)
        );
        assert_eq!(automatic_renderer_fallback(Renderer::Gl), None);
        assert_eq!(automatic_renderer_fallback(Renderer::Vulkan), None);
        assert_eq!(automatic_renderer_fallback(Renderer::Pixman), None);
    }

    #[test]
    fn backend_arguments_use_supported_weston_options() {
        let mut headless = Command::new("weston");
        add_backend_arguments(&mut headless, ActiveBackend::Headless, 1280, 720, None);
        let headless = headless.get_args().collect::<Vec<_>>();
        assert_eq!(
            headless,
            [OsStr::new("--width=1280"), OsStr::new("--height=720")]
        );

        let mut drm = Command::new("weston");
        add_backend_arguments(
            &mut drm,
            ActiveBackend::Drm,
            1280,
            720,
            Some(Path::new("/dev/dri/card2")),
        );
        let drm = drm.get_args().collect::<Vec<_>>();
        assert_eq!(
            drm,
            [
                OsStr::new("--continue-without-input"),
                OsStr::new("--drm-device=card2")
            ]
        );
    }

    #[test]
    fn finds_connected_output_device_and_render_node() {
        let root = std::env::temp_dir().join(format!("vvland-drm-test-{}", std::process::id()));
        let dev_root = root.join("dev");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("card0-HDMI-A-1")).unwrap();
        fs::write(root.join("card0-HDMI-A-1/status"), "connected\n").unwrap();
        fs::create_dir_all(root.join("card0-DP-1")).unwrap();
        fs::write(root.join("card0-DP-1/status"), "disconnected\n").unwrap();
        fs::create_dir_all(root.join("card0/device/drm/renderD128")).unwrap();
        assert_eq!(connected_drm_outputs(&root), ["HDMI-A-1"]);
        assert_eq!(
            drm_device_for_output(&root, &dev_root, "HDMI-A-1"),
            Some(dev_root.join("card0"))
        );
        assert_eq!(
            drm_render_node(&root, &dev_root, Path::new("/dev/dri/card0")),
            Some(dev_root.join("renderD128"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generated_headless_config_renders_pipewire_output_directly() {
        let config = weston_config(
            ActiveBackend::Headless,
            "headless",
            1280,
            720,
            30,
            Renderer::Pixman,
            false,
            false,
            None,
            "us",
            None,
            None,
        );
        assert!(config.contains("renderer=pixman"));
        assert!(config.contains("[pipewire]\nnum-outputs=1\n"));
        assert!(config.contains("[output]\nname=pipewire\nmode=1280x720@30\n"));
        // Headless outputs have no native mode, so they must never be mirrored (mirroring one
        // aborts Weston in wet_output_compute_output_from_mirror), and no separate headless
        // output is created.
        assert!(!config.contains("mirror-of"));
        assert!(!config.contains("name=headless"));
        assert!(config.contains("keymap_model=pc105\nkeymap_layout=us"));
    }

    #[test]
    fn generated_drm_config_sizes_pipewire_output_to_capture_mode() {
        let config = weston_config(
            ActiveBackend::Drm,
            "DP-4",
            1920,
            1080,
            30,
            Renderer::Gl,
            false,
            false,
            None,
            "us",
            None,
            None,
        );
        assert!(config.contains("name=DP-4\nmode=preferred"));
        assert!(config.contains("name=pipewire\nmode=1920x1080@30\nmirror-of=DP-4"));
    }

    #[test]
    fn single_app_mode_drops_the_shell_panel() {
        let desktop = weston_config(
            ActiveBackend::Headless,
            "headless",
            1280,
            720,
            30,
            Renderer::Pixman,
            false,
            false,
            None,
            "us",
            None,
            None,
        );
        assert!(desktop.contains("panel-position=top"));

        let single_app = weston_config(
            ActiveBackend::Headless,
            "headless",
            1280,
            720,
            30,
            Renderer::Pixman,
            true,
            false,
            None,
            "us",
            None,
            None,
        );
        assert!(single_app.contains("panel-position=none"));
        assert!(!single_app.contains("panel-position=top"));
    }

    #[test]
    fn private_wayland_runtime_preserves_pipewire_runtime() {
        let mut command = Command::new("weston");
        set_runtime_environment(
            &mut command,
            Path::new("/private/vvland"),
            Some(OsStr::new("/run/user/1000")),
        );
        let environment = command.get_envs().collect::<Vec<_>>();
        assert!(environment.contains(&(
            OsStr::new("XDG_RUNTIME_DIR"),
            Some(OsStr::new("/private/vvland"))
        )));
        assert!(environment.contains(&(
            OsStr::new("PIPEWIRE_RUNTIME_DIR"),
            Some(OsStr::new("/run/user/1000"))
        )));
    }

    #[test]
    fn private_wayland_runtime_preserves_pulse_routing_for_clients() {
        let mut command = Command::new("google-chrome");
        set_client_environment(
            &mut command,
            Path::new("/private/vvland"),
            "wayland-vvland",
            Some(OsStr::new("unix:/run/user/1000/pulse/native")),
            Some(OsStr::new("vvland_1234")),
        );
        let environment = command.get_envs().collect::<Vec<_>>();
        assert!(environment.contains(&(
            OsStr::new("XDG_RUNTIME_DIR"),
            Some(OsStr::new("/private/vvland"))
        )));
        assert!(environment.contains(&(
            OsStr::new("WAYLAND_DISPLAY"),
            Some(OsStr::new("wayland-vvland"))
        )));
        assert!(environment.contains(&(
            OsStr::new("PULSE_SERVER"),
            Some(OsStr::new("unix:/run/user/1000/pulse/native"))
        )));
        assert!(environment.contains(&(OsStr::new("PULSE_SINK"), Some(OsStr::new("vvland_1234")))));
    }

    #[test]
    fn startup_error_contains_bounded_weston_log() {
        let root = std::env::temp_dir().join(format!("vvland-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let log_path = root.join("weston.log");
        fs::write(&log_path, "Failed to initialize PipeWire\n").unwrap();

        let error = weston_startup_error(
            ActiveBackend::Headless,
            &io::Error::new(io::ErrorKind::BrokenPipe, "input module disconnected"),
            &log_path,
        );
        assert!(error.to_string().contains("input module disconnected"));
        assert!(error.to_string().contains("Failed to initialize PipeWire"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires live Weston and PipeWire services"]
    fn automatic_backend_falls_back_to_headless() {
        let mut config =
            Config::try_parse_from(["vvland", "--doctor", "--no-audio", "--xwayland=off"]).unwrap();
        config.drm_device = Some("/dev/dri/vvland-test-missing".into());
        let session = WestonSession::start(
            &config,
            CompositorEnvironment {
                width: 800,
                height: 600,
                pulse_server: None,
                pulse_sink: None,
                app_window: None,
            },
        )
        .unwrap();

        assert_eq!(session.backend(), ActiveBackend::Headless);
    }

    #[test]
    fn reads_first_connector_mode_as_native() {
        let root = std::env::temp_dir().join(format!("vvland-mode-test-{}", std::process::id()));
        let connector = root.join("card0-HDMI-A-1");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&connector).unwrap();
        fs::write(connector.join("modes"), "2560x1440\n1920x1080\n").unwrap();
        assert_eq!(drm_native_mode(&root, "HDMI-A-1"), Some((2560, 1440)));
        fs::remove_dir_all(root).unwrap();
    }
}
