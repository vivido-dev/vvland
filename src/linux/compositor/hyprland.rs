//! The Hyprland backend.
//!
//! Hyprland shares the wlroots protocol surface with Sway — `zwlr_screencopy_manager_v1` for
//! capture, the virtual keyboard and `zwlr_virtual_pointer_manager_v1` for input — so those
//! modules are reused verbatim. What differs is the launch: Hyprland has no standalone headless
//! backend (aquamarine needs a GPU device for its allocator), so it is started on whatever backend
//! the host offers with *every* physical connector disabled, and the desktop lives on a headless
//! output this module creates over Hyprland's IPC socket once the compositor is up. The same
//! socket disables the host's real input devices, which aquamarine would otherwise attach to the
//! session, and answers the window-observation methods.
//!
//! The Wayland socket is handed over rather than discovered: vvland binds it inside the private
//! runtime directory and passes the descriptor, so the display name is known before Hyprland runs.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::Config;
use crate::linux::app::{AppLaunch, is_unix_pulse_server};
use crate::linux::launcher::{
    RuntimeDirectory, child_logs_enabled, child_output, confirm_started, pipe, push_extra_config,
    read_extra_config, sanitize_child_environment, set_client_environment, set_pulse_environment,
    start_bounded_log, startup_error, terminate_group, write_private_file, xwayland_enabled,
};

use super::wlr_input::InputChannel;
use super::{AppWindow, CompositorEnvironment, CompositorWindow, WindowRect};

/// How long Hyprland is given to create its IPC socket.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the negotiated headless output is given to appear after `output create`.
const OUTPUT_TIMEOUT: Duration = Duration::from_secs(5);
const IPC_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_IPC_REQUEST_BYTES: usize = 4_096;
const MAX_IPC_REPLY_BYTES: usize = 1_048_576;
const MAX_WINDOWS: usize = 4096;
const MAX_DEVICES: usize = 1024;

/// The Wayland display vvland binds and hands to Hyprland.
const WAYLAND_DISPLAY: &str = "wayland-vvland";
/// The name of the headless output the desktop lives on.
///
/// Named rather than left to aquamarine's `HEADLESS-N` counter: Hyprland's own fallback output
/// already takes `HEADLESS-1`, so a generated name is the only way the configuration can address
/// this output before it exists.
const OUTPUT_NAME: &str = "vvland";

/// The room Hyprland needs below the session's runtime directory.
///
/// It binds `/hypr/<signature>/.socket2.sock`, where the signature is a forty-character build
/// hash, a ten-digit timestamp and a ten-digit nonce joined by underscores. Overrunning
/// `sun_path` does not fail Hyprland's startup — it logs "IPC will not work" and runs on without
/// the socket this module needs — so the room is reserved when the directory is created.
const INSTANCE_PATH_BUDGET: usize = "/hypr/".len() + 62 + "/.socket2.sock".len();

/// The oldest Hyprland whose configuration this module can generate.
///
/// 0.53 rejects both `windowrulev2` and the flat `windowrule =` form; the generated configuration
/// uses the block window-rule syntax that replaced them, so older releases cannot parse it.
const MINIMUM_VERSION: (u32, u32) = (0, 53);

pub struct HyprlandSession {
    runtime: RuntimeDirectory,
    child: Child,
    launched: Vec<Child>,
    input: InputChannel,
    process_group: i32,
    ipc_socket: PathBuf,
    wayland_socket: PathBuf,
    pulse_server: Option<OsString>,
    pulse_sink: Option<OsString>,
    log_thread: Option<thread::JoinHandle<()>>,
}

