use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::{Config, Renderer};
use crate::linux::app::{AppLaunch, is_unix_pulse_server};
use crate::linux::launcher::{
    RuntimeDirectory, child_logs_enabled, pipe, sanitize_child_environment, start_bounded_log,
    startup_error, terminate_group, write_private_file, xwayland_enabled,
};

use super::sway_input::InputChannel;
use super::{AppWindow, CompositorEnvironment};

const READY_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_LAUNCHERS: u32 = 4096;
const MAX_LAUNCHER_BYTES: usize = 65_536;
const SWAY_IPC_MAGIC: &[u8; 6] = b"i3-ipc";
const SWAY_IPC_COMMAND: u32 = 0;
/// i3-ipc GET_TREE; used by the capability-gated window observation methods.
const SWAY_IPC_GET_TREE: u32 = 4;
const MAX_IPC_REPLY_BYTES: usize = 1_048_576;
const MAX_TREE_DEPTH: usize = 128;
const MAX_WINDOWS: usize = 4096;
const IPC_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a launched application is given to fall over before it counts as running.
const APP_LIVENESS_PROBE: Duration = Duration::from_millis(500);

fn automatic_renderer_fallback(renderer: Renderer) -> Option<Renderer> {
    (renderer == Renderer::Auto).then_some(Renderer::Pixman)
}

pub struct SwaySession {
    runtime: RuntimeDirectory,
    child: Child,
    pulse_server: Option<OsString>,
    input: InputChannel,
    process_group: i32,
    sway_socket: PathBuf,
    wayland_socket: PathBuf,
    launcher_sequence: u32,
    log_thread: Option<thread::JoinHandle<()>>,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct SwayRect {
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct SwayWindow {
    pub id: u64,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub xwayland_class: Option<String>,
    pub pid: Option<u32>,
    pub rect: SwayRect,
    pub focused: bool,
    pub fullscreen: bool,
}

impl SwaySession {
    pub fn start(config: &Config, environment: CompositorEnvironment<'_>) -> io::Result<Self> {
        require_supported_sway(&config.sway)?;
        let first = match Self::start_once(config, &environment, config.renderer) {
            Ok(session) => return Ok(session),
            Err(error) => error,
        };
        let Some(fallback) = automatic_renderer_fallback(config.renderer) else {
            return Err(first);
        };
        Self::start_once(config, &environment, fallback).map_err(|second| {
            io::Error::other(format!(
                "Sway failed with the automatic renderer ({first}) and Pixman ({second})"
            ))
        })
    }

    fn start_once(
        config: &Config,
        environment: &CompositorEnvironment<'_>,
        renderer: Renderer,
    ) -> io::Result<Self> {
        let runtime = RuntimeDirectory::create()?;
        let config_path = runtime.path.join("config");
        let generated = sway_config(
            environment.width,
            environment.height,
            config.fps,
            environment.app_window,
            xwayland_enabled(config.xwayland),
            config.xkb_model.as_deref(),
            &config.xkb_layout,
            config.xkb_variant.as_deref(),
            config.xkb_options.as_deref(),
        );
        write_private_file(&config_path, generated.as_bytes(), 0o600)?;

        let (log_read, log_write) = pipe()?;
        let log_write_clone = log_write.try_clone()?;
        let mut command = Command::new(&config.sway);
        command
            .arg("--unsupported-gpu")
            .arg("--config")
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_write_clone))
            .stderr(Stdio::from(log_write));
        sanitize_child_environment(&mut command);
        command
            .env("XDG_RUNTIME_DIR", &runtime.path)
            .env("XDG_SESSION_TYPE", "wayland")
            .env("XDG_CURRENT_DESKTOP", "sway")
            .env("XDG_SESSION_DESKTOP", "sway")
            .env("WLR_BACKENDS", "headless")
            .env("WLR_HEADLESS_OUTPUTS", "1")
            .env("WLR_LIBINPUT_NO_DEVICES", "1");
        if let Some(renderer) = renderer.as_wlroots() {
            command.env("WLR_RENDERER", renderer);
        }
        set_pulse_environment(
            &mut command,
            environment.pulse_server,
            environment.pulse_sink,
        );

        // SAFETY: only async-signal-safe libc calls are made after fork and before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command.spawn()?;
        drop(command);
        let process_group = i32::try_from(child.id())
            .map_err(|_| io::Error::other("Sway PID exceeds process-group range"))?;
        let log_path = runtime.path.join("sway.log");
        let log_thread = match start_bounded_log("vvland-sway-log", log_read, log_path.clone()) {
            Ok(thread) => thread,
            Err(error) => {
                terminate_group(process_group, &mut child);
                return Err(error);
            }
        };

        let sockets = wait_for_sockets(&runtime.path, child.id(), &mut child, READY_TIMEOUT);
        let (wayland_socket, sway_socket) = match sockets {
            Ok(sockets) => sockets,
            Err(error) => {
                terminate_group(process_group, &mut child);
                let _ = log_thread.join();
                return Err(sway_startup_error(&error, &log_path));
            }
        };
        let input = match InputChannel::connect(
            &wayland_socket,
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
                return Err(sway_startup_error(&error, &log_path));
            }
        };

