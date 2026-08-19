//! Headless daemon lifecycle, readiness reporting, and registry publication.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use crate::cli::{Backend, CompositorChoice, Config, Renderer, Xwayland};

use super::control::ControlContext;
use super::control::server::{self, BoundControl};
use super::host::{DesktopHost, RequestedSize};
use super::launcher;
use super::runtime::{RuntimePaths, SessionRegistry};

// The readiness backstop, not a budget. vvmux's daemon is a mux and can be ready in 5 s; this
// daemon provisions a compositor, its PipeWire capture, and a Pulse sink before writing OK, and
// each `pactl` probe alone can take seconds on a real host. 30 s matches the control protocol's
// default request timeout. On timeout the daemon is left running and reports its own readiness
// when it gets there; the launcher only stops waiting.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const READINESS_LIMIT: usize = 4096;

pub fn launch(config: &Config, session: &str, foreground: bool) -> io::Result<()> {
    if foreground {
        return server(config, session, None);
    }
    launch_daemon(config, session)
}

pub fn server(config: &Config, session: &str, ready_handle: Option<i32>) -> io::Result<()> {
    let mut readiness = ReadinessWriter::from_metadata(ready_handle)?;
    let result = run_server(config, session, &mut readiness);
    if let Err(error) = &result {
        readiness.failure(error);
    }
    result
}

fn run_server(config: &Config, session: &str, readiness: &mut ReadinessWriter) -> io::Result<()> {
    let paths = RuntimePaths::for_session(session)?;
    paths.prepare_server_endpoint(session)?;
    let bound = BoundControl::bind(&paths.socket)?;
    let mut socket_guard = SocketGuard(Some(paths.socket.clone()));

    let mut host = DesktopHost::provision(config, RequestedSize::Configured)?;
    host.launch_initial(&config.program)?;
    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    let registry = paths.write_registry(session, &nonce, host.resolved(), host.dimensions())?;
    let registry_guard = RegistryGuard {
        paths,
        registry: Some(registry),
    };
    socket_guard.0 = None;

    let stopping = Arc::new(AtomicBool::new(false));
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        signal_hook::flag::register(signal, stopping.clone())?;
    }
    readiness.success()?;
    let context = ControlContext {
        session: session.to_owned(),
        fps: config.fps,
        app: config.app.clone(),
        xkb_model: config.xkb_model.clone(),
        xkb_layout: config.xkb_layout.clone(),
        xkb_variant: config.xkb_variant.clone(),
        xkb_options: config.xkb_options.clone(),
        config: config.clone(),
    };
    let result = server::run(bound, host, context, stopping);
    drop(registry_guard);
    result
}

struct SocketGuard(Option<std::path::PathBuf>);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

struct RegistryGuard {
    paths: RuntimePaths,
    registry: Option<SessionRegistry>,
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        if let Some(registry) = &self.registry {
            let _ = self.paths.remove_instance(registry);
        }
    }
}

pub(super) fn launch_daemon(config: &Config, session: &str) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let (mut readiness, writer) = readiness_pipe()?;
    let writer_descriptor = writer.as_raw_fd();
    let mut command = Command::new(executable);
    append_server_config(&mut command, config);
    command
        .arg("__server")
        .arg("--session")
        .arg(session)
        .arg("--ready-handle")
        .arg(writer_descriptor.to_string());
    if !config.program.is_empty() {
        command.arg("--").args(&config.program);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    scrub_daemon_environment(&mut command, std::env::vars_os());
    // SAFETY: only async-signal-safe libc calls are made between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            clear_close_on_exec(writer_descriptor)
        });
    }
    let child = command.spawn()?;
    drop(writer);
    readiness.wait(child, STARTUP_TIMEOUT)
}

