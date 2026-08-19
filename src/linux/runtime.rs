//! Owner-only rendezvous and process identity for headless vvland sessions.
//!
//! A registry is not proof that a PID still names the daemon that wrote it. Every liveness and
//! teardown decision therefore includes Linux process birth time, and every deletion compares the
//! complete recorded instance before touching its socket or registry.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::compositor::ResolvedCompositor;

pub const CONTROL_PROTOCOL_VERSION: u16 = 1;
const REGISTRY_SCHEMA: u32 = 2;
const MAX_REGISTRY_BYTES: u64 = 16 * 1024;
const MIN_DIMENSION: u32 = 64;
const MAX_DIMENSION: u32 = 8192;

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub socket: PathBuf,
    pub registry: PathBuf,
    endpoint_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "platform", rename_all = "snake_case")]
pub enum ProcessBirth {
    Linux { start_ticks: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionRegistry {
    pub schema: u32,
    pub name: String,
    pub pid: u32,
    pub instance_nonce: String,
    pub vvland_version: String,
    pub protocol_version: u16,
    pub endpoint_id: String,
    pub process_birth: ProcessBirth,
    pub socket: PathBuf,
    pub compositor: String,
    pub width: u32,
    pub height: u32,
}

impl RuntimePaths {
    pub fn for_session(name: &str) -> io::Result<Self> {
        validate_session_name(name)?;
        Self::for_session_in_root(name, &runtime_root()?)
    }

    fn for_session_in_root(name: &str, root: &Path) -> io::Result<Self> {
        validate_session_name(name)?;
        ensure_private_directory(root, effective_uid())?;
        let hash = hex(&Sha256::digest(name.as_bytes())[..16]);
        let socket = root.join(format!("session-{hash}.sock"));
        let registry = root.join(format!("session-{hash}.json"));
        let endpoint_id = hex(&domain_hash(
            b"vvland endpoint identity v1\0",
            socket.as_os_str().as_bytes(),
        ));
        Ok(Self {
            socket,
            registry,
            endpoint_id,
        })
    }

    /// Publish rendezvous after the H5 server has bound its control socket.
    #[allow(dead_code)]
    pub fn write_registry(
        &self,
        name: &str,
        nonce: &[u8; 32],
        compositor: ResolvedCompositor,
        dimensions: (u32, u32),
    ) -> io::Result<SessionRegistry> {
        let registry = SessionRegistry {
            schema: REGISTRY_SCHEMA,
            name: name.to_owned(),
            pid: std::process::id(),
            instance_nonce: hex(nonce),
            vvland_version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: CONTROL_PROTOCOL_VERSION,
            endpoint_id: self.endpoint_id.clone(),
            process_birth: process_birth(std::process::id())?,
            socket: self.socket.clone(),
            compositor: compositor_name(compositor).to_owned(),
            width: dimensions.0,
            height: dimensions.1,
        };
        validate_registry(&registry)?;
        self.validate_identity(&registry)?;
        write_registry_file(&self.registry, &registry)?;
        Ok(registry)
    }

    pub fn read_registry(&self) -> io::Result<SessionRegistry> {
        let registry = read_registry_file(&self.registry)?;
        self.validate_identity(&registry)?;
        Ok(registry)
    }

    /// Reserve this session's rendezvous names, reaping only a proven-stale prior instance.
    pub fn prepare_server_endpoint(&self, name: &str) -> io::Result<()> {
        match self.read_registry() {
            Ok(registry) if registry_process_matches(&registry) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("vvland session {name:?} is already running"),
                ));
            }
            Ok(registry) => {
                self.remove_instance(&registry)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if self.socket.exists() {
                    match UnixStream::connect(&self.socket) {
                        Ok(_) => {
                            return Err(io::Error::new(
                                io::ErrorKind::AlreadyExists,
                                format!("vvland session {name:?} is already starting"),
                            ));
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                            ) =>
                        {
                            match fs::remove_file(&self.socket) {
                                Ok(()) => {}
                                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                                Err(error) => return Err(error),
                            }
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn validate_identity(&self, registry: &SessionRegistry) -> io::Result<()> {
        if registry.socket != self.socket || registry.endpoint_id != self.endpoint_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session registry endpoint identity does not match its session name",
            ));
        }
        Ok(())
    }

    /// Remove artifacts only if the registry still records the exact expected daemon instance.
    pub fn remove_instance(&self, expected: &SessionRegistry) -> io::Result<bool> {
        let actual = match self.read_registry() {
            Ok(actual) => actual,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if !same_instance(&actual, expected) {
            return Ok(false);
        }

        // Revalidate immediately before each mutation. A replacement daemon may legitimately use
        // the same session paths, and teardown for the old instance must never remove its files.
        if self
            .read_registry()
            .is_ok_and(|current| same_instance(&current, expected))
        {
            match fs::remove_file(&self.socket) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        } else {
            return Ok(false);
        }
        if self
            .read_registry()
            .is_ok_and(|current| same_instance(&current, expected))
        {
            fs::remove_file(&self.registry)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub fn validate_session_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('.')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session name must be 1-64 ASCII letters, digits, '.', '-' or '_' and not start '.'",
        ));
    }
    Ok(())
}

pub fn list_registries() -> io::Result<Vec<SessionRegistry>> {
    let root = runtime_root()?;
    list_registries_in_root(&root)
}

fn list_registries_in_root(root: &Path) -> io::Result<Vec<SessionRegistry>> {
    let mut sessions = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !file_name.starts_with("session-") || !file_name.ends_with(".json") {
            continue;
        }
        let Ok(registry) = read_registry_file(&entry.path()) else {
            continue;
        };
        let Ok(paths) = RuntimePaths::for_session_in_root(&registry.name, root) else {
            continue;
        };
        if paths.registry != entry.path() || paths.validate_identity(&registry).is_err() {
            continue;
        }
        if registry_process_matches(&registry) {
            sessions.push(registry);
        } else {
            let _ = paths.remove_instance(&registry);
        }
    }
    sessions.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(sessions)
}

pub fn print_sessions() -> io::Result<()> {
    for session in list_registries()? {
        println!(
            "{}\tpid {}\t{} {}x{}",
            session.name, session.pid, session.compositor, session.width, session.height
        );
    }
    Ok(())
}

pub fn terminate_session(name: &str) -> io::Result<()> {
    validate_session_name(name)?;
    let paths = RuntimePaths::for_session(name)?;
    let registry = paths.read_registry().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("vvland session {name:?} does not exist"),
            )
        } else {
            error
        }
    })?;
    if !registry_process_matches(&registry) {
        let _ = paths.remove_instance(&registry);
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("vvland session {name:?} is no longer running"),
        ));
    }

    let pidfd = open_pidfd(registry.pid).inspect_err(|error| {
        if error.raw_os_error() == Some(libc::ESRCH) {
            let _ = paths.remove_instance(&registry);
        }
    })?;
    // Opening a pidfd pins the process object. Re-read `/proc` afterward so a PID recycled between
    // the first liveness check and pidfd_open cannot receive the signal.
    if !process_birth(registry.pid).is_ok_and(|actual| actual == registry.process_birth) {
        let _ = paths.remove_instance(&registry);
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("vvland session {name:?} is no longer running"),
        ));
    }
    send_pidfd_signal(&pidfd, libc::SIGTERM)
}