impl HyprlandSession {
    pub fn start(config: &Config, environment: CompositorEnvironment<'_>) -> io::Result<Self> {
        require_supported_hyprland(&config.hyprland)?;
        let runtime = RuntimeDirectory::create_short(INSTANCE_PATH_BUDGET)?;
        let config_path = runtime.path.join("hyprland.conf");
        let extra = config
            .extra_config
            .as_deref()
            .map(read_extra_config)
            .transpose()?;
        let generated = hyprland_config(
            environment.width,
            environment.height,
            config.fps,
            environment.app_window,
            xwayland_enabled(config.xwayland),
            config.xkb_model.as_deref(),
            &config.xkb_layout,
            config.xkb_variant.as_deref(),
            config.xkb_options.as_deref(),
            extra.as_deref(),
        )?;
        write_private_file(&config_path, generated.as_bytes(), 0o600)?;

        // The display socket is ours: binding it here fixes the display name and removes the
        // race of scanning the runtime directory for whatever socket the compositor happened to
        // pick.
        let wayland_socket = runtime.path.join(WAYLAND_DISPLAY);
        let listener = UnixListener::bind(&wayland_socket)?;
        let listener_fd = listener.as_raw_fd();

        let (log_read, log_write) = pipe()?;
        let log_write_clone = log_write.try_clone()?;
        let mut command = Command::new(&config.hyprland);
        command
            .arg("--config")
            .arg(&config_path)
            .arg("--socket")
            .arg(WAYLAND_DISPLAY)
            .arg("--wayland-fd")
            .arg(listener_fd.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_write_clone))
            .stderr(Stdio::from(log_write));
        sanitize_child_environment(&mut command);
        command
            .env("XDG_RUNTIME_DIR", &runtime.path)
            .env("XDG_SESSION_TYPE", "wayland")
            .env("XDG_CURRENT_DESKTOP", "Hyprland")
            .env("XDG_SESSION_DESKTOP", "Hyprland")
            // Hyprland otherwise pushes this session's WAYLAND_DISPLAY into the user's systemd
            // and D-Bus activation environments, where every later host process would inherit it.
            .env("HYPRLAND_NO_SD_VARS", "1")
            .env("HYPRLAND_NO_SD_NOTIFY", "1")
            .env("HYPRLAND_NO_CRASHREPORTER", "1");
        set_pulse_environment(
            &mut command,
            environment.pulse_server,
            environment.pulse_sink,
        );