        Ok(Self {
            runtime,
            child,
            pulse_server: environment.pulse_server.map(OsStr::to_owned),
            input,
            process_group,
            sway_socket,
            wayland_socket,
            launcher_sequence: 0,
            log_thread: Some(log_thread),
        })
    }

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
        &self.sway_socket
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
        let mut script = b"#!/bin/sh\nexec".to_vec();
        for argument in program {
            script.push(b' ');
            append_shell_quoted(&mut script, argument.as_os_str());
        }
        script.push(b'\n');
        self.launch_script(&script)
    }

    pub fn launch_shell_command(&mut self, command_text: &str) -> io::Result<()> {
        let mut script = b"#!/bin/sh\nexec /bin/sh -lc ".to_vec();
        append_shell_quoted(&mut script, OsStr::new(command_text));
        script.push(b'\n');
        self.launch_script(&script)
    }

    /// Launch the single application of `--app` mode.
    ///
    /// Sway execs the launcher script itself, so the child inherits Sway's environment rather
    /// than anything this process sets: the profile environment and the app's Pulse routing have
    /// to be exported inside the script.
    pub fn launch_app(&mut self, launch: &AppLaunch) -> io::Result<()> {
        if launch.program.is_empty() {
            return Ok(());
        }
        let mut script = b"#!/bin/sh\n".to_vec();
        for (name, value) in &launch.env {
            append_script_export(&mut script, name, value);
        }
        // A snap-confined browser runs behind snap's own Pulse mediation; a raw host unix socket
        // can drop it to ALSA, while PULSE_SINK alone still selects the private sink.
        if launch.snap_confined {
            if let Some(server) = &self.pulse_server {
                if is_unix_pulse_server(server) {
                    script.extend_from_slice(b"unset PULSE_SERVER\n");
                }
            }
        }
        if !child_logs_enabled() {
            script.extend_from_slice(b"exec >/dev/null 2>&1\n");
        }
        // Sway execs the script itself, so there is no child handle to probe. Record the exit
        // status instead: an application that dies on startup leaves the file behind, and
        // `confirm_app_started` turns that into an error rather than a black desktop. The file is
        // named after this launcher so an earlier launch's status can never be mistaken for it.
        let sequence = self.allocate_launcher()?;
        let status_path = self.runtime.path.join(format!("app-status-{sequence:04x}"));
        for argument in &launch.program {
            append_shell_quoted(&mut script, argument.as_os_str());
            script.push(b' ');
        }
        script.extend_from_slice(b"\nstatus=$?\nprintf '%s' \"$status\" > ");
        append_shell_quoted(&mut script, status_path.as_os_str());
        script.push(b'\n');
        self.run_launcher(sequence, &script)?;
        confirm_app_started(&status_path)
    }

    fn launch_script(&mut self, script: &[u8]) -> io::Result<()> {
        let sequence = self.allocate_launcher()?;
        self.run_launcher(sequence, script)
    }

    /// Claim the next launcher slot, enforcing the per-session launcher cap.
    fn allocate_launcher(&mut self) -> io::Result<u32> {
        self.launcher_sequence = self
            .launcher_sequence
            .checked_add(1)
            .filter(|sequence| *sequence <= MAX_LAUNCHERS)
            .ok_or_else(|| io::Error::other("Sway launcher limit exhausted"))?;
        Ok(self.launcher_sequence)
    }

    fn run_launcher(&self, sequence: u32, script: &[u8]) -> io::Result<()> {
        if script.len() > MAX_LAUNCHER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Sway launcher command exceeds 64 KiB",
            ));
        }
        let path = self.runtime.path.join(format!("launch-{sequence:04x}"));
        write_private_file(&path, script, 0o500)?;
        let command_text = format!("exec {}", path.display());
        let reply = sway_command(&self.sway_socket, command_text.as_bytes())?;
        validate_sway_reply(&reply)
    }
}

