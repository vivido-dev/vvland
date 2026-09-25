//! Launcher scaffolding shared by every compositor backend.
//!
//! The sessions independently grew the same private runtime directory, private file writer,
//! bounded log drain, process-group teardown, and PATH probe (plan D7). They live here once; the
//! backends keep only what genuinely differs — readiness protocols, the launcher mechanism
//! (Weston and Hyprland spawn directly, Sway execs through its IPC), and the input transport.

use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::Xwayland;

pub const MAX_LOG_BYTES: u64 = 1_048_576;
pub const ERROR_LOG_BYTES: usize = 8_192;

/// A private 0700 directory holding the compositor socket, generated config, and bounded log.
pub struct RuntimeDirectory {
    pub path: PathBuf,
}

/// The longest path a `sockaddr_un` can carry, once the NUL terminator is subtracted.
pub const MAX_UNIX_SOCKET_PATH: usize = 107;

/// `vv` plus six hex digits, and the `/` that joins it to its base.
const SHORT_NAME_LENGTH: usize = 9;

impl RuntimeDirectory {
    pub fn create() -> io::Result<Self> {
        Self::create_in(&default_base(), "vvland-", 8)
    }

    /// A private runtime directory short enough for a compositor that binds sockets below it.
    ///
    /// Hyprland binds `<directory>/hypr/<instance signature>/.socket.sock`, and the signature
    /// alone is its sixty-odd-character build hash, timestamp and nonce. The ordinary name
    /// overruns `sun_path` on a perfectly normal `/run/user/<uid>`, and Hyprland's answer to that
    /// is to log "IPC will not work" and carry on — so the room is reserved up front, and `/tmp`
    /// stands in when even a short name does not fit under `XDG_RUNTIME_DIR`.
    ///
    /// `reserve` is the longest path the caller will append, the joining `/` included.
    pub fn create_short(reserve: usize) -> io::Result<Self> {
        let mut bases = vec![default_base()];
        if bases[0] != Path::new("/tmp") {
            bases.push(PathBuf::from("/tmp"));
        }
        for base in &bases {
            if base.as_os_str().len() + SHORT_NAME_LENGTH + reserve <= MAX_UNIX_SOCKET_PATH {
                return Self::create_in(base, "vv", 3);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "no runtime directory is short enough to hold a {reserve}-byte socket path; \
                 tried {}",
                bases
                    .iter()
                    .map(|base| base.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ))
    }

    fn create_in(base: &Path, prefix: &str, random_bytes: usize) -> io::Result<Self> {
        for _ in 0..32 {
            let mut random = [0_u8; 8];
            getrandom::fill(&mut random[..random_bytes])
                .map_err(|error| io::Error::other(error.to_string()))?;
            let width = random_bytes * 2;
            let path = base.join(format!(
                "{prefix}{:0width$x}",
                u64::from_be_bytes(random) >> (64 - random_bytes * 8)
            ));
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a private vvland runtime directory",
        ))
    }
}

fn default_base() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

impl Drop for RuntimeDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn write_private_file(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

/// The largest `--extra-config` file a session will splice into its generated configuration.
///
/// The text is held in memory, written into the private runtime directory, and parsed by the
/// compositor, so it is bounded like every other caller-supplied input.
pub const MAX_EXTRA_CONFIG_BYTES: u64 = 65_536;

/// Read the `--extra-config` file that is appended to a generated compositor configuration.
///
/// The file is the session's escape hatch for directives vvland does not generate — `exec-once`
/// for a dock or a status bar, extra binds, window rules — because the generated configuration is
/// self-contained and the user's own `hyprland.conf` or `config` is never read. The bytes reach a
/// line-oriented parser verbatim, so the size is bounded and the control characters that parser
/// cannot carry are refused here rather than silently truncating a directive.
pub fn read_extra_config(path: &Path) -> io::Result<String> {
    let describe = |error: io::Error| {
        io::Error::new(
            error.kind(),
            format!("--extra-config {}: {error}", path.display()),
        )
    };
    let metadata = fs::metadata(path).map_err(describe)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("--extra-config {} is not a regular file", path.display()),
        ));
    }
    if metadata.len() > MAX_EXTRA_CONFIG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "--extra-config {} is {} bytes; the limit is {MAX_EXTRA_CONFIG_BYTES}",
                path.display(),
                metadata.len()
            ),
        ));
    }
    let text = fs::read_to_string(path).map_err(describe)?;
    if let Some(offending) = text
        .chars()
        .find(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "--extra-config {} contains {offending:?}, which a compositor configuration \
                 cannot carry",
                path.display()
            ),
        ));
    }
    Ok(text)
}

