//! Single-app mode: the profile table and everything needed to build one launch.
//!
//! "Run one Wayland app alone" means starting a headless compositor and auto-launching exactly one
//! application inside it — not embedding a compositor (plan decision 2). A profile carries the
//! discovery order, the Wayland-mode environment, the default arguments, and the compositor the
//! app prefers; `--` arguments append to the profile's own.
//!
//! The pipeline resolves the profile before the compositor probe (its preference steers the
//! probe), bakes the window rule into the generated compositor configuration, and launches the
//! application once the session is ready.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::cli::CompositorChoice;
use crate::linux::launcher::command_in_path;

/// How a named application is started inside the nested compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppProfile {
    /// The `--app` value.
    pub name: &'static str,
    /// Binary names to look for, in order of preference.
    pub binary_names: &'static [&'static str],
    /// Environment forced on the child so it takes its Wayland path.
    pub env: &'static [(&'static str, &'static str)],
    /// Arguments prepended to any `--` passthrough.
    pub args: &'static [&'static str],
    /// The compositor this app prefers; `Auto` defers to the normal probe order.
    pub compositor: CompositorChoice,
    /// Whether the single window should be made fullscreen (Sway: a `for_window` rule).
    pub fullscreen: bool,
    /// Whether snap confinement needs the raw host `PULSE_SERVER` withheld.
    pub snap_aware: bool,
    /// Whether the application needs its own D-Bus session bus.
    ///
    /// D-Bus single-instance applications hand a launch request to whichever instance already
    /// owns their name on the bus. With the host's session bus inherited, that instance is on the
    /// host's desktop: the nested compositor gets no window at all. A private bus also keeps the
    /// nested session from talking to the host's services, which is the point of running it
    /// isolated.
    pub private_dbus: bool,
    /// The Wayland `app_id` used to target the window from the compositor config.
    pub app_id: &'static str,
}

/// The built-in profile table.
///
/// Sway is preferred for app mode: it can force a single fullscreen window over IPC and draws no
/// panel, whereas Weston's `[shell]` panel cannot be reliably hidden across the 13..16 range
/// (plan D4/D5). Weston still runs the app when it is the only compositor available or when a
/// DRM flag names it.
pub const PROFILES: &[AppProfile] = &[
    AppProfile {
        name: "google-chrome",
        binary_names: &[
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
        ],
        // The proven invocation from the manual reference run (`veston.txt`).
        env: &[],
        args: &[
            "--ozone-platform=wayland",
            "--start-maximized",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-breakpad",
            "--disable-crash-reporter",
            "--disable-metrics",
            "--disable-metrics-repo-reporting",
            "--disable-component-update",
            "--disable-background-networking",
            "--disable-sync",
            "--disable-features=PassageEmbeddings,HistoryEmbeddings,OptimizationGuideModelDownloading",
        ],
        compositor: CompositorChoice::Sway,
        fullscreen: true,
        snap_aware: true,
        // Chrome's single-instance check is keyed on its user-data directory rather than the
        // session bus, and it reaches the nested compositor with the host bus inherited.
        private_dbus: false,
        app_id: "google-chrome",
    },
    AppProfile {
        name: "thunar",
        binary_names: &["thunar", "Thunar"],
        env: &[("GDK_BACKEND", "wayland")],
        args: &[],
        compositor: CompositorChoice::Sway,
        fullscreen: true,
        snap_aware: false,
        private_dbus: true,
        app_id: "thunar",
    },
];

/// Look up a profile by its `--app` name.
pub fn profile(name: &str) -> io::Result<&'static AppProfile> {
    PROFILES
        .iter()
        .find(|profile| profile.name == name)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown --app profile {name:?}; known profiles: {}",
                    known()
                ),
            )
        })
}