impl Drop for SwaySession {
    fn drop(&mut self) {
        let _ = self.input.shutdown();
        terminate_group(self.process_group, &mut self.child);
        if let Some(log_thread) = self.log_thread.take() {
            let _ = log_thread.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn sway_config(
    width: u32,
    height: u32,
    fps: u32,
    app_window: Option<AppWindow>,
    xwayland: bool,
    xkb_model: Option<&str>,
    xkb_layout: &str,
    xkb_variant: Option<&str>,
    xkb_options: Option<&str>,
) -> String {
    let mut config = format!(
        "xwayland {}\n\
         swaybg_command -\n\
         seat seat0 fallback true\n\
         output HEADLESS-1 mode --custom {width}x{height}@{fps}Hz scale 1\n\
         focus_follows_mouse yes\n\
         default_border pixel 1\n\
         default_floating_border pixel 1\n\
         set $mod Mod4\n\
         bindsym $mod+Shift+q kill\n\
         bindsym $mod+f fullscreen toggle\n\
         bindsym $mod+h focus left\n\
         bindsym $mod+j focus down\n\
         bindsym $mod+k focus up\n\
         bindsym $mod+l focus right\n\
         bindsym $mod+Shift+h move left\n\
         bindsym $mod+Shift+j move down\n\
         bindsym $mod+Shift+k move up\n\
         bindsym $mod+Shift+l move right\n",
        if xwayland { "enable" } else { "disable" },
    );
    for workspace in 1..=9 {
        config.push_str(&format!(
            "bindsym $mod+{workspace} workspace number {workspace}\n\
             bindsym $mod+Shift+{workspace} move container to workspace number {workspace}\n"
        ));
    }
    // Single-app mode: the one window owns the whole output from its first map, with no border
    // and no chance for a second window to tile beside it (plan D4).
    if let Some(app) = app_window {
        let app_id = sway_escape(app.app_id);
        config.push_str(&format!("for_window [app_id=\"{app_id}\"] border none\n"));
        config.push_str("for_window [title=\".*\"] border none\n");
        if app.fullscreen {
            config.push_str(&format!(
                "for_window [app_id=\"{app_id}\"] fullscreen enable\n"
            ));
            // Xwayland clients and apps whose app_id does not match the profile still need the
            // output to themselves, so match on the X11 class and on any title as a fallback.
            config.push_str(&format!(
                "for_window [class=\"{app_id}\"] fullscreen enable\n"
            ));
        }
    }
    config.push_str("input type:keyboard {\n");
    config.push_str(&format!(
        "    xkb_model \"{}\"\n    xkb_layout \"{}\"\n",
        sway_escape(xkb_model.unwrap_or("pc105")),
        sway_escape(xkb_layout)
    ));
    if let Some(variant) = xkb_variant {
        config.push_str(&format!("    xkb_variant \"{}\"\n", sway_escape(variant)));
    }
    if let Some(options) = xkb_options {
        config.push_str(&format!("    xkb_options \"{}\"\n", sway_escape(options)));
    }
    config.push_str("}\n");
    config
}

fn sway_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn set_pulse_environment(
    command: &mut Command,
    pulse_server: Option<&OsStr>,
    pulse_sink: Option<&OsStr>,
) {
    command.env(
        "PULSE_SERVER",
        pulse_server.unwrap_or_else(|| OsStr::new("unix:/dev/null")),
    );
    if let Some(sink) = pulse_sink {
        command.env("PULSE_SINK", sink);
    }
}

pub(crate) fn sway_supported(version: &str) -> bool {
    version.split_whitespace().any(|word| {
        let mut components = word.split('.');
        let major = components
            .next()
            .and_then(|value| value.parse::<u32>().ok());
        let minor = components
            .next()
            .and_then(|value| {
                value
                    .trim_end_matches(|character: char| !character.is_ascii_digit())
                    .parse::<u32>()
                    .ok()
            })
            .unwrap_or(0);
        major.is_some_and(|major| major > 1 || major == 1 && minor >= 9)
    })
}

fn require_supported_sway(program: &Path) -> io::Result<()> {
    let version = sway_version(program).map_err(|error| {
        io::Error::new(error.kind(), format!("could not execute sway: {error}"))
    })?;
    if sway_supported(&version) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("Sway 1.9 or newer is required; found {version}"),
        ))
    }
}