        // SAFETY: only async-signal-safe libc calls are made after fork and before exec.
        unsafe {
            command.pre_exec(move || {
                // The handed-over listening socket must survive exec.
                if libc::fcntl(listener_fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::setpgid(0, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command.spawn()?;
        // Command keeps the configured stdio handles; release the log writers and this process's
        // copy of the display socket before any failure path waits on the log reader.
        drop(command);
        drop(listener);
        let process_group = i32::try_from(child.id())
            .map_err(|_| io::Error::other("Hyprland PID exceeds process-group range"))?;
        let log_path = runtime.path.join("hyprland.log");
        let log_thread = match start_bounded_log("vvland-hyprland-log", log_read, log_path.clone())
        {
            Ok(thread) => thread,
            Err(error) => {
                terminate_group(process_group, &mut child);
                return Err(error);
            }
        };

        let prepared =
            wait_for_ipc_socket(&runtime.path, &mut child, READY_TIMEOUT).and_then(|ipc_socket| {
                prepare_output(&ipc_socket, environment.width, environment.height)?;
                disable_host_input_devices(&ipc_socket)?;
                Ok(ipc_socket)
            });
        let ipc_socket = match prepared {
            Ok(socket) => socket,
            Err(error) => {
                terminate_group(process_group, &mut child);
                let _ = log_thread.join();
                return Err(hyprland_startup_error(&error, &log_path));
            }
        };

        let input = match InputChannel::connect(
            &wayland_socket,
            "Hyprland",
            environment.width,
            environment.height,
            config.xkb_model.as_deref(),
            &config.xkb_layout,
            config.xkb_variant.as_deref(),
            config.xkb_options.clone(),
        ) {
            Ok(input) => input,
            Err(error) => {
                terminate_group(process_group, &mut child);
                let _ = log_thread.join();
                return Err(hyprland_startup_error(&error, &log_path));
            }
        };

        Ok(Self {
            runtime,
            child,
            launched: Vec::new(),
            input,
            process_group,
            ipc_socket,
            wayland_socket,
            pulse_server: environment.pulse_server.map(OsStr::to_owned),
            pulse_sink: environment.pulse_sink.map(OsStr::to_owned),
            log_thread: Some(log_thread),
        })
    }

    /// Always headless: the desktop is the generated output, never a connector.
    pub fn backend_name(&self) -> &'static str {
        "headless"
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn wayland_socket(&self) -> &Path {
        &self.wayland_socket
    }

    pub fn ipc_socket(&self) -> &Path {
        &self.ipc_socket
    }

    pub fn input_mut(&mut self) -> &mut InputChannel {
        &mut self.input
    }

    pub fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    /// Spawn a client directly rather than through Hyprland's `exec` dispatcher.
    ///
    /// The dispatcher would hand the program to a shell inside the compositor with no child
    /// handle to watch; spawning here keeps the process in this session's group, so the ordinary
    /// teardown reaps it, and keeps a handle for the `--app` liveness probe.
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
        self.prepare_client(&mut command, self.pulse_server.as_deref());
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
        self.prepare_client(&mut command, self.app_pulse_server(launch));
        for (name, value) in &launch.env {
            command.env(name, value);
        }
        let mut child = command.spawn()?;
        let started = confirm_started(&launch.binary.to_string_lossy(), &mut child);
        self.launched.push(child);
        started
    }

    /// Point a client at this session: private runtime directory, display, and Pulse routing, in
    /// this session's process group so teardown reaps it.
    fn prepare_client(&self, command: &mut Command, pulse_server: Option<&OsStr>) {
        // Sanitize first so the explicit routing below survives the superset filter.
        sanitize_child_environment(command);
        set_client_environment(
            command,
            &self.runtime.path,
            WAYLAND_DISPLAY,
            pulse_server,
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
    }

    /// The Pulse server this application should see.
    ///
    /// A snap-confined browser already runs behind snap's own Pulse mediation; a raw host unix
    /// socket can drop it to ALSA, while `PULSE_SINK` alone still selects the private sink.
    fn app_pulse_server(&self, launch: &AppLaunch) -> Option<&OsStr> {
        let server = self.pulse_server.as_deref()?;
        if launch.snap_confined && is_unix_pulse_server(server) {
            return None;
        }
        Some(server)
    }
}

impl Drop for HyprlandSession {
    fn drop(&mut self) {
        let _ = self.input.shutdown();
        // Launched clients were spawned into this group, so the one signal ends them too; the
        // handles are kept only for the `--app` liveness probe.
        terminate_group(self.process_group, &mut self.child);
        if let Some(log_thread) = self.log_thread.take() {
            let _ = log_thread.join();
        }
    }
}

/// Create the headless output and wait for it to come up at the negotiated geometry.
///
/// The configuration already carries the mode for `OUTPUT_NAME`, so creating the output is enough
/// to apply it; the poll is what turns "Hyprland ignored the rule" into a startup error instead of
/// a session streaming a black or wrongly-sized desktop.
fn prepare_output(socket: &Path, width: u32, height: u32) -> io::Result<()> {
    expect_ok(
        socket,
        &format!("output create headless {OUTPUT_NAME}"),
        "create the headless output",
    )?;
    let deadline = Instant::now() + OUTPUT_TIMEOUT;
    loop {
        let monitors = query_monitors(socket)?;
        if monitors.iter().any(|monitor| {
            monitor.name == OUTPUT_NAME && (monitor.width, monitor.height) == (width, height)
        }) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "Hyprland did not bring up a {width}x{height} headless output; it reported {}",
                    describe_monitors(&monitors)
                ),
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Detach every input device aquamarine attached from the host seat.
///
/// Hyprland has no equivalent of wlroots' `WLR_LIBINPUT_NO_DEVICES`: on a host with a physical
/// keyboard the session would otherwise be typed into from the console. Failing here fails the
/// session, because a half-isolated desktop is worse than none.
fn disable_host_input_devices(socket: &Path) -> io::Result<()> {
    for device in query_device_names(socket)? {
        expect_ok(
            socket,
            &format!("keyword device[{device}]:enabled false"),
            "disable a host input device",
        )?;
    }
    Ok(())
}

struct MonitorInfo {
    name: String,
    width: u32,
    height: u32,
}

fn describe_monitors(monitors: &[MonitorInfo]) -> String {
    if monitors.is_empty() {
        return "no enabled outputs".to_owned();
    }
    monitors
        .iter()
        .map(|monitor| format!("{} {}x{}", monitor.name, monitor.width, monitor.height))
        .collect::<Vec<_>>()
        .join(", ")
}

fn query_monitors(socket: &Path) -> io::Result<Vec<MonitorInfo>> {
    let reply = hyprland_message(socket, "j/monitors")?;
    let monitors: serde_json::Value = parse_json(&reply)?;
    let monitors = monitors
        .as_array()
        .ok_or_else(|| invalid_reply("Hyprland monitor list is not an array"))?;
    monitors
        .iter()
        .map(|monitor| {
            Ok(MonitorInfo {
                name: required_string(monitor, "name")?,
                width: required_u32(monitor, "width")?,
                height: required_u32(monitor, "height")?,
            })
        })
        .collect()
}

/// The names of every keyboard, mouse, touch device and tablet Hyprland currently drives.
fn query_device_names(socket: &Path) -> io::Result<Vec<String>> {
    device_names(&parse_json(&hyprland_message(socket, "j/devices")?)?)
}

fn device_names(devices: &serde_json::Value) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for group in ["keyboards", "mice", "touch", "tablets"] {
        let Some(entries) = devices.get(group) else {
            continue;
        };
        let entries = entries
            .as_array()
            .ok_or_else(|| invalid_reply("Hyprland device group is not an array"))?;
        for entry in entries {
            if names.len() >= MAX_DEVICES {
                return Err(invalid_reply("Hyprland reported too many input devices"));
            }
            let name = required_string(entry, "name")?;
            // The name is interpolated into an IPC command, and Hyprland's own device rules key
            // on it; anything that could close the bracket or start a second command is refused
            // rather than sent.
            if name.is_empty()
                || name.len() > 256
                || name
                    .chars()
                    .any(|character| "[];\n\r".contains(character) || character.is_control())
            {
                return Err(invalid_reply(format!(
                    "Hyprland reported an input device with an unusable name {name:?}"
                )));
            }
            names.push(name);
        }
    }
    Ok(names)
}

pub fn query_windows(socket: &Path) -> io::Result<Vec<CompositorWindow>> {
    let reply = hyprland_message(socket, "j/clients")?;
    let clients: serde_json::Value = parse_json(&reply)?;
    let clients = clients
        .as_array()
        .ok_or_else(|| invalid_reply("Hyprland client list is not an array"))?;
    if clients.len() > MAX_WINDOWS {
        return Err(invalid_reply("Hyprland reported too many windows"));
    }
    clients.iter().map(window_from_client).collect()
}

fn window_from_client(client: &serde_json::Value) -> io::Result<CompositorWindow> {
    let address = required_string(client, "address")?;
    let id = u64::from_str_radix(address.trim_start_matches("0x"), 16)
        .map_err(|_| invalid_reply(format!("Hyprland window address {address:?} is not hex")))?;
    let class = optional_string(client, "class")?.filter(|class| !class.is_empty());
    let xwayland = client
        .get("xwayland")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let pid = client
        .get("pid")
        .and_then(serde_json::Value::as_i64)
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0);
    let at = required_pair(client, "at")?;
    let size = required_pair(client, "size")?;
    let width = u32::try_from(size.0)
        .map_err(|_| invalid_reply("Hyprland window width is out of range"))?;
    let height = u32::try_from(size.1)
        .map_err(|_| invalid_reply("Hyprland window height is out of range"))?;
    Ok(CompositorWindow {
        id,
        title: optional_string(client, "title")?,
        // An Xwayland window's `class` is its X11 class, which is what the Sway tree reports
        // separately; keeping them apart lets one caller match either without guessing.
        app_id: if xwayland { None } else { class.clone() },
        xwayland_class: if xwayland { class } else { None },
        pid,
        rect: WindowRect {
            x: at.0,
            y: at.1,
            width,
            height,
        },
        focused: client
            .get("focusHistoryID")
            .and_then(serde_json::Value::as_i64)
            == Some(0),
        fullscreen: client
            .get("fullscreen")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
            != 0,
    })
}

/// Send one command and require Hyprland's bare `ok` acknowledgement.
fn expect_ok(socket: &Path, command: &str, what: &str) -> io::Result<()> {
    let reply = hyprland_message(socket, command)?;
    let reply = String::from_utf8_lossy(&reply);
    if reply.trim() == "ok" {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "Hyprland refused to {what}: {}",
        reply.trim()
    )))
}

fn hyprland_message(socket: &Path, command: &str) -> io::Result<Vec<u8>> {
    if command.len() > MAX_IPC_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Hyprland IPC command exceeds 4 KiB",
        ));
    }
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(IPC_TIMEOUT))?;
    stream.set_write_timeout(Some(IPC_TIMEOUT))?;
    stream.write_all(command.as_bytes())?;
    stream.flush()?;
    let mut reply = Vec::new();
    // Hyprland answers and closes; the cap keeps a wedged or hostile peer from growing this.
    let mut limited = stream.take(MAX_IPC_REPLY_BYTES as u64 + 1);
    limited.read_to_end(&mut reply)?;
    if reply.len() > MAX_IPC_REPLY_BYTES {
        return Err(invalid_reply("Hyprland IPC reply exceeds 1 MiB"));
    }
    Ok(reply)
}

