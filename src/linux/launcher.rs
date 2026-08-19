//! Launcher scaffolding shared by every compositor backend.
//!
//! Both sessions independently grew the same private runtime directory, private file writer,
//! bounded log drain, process-group teardown, and PATH probe (plan D7). They live here once; the
//! backends keep only what genuinely differs — readiness protocols, the launcher mechanism
//! (Weston spawns directly, Sway execs through its IPC), and the input transport.

use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
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

impl RuntimeDirectory {
    pub fn create() -> io::Result<Self> {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        for _ in 0..32 {
            let mut random = [0_u8; 8];
            getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
            let path = base.join(format!("vvland-{:016x}", u64::from_be_bytes(random)));
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
/// Unified on the Sway superset and applied to both compositors and every launched program:
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