pub(crate) fn sway_version(program: &Path) -> io::Result<String> {
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
    let version = if stdout.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };
    Ok(version.to_owned())
}

/// Emit `export NAME='value'` with the value single-quoted, so no profile value can inject shell.
fn append_script_export(output: &mut Vec<u8>, name: &OsStr, value: &OsStr) {
    output.extend_from_slice(b"export ");
    output.extend_from_slice(name.as_bytes());
    output.push(b'=');
    append_shell_quoted(output, value);
    output.push(b'\n');
}

fn append_shell_quoted(output: &mut Vec<u8>, value: &OsStr) {
    output.push(b'\'');
    for byte in value.as_bytes() {
        if *byte == b'\'' {
            output.extend_from_slice(b"'\\''");
        } else {
            output.push(*byte);
        }
    }
    output.push(b'\'');
}

fn validate_sway_reply(reply: &[u8]) -> io::Result<()> {
    let value: serde_json::Value = serde_json::from_slice(reply)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let results = value
        .as_array()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid Sway command reply"))?;
    if results
        .iter()
        .all(|result| result.get("success").and_then(serde_json::Value::as_bool) == Some(true))
    {
        return Ok(());
    }
    let reason = results
        .iter()
        .find_map(|result| result.get("error").and_then(serde_json::Value::as_str))
        .unwrap_or("Sway rejected the command");
    Err(io::Error::other(reason.to_owned()))
}

fn sway_command(socket: &Path, payload: &[u8]) -> io::Result<Vec<u8>> {
    sway_message(socket, SWAY_IPC_COMMAND, payload)
}

fn sway_message(socket: &Path, kind: u32, payload: &[u8]) -> io::Result<Vec<u8>> {
    let payload_length = u32::try_from(payload.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Sway IPC command exceeds the protocol length",
        )
    })?;
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(IPC_TIMEOUT))?;
    stream.set_write_timeout(Some(IPC_TIMEOUT))?;
    stream.write_all(SWAY_IPC_MAGIC)?;
    stream.write_all(&payload_length.to_le_bytes())?;
    stream.write_all(&kind.to_le_bytes())?;
    stream.write_all(payload)?;
    stream.flush()?;

    let mut header = [0_u8; 14];
    stream.read_exact(&mut header)?;
    if &header[..6] != SWAY_IPC_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Sway IPC reply magic",
        ));
    }
    let reply_length = u32::from_le_bytes(
        header[6..10]
            .try_into()
            .expect("fixed-size Sway IPC length"),
    );
    let reply_type =
        u32::from_le_bytes(header[10..14].try_into().expect("fixed-size Sway IPC type"));
    if reply_type != kind {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Sway returned the wrong IPC reply type",
        ));
    }
    let reply_length = usize::try_from(reply_length).expect("u32 fits usize on supported Linux");
    if reply_length > MAX_IPC_REPLY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Sway IPC reply exceeds 1 MiB",
        ));
    }
    let mut reply = vec![0; reply_length];
    stream.read_exact(&mut reply)?;
    Ok(reply)
}

pub fn query_windows(socket: &Path) -> io::Result<Vec<SwayWindow>> {
    let reply = sway_message(socket, SWAY_IPC_GET_TREE, b"")?;
    let tree: serde_json::Value = serde_json::from_slice(&reply)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut windows = Vec::new();
    collect_windows(&tree, 0, &mut windows)?;
    Ok(windows)
}