/// Append the user's extra configuration to a generated one, under a banner naming its origin.
///
/// It goes last deliberately: a compositor configuration resolves a repeated directive in favour
/// of the later line, so the escape hatch can override what vvland generated. Overriding the
/// output directives breaks capture, which is the user's call to make.
pub fn push_extra_config(config: &mut String, extra: Option<&str>) {
    let Some(extra) = extra else {
        return;
    };
    if !config.ends_with('\n') {
        config.push('\n');
    }
    config
        .push_str("# --extra-config, appended verbatim; it overrides the generated lines above.\n");
    config.push_str(extra);
    if !extra.ends_with('\n') {
        config.push('\n');
    }
}

pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1; 2];
    // SAFETY: descriptors points to exactly two writable integers.
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 returned two newly owned descriptors.
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

pub fn socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1; 2];
    // SAFETY: descriptors points to exactly two writable integers.
    let result = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            descriptors.as_mut_ptr(),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: socketpair returned two newly owned descriptors.
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

pub fn start_bounded_log(
    thread_name: &str,
    read_fd: OwnedFd,
    path: PathBuf,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name(thread_name.to_owned())
        .spawn(move || {
            let mut input = File::from(read_fd);
            let Ok(mut output) = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(path)
            else {
                return;
            };
            let mut written = 0_u64;
            let mut buffer = [0_u8; 8192];
            while let Ok(count) = input.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                if written.saturating_add(count as u64) > MAX_LOG_BYTES {
                    if output.set_len(0).is_err() || output.seek(SeekFrom::Start(0)).is_err() {
                        break;
                    }
                    written = 0;
                }
                if output.write_all(&buffer[..count]).is_err() {
                    break;
                }
                written = written.saturating_add(count as u64);
            }
        })
}

/// Append the bounded tail of a compositor log to a startup failure.
pub fn startup_error(summary: String, compositor_name: &str, log_path: &Path) -> io::Error {
    let Ok(log) = fs::read(log_path) else {
        return io::Error::other(summary);
    };
    let start = log.len().saturating_sub(ERROR_LOG_BYTES);
    let tail = String::from_utf8_lossy(&log[start..]);
    let tail = tail.trim();
    if tail.is_empty() {
        io::Error::other(summary)
    } else {
        io::Error::other(format!("{summary}; {compositor_name} log:\n{tail}"))
    }
}

pub fn terminate_group(group: i32, child: &mut Child) {
    // SAFETY: the negative PID targets only the process group created for this compositor child.
    unsafe {
        libc::kill(-group, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let _ = child.try_wait();
        if !process_group_exists(group) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: the owned process group did not exit after SIGTERM.
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
    let _ = child.wait();
}

pub fn process_group_exists(group: i32) -> bool {
    // SAFETY: signal zero only checks whether the owned process group still exists.
    if unsafe { libc::kill(-group, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().kind() == io::ErrorKind::PermissionDenied
}

pub fn command_in_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|path| {
            let candidate = path.join(program);
            candidate.is_file()
                && candidate
                    .metadata()
                    .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
        })
    })
}

pub fn xwayland_enabled(policy: Xwayland) -> bool {
    match policy {
        Xwayland::On => true,
        Xwayland::Off => false,
        Xwayland::Auto => command_in_path("Xwayland"),
    }
}

/// Strip every variable that would leak this session's credentials or the host's display.
///
/// Unified on the Sway superset and applied to every compositor and every launched program:
/// a nested client that inherits the outer `WAYLAND_DISPLAY` connects to the wrong compositor,
/// and `VIVID_*` carries the root secret. `PIPEWIRE_RUNTIME_DIR` and the private Pulse routing
/// are re-set explicitly afterwards by the backends that need them.
pub fn sanitize_child_environment(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        if remove_child_environment(&name) {
            command.env_remove(name);
        }
    }
}