fn append_server_config(command: &mut Command, config: &Config) {
    command.arg(format!(
        "--compositor={}",
        compositor_name(config.compositor)
    ));
    command.arg(format!("--backend={}", backend_name(config.backend)));
    command.arg("--weston").arg(&config.weston);
    command.arg("--sway").arg(&config.sway);
    command.arg(format!("--renderer={}", renderer_name(config.renderer)));
    command.arg(format!("--fps={}", config.fps));
    command.arg(format!("--bitrate={}", config.bitrate));
    command.arg(format!("--gop-seconds={}", config.gop_seconds));
    command.arg(format!(
        "--max-access-unit-bytes={}",
        config.max_access_unit_bytes
    ));
    command.arg(format!("--xwayland={}", xwayland_name(config.xwayland)));
    command.arg(format!("--xkb-layout={}", config.xkb_layout));
    if let Some(app) = &config.app {
        command.arg("--app").arg(app);
    }
    if let Some(value) = &config.drm_device {
        command.arg("--drm-device").arg(value);
    }
    if let Some(value) = &config.drm_output {
        command.arg("--drm-output").arg(value);
    }
    if let Some(value) = config.width {
        command.arg(format!("--width={value}"));
    }
    if let Some(value) = config.height {
        command.arg(format!("--height={value}"));
    }
    if config.no_audio {
        command.arg("--no-audio");
    }
    if config.require_audio {
        command.arg("--require-audio");
    }
    if let Some(value) = &config.audio_capture_server {
        command.arg("--audio-capture-server").arg(value);
    }
    if let Some(value) = &config.xkb_model {
        command.arg("--xkb-model").arg(value);
    }
    if let Some(value) = &config.xkb_variant {
        command.arg("--xkb-variant").arg(value);
    }
    if let Some(value) = &config.xkb_options {
        command.arg("--xkb-options").arg(value);
    }
}

fn compositor_name(value: CompositorChoice) -> &'static str {
    match value {
        CompositorChoice::Auto => "auto",
        CompositorChoice::Weston => "weston",
        CompositorChoice::Sway => "sway",
    }
}

fn backend_name(value: Backend) -> &'static str {
    match value {
        Backend::Auto => "auto",
        Backend::Drm => "drm",
        Backend::Headless => "headless",
    }
}

fn renderer_name(value: Renderer) -> &'static str {
    match value {
        Renderer::Auto => "auto",
        Renderer::Gl => "gl",
        Renderer::Gles2 => "gles2",
        Renderer::Vulkan => "vulkan",
        Renderer::Pixman => "pixman",
    }
}

fn xwayland_name(value: Xwayland) -> &'static str {
    match value {
        Xwayland::Auto => "auto",
        Xwayland::On => "on",
        Xwayland::Off => "off",
    }
}

fn scrub_daemon_environment(
    command: &mut Command,
    environment: impl IntoIterator<Item = (OsString, OsString)>,
) {
    for (key, _) in environment {
        let key_text = key.to_string_lossy();
        if key_text.starts_with("VIVID_")
            || matches!(key_text.as_ref(), "TMUX" | "TMUX_PANE" | "STY")
        {
            command.env_remove(&key);
        }
    }
}

struct ReadinessWriter {
    file: Option<File>,
}

impl ReadinessWriter {
    fn from_metadata(handle: Option<i32>) -> io::Result<Self> {
        let Some(descriptor) = handle else {
            return Ok(Self { file: None });
        };
        if descriptor <= libc::STDERR_FILENO {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "readiness descriptor is not private",
            ));
        }
        // SAFETY: status is a valid output buffer and descriptor was supplied by argv.
        let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe { libc::fstat(descriptor, &mut status) } == -1 {
            return Err(io::Error::last_os_error());
        }
        if status.st_mode & libc::S_IFMT != libc::S_IFIFO {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "readiness descriptor is not a pipe",
            ));
        }
        set_close_on_exec(descriptor)?;
        // SAFETY: validation above establishes an owned inherited pipe descriptor.
        let file = unsafe { File::from_raw_fd(descriptor) };
        Ok(Self { file: Some(file) })
    }

    fn success(&mut self) -> io::Result<()> {
        self.write_result(b"OK\n")
    }

    fn failure(&mut self, error: &io::Error) {
        let mut bytes = format!("ERR\n{error}").into_bytes();
        bytes.truncate(READINESS_LIMIT);
        let _ = self.write_result(&bytes);
    }

    fn write_result(&mut self, bytes: &[u8]) -> io::Result<()> {
        let Some(mut file) = self.file.take() else {
            return Ok(());
        };
        file.write_all(bytes)?;
        file.flush()
    }
}