fn collect_windows(
    node: &serde_json::Value,
    depth: usize,
    windows: &mut Vec<SwayWindow>,
) -> io::Result<()> {
    if depth > MAX_TREE_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Sway tree exceeds the nesting limit",
        ));
    }
    let app_id = optional_string(node, "app_id")?;
    let properties = node
        .get("window_properties")
        .and_then(serde_json::Value::as_object);
    let xwayland_class = properties
        .and_then(|properties| properties.get("class"))
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid_tree("Sway window class is not a string"))
        })
        .transpose()?;
    let is_view = app_id.is_some()
        || node.get("window").is_some_and(|window| !window.is_null())
        || xwayland_class.is_some();
    if is_view {
        if windows.len() >= MAX_WINDOWS {
            return Err(invalid_tree("Sway tree contains too many windows"));
        }
        let id = required_u64(node, "id")?;
        let pid = node
            .get("pid")
            .filter(|value| !value.is_null())
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|pid| u32::try_from(pid).ok())
                    .ok_or_else(|| invalid_tree("Sway window PID is out of range"))
            })
            .transpose()?;
        let rect = node
            .get("rect")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| invalid_tree("Sway window omitted its rectangle"))?;
        let x = required_i64(rect, "x")?;
        let y = required_i64(rect, "y")?;
        let width = required_u32(rect, "width")?;
        let height = required_u32(rect, "height")?;
        let title = optional_string(node, "name")?;
        windows.push(SwayWindow {
            id,
            title,
            app_id,
            xwayland_class,
            pid,
            rect: SwayRect {
                x,
                y,
                width,
                height,
            },
            focused: node
                .get("focused")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            fullscreen: node
                .get("fullscreen_mode")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
                != 0,
        });
    }
    for key in ["nodes", "floating_nodes"] {
        let Some(children) = node.get(key) else {
            continue;
        };
        let children = children
            .as_array()
            .ok_or_else(|| invalid_tree("Sway tree children are not an array"))?;
        for child in children {
            collect_windows(child, depth + 1, windows)?;
        }
    }
    Ok(())
}

fn optional_string(node: &serde_json::Value, key: &str) -> io::Result<Option<String>> {
    node.get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid_tree(format!("Sway window {key} is not a string")))
        })
        .transpose()
}

fn required_u64(node: &serde_json::Value, key: &str) -> io::Result<u64> {
    node.get(key)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| invalid_tree(format!("Sway window {key} is missing or out of range")))
}

fn required_i64(node: &serde_json::Map<String, serde_json::Value>, key: &str) -> io::Result<i64> {
    node.get(key)
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| invalid_tree(format!("Sway window rectangle {key} is out of range")))
}

fn required_u32(node: &serde_json::Map<String, serde_json::Value>, key: &str) -> io::Result<u32> {
    node.get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| invalid_tree(format!("Sway window rectangle {key} is out of range")))
}

fn invalid_tree(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn wait_for_sockets(
    runtime: &Path,
    sway_pid: u32,
    child: &mut Child,
    timeout: Duration,
) -> io::Result<(PathBuf, PathBuf)> {
    let deadline = Instant::now() + timeout;
    let sway_prefix = format!("sway-ipc.{}.{sway_pid}.", unsafe { libc::geteuid() });
    loop {
        let mut wayland = None;
        let mut sway = None;
        for entry in fs::read_dir(runtime)?.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let is_socket = entry
                .file_type()
                .map(|kind| kind.is_socket())
                .unwrap_or(false);
            if !is_socket {
                continue;
            }
            if name.starts_with("wayland-") {
                if wayland.replace(entry.path()).is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Sway created multiple Wayland sockets",
                    ));
                }
            } else if name.starts_with(&sway_prefix) {
                sway = Some(entry.path());
            }
        }
        if let (Some(wayland), Some(sway)) = (wayland, sway) {
            return Ok((wayland, sway));
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!("Sway exited with {status}")));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Sway Wayland and IPC sockets were not created",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Fail if the launched application recorded an exit status within the probe window.
fn confirm_app_started(status_path: &Path) -> io::Result<()> {
    thread::sleep(APP_LIVENESS_PROBE);
    let Ok(status) = fs::read_to_string(status_path) else {
        return Ok(());
    };
    Err(io::Error::other(format!(
        "the launched application exited immediately with status {}",
        status.trim()
    )))
}