fn parse_json(reply: &[u8]) -> io::Result<serde_json::Value> {
    serde_json::from_slice(reply).map_err(|error| {
        invalid_reply(format!(
            "Hyprland IPC reply is not the expected JSON: {error}"
        ))
    })
}

fn required_string(value: &serde_json::Value, key: &str) -> io::Result<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| invalid_reply(format!("Hyprland reply omitted the {key} string")))
}

fn optional_string(value: &serde_json::Value, key: &str) -> io::Result<Option<String>> {
    value
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid_reply(format!("Hyprland reply field {key} is not a string")))
        })
        .transpose()
}

fn required_u32(value: &serde_json::Value, key: &str) -> io::Result<u32> {
    value
        .get(key)
        .and_then(serde_json::Value::as_i64)
        .and_then(|number| u32::try_from(number).ok())
        .ok_or_else(|| invalid_reply(format!("Hyprland reply field {key} is out of range")))
}

/// Read a `[x, y]` pair, which Hyprland uses for both window positions and sizes.
fn required_pair(value: &serde_json::Value, key: &str) -> io::Result<(i64, i64)> {
    let pair = value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .filter(|pair| pair.len() == 2)
        .ok_or_else(|| invalid_reply(format!("Hyprland reply field {key} is not a pair")))?;
    let first = pair[0]
        .as_i64()
        .ok_or_else(|| invalid_reply(format!("Hyprland reply field {key} is not numeric")))?;
    let second = pair[1]
        .as_i64()
        .ok_or_else(|| invalid_reply(format!("Hyprland reply field {key} is not numeric")))?;
    Ok((first, second))
}