pub fn registry_process_matches(registry: &SessionRegistry) -> bool {
    process_birth(registry.pid).is_ok_and(|actual| actual == registry.process_birth)
}

fn read_registry_file(path: &Path) -> io::Result<SessionRegistry> {
    let bytes = read_registry_bytes(path)?;
    decode_registry(&bytes)
}

fn decode_registry(bytes: &[u8]) -> io::Result<SessionRegistry> {
    let registry: SessionRegistry = serde_json::from_slice(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid vvland session registry: {error}"),
        )
    })?;
    validate_registry(&registry)?;
    Ok(registry)
}

fn validate_registry(registry: &SessionRegistry) -> io::Result<()> {
    validate_session_name(&registry.name)?;
    let valid = registry.schema == REGISTRY_SCHEMA
        && registry.pid != 0
        && is_lower_hex(&registry.instance_nonce, 64)
        && !registry.vvland_version.is_empty()
        && registry.vvland_version.len() <= 64
        && registry.protocol_version == CONTROL_PROTOCOL_VERSION
        && is_lower_hex(&registry.endpoint_id, 64)
        && registry.socket.is_absolute()
        && matches!(registry.compositor.as_str(), "weston" | "sway")
        && (MIN_DIMENSION..=MAX_DIMENSION).contains(&registry.width)
        && (MIN_DIMENSION..=MAX_DIMENSION).contains(&registry.height);
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "vvland session registry fields are invalid",
        ));
    }
    Ok(())
}