/// The known profile names, for help and error text.
pub fn known() -> String {
    PROFILES
        .iter()
        .map(|profile| profile.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A resolved launch: the argument vector and the environment the child needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppLaunch {
    /// The full argv, including any private-bus wrapper.
    pub program: Vec<OsString>,
    /// The application binary itself, for diagnostics.
    pub binary: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    /// Set when the resolved binary is snap-confined; snap rewrites `XDG_RUNTIME_DIR` and
    /// mediates Pulse itself, so a raw host `PULSE_SERVER` must be withheld (kitweb's finding).
    pub snap_confined: bool,
    pub fullscreen: bool,
    pub app_id: &'static str,
}

/// The wrapper that gives a launched application its own D-Bus session bus.
const PRIVATE_BUS_LAUNCHER: &str = "dbus-run-session";

impl AppProfile {
    /// Build the launch for this profile, appending any `--` passthrough arguments.
    pub fn launch(&self, passthrough: &[OsString]) -> io::Result<AppLaunch> {
        let binary = self.discover().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "--app {} found none of: {}",
                    self.name,
                    self.binary_names.join(", ")
                ),
            )
        })?;
        Ok(self.launch_with(&binary, passthrough))
    }

    /// The argv/env half of [`launch`], separated so it is testable without a real binary.
    pub fn launch_with(&self, binary: &Path, passthrough: &[OsString]) -> AppLaunch {
        self.launch_parts(
            binary,
            passthrough,
            self.private_dbus && private_bus_available(),
        )
    }

    /// [`launch_with`] with the private-bus decision supplied, so it can be tested both ways.
    fn launch_parts(
        &self,
        binary: &Path,
        passthrough: &[OsString],
        private_bus: bool,
    ) -> AppLaunch {
        let mut program = Vec::with_capacity(3 + self.args.len() + passthrough.len());
        if private_bus {
            // dbus-run-session starts a fresh bus and overrides DBUS_SESSION_BUS_ADDRESS for the
            // command it runs, so nothing else has to unset the inherited one.
            program.push(OsString::from(PRIVATE_BUS_LAUNCHER));
            program.push(OsString::from("--"));
        }
        program.push(binary.as_os_str().to_owned());
        program.extend(self.args.iter().map(OsString::from));
        program.extend(passthrough.iter().cloned());
        AppLaunch {
            program,
            binary: binary.to_owned(),
            env: self
                .env
                .iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value)))
                .collect(),
            snap_confined: self.snap_aware && is_snap_binary(binary),
            fullscreen: self.fullscreen,
            app_id: self.app_id,
        }
    }

    /// The best of this profile's binaries present on `PATH`.
    ///
    /// A non-snap binary wins over a snap one at the same preference level: snap confinement
    /// cannot reach a nested compositor's private Wayland socket (see [`SNAP_WAYLAND_GAP`]).
    pub fn discover(&self) -> Option<PathBuf> {
        prefer_non_snap(
            self.binary_names
                .iter()
                .filter(|name| command_in_path(name))
                .filter_map(|name| which_path(name))
                .collect(),
        )
    }
}

fn private_bus_available() -> bool {
    command_in_path(PRIVATE_BUS_LAUNCHER)
}

/// Pick the first non-snap candidate, falling back to the first candidate of any kind.
///
/// Snap confinement cannot reach the nested compositor's private Wayland socket
/// ([`SNAP_WAYLAND_GAP`]), so a deb or flatpak build of the same application always wins.
fn prefer_non_snap(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|path| !is_snap_binary(path))
        .or_else(|| candidates.first())
        .cloned()
}

/// Whether a resolved binary path is snap-confined.
///
/// Pure so it can be unit-tested with fixture paths; the `snap list` fallback below is the only
/// part that touches the host (kitweb `browser.rs:277-307`).
pub fn is_snap_path(path: &Path) -> bool {
    let path = path.to_string_lossy();
    path.starts_with("/snap/") || path.contains("/snap/bin/")
}