fn invalid_reply(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[allow(clippy::too_many_arguments)]
fn hyprland_config(
    width: u32,
    height: u32,
    fps: u32,
    app_window: Option<AppWindow>,
    xwayland: bool,
    xkb_model: Option<&str>,
    xkb_layout: &str,
    xkb_variant: Option<&str>,
    xkb_options: Option<&str>,
    extra: Option<&str>,
) -> io::Result<String> {
    // Hyprland's verbose logging also writes an *unbounded* file of its own inside the runtime
    // directory, which vvland's bounded drain cannot cap — its quiet default stops growing after
    // startup, so the detail is reserved for the debugging switch every backend already honours.
    let verbose = child_logs_enabled();
    let mut config = format!(
        "# Generated by vvland for one isolated session.\n\
         debug:disable_logs = {quiet}\n\
         debug:enable_stdout_logs = {verbose}\n\
         # Hyprland draws its warnings over the desktop, where they would be captured,\n\
         # encoded and streamed; a session's warnings belong in the log.\n\
         debug:suppress_errors = true\n\
         ecosystem:no_update_news = true\n\
         ecosystem:no_donation_nag = true\n\
         misc:disable_hyprland_logo = true\n\
         misc:disable_hyprland_guiutils_check = true\n\
         misc:disable_watchdog_warning = true\n\
         misc:disable_splash_rendering = true\n\
         misc:force_default_wallpaper = 0\n\
         misc:disable_autoreload = true\n\
         animations:enabled = false\n\
         decoration:rounding = 0\n\
         decoration:blur:enabled = false\n\
         decoration:shadow:enabled = false\n\
         general:gaps_in = 0\n\
         general:gaps_out = 0\n\
         general:border_size = 1\n\
         general:allow_tearing = false\n",
        quiet = !verbose,
    );
    // Every connector the host has stays dark: aquamarine may have opened a real GPU to get an
    // allocator, but this session only ever renders to the headless output created over IPC.
    config.push_str("monitor = , disable\n");
    config.push_str(&format!(
        "monitor = {OUTPUT_NAME}, {width}x{height}@{fps}, 0x0, 1\n"
    ));
    config.push_str(&format!(
        "xwayland:enabled = {}\n",
        if xwayland { "true" } else { "false" }
    ));
    config.push_str(&format!(
        "input:kb_model = {}\n",
        config_value(xkb_model.unwrap_or("pc105"), "--xkb-model")?
    ));
    config.push_str(&format!(
        "input:kb_layout = {}\n",
        config_value(xkb_layout, "--xkb-layout")?
    ));
    if let Some(variant) = xkb_variant {
        config.push_str(&format!(
            "input:kb_variant = {}\n",
            config_value(variant, "--xkb-variant")?
        ));
    }
    if let Some(options) = xkb_options {
        config.push_str(&format!(
            "input:kb_options = {}\n",
            config_value(options, "--xkb-options")?
        ));
    }
    for binding in [
        "SUPER SHIFT, Q, killactive",
        "SUPER, F, fullscreen, 1",
        "SUPER, H, movefocus, l",
        "SUPER, J, movefocus, d",
        "SUPER, K, movefocus, u",
        "SUPER, L, movefocus, r",
        "SUPER SHIFT, H, movewindow, l",
        "SUPER SHIFT, J, movewindow, d",
        "SUPER SHIFT, K, movewindow, u",
        "SUPER SHIFT, L, movewindow, r",
    ] {
        config.push_str(&format!("bind = {binding}\n"));
    }
    for workspace in 1..=9 {
        config.push_str(&format!(
            "bind = SUPER, {workspace}, workspace, {workspace}\n"
        ));
        config.push_str(&format!(
            "bind = SUPER SHIFT, {workspace}, movetoworkspace, {workspace}\n"
        ));
    }
    // Single-app mode: the one window owns the whole output from its first map, with no border
    // and no chance for a second window to tile beside it.
    if let Some(app) = app_window {
        config.push_str(
            "windowrule {\n\
             \x20   name = vvland-borderless\n\
             \x20   match:class = .*\n\
             \x20   border_size = 0\n\
             }\n",
        );
        if app.fullscreen {
            let class = config_value(app.app_id, "the application profile")?;
            config.push_str(&format!(
                "windowrule {{\n\
                 \x20   name = vvland-app\n\
                 \x20   match:class = ^({class})$\n\
                 \x20   fullscreen = true\n\
                 }}\n"
            ));
        }
    }
    push_extra_config(&mut config, extra);
    Ok(config)
}

/// Reject a configuration value that would not survive Hyprland's line-oriented parser.
///
/// Values reach here from the command line. Hyprland reads a value to the end of the line and
/// treats `#` as a comment, so a newline would inject a second directive and a `#` would silently
/// truncate the value; both are refused rather than quietly mangled.
fn config_value<'a>(value: &'a str, option: &str) -> io::Result<&'a str> {
    if value.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{option} must not be empty for Hyprland"),
        ));
    }
    if value
        .chars()
        .any(|character| character.is_control() || "#{}".contains(character))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{option} contains a character Hyprland's configuration cannot carry"),
        ));
    }
    Ok(value)
}