struct ReadinessReader {
    reader: File,
}

impl ReadinessReader {
    fn wait(&mut self, mut child: Child, timeout: Duration) -> io::Result<()> {
        let bytes = self.read_result(timeout)?;
        if bytes == b"OK\n" {
            return Ok(());
        }
        let _ = child.wait();
        if let Some(message) = bytes.strip_prefix(b"ERR\n") {
            Err(io::Error::other(format!(
                "vvland server startup failed: {}",
                String::from_utf8_lossy(message)
            )))
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "vvland server exited without a readiness result",
            ))
        }
    }

    fn read_result(&mut self, timeout: Duration) -> io::Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 256];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "vvland server startup timed out",
                ));
            }
            if !poll_readable(self.reader.as_raw_fd(), remaining)? {
                continue;
            }
            match self.reader.read(&mut chunk) {
                Ok(0) => return Ok(bytes),
                Ok(read) => {
                    if bytes.len() + read > READINESS_LIMIT {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "vvland startup diagnostic exceeded 4 KiB",
                        ));
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    if bytes == b"OK\n" {
                        return Ok(bytes);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }
}

fn readiness_pipe() -> io::Result<(ReadinessReader, OwnedFd)> {
    let (reader, writer) = launcher::pipe()?;
    let reader = File::from(relocate(reader)?);
    let writer = relocate(writer)?;
    Ok((ReadinessReader { reader }, writer))
}

fn relocate(descriptor: OwnedFd) -> io::Result<OwnedFd> {
    if descriptor.as_raw_fd() > libc::STDERR_FILENO {
        return Ok(descriptor);
    }
    // SAFETY: fcntl duplicates a valid owned descriptor.
    let duplicated = unsafe {
        libc::fcntl(
            descriptor.as_raw_fd(),
            libc::F_DUPFD_CLOEXEC,
            libc::STDERR_FILENO + 1,
        )
    };
    if duplicated == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_DUPFD_CLOEXEC returned a new owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

fn clear_close_on_exec(descriptor: RawFd) -> io::Result<()> {
    update_close_on_exec(descriptor, false)
}

fn set_close_on_exec(descriptor: RawFd) -> io::Result<()> {
    update_close_on_exec(descriptor, true)
}

fn update_close_on_exec(descriptor: RawFd, enabled: bool) -> io::Result<()> {
    // SAFETY: fcntl is called on the inherited descriptor.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    let flags = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn poll_readable(descriptor: RawFd, timeout: Duration) -> io::Result<bool> {
    let mut poll = libc::pollfd {
        fd: descriptor,
        events: libc::POLLIN,
        revents: 0,
    };
    let milliseconds = i32::try_from(timeout.as_millis().max(1)).unwrap_or(i32::MAX);
    match unsafe { libc::poll(&mut poll, 1, milliseconds) } {
        -1 => {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error)
            }
        }
        0 => Ok(false),
        _ => Ok(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_environment_scrubs_outer_session_and_vivid_values() {
        let mut command = Command::new("true");
        command.env("KEEP_ME", "yes");
        scrub_daemon_environment(
            &mut command,
            [
                (
                    OsString::from("VIVID_ROOT_SECRET"),
                    OsString::from("secret"),
                ),
                (OsString::from("TMUX"), OsString::from("outer")),
                (OsString::from("KEEP_ME"), OsString::from("yes")),
            ],
        );
        let debug = format!("{command:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("outer"));
    }
}