pub fn remove_child_environment(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with("VIVID_")
        || name.starts_with("WLR_")
        || matches!(
            name.as_ref(),
            "WAYLAND_DISPLAY"
                | "WAYLAND_SOCKET"
                | "DISPLAY"
                | "SWAYSOCK"
                | "I3SOCK"
                // An outer Hyprland's instance signature would point a nested client's `hyprctl`
                // at the host compositor rather than this session's.
                | "HYPRLAND_INSTANCE_SIGNATURE"
                | "HYPRLAND_CMD"
                | "PULSE_SERVER"
                | "PULSE_SINK"
                | "PULSE_SOURCE"
        )
}

/// How long a launched application is given to fall over before it counts as running.
const LIVENESS_PROBE: Duration = Duration::from_millis(500);

/// Whether launched children should keep their stdout/stderr.
///
/// Off by default: a chatty browser writing to the producer's terminal corrupts the very display
/// this session is streaming. `VVLAND_CHILD_LOGS` turns it back on for debugging.
pub fn child_logs_enabled() -> bool {
    std::env::var_os("VVLAND_CHILD_LOGS").is_some_and(|value| !value.is_empty())
}

pub fn child_output() -> (Stdio, Stdio) {
    if child_logs_enabled() {
        (Stdio::inherit(), Stdio::inherit())
    } else {
        (Stdio::null(), Stdio::null())
    }
}

/// Confirm a freshly spawned application did not exit immediately.
///
/// A Wayland client that cannot reach the compositor — wrong `WAYLAND_DISPLAY`, missing binary
/// dependency, snap confinement refusing the socket — dies within milliseconds and otherwise
/// leaves a silent black desktop with no explanation (kitweb `browser.rs:41-44, 95-98`).
pub fn confirm_started(name: &str, child: &mut Child) -> io::Result<()> {
    thread::sleep(LIVENESS_PROBE);
    match child.try_wait()? {
        Some(status) => Err(io::Error::other(format!(
            "{name} exited immediately with status {status}"
        ))),
        None => Ok(()),
    }
}

/// Point a launched client at one session: its private runtime directory, display, session bus,
/// and Pulse routing. Shared because every direct-spawn backend needs exactly this set.
pub fn set_client_environment(
    command: &mut Command,
    runtime: &Path,
    wayland_display: &str,
    bus_address: &OsStr,
    pulse_server: Option<&OsStr>,
    pulse_sink: Option<&OsStr>,
) {
    command
        .env("XDG_RUNTIME_DIR", runtime)
        .env("WAYLAND_DISPLAY", wayland_display)
        .env("DBUS_SESSION_BUS_ADDRESS", bus_address);
    set_pulse_environment(command, pulse_server, pulse_sink);
}

/// The D-Bus daemon every session runs, and the one a `--doctor` run looks for.
pub const DBUS_DAEMON: &str = "dbus-daemon";
/// The session bus socket, below the private runtime directory.
const SESSION_BUS_SOCKET: &str = "bus";
/// How long the session bus is given to accept connections.
const SESSION_BUS_TIMEOUT: Duration = Duration::from_secs(5);

/// What the services a session bus activates need to know about the desktop they serve.
pub struct BusEnvironment<'a> {
    /// The session's private runtime directory, which also holds the bus socket.
    pub runtime: &'a Path,
    /// The display name, relative to `runtime`, clients of this desktop connect to.
    pub wayland_display: &'a str,
    /// `XDG_CURRENT_DESKTOP`, which selects the portal backend.
    pub desktop: &'a str,
    pub pulse_server: Option<&'a OsStr>,
    pub pulse_sink: Option<&'a OsStr>,
}

/// A private D-Bus session bus for one desktop.
///
/// Inheriting the host's session bus sends a nested client's service activations to the host:
/// the user's systemd starts `xdg-desktop-portal` with its own environment, which has no display
/// on a host reached over SSH, so every GTK application waits out the portal's start timeout
/// before it maps. A single-instance application also hands its launch to an instance already on
/// the host bus, and the nested desktop gets no window. A bus of its own fixes both: the daemon
/// runs no systemd activation, so it spawns services itself, and they inherit the environment set
/// here — this session's runtime directory and display.
///
/// The daemon owns a process group, so teardown reaps the services it activated with it.
pub struct SessionBus {
    child: Child,
    process_group: i32,
    address: OsString,
    log_thread: Option<thread::JoinHandle<()>>,
}