pub(crate) fn hyprland_supported(version: &str) -> bool {
    parse_version(version).is_some_and(|found| found >= MINIMUM_VERSION)
}

/// The `major.minor` of the first version-shaped word in `Hyprland --version` output.
fn parse_version(version: &str) -> Option<(u32, u32)> {
    version.split_whitespace().find_map(|word| {
        let word = word.trim_start_matches('v');
        let mut components = word.split('.');
        let major = components.next()?.parse::<u32>().ok()?;
        let minor = components
            .next()?
            .split(|character: char| !character.is_ascii_digit())
            .next()?
            .parse::<u32>()
            .ok()?;
        Some((major, minor))
    })
}

fn require_supported_hyprland(program: &Path) -> io::Result<()> {
    let version = hyprland_version(program).map_err(|error| {
        io::Error::new(error.kind(), format!("could not execute Hyprland: {error}"))
    })?;
    if hyprland_supported(&version) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "Hyprland {}.{} or newer is required; found {version}",
                MINIMUM_VERSION.0, MINIMUM_VERSION.1
            ),
        ))
    }
}

pub(crate) fn hyprland_version(program: &Path) -> io::Result<String> {
    let mut command = Command::new(program);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .env("LC_ALL", "C");
    sanitize_child_environment(&mut command);
    let output = command.output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{} --version exited with {}",
            program.display(),
            output.status
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if stdout.trim().is_empty() {
        stderr
    } else {
        stdout
    };
    // The banner continues with build, date and library lines; only the first is the version.
    Ok(text.lines().next().unwrap_or_default().trim().to_owned())
}

/// Wait for the IPC socket of the instance this session started.
///
/// The runtime directory is private and fresh, so the single `hypr/<signature>` entry inside it is
/// unambiguously ours — no instance signature has to be guessed or parsed out of the log.
fn wait_for_ipc_socket(
    runtime: &Path,
    child: &mut Child,
    timeout: Duration,
) -> io::Result<PathBuf> {
    let deadline = Instant::now() + timeout;
    let instances = runtime.join("hypr");
    loop {
        if let Some(socket) = find_ipc_socket(&instances)? {
            return Ok(socket);
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!("Hyprland exited with {status}")));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Hyprland IPC socket was not created",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn find_ipc_socket(instances: &Path) -> io::Result<Option<PathBuf>> {
    let entries = match fs::read_dir(instances) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut found = None;
    for entry in entries.flatten() {
        let socket = entry.path().join(".socket.sock");
        let is_socket = fs::metadata(&socket)
            .map(|metadata| metadata.file_type().is_socket())
            .unwrap_or(false);
        if is_socket && found.replace(socket).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the private runtime directory holds more than one Hyprland instance",
            ));
        }
    }
    Ok(found)
}