fn sway_startup_error(error: &io::Error, log_path: &Path) -> io::Error {
    startup_error(
        format!("Sway did not become ready: {error}"),
        "Sway",
        log_path,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::os::unix::net::UnixListener;

    #[test]
    fn automatic_renderer_retries_only_for_auto() {
        assert_eq!(
            automatic_renderer_fallback(Renderer::Auto),
            Some(Renderer::Pixman)
        );
        assert_eq!(automatic_renderer_fallback(Renderer::Gles2), None);
    }

    #[test]
    fn generated_config_is_headless_and_bounded() {
        let config = sway_config(
            1280,
            720,
            30,
            None,
            false,
            Some("pc105"),
            "us",
            None,
            Some("compose:ralt"),
        );
        assert!(config.contains("output HEADLESS-1 mode --custom 1280x720@30Hz"));
        assert!(config.contains("xwayland disable"));
        assert!(config.contains("xkb_options \"compose:ralt\""));
        assert!(!config.contains("include"));
        assert!(!config.contains("exec "));
    }

    #[test]
    fn config_values_are_escaped_before_insertion() {
        let config = sway_config(640, 360, 30, None, false, None, "us\"\\layout", None, None);
        assert!(config.contains(r#"xkb_layout "us\"\\layout""#));
    }

    #[test]
    fn single_app_mode_dedicates_the_output_to_one_window() {
        let plain = sway_config(1280, 720, 30, None, false, None, "us", None, None);
        assert!(!plain.contains("for_window"));

        let app = sway_config(
            1280,
            720,
            30,
            Some(AppWindow {
                app_id: "thunar",
                fullscreen: true,
            }),
            false,
            None,
            "us",
            None,
            None,
        );
        assert!(app.contains(r#"for_window [app_id="thunar"] fullscreen enable"#));
        assert!(app.contains(r#"for_window [app_id="thunar"] border none"#));
        // Xwayland clients report a class rather than an app_id.
        assert!(app.contains(r#"for_window [class="thunar"] fullscreen enable"#));
        // A profile that does not want fullscreen still gets the borderless single window.
        let windowed = sway_config(
            1280,
            720,
            30,
            Some(AppWindow {
                app_id: "thunar",
                fullscreen: false,
            }),
            false,
            None,
            "us",
            None,
            None,
        );
        assert!(!windowed.contains("fullscreen enable"));
        assert!(windowed.contains(r#"for_window [app_id="thunar"] border none"#));
    }

    #[test]
    fn app_ids_are_escaped_before_insertion() {
        // An app_id is a static profile value today, but it lands inside a quoted config string;
        // escaping it keeps that true if profiles ever become configurable.
        let config = sway_config(
            640,
            360,
            30,
            Some(AppWindow {
                app_id: "ev\"il",
                fullscreen: true,
            }),
            false,
            None,
            "us",
            None,
            None,
        );
        assert!(config.contains(r#"app_id="ev\"il""#), "{config}");
    }

    #[test]
    fn launcher_scripts_export_profile_environment_and_quote_it() {
        let mut script = Vec::new();
        append_script_export(
            &mut script,
            OsStr::new("MOZ_ENABLE_WAYLAND"),
            OsStr::new("1"),
        );
        append_script_export(
            &mut script,
            OsStr::new("EVIL"),
            OsStr::new("a'; rm -rf /; echo '"),
        );
        let script = String::from_utf8(script).unwrap();
        assert_eq!(
            script.lines().next().unwrap(),
            "export MOZ_ENABLE_WAYLAND='1'"
        );
        // The injected command text stays inside one single-quoted word: every apostrophe is
        // closed, escaped, and reopened, so nothing in a profile value can reach the shell.
        assert_eq!(
            script.lines().nth(1).unwrap(),
            r"export EVIL='a'\''; rm -rf /; echo '\'''"
        );
    }

    #[test]
    fn shell_quoting_preserves_apostrophes_and_newlines() {
        let mut quoted = Vec::new();
        append_shell_quoted(&mut quoted, OsStr::new("a'b\nc"));
        assert_eq!(quoted, b"'a'\\''b\nc'");
    }

    #[test]
    fn command_reply_requires_every_command_to_succeed() {
        assert!(validate_sway_reply(br#"[{"success":true}]"#).is_ok());
        assert!(validate_sway_reply(br#"[{"success":false,"error":"bad"}]"#).is_err());
        assert!(validate_sway_reply(b"not-json").is_err());
    }

    #[test]
    fn sway_ipc_command_is_framed_and_bounded() {
        let runtime = RuntimeDirectory::create().unwrap();
        let socket = runtime.path.join("test-ipc");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut header = [0_u8; 14];
            stream.read_exact(&mut header).unwrap();
            assert_eq!(&header[..6], SWAY_IPC_MAGIC);
            let length = u32::from_le_bytes(header[6..10].try_into().unwrap());
            assert_eq!(u32::from_le_bytes(header[10..14].try_into().unwrap()), 0);
            let mut payload = vec![0; length as usize];
            stream.read_exact(&mut payload).unwrap();
            assert_eq!(payload, b"exec /private/launcher");
            let reply = br#"[{"success":true}]"#;
            stream.write_all(SWAY_IPC_MAGIC).unwrap();
            stream
                .write_all(&(reply.len() as u32).to_le_bytes())
                .unwrap();
            stream.write_all(&SWAY_IPC_COMMAND.to_le_bytes()).unwrap();
            stream.write_all(reply).unwrap();
        });
        let reply = sway_command(&socket, b"exec /private/launcher").unwrap();
        validate_sway_reply(&reply).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn window_query_uses_get_tree_and_returns_bounded_views() {
        let runtime = RuntimeDirectory::create().unwrap();
        let socket = runtime.path.join("window-ipc");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut header = [0_u8; 14];
            stream.read_exact(&mut header).unwrap();
            assert_eq!(&header[..6], SWAY_IPC_MAGIC);
            assert_eq!(u32::from_le_bytes(header[6..10].try_into().unwrap()), 0);
            assert_eq!(
                u32::from_le_bytes(header[10..14].try_into().unwrap()),
                SWAY_IPC_GET_TREE
            );
            let reply = br#"{"nodes":[{"id":7,"name":"Editor","app_id":"org.example.Editor","pid":99,"rect":{"x":0,"y":0,"width":640,"height":480},"focused":true,"fullscreen_mode":0,"nodes":[],"floating_nodes":[]}],"floating_nodes":[]}"#;
            stream.write_all(SWAY_IPC_MAGIC).unwrap();
            stream
                .write_all(&(reply.len() as u32).to_le_bytes())
                .unwrap();
            stream.write_all(&SWAY_IPC_GET_TREE.to_le_bytes()).unwrap();
            stream.write_all(reply).unwrap();
        });
        let windows = query_windows(&socket).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, 7);
        assert_eq!(windows[0].app_id.as_deref(), Some("org.example.Editor"));
        server.join().unwrap();
    }

    #[test]
    fn window_tree_parser_preserves_wayland_and_xwayland_identity() {
        let tree = serde_json::json!({
            "nodes": [{
                "id": 10,
                "name": "Native",
                "app_id": "org.example.Native",
                "pid": 123,
                "rect": {"x": -4, "y": 2, "width": 800, "height": 600},
                "focused": true,
                "fullscreen_mode": 1,
                "nodes": [],
                "floating_nodes": [],
            }],
            "floating_nodes": [{
                "id": 11,
                "name": "Legacy",
                "app_id": null,
                "window": 77,
                "window_properties": {"class": "LegacyApp"},
                "pid": 456,
                "rect": {"x": 5, "y": 6, "width": 320, "height": 240},
                "focused": false,
                "fullscreen_mode": 0,
                "nodes": [],
                "floating_nodes": [],
            }],
        });
        let mut windows = Vec::new();
        super::collect_windows(&tree, 0, &mut windows).unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].app_id.as_deref(), Some("org.example.Native"));
        assert_eq!(windows[0].rect.x, -4);
        assert!(windows[0].focused);
        assert!(windows[0].fullscreen);
        assert_eq!(windows[1].app_id, None);
        assert_eq!(windows[1].xwayland_class.as_deref(), Some("LegacyApp"));
    }

    #[test]
    fn window_tree_parser_rejects_unbounded_or_malformed_views() {
        let malformed = serde_json::json!({
            "id": 1,
            "app_id": "missing-rect",
            "nodes": [],
            "floating_nodes": [],
        });
        let mut windows = Vec::new();
        assert!(super::collect_windows(&malformed, 0, &mut windows).is_err());

        let empty = serde_json::json!({"nodes": [], "floating_nodes": []});
        assert!(super::collect_windows(&empty, MAX_TREE_DEPTH + 1, &mut windows).is_err());
    }

    /// D4: `--app` must bring up exactly one window that owns the whole output.
    ///
    /// Runs a real headless Sway and a real application, then reads the layout back over the
    /// compositor's own IPC rather than trusting the generated configuration. Needs a GUI
    /// application on the host, so it is opt-in.
    #[test]
    #[ignore = "requires a live Sway and the profile's application"]
    fn single_app_mode_brings_up_exactly_one_fullscreen_window() {
        // Overridable so both profiles can be exercised on a host that has each application.
        let name = std::env::var("VVLAND_TEST_APP").unwrap_or_else(|_| "google-chrome".into());
        let config = Config::try_parse_from([
            "vvland",
            "--doctor",
            "--compositor=sway",
            &format!("--app={name}"),
            "--no-audio",
        ])
        .unwrap();
        let profile = crate::linux::app::profile(&name).unwrap();
        let launch = profile.launch(&[]).unwrap();
        let mut session = SwaySession::start(
            &config,
            CompositorEnvironment {
                width: 1280,
                height: 720,
                pulse_server: None,
                pulse_sink: None,
                app_window: Some(AppWindow {
                    app_id: launch.app_id,
                    fullscreen: launch.fullscreen,
                }),
            },
        )
        .unwrap();
        session.launch_app(&launch).unwrap();

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut windows = Vec::new();
        while Instant::now() < deadline {
            windows = query_windows(&session.sway_socket).unwrap();
            if !windows.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(250));
        }
        eprintln!("observed windows: {windows:?}");
        assert_eq!(windows.len(), 1, "expected exactly one window: {windows:?}");
        let window = &windows[0];
        assert!(
            window.fullscreen,
            "{} did not take the whole output",
            window.title.as_deref().unwrap_or("<unnamed>")
        );
    }

    /// The IPC launcher has no child handle, so an application that dies on startup must be
    /// reported through the status file rather than leaving a silently black desktop.
    #[test]
    #[ignore = "requires a live Sway"]
    fn an_application_that_dies_on_startup_is_reported() {
        let config =
            Config::try_parse_from(["vvland", "--doctor", "--compositor=sway", "--no-audio"])
                .unwrap();
        let mut session = SwaySession::start(
            &config,
            CompositorEnvironment {
                width: 640,
                height: 360,
                pulse_server: None,
                pulse_sink: None,
                app_window: None,
            },
        )
        .unwrap();
        let launch = AppLaunch {
            program: vec![
                OsString::from("/bin/sh"),
                OsString::from("-c"),
                OsString::from("exit 7"),
            ],
            binary: PathBuf::from("/bin/sh"),
            env: Vec::new(),
            snap_confined: false,
            fullscreen: true,
            app_id: "test",
        };
        let error = session.launch_app(&launch).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("exited immediately"), "{message}");
        assert!(message.contains('7'), "{message}");

        // A living application produces no status file and therefore no error.
        let living = AppLaunch {
            program: vec![OsString::from("sleep"), OsString::from("30")],
            ..launch
        };
        assert!(session.launch_app(&living).is_ok());
    }

    #[test]
    fn parses_supported_sway_versions() {
        assert!(sway_supported("sway version 1.9"));
        assert!(sway_supported("sway version 1.10-dev"));
        assert!(sway_supported("sway version 2.0"));
        assert!(!sway_supported("sway version 1.8.1"));
        assert!(!sway_supported("unknown"));
    }
}