impl SessionBus {
    pub fn start(environment: BusEnvironment<'_>) -> io::Result<Self> {
        let socket = environment.runtime.join(SESSION_BUS_SOCKET);
        let address = bus_address(&socket);
        let (log_read, log_write) = pipe()?;
        let log_write_clone = log_write.try_clone()?;
        let mut listen = OsString::from("--address=");
        listen.push(&address);
        let mut command = Command::new(DBUS_DAEMON);
        command
            .args(["--session", "--nofork", "--nopidfile"])
            .arg(listen)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_write_clone))
            .stderr(Stdio::from(log_write));
        sanitize_child_environment(&mut command);
        command
            .env("XDG_RUNTIME_DIR", environment.runtime)
            .env("WAYLAND_DISPLAY", environment.wayland_display)
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            .env("XDG_SESSION_TYPE", "wayland")
            .env("XDG_CURRENT_DESKTOP", environment.desktop)
            .env("XDG_SESSION_DESKTOP", environment.desktop);
        set_pulse_environment(
            &mut command,
            environment.pulse_server,
            environment.pulse_sink,
        );
        // SAFETY: setpgid is async-signal-safe and creates a group owned by this daemon.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("could not start the session's {DBUS_DAEMON}: {error}"),
            )
        })?;
        // Release the log writers so a failure below does not wait on a reader that never ends.
        drop(command);
        let process_group = i32::try_from(child.id())
            .map_err(|_| io::Error::other("dbus-daemon PID exceeds process-group range"))?;
        let log_path = environment.runtime.join("dbus.log");
        let log_thread = match start_bounded_log("vvland-dbus-log", log_read, log_path.clone()) {
            Ok(thread) => thread,
            Err(error) => {
                terminate_group(process_group, &mut child);
                return Err(error);
            }
        };
        let mut bus = Self {
            child,
            process_group,
            address,
            log_thread: Some(log_thread),
        };
        if let Err(error) = bus.wait_ready(&socket) {
            drop(bus);
            return Err(startup_error(
                format!("the session's {DBUS_DAEMON} did not become ready: {error}"),
                DBUS_DAEMON,
                &log_path,
            ));
        }
        Ok(bus)
    }

    /// The `DBUS_SESSION_BUS_ADDRESS` of this bus.
    pub fn address(&self) -> &OsStr {
        &self.address
    }

    fn wait_ready(&mut self, socket: &Path) -> io::Result<()> {
        let deadline = Instant::now() + SESSION_BUS_TIMEOUT;
        loop {
            if UnixStream::connect(socket).is_ok() {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait()? {
                return Err(io::Error::other(format!("it exited with {status}")));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "its socket accepted no connection",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for SessionBus {
    fn drop(&mut self) {
        terminate_group(self.process_group, &mut self.child);
        if let Some(log_thread) = self.log_thread.take() {
            let _ = log_thread.join();
        }
    }
}

/// A `unix:path=` D-Bus address, with every byte outside the address grammar's
/// optionally-escaped set percent-encoded.
fn bus_address(socket: &Path) -> OsString {
    let mut address = String::from("unix:path=");
    for &byte in socket.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-_/.\\*".contains(&byte) {
            address.push(char::from(byte));
        } else {
            address.push_str(&format!("%{byte:02x}"));
        }
    }
    OsString::from(address)
}

pub fn set_pulse_environment(
    command: &mut Command,
    pulse_server: Option<&OsStr>,
    pulse_sink: Option<&OsStr>,
) {
    if let Some(server) = pulse_server {
        command.env("PULSE_SERVER", server);
    }
    if let Some(sink) = pulse_sink {
        command.env("PULSE_SINK", sink);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_config_is_read_bounded_and_single_line_safe() {
        let directory = RuntimeDirectory::create().expect("private runtime directory");
        let path = directory.path.join("extra.conf");

        fs::write(&path, "exec-once = nwg-dock-hyprland\n").expect("write extra config");
        assert_eq!(
            read_extra_config(&path).expect("readable extra config"),
            "exec-once = nwg-dock-hyprland\n"
        );

        // A NUL or an escape sequence would be truncated or reinterpreted by the parser.
        fs::write(&path, "exec-once = dock\u{1b}[2J\n").expect("write hostile extra config");
        let error = read_extra_config(&path).expect_err("control characters are refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        fs::write(&path, vec![b'#'; MAX_EXTRA_CONFIG_BYTES as usize + 1]).expect("write oversize");
        let error = read_extra_config(&path).expect_err("oversize files are refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let error =
            read_extra_config(&directory.path).expect_err("a directory is not a config file");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let error = read_extra_config(&directory.path.join("absent.conf"))
            .expect_err("a missing file is reported, not ignored");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn extra_config_is_appended_last_and_newline_terminated() {
        let mut config = "monitor = , disable\n".to_owned();
        push_extra_config(&mut config, None);
        assert_eq!(config, "monitor = , disable\n");

        // The generated body stays intact and the extra directives follow it, so a repeated
        // directive resolves in the caller's favour.
        push_extra_config(&mut config, Some("exec-once = waybar"));
        assert!(config.starts_with("monitor = , disable\n"), "{config}");
        assert!(config.ends_with("exec-once = waybar\n"), "{config}");
        assert!(config.contains("# --extra-config"), "{config}");
    }

    #[test]
    fn child_environment_filter_excludes_credentials_and_host_display() {
        // The 1.5 discovery names and the root secret must never reach a launched child, and a
        // nested client must never inherit the host compositor's display.
        for name in [
            "VIVID_ENDPOINT_CONTROL",
            "VIVID_ENDPOINT_INTERACTIVE",
            "VIVID_ENDPOINT_REALTIME",
            "VIVID_ENDPOINT_BULK",
            "VIVID_ROOT_SECRET",
            "VIVID_TOKEN",
            "WLR_RENDERER",
            "WAYLAND_DISPLAY",
            "WAYLAND_SOCKET",
            "DISPLAY",
            "SWAYSOCK",
            "I3SOCK",
            "HYPRLAND_INSTANCE_SIGNATURE",
            "HYPRLAND_CMD",
            "PULSE_SERVER",
            "PULSE_SINK",
            "PULSE_SOURCE",
        ] {
            assert!(remove_child_environment(OsStr::new(name)), "{name}");
        }
        assert!(!remove_child_environment(OsStr::new("PATH")));
        assert!(!remove_child_environment(OsStr::new("HOME")));
        assert!(!remove_child_environment(OsStr::new(
            "PIPEWIRE_RUNTIME_DIR"
        )));
    }

    #[test]
    fn sanitized_child_environment_strips_every_vivid_secret() {
        let _guard = crate::cli::tests::TEST_ENV_LOCK.lock().unwrap();
        // SAFETY: test-only environment mutation, isolated to this test's process.
        unsafe {
            std::env::set_var("VIVID_ROOT_SECRET", "0123456789abcdef0123456789abcdef");
            std::env::set_var("VIVID_ENDPOINT_CONTROL", "unix:/tmp/vivid.sock");
            std::env::set_var("SWAYSOCK", "/run/user/1000/sway.sock");
        }
        let mut command = Command::new("true");
        sanitize_child_environment(&mut command);
        for (name, value) in command.get_envs() {
            // A removed variable appears with a `None` value; a leaked one keeps its value.
            if value.is_none() {
                continue;
            }
            let name = name.to_string_lossy();
            assert!(
                !name.starts_with("VIVID_") && !name.starts_with("SWAYSOCK"),
                "child environment leaked {name}"
            );
        }
        // The ordinary environment survives.
        assert!(std::env::var_os("PATH").is_some());
    }

    #[test]
    fn child_logs_are_off_unless_explicitly_enabled() {
        let _guard = crate::cli::tests::TEST_ENV_LOCK.lock().unwrap();
        // SAFETY: test-only environment mutation, isolated to this test's process.
        unsafe { std::env::remove_var("VVLAND_CHILD_LOGS") };
        assert!(!child_logs_enabled());
        // SAFETY: test-only environment mutation, reverted below.
        unsafe { std::env::set_var("VVLAND_CHILD_LOGS", "") };
        assert!(!child_logs_enabled(), "an empty value stays off");
        // SAFETY: test-only environment mutation, reverted below.
        unsafe { std::env::set_var("VVLAND_CHILD_LOGS", "1") };
        assert!(child_logs_enabled());
        // SAFETY: test-only environment mutation, restoring the default.
        unsafe { std::env::remove_var("VVLAND_CHILD_LOGS") };
    }

    #[test]
    fn an_application_that_exits_immediately_is_reported() {
        let mut failing = Command::new("sh")
            .args(["-c", "exit 3"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let error = confirm_started("google-chrome", &mut failing).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("google-chrome"), "{message}");
        assert!(message.contains("exited immediately"), "{message}");

        let mut living = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        assert!(confirm_started("google-chrome", &mut living).is_ok());
        let _ = living.kill();
        let _ = living.wait();
    }

    #[test]
    fn bus_address_escapes_bytes_outside_the_address_grammar() {
        assert_eq!(
            bus_address(Path::new("/run/user/1000/vv83c7dd/bus")),
            "unix:path=/run/user/1000/vv83c7dd/bus"
        );
        // `,` and `;` separate address parts and `=` keys from values; a space is not allowed.
        assert_eq!(
            bus_address(Path::new("/tmp/a b,c;d=e%/bus")),
            "unix:path=/tmp/a%20b%2cc%3bd%3de%25/bus"
        );
    }

    /// Start a bus whose activatable services include one, named after the session's runtime
    /// directory, that records its environment.
    ///
    /// The service never claims its name, so the activation itself never completes; the record is
    /// all the test needs. `XDG_DATA_DIRS` points the daemon's standard service directories at it.
    fn bus_with_recording_service(data: &Path) -> (RuntimeDirectory, SessionBus) {
        let runtime = RuntimeDirectory::create().unwrap();
        // Written first: the daemon watches only the service directories present at startup.
        let services = data.join("dbus-1/services");
        fs::create_dir_all(&services).unwrap();
        let name = runtime
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace('-', "_");
        fs::write(
            services.join(format!("dev.vivido.{name}.service")),
            format!(
                "[D-BUS Service]\nName=dev.vivido.{name}\n\
                 Exec=/bin/sh -c 'env > {}/activated-env; exec sleep 30'\n",
                runtime.path.display()
            ),
        )
        .unwrap();
        let bus = SessionBus::start(BusEnvironment {
            runtime: &runtime.path,
            wayland_display: "wayland-vvland",
            desktop: "Hyprland",
            pulse_server: Some(OsStr::new("unix:/private/pulse")),
            pulse_sink: None,
        })
        .unwrap();
        (runtime, bus)
    }

    fn activate_recording_service(runtime: &RuntimeDirectory, bus: &SessionBus) {
        let name = runtime
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace('-', "_");
        let status = Command::new("dbus-send")
            .arg(format!("--bus={}", bus.address().to_string_lossy()))
            .args([
                "--type=method_call",
                "--dest=org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus.StartServiceByName",
                &format!("string:dev.vivido.{name}"),
                "uint32:0",
            ])
            .status()
            .expect("dbus-send is installed");
        assert!(status.success());
    }

    fn wait_for_file(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = fs::read_to_string(path) {
                if !text.is_empty() {
                    return text;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn session_bus_activates_services_into_this_desktop() {
        let _guard = crate::cli::tests::TEST_ENV_LOCK.lock().unwrap();
        let data = RuntimeDirectory::create().unwrap();
        // SAFETY: test-only environment mutation, reverted below under the same lock.
        unsafe {
            std::env::set_var("XDG_DATA_DIRS", &data.path);
            std::env::set_var("VIVID_ROOT_SECRET", "0123456789abcdef0123456789abcdef");
            std::env::set_var("WAYLAND_DISPLAY", "wayland-host");
        }
        let (runtime, bus) = bus_with_recording_service(&data.path);
        // SAFETY: restoring the environment changed above.
        unsafe {
            std::env::remove_var("XDG_DATA_DIRS");
            std::env::remove_var("VIVID_ROOT_SECRET");
            std::env::remove_var("WAYLAND_DISPLAY");
        }

        activate_recording_service(&runtime, &bus);
        let recorded = wait_for_file(&runtime.path.join("activated-env"));
        // Only the variables under test are ever reported: the rest of a service's environment is
        // the invoking user's, and a failure message is no place for it.
        let session_lines = recorded
            .lines()
            .filter(|line| {
                ["XDG_", "WAYLAND_", "DBUS_", "PULSE_"]
                    .iter()
                    .any(|prefix| line.starts_with(prefix))
            })
            .collect::<Vec<_>>();
        // The service reaches this desktop's display and bus, not the host's.
        let runtime_line = format!("XDG_RUNTIME_DIR={}", runtime.path.display());
        for expected in [
            runtime_line.as_str(),
            "WAYLAND_DISPLAY=wayland-vvland",
            "XDG_CURRENT_DESKTOP=Hyprland",
            "PULSE_SERVER=unix:/private/pulse",
        ] {
            assert!(
                session_lines.contains(&expected),
                "{expected} missing from {session_lines:#?}"
            );
        }
        // The daemon exports its own address, with the bus GUID appended.
        let bus_prefix = format!(
            "DBUS_SESSION_BUS_ADDRESS={},guid=",
            bus.address().to_string_lossy()
        );
        assert!(
            session_lines
                .iter()
                .any(|line| line.starts_with(&bus_prefix)),
            "{bus_prefix} missing from {session_lines:#?}"
        );
        let leaked = recorded
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name))
            .filter(|name| name.starts_with("VIVID_"))
            .collect::<Vec<_>>();
        assert!(
            leaked.is_empty(),
            "session credentials reached an activated service: {leaked:?}"
        );
    }

    #[test]
    fn dropping_one_session_bus_leaves_another_intact() {
        let _guard = crate::cli::tests::TEST_ENV_LOCK.lock().unwrap();
        let data = RuntimeDirectory::create().unwrap();
        // SAFETY: test-only environment mutation, reverted below under the same lock.
        unsafe { std::env::set_var("XDG_DATA_DIRS", &data.path) };
        // Both sessions use the same socket and display names inside their own directories.
        let (first_runtime, first) = bus_with_recording_service(&data.path);
        let (second_runtime, second) = bus_with_recording_service(&data.path);
        // SAFETY: restoring the environment changed above.
        unsafe { std::env::remove_var("XDG_DATA_DIRS") };

        activate_recording_service(&first_runtime, &first);
        wait_for_file(&first_runtime.path.join("activated-env"));
        let first_group = first.process_group;
        drop(first);

        // The daemon and the service it activated went with it.
        assert!(!process_group_exists(first_group));
        // The other session's bus still accepts connections and activates services.
        assert!(UnixStream::connect(second_runtime.path.join(SESSION_BUS_SOCKET)).is_ok());
        activate_recording_service(&second_runtime, &second);
        wait_for_file(&second_runtime.path.join("activated-env"));
    }

    #[test]
    fn runtime_directory_is_private_and_removed() {
        let runtime = RuntimeDirectory::create().unwrap();
        let path = runtime.path.clone();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(runtime);
        assert!(!path.exists());
    }

    #[test]
    fn bounded_log_never_exceeds_its_cap() {
        let runtime = RuntimeDirectory::create().unwrap();
        let log = runtime.path.join("bounded.log");
        let (read, write) = pipe().unwrap();
        let logger = start_bounded_log("vvland-test-log", read, log.clone()).unwrap();
        let mut writer = File::from(write);
        writer
            .write_all(&vec![b'x'; MAX_LOG_BYTES as usize + 1])
            .unwrap();
        drop(writer);
        logger.join().unwrap();
        assert!(fs::metadata(log).unwrap().len() <= MAX_LOG_BYTES);
    }

    #[test]
    fn termination_cleans_descendants_after_the_compositor_exits() {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30 & printf '%s\n' \"$!\""])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: setpgid is async-signal-safe and creates a group owned by this test child.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let group = i32::try_from(child.id()).unwrap();
        let mut descendant = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut descendant)
            .unwrap();
        let descendant = descendant.trim().parse::<i32>().unwrap();
        assert!(child.wait().unwrap().success());
        assert!(process_group_exists(group));

        terminate_group(group, &mut child);

        assert!(!process_group_exists(group));
        // SAFETY: signal zero only verifies that the reported descendant PID is gone.
        assert_eq!(unsafe { libc::kill(descendant, 0) }, -1);
    }
}