fn hyprland_startup_error(error: &io::Error, log_path: &Path) -> io::Error {
    let reported = startup_error(
        format!("Hyprland did not become ready: {error}"),
        "Hyprland",
        log_path,
    );
    let message = reported.to_string();
    if !aquamarine_found_no_gpu(&message) {
        return reported;
    }
    // The one failure worth naming outright, because nothing about it suggests the remedy:
    // aquamarine has no headless-only mode, so it refuses to start without a device to allocate
    // buffers from.
    io::Error::other(format!(
        "{message}\n\
         hint: Hyprland has no standalone headless backend — aquamarine needs a GPU device that \
         no other compositor is holding. Use --compositor sway or --compositor weston on this \
         host, or run VVLAND_CHILD_LOGS=1 for Hyprland's own startup log."
    ))
}

/// Whether a startup failure is aquamarine refusing to run without a usable GPU.
fn aquamarine_found_no_gpu(message: &str) -> bool {
    message.contains("no allocator available") || message.contains("CBackend::create() failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_gate_matches_the_generated_configuration() {
        assert!(hyprland_supported(
            "Hyprland 0.53.3 built from branch v0.53.3 at commit abc"
        ));
        assert!(hyprland_supported("Hyprland 1.0"));
        assert!(hyprland_supported("Hyprland 0.54.0-dev"));
        // 0.52 and older parse neither the block window-rule syntax nor its predecessors the
        // same way, so they are refused rather than started into a config error.
        assert!(!hyprland_supported("Hyprland 0.52.1"));
        assert!(!hyprland_supported("Hyprland 0.41.2"));
        assert!(!hyprland_supported("unknown"));
    }

    #[test]
    fn a_host_without_a_usable_gpu_is_told_what_to_do_about_it() {
        // Aquamarine's refusal reads as an internal C++ error; on its own it sends an operator
        // looking for a vvland bug rather than at the host's GPU.
        assert!(aquamarine_found_no_gpu(
            "Hyprland log:\nCannot open backend: no allocator available"
        ));
        assert!(aquamarine_found_no_gpu(
            "terminate called after throwing an instance of 'std::runtime_error'\n  what():  \
             CBackend::create() failed!"
        ));
        assert!(!aquamarine_found_no_gpu(
            "Hyprland did not become ready: Hyprland IPC socket was not created"
        ));
    }

    #[test]
    fn generated_config_pins_the_headless_output_and_darkens_connectors() {
        let config =
            hyprland_config(1920, 1080, 30, None, true, None, "us", None, None, None).unwrap();
        // Hyprland's own log file is not drained by vvland and grows without bound while its
        // verbose mode is on, so an ordinary session leaves that mode alone.
        assert!(
            config.contains("debug:suppress_errors = true\n"),
            "{config}"
        );
        assert!(config.contains("monitor = , disable\n"), "{config}");
        assert!(
            config.contains("monitor = vvland, 1920x1080@30, 0x0, 1\n"),
            "{config}"
        );
        assert!(config.contains("xwayland:enabled = true\n"), "{config}");
        assert!(config.contains("input:kb_layout = us\n"), "{config}");
        // No window rules without `--app`: an ordinary desktop keeps Hyprland's own behaviour.
        assert!(!config.contains("windowrule"), "{config}");
    }

    #[test]
    fn extra_configuration_follows_the_generated_directives() {
        // The user's own hyprland.conf is never read, so `exec-once` for a dock or a bar arrives
        // through --extra-config and must land after — and therefore win over — the generated
        // body it is appended to.
        let config = hyprland_config(
            1920,
            1080,
            30,
            None,
            true,
            None,
            "us",
            None,
            None,
            Some("exec-once = nwg-dock-hyprland\nbind = SUPER, D, exec, wofi\n"),
        )
        .unwrap();
        let generated_end = config
            .find("exec-once")
            .expect("extra configuration is present");
        assert!(
            config[..generated_end].contains("monitor = vvland, 1920x1080@30, 0x0, 1\n"),
            "{config}"
        );
        assert!(
            config.ends_with("bind = SUPER, D, exec, wofi\n"),
            "{config}"
        );
    }

    #[test]
    fn app_mode_gives_the_single_window_the_whole_output() {
        let config = hyprland_config(
            1280,
            720,
            30,
            Some(AppWindow {
                app_id: "google-chrome",
                fullscreen: true,
            }),
            false,
            None,
            "us",
            None,
            None,
            None,
        )
        .unwrap();
        assert!(config.contains("border_size = 0"), "{config}");
        assert!(
            config.contains("match:class = ^(google-chrome)$"),
            "{config}"
        );
        assert!(config.contains("fullscreen = true"), "{config}");
        assert!(config.contains("xwayland:enabled = false\n"), "{config}");
    }

    #[test]
    fn configuration_values_that_would_inject_a_directive_are_refused() {
        // Hyprland reads a value to end of line: a newline would append a directive of the
        // caller's choosing, and `#` would truncate the value without a word of complaint.
        for hostile in ["us\nmonitor = , preferred", "us # comment", "us{", "us\r"] {
            let error = hyprland_config(640, 360, 30, None, true, None, hostile, None, None, None)
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{hostile:?}");
        }
        assert!(hyprland_config(640, 360, 30, None, true, None, "", None, None, None).is_err());
    }

    #[test]
    fn a_client_reply_becomes_a_window() {
        let clients = serde_json::json!([{
            "address": "0x5c052d184260",
            "at": [21, 21],
            "size": [1878, 1038],
            "class": "foot",
            "title": "foot — vvland",
            "pid": 4242,
            "xwayland": false,
            "fullscreen": 0,
            "focusHistoryID": 0,
        }]);
        let windows: Vec<_> = clients
            .as_array()
            .unwrap()
            .iter()
            .map(window_from_client)
            .collect::<io::Result<_>>()
            .unwrap();
        let window = &windows[0];
        assert_eq!(window.id, 0x5c05_2d18_4260);
        assert_eq!(window.app_id.as_deref(), Some("foot"));
        assert_eq!(window.xwayland_class, None);
        assert_eq!(window.pid, Some(4242));
        assert_eq!(window.rect.x, 21);
        assert_eq!(window.rect.width, 1878);
        assert!(window.focused);
        assert!(!window.fullscreen);
    }

    #[test]
    fn an_xwayland_client_reports_its_x11_class_separately() {
        let client = serde_json::json!({
            "address": "0x1",
            "at": [0, 0],
            "size": [800, 600],
            "class": "Xterm",
            "title": "xterm",
            "pid": 7,
            "xwayland": true,
            "fullscreen": 2,
            "focusHistoryID": 3,
        });
        let window = window_from_client(&client).unwrap();
        assert_eq!(window.app_id, None);
        assert_eq!(window.xwayland_class.as_deref(), Some("Xterm"));
        assert!(window.fullscreen);
        assert!(!window.focused);
    }

    #[test]
    fn malformed_client_replies_are_rejected_rather_than_guessed() {
        for hostile in [
            serde_json::json!({"address": "zzz", "at": [0, 0], "size": [1, 1]}),
            serde_json::json!({"address": "0x1", "at": [0], "size": [1, 1]}),
            serde_json::json!({"address": "0x1", "at": [0, 0], "size": [-1, 1]}),
            serde_json::json!({"address": "0x1", "at": [0, 0]}),
        ] {
            assert!(window_from_client(&hostile).is_err(), "{hostile}");
        }
    }

    #[test]
    fn every_host_input_device_is_named_for_disabling() {
        let devices = serde_json::json!({
            "mice": [{"name": "logitech-mouse"}],
            "keyboards": [{"name": "power-button"}, {"name": "at-translated-set-2-keyboard"}],
            "tablets": [],
        });
        assert_eq!(
            device_names(&devices).unwrap(),
            [
                "power-button",
                "at-translated-set-2-keyboard",
                "logitech-mouse"
            ]
        );
    }

    #[test]
    fn device_names_that_could_forge_an_ipc_command_are_refused() {
        // The name is interpolated into `keyword device[<name>]:enabled false`, so a bracket or
        // a newline in it would be a second command rather than a device rule.
        for hostile in ["kbd];dispatch exit", "kbd\nkeyword misc:vfr false", ""] {
            let devices = serde_json::json!({"keyboards": [{"name": hostile}]});
            assert!(device_names(&devices).is_err(), "{hostile:?}");
        }
    }
}