fn is_snap_binary(path: &Path) -> bool {
    if is_snap_path(path) {
        return true;
    }
    // Ubuntu ships firefox and chromium as snaps behind a /usr/bin shim, so the path alone is not
    // conclusive; ask snapd about the package that owns the file name.
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(snap_package_installed)
}

fn snap_package_installed(package: &str) -> bool {
    Command::new("snap")
        .args(["list", package])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Why a snap-confined application cannot reach the nested compositor.
///
/// Confirmed on Ubuntu with the Firefox snap: snapd rewrites `XDG_RUNTIME_DIR` to
/// `/run/user/<uid>/snap.<name>/`, and its AppArmor profile only permits Wayland sockets at the
/// standard `/run/user/<uid>/wayland-N` path. vvland's socket lives in a private 0700 directory
/// by design, so the snap's connect is denied. Putting the socket in the shared runtime directory
/// would expose the nested session to every process on the host, so vvland does not do it: the
/// fix is a non-snap build of the application (plan risk R6).
pub const SNAP_WAYLAND_GAP: &str = "is snap-confined; snap's sandbox only allows Wayland sockets at $XDG_RUNTIME_DIR/wayland-N, \
     so it cannot connect to vvland's private compositor socket. Install the non-snap (deb or \
     flatpak) build, or run it under --xwayland";

/// Whether a Pulse server address is a raw host unix socket.
///
/// Snap-confined browsers run behind snap's own Pulse mediation; handing them a raw host socket
/// can drop them to ALSA, while `PULSE_SINK` alone still selects the private sink.
pub fn is_unix_pulse_server(server: &OsStr) -> bool {
    server.to_string_lossy().starts_with("unix:")
}

fn which_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_profiles_list_the_known_ones() {
        let error = profile("emacs").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let message = error.to_string();
        assert!(message.contains("emacs"));
        for name in ["google-chrome", "thunar"] {
            assert!(message.contains(name), "{message}");
            assert_eq!(profile(name).unwrap().name, name);
        }
    }

    #[test]
    fn profile_arguments_come_before_the_passthrough() {
        let chrome = profile("google-chrome").unwrap();
        let launch = chrome.launch_with(
            Path::new("/usr/bin/google-chrome"),
            &[OsString::from("--user-data-dir=/tmp/x")],
        );
        assert_eq!(
            launch.program,
            [
                "/usr/bin/google-chrome",
                "--ozone-platform=wayland",
                "--start-maximized",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-breakpad",
                "--disable-crash-reporter",
                "--disable-metrics",
                "--disable-metrics-repo-reporting",
                "--disable-component-update",
                "--disable-background-networking",
                "--disable-sync",
                "--disable-features=PassageEmbeddings,HistoryEmbeddings,OptimizationGuideModelDownloading",
                "--user-data-dir=/tmp/x",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn profiles_force_the_wayland_backend() {
        let thunar = profile("thunar")
            .unwrap()
            .launch_with(Path::new("/usr/bin/thunar"), &[]);
        assert!(
            thunar
                .env
                .contains(&(OsString::from("GDK_BACKEND"), OsString::from("wayland")))
        );
        assert_eq!(thunar.app_id, "thunar");
        // Chrome takes its Wayland path from an argument, not the environment.
        assert!(profile("google-chrome").unwrap().env.is_empty());
    }

    #[test]
    fn a_dbus_single_instance_application_gets_its_own_bus() {
        // Without this, Thunar hands the launch to whichever instance already owns its name on
        // the inherited host bus and the nested compositor never sees a window.
        let thunar = profile("thunar").unwrap();
        assert!(thunar.private_dbus);
        let wrapped = thunar.launch_parts(Path::new("/usr/bin/thunar"), &[], true);
        assert_eq!(
            wrapped.program,
            ["dbus-run-session", "--", "/usr/bin/thunar"].map(OsString::from)
        );
        // The application binary stays reportable for diagnostics.
        assert_eq!(wrapped.binary, Path::new("/usr/bin/thunar"));

        // Without the wrapper available the application is still launched, just on the host bus.
        let bare = thunar.launch_parts(Path::new("/usr/bin/thunar"), &[], false);
        assert_eq!(bare.program, ["/usr/bin/thunar"].map(OsString::from));

        // Chrome does not need one, so it is never wrapped.
        let chrome = profile("google-chrome").unwrap();
        assert!(!chrome.private_dbus);
        assert_eq!(
            chrome
                .launch_with(Path::new("/usr/bin/google-chrome"), &[])
                .program
                .first()
                .unwrap(),
            &OsString::from("/usr/bin/google-chrome")
        );
    }

    #[test]
    fn the_private_bus_wrapper_precedes_profile_and_passthrough_arguments() {
        let thunar = profile("thunar").unwrap();
        let launch = thunar.launch_parts(
            Path::new("/usr/bin/thunar"),
            &[OsString::from("/tmp")],
            true,
        );
        assert_eq!(
            launch.program,
            ["dbus-run-session", "--", "/usr/bin/thunar", "/tmp"].map(OsString::from)
        );
    }

    #[test]
    fn discovery_prefers_a_non_snap_binary() {
        let snap = PathBuf::from("/snap/bin/chromium");
        let plain = PathBuf::from("/usr/bin/google-chrome");
        assert_eq!(
            prefer_non_snap(vec![snap.clone(), plain.clone()]),
            Some(plain.clone())
        );
        // Order within the profile does not rescue a snap when a plain build exists.
        assert_eq!(
            prefer_non_snap(vec![plain.clone(), snap.clone()]),
            Some(plain)
        );
        // A snap is still better than nothing; the launch warns rather than refusing.
        assert_eq!(prefer_non_snap(vec![snap.clone()]), Some(snap));
        assert_eq!(prefer_non_snap(Vec::new()), None);
    }

    #[test]
    fn snap_paths_are_detected_without_touching_snapd() {
        assert!(is_snap_path(Path::new("/snap/bin/chromium")));
        assert!(is_snap_path(Path::new("/snap/chromium/current/chromium")));
        assert!(is_snap_path(Path::new("/usr/local/snap/bin/chromium")));
        assert!(!is_snap_path(Path::new("/usr/bin/google-chrome")));
        assert!(!is_snap_path(Path::new("/opt/google/chrome/chrome")));

        // A snap path is confinement only for a profile that opts into snap handling.
        let launch = profile("thunar")
            .unwrap()
            .launch_with(Path::new("/snap/bin/thunar"), &[]);
        assert!(!launch.snap_confined, "thunar is not snap-aware");
        let launch = profile("google-chrome")
            .unwrap()
            .launch_with(Path::new("/snap/bin/chromium"), &[]);
        assert!(launch.snap_confined);
    }

    #[test]
    fn raw_unix_pulse_servers_are_recognized() {
        assert!(is_unix_pulse_server(OsStr::new(
            "unix:/run/user/1000/pulse/native"
        )));
        assert!(!is_unix_pulse_server(OsStr::new("tcp:127.0.0.1:4713")));
    }

    #[test]
    fn help_text_lists_every_built_in_profile() {
        // The `--app` help line names the profiles; keep it honest as the table grows.
        use clap::CommandFactory;
        let help = crate::cli::Config::command().render_long_help().to_string();
        for profile in PROFILES {
            assert!(help.contains(profile.name), "help omits {}", profile.name);
        }
    }

    #[test]
    fn every_profile_prefers_a_compositor_that_can_isolate_one_window() {
        // Weston's shell panel cannot be reliably hidden across 13..16, so app mode prefers Sway
        // wherever the profile expresses a preference (plan D4).
        for profile in PROFILES {
            assert_ne!(
                profile.compositor,
                CompositorChoice::Weston,
                "{}",
                profile.name
            );
            assert!(!profile.binary_names.is_empty(), "{}", profile.name);
            assert!(!profile.app_id.is_empty(), "{}", profile.name);
        }
    }
}