fn read_registry_bytes(path: &Path) -> io::Result<Vec<u8>> {
    let uid = effective_uid();
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !safe_registry_metadata(metadata.is_file(), metadata.uid(), metadata.mode(), uid) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe vvland session registry",
        ));
    }
    read_bounded_registry(&mut file, metadata.len())
}

fn safe_registry_metadata(is_file: bool, uid: u32, mode: u32, expected_uid: u32) -> bool {
    is_file && uid == expected_uid && mode & 0o077 == 0
}

fn read_bounded_registry(reader: &mut impl Read, length: u64) -> io::Result<Vec<u8>> {
    if length > MAX_REGISTRY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "vvland session registry exceeds 16 KiB",
        ));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    reader
        .take(MAX_REGISTRY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REGISTRY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "vvland session registry exceeds 16 KiB",
        ));
    }
    Ok(bytes)
}

#[allow(dead_code)] // Called by `write_registry`, whose production caller lands in H5.
fn write_registry_file(path: &Path, registry: &SessionRegistry) -> io::Result<()> {
    let bytes = serde_json::to_vec(registry).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_REGISTRY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "vvland session registry exceeds 16 KiB",
        ));
    }
    let temporary = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        &registry.instance_nonce[..16]
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true).mode(0o600);
    let mut file = options.open(&temporary)?;
    let result = file
        .write_all(&bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            drop(file);
            fs::rename(&temporary, path)
        });
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn same_instance(left: &SessionRegistry, right: &SessionRegistry) -> bool {
    left.schema == right.schema
        && left.name == right.name
        && left.pid == right.pid
        && left.instance_nonce == right.instance_nonce
        && left.endpoint_id == right.endpoint_id
        && left.process_birth == right.process_birth
        && left.socket == right.socket
}

fn process_birth(pid: u32) -> io::Result<ProcessBirth> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let end = stat.rfind(") ").ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "malformed Linux process stat")
    })?;
    let start_ticks = stat[end + 2..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Linux process start time is missing",
            )
        })?
        .parse::<u64>()
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Linux process start time is invalid",
            )
        })?;
    Ok(ProcessBirth::Linux { start_ticks })
}

fn runtime_root() -> io::Result<PathBuf> {
    let uid = effective_uid();
    let root = if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(path);
        validate_runtime_parent(&path, uid)?;
        path.join("vvland")
    } else {
        PathBuf::from(format!("/tmp/vvland-{uid}"))
    };
    ensure_private_directory(&root, uid)?;
    Ok(root)
}

fn validate_runtime_parent(path: &Path, uid: u32) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "XDG_RUNTIME_DIR must be an owner-controlled directory",
        ));
    }
    Ok(())
}

fn ensure_private_directory(path: &Path, uid: u32) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != uid
                || metadata.mode() & 0o077 != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("unsafe vvland runtime directory {}", path.display()),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            let metadata = fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != uid
                || metadata.mode() & 0o077 != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "runtime directory ownership changed during creation",
                ));
            }
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

fn open_pidfd(pid: u32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: pidfd_open returned a new owned descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }
}

fn send_pidfd_signal(pidfd: &OwnedFd, signal: i32) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[allow(dead_code)] // Called by `write_registry`, whose production caller lands in H5.
fn compositor_name(compositor: ResolvedCompositor) -> &'static str {
    match compositor {
        ResolvedCompositor::Weston => "weston",
        ResolvedCompositor::Sway => "sway",
    }
}

fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn domain_hash(domain: &[u8], value: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(value);
    hash.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_lower_hex(value: &str, expected_length: usize) -> bool {
    value.len() == expected_length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let mut random = [0_u8; 8];
            getrandom::fill(&mut random).unwrap();
            let path = std::env::temp_dir().join(format!(
                "vvland-runtime-test-{}-{}",
                std::process::id(),
                hex(&random)
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn session_names_cannot_be_paths() {
        for invalid in ["", "..", "../x", "/tmp/x", ".hidden", "contains space"] {
            assert!(validate_session_name(invalid).is_err(), "{invalid}");
        }
        let too_long = "x".repeat(65);
        assert!(validate_session_name(&too_long).is_err());
        for valid in ["default", "work-1", "project.dev", "a_b"] {
            validate_session_name(valid).unwrap();
        }
    }

    #[test]
    fn registry_round_trip_binds_the_expected_endpoint() {
        let root = TestRoot::new();
        let paths = RuntimePaths::for_session_in_root("work", &root.0).unwrap();
        let registry = paths
            .write_registry("work", &[7; 32], ResolvedCompositor::Sway, (1920, 1080))
            .unwrap();
        assert_eq!(paths.read_registry().unwrap(), registry);
        assert_eq!(registry.process_birth, process_birth(registry.pid).unwrap());
        assert_eq!(registry.protocol_version, CONTROL_PROTOCOL_VERSION);
        assert_eq!(registry.socket, paths.socket);
    }

    #[test]
    fn schema_and_protocol_mismatches_are_rejected() {
        let root = TestRoot::new();
        let paths = RuntimePaths::for_session_in_root("work", &root.0).unwrap();
        let registry = paths
            .write_registry("work", &[8; 32], ResolvedCompositor::Weston, (800, 600))
            .unwrap();
        for schema in [0, 3] {
            let mut invalid = registry.clone();
            invalid.schema = schema;
            assert!(decode_registry(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
        let mut invalid = registry;
        invalid.protocol_version += 1;
        assert!(decode_registry(&serde_json::to_vec(&invalid).unwrap()).is_err());
    }

    #[test]
    fn registry_reads_reject_unsafe_metadata_and_oversize_files() {
        assert!(!safe_registry_metadata(true, 41, 0o100600, 42));
        assert!(!safe_registry_metadata(true, 42, 0o100640, 42));
        assert!(safe_registry_metadata(true, 42, 0o100600, 42));

        let root = TestRoot::new();
        let paths = RuntimePaths::for_session_in_root("work", &root.0).unwrap();
        fs::write(&paths.registry, vec![b'x'; MAX_REGISTRY_BYTES as usize + 1]).unwrap();
        fs::set_permissions(&paths.registry, fs::Permissions::from_mode(0o600)).unwrap();
        let error = paths.read_registry().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        fs::set_permissions(&paths.registry, fs::Permissions::from_mode(0o640)).unwrap();
        let error = paths.read_registry().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        fs::remove_file(&paths.registry).unwrap();
        let target = root.0.join("target.json");
        fs::write(&target, b"{}").unwrap();
        std::os::unix::fs::symlink(&target, &paths.registry).unwrap();
        assert!(paths.read_registry().is_err());
    }

    #[test]
    fn runtime_roots_reject_group_access_and_symlinks() {
        let root = TestRoot::new();
        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(RuntimePaths::for_session_in_root("work", &root.0).is_err());

        let target = TestRoot::new();
        let link = target.0.with_extension("link");
        std::os::unix::fs::symlink(&target.0, &link).unwrap();
        assert!(RuntimePaths::for_session_in_root("work", &link).is_err());
        fs::remove_file(link).unwrap();
    }

    #[test]
    fn birth_mismatch_is_not_alive() {
        let mut registry = test_registry("work", PathBuf::from("/tmp/work.sock"));
        let ProcessBirth::Linux { start_ticks } = &mut registry.process_birth;
        *start_ticks = start_ticks.wrapping_add(1);
        assert!(!registry_process_matches(&registry));
    }

    #[test]
    fn server_endpoint_reservation_never_unlinks_a_live_startup() {
        let root = TestRoot::new();
        let paths = RuntimePaths::for_session_in_root("work", &root.0).unwrap();
        let listener = match std::os::unix::net::UnixListener::bind(&paths.socket) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("SKIPPED (sandbox): rerun where socket creation is permitted");
                return;
            }
            Err(error) => panic!("socket bind failed: {error}"),
        };
        let error = paths.prepare_server_endpoint("work").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(paths.socket.exists());

        drop(listener);
        paths.prepare_server_endpoint("work").unwrap();
        assert!(!paths.socket.exists());
    }

    #[test]
    fn exact_instance_removal_preserves_other_owners_and_reused_ids() {
        let root = TestRoot::new();
        let a = RuntimePaths::for_session_in_root("a", &root.0).unwrap();
        let b = RuntimePaths::for_session_in_root("b", &root.0).unwrap();
        let registry_a = a
            .write_registry("a", &[1; 32], ResolvedCompositor::Sway, (800, 600))
            .unwrap();
        let mut registry_b = b
            .write_registry("b", &[2; 32], ResolvedCompositor::Weston, (1024, 768))
            .unwrap();
        assert_eq!(registry_a.pid, registry_b.pid);
        let ProcessBirth::Linux { start_ticks } = &mut registry_b.process_birth;
        *start_ticks = start_ticks.wrapping_add(1);
        write_registry_file(&b.registry, &registry_b).unwrap();
        let b_before = fs::read(&b.registry).unwrap();

        assert!(a.remove_instance(&registry_a).unwrap());
        assert!(!a.registry.exists());
        assert_eq!(fs::read(&b.registry).unwrap(), b_before);

        let replacement = a
            .write_registry("a", &[3; 32], ResolvedCompositor::Sway, (800, 600))
            .unwrap();
        assert!(!a.remove_instance(&registry_a).unwrap());
        assert_eq!(a.read_registry().unwrap(), replacement);
    }

    #[test]
    fn listing_reaps_only_stale_instances_and_sorts_survivors() {
        let root = TestRoot::new();
        let live_b = RuntimePaths::for_session_in_root("b", &root.0).unwrap();
        let live_a = RuntimePaths::for_session_in_root("a", &root.0).unwrap();
        let stale = RuntimePaths::for_session_in_root("stale", &root.0).unwrap();
        live_b
            .write_registry("b", &[4; 32], ResolvedCompositor::Sway, (800, 600))
            .unwrap();
        live_a
            .write_registry("a", &[5; 32], ResolvedCompositor::Weston, (1024, 768))
            .unwrap();
        let mut stale_registry = stale
            .write_registry("stale", &[6; 32], ResolvedCompositor::Sway, (1280, 720))
            .unwrap();
        let ProcessBirth::Linux { start_ticks } = &mut stale_registry.process_birth;
        *start_ticks = start_ticks.wrapping_add(1);
        write_registry_file(&stale.registry, &stale_registry).unwrap();

        let sessions = list_registries_in_root(&root.0).unwrap();
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(!stale.registry.exists());
        assert!(live_a.registry.exists());
        assert!(live_b.registry.exists());
    }

    #[test]
    fn current_process_birth_is_stable() {
        assert_eq!(
            process_birth(std::process::id()).unwrap(),
            process_birth(std::process::id()).unwrap()
        );
    }

    fn test_registry(name: &str, socket: PathBuf) -> SessionRegistry {
        SessionRegistry {
            schema: REGISTRY_SCHEMA,
            name: name.into(),
            pid: std::process::id(),
            instance_nonce: "01".repeat(32),
            vvland_version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: CONTROL_PROTOCOL_VERSION,
            endpoint_id: "02".repeat(32),
            process_birth: process_birth(std::process::id()).unwrap(),
            socket,
            compositor: "sway".into(),
            width: 800,
            height: 600,
        }
    }
}
