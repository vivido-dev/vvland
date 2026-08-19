use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// Which compositor to run. `auto` probes the host once at startup (plan D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CompositorChoice {
    Auto,
    Weston,
    Sway,
}

/// Weston's output backend. Sway is headless-only, so this is Weston-only (plan D11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Backend {
    Auto,
    Drm,
    Headless,
}

/// The union of both compositors' renderer names.
///
/// `gl` is Weston's name and `gles2` is wlroots'; each is rejected for the other compositor when
/// the compositor was named explicitly, and degrades to automatic selection under
/// `--compositor auto` (plan D11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Renderer {
    Auto,
    Gl,
    Gles2,
    Vulkan,
    Pixman,
}

#[cfg(target_os = "linux")]
impl Renderer {
    /// Weston's `--renderer` value. wlroots' `gles2` has no Weston spelling, so it degrades to
    /// Weston's own automatic selection; `validate` rejects the combination outright whenever the
    /// compositor was named explicitly.
    pub fn as_weston(self) -> &'static str {
        match self {
            Self::Auto | Self::Gles2 => "auto",
            Self::Gl => "gl",
            Self::Vulkan => "vulkan",
            Self::Pixman => "pixman",
        }
    }

    /// wlroots' `WLR_RENDERER` value, or `None` to leave wlroots' own selection alone.
    pub fn as_wlroots(self) -> Option<&'static str> {
        match self {
            Self::Auto | Self::Gl => None,
            Self::Gles2 => Some("gles2"),
            Self::Vulkan => Some("vulkan"),
            Self::Pixman => Some("pixman"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Xwayland {
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Start a named isolated desktop and its owner-only control socket.
    Serve {
        #[arg(long)]
        session: String,
        /// Stay attached instead of starting a detached session daemon.
        #[arg(long)]
        foreground: bool,
        /// Program arguments for this headless session.
        #[arg(last = true, allow_hyphen_values = true)]
        serve_program: Vec<OsString>,
    },
    /// Send one control request to an isolated desktop.
    Msg(MsgOptions),
    /// List live isolated desktops owned by this user.
    List,
    /// Terminate one isolated desktop by exact session name.
    KillSession {
        #[arg(short = 't', long = "target")]
        target: String,
    },
    /// Internal daemon entry point. Not part of the public command line.
    #[command(name = "__server", hide = true)]
    Server {
        #[arg(long)]
        session: String,
        #[arg(long)]
        ready_handle: Option<i32>,
        #[arg(last = true, allow_hyphen_values = true)]
        server_program: Vec<OsString>,
    },
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct MsgOptions {
    /// Exact session name. Defaults through VVLAND_SESSION or unambiguous discovery.
    #[arg(short = 't', long = "target")]
    pub target: Option<String>,
    /// Connect to an explicit owner-only Unix socket.
    #[arg(long)]
    pub socket: Option<PathBuf>,
    #[command(subcommand)]
    pub message: MsgCommand,
}

#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
pub enum MsgCommand {
    Ping,
    Capabilities,
    Inspect,
    Key(MsgKey),
    Typing(MsgTyping),
    Mouse {
        #[command(subcommand)]
        action: MsgMouseAction,
    },
    Launch(MsgLaunch),
    ListWindows,
    Screenshot(MsgScreenshot),
    Wait {
        #[command(subcommand)]
        condition: MsgWaitCondition,
    },
    Subscribe(MsgSubscribe),
    Shutdown,
}

#[derive(Clone, Args, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MsgKey {
    pub key: String,
    #[arg(long, value_delimiter = ',')]
    #[serde(default)]
    pub mods: Vec<String>,
    #[arg(long, default_value_t = 1)]
    #[serde(default = "default_repeat")]
    pub repeat: u16,
    #[arg(long)]
    pub hold_ms: Option<u64>,
}

impl std::fmt::Debug for MsgKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MsgKey")
            .field("key", &"<redacted>")
            .field("mods", &self.mods.len())
            .field("repeat", &self.repeat)
            .field("hold_ms", &self.hold_ms)
            .finish()
    }
}

const fn default_repeat() -> u16 {
    1
}

#[derive(Clone, Args, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MsgTyping {
    pub text: String,
}

impl std::fmt::Debug for MsgTyping {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MsgTyping")
            .field("text", &format_args!("<{} bytes>", self.text.len()))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Left,
    Middle,
    Right,
    Side,
    Extra,
}

impl Button {
    pub fn evdev_code(self) -> u32 {
        match self {
            Self::Left => 272,
            Self::Right => 273,
            Self::Middle => 274,
            Self::Side => 275,
            Self::Extra => 276,
        }
    }
}

#[derive(Debug, Clone, Subcommand, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum MsgMouseAction {
    Move {
        x: u32,
        y: u32,
    },
    Click {
        #[arg(long, value_enum, default_value_t = Button::Left)]
        button: Button,
        #[arg(long, requires = "y")]
        x: Option<u32>,
        #[arg(long, requires = "x")]
        y: Option<u32>,
        #[arg(long, default_value_t = 1)]
        count: u8,
    },
    Down {
        #[arg(long, value_enum, default_value_t = Button::Left)]
        button: Button,
        #[arg(long, requires = "y")]
        x: Option<u32>,
        #[arg(long, requires = "x")]
        y: Option<u32>,
    },
    Up {
        #[arg(long, value_enum, default_value_t = Button::Left)]
        button: Button,
        #[arg(long, requires = "y")]
        x: Option<u32>,
        #[arg(long, requires = "x")]
        y: Option<u32>,
    },
    Drag {
        #[arg(long, value_enum, default_value_t = Button::Left)]
        button: Button,
        #[arg(long)]
        from_x: u32,
        #[arg(long)]
        from_y: u32,
        #[arg(long)]
        to_x: u32,
        #[arg(long)]
        to_y: u32,
        #[arg(long, default_value_t = 16)]
        steps: u16,
        #[arg(long, default_value_t = 8)]
        step_ms: u16,
    },
    Scroll {
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        vertical: i32,
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        horizontal: i32,
        #[arg(long, requires = "y")]
        x: Option<u32>,
        #[arg(long, requires = "x")]
        y: Option<u32>,
    },
}

#[derive(Clone, Args, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MsgLaunch {
    #[arg(long)]
    #[serde(default)]
    pub shell: bool,
    #[arg(last = true, required = true, allow_hyphen_values = true)]
    pub program: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, ValueEnum, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScreenshotFormat {
    #[default]
    Png,
    Jpeg,
    Raw,
}

#[derive(Debug, Clone, Copy, Default, ValueEnum, Serialize, Deserialize, PartialEq, Eq)]
pub enum ScreenshotScale {
    #[value(name = "1")]
    #[serde(rename = "1")]
    #[default]
    Full,
    #[value(name = "1/2")]
    #[serde(rename = "1/2")]
    Half,
    #[value(name = "1/4")]
    #[serde(rename = "1/4")]
    Quarter,
}

impl ScreenshotScale {
    pub fn denominator(self) -> u32 {
        match self {
            Self::Full => 1,
            Self::Half => 2,
            Self::Quarter => 4,
        }
    }
}

#[derive(Debug, Clone, Args, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MsgScreenshot {
    #[arg(long, value_enum, default_value_t = ScreenshotFormat::Png)]
    #[serde(default)]
    pub format: ScreenshotFormat,
    /// Write through the daemon to a new absolute path instead of stdout.
    #[arg(long)]
    pub output: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = ScreenshotScale::Full)]
    #[serde(default)]
    pub scale: ScreenshotScale,
    #[arg(long, default_value_t = 85)]
    #[serde(default = "default_screenshot_quality")]
    pub quality: u8,
    /// Reject a retained frame older than this duration.
    #[arg(long = "max-age", value_parser = parse_control_duration)]
    pub max_age_ms: Option<u64>,
    /// Wait for one newly captured frame before taking the screenshot.
    #[arg(long)]
    #[serde(default)]
    pub fresh: bool,
    #[arg(long, default_value_t = 30_000)]
    #[serde(default = "default_control_timeout")]
    pub timeout_ms: u64,
}

const fn default_screenshot_quality() -> u8 {
    85
}

const fn default_control_timeout() -> u64 {
    30_000
}

#[derive(Debug, Clone, Args, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct MsgSubscribe {
    /// Comma-separated event kinds. Omit for all non-high-rate events.
    #[arg(long, value_delimiter = ',')]
    #[serde(default)]
    pub events: Vec<String>,
    /// Resume with retained events newer than this event sequence when available.
    #[arg(long)]
    pub since_event: Option<u64>,
}

impl std::fmt::Debug for MsgLaunch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MsgLaunch")
            .field(
                "program",
                &format_args!("<{} arguments>", self.program.len()),
            )
            .field("shell", &self.shell)
            .finish()
    }
}

#[derive(Debug, Clone, Subcommand, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "condition", rename_all = "snake_case", deny_unknown_fields)]
pub enum MsgWaitCondition {
    Frame {
        #[arg(long)]
        after_frame: Option<u64>,
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
    },
    Exit {
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
    },
    ScreenChange {
        #[arg(long)]
        after_screen: Option<u64>,
        #[arg(long)]
        exact: bool,
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
    },
    ScreenStable {
        #[arg(long = "quiet", default_value = "250ms", value_parser = parse_control_duration)]
        quiet_ms: u64,
        #[arg(long)]
        after_screen: Option<u64>,
        #[arg(long)]
        exact: bool,
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
    },
    Window {
        /// Exact Wayland app_id to wait for.
        #[arg(long)]
        app_id: String,
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
    },
}

fn parse_control_duration(value: &str) -> Result<u64, String> {
    let (number, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 1_u64)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1_000)
    } else if let Some(value) = value.strip_suffix('m') {
        (value, 60_000)
    } else if let Some(value) = value.strip_suffix('h') {
        (value, 3_600_000)
    } else {
        (value, 1)
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|number| number.checked_mul(multiplier))
        .ok_or_else(|| "duration must be an integer with ms, s, m, or h suffix".to_owned())
}

#[derive(Clone, Parser)]
#[command(
    name = "vvland",
    version,
    about = "Run one Wayland app or compositor inside Vivido over Vivid",
    long_about = "Start an isolated Weston or Sway desktop on Linux, capture it, encode \
                  H.264/Opus, and stream it through the private Vivid endpoint inherited from \
                  Vivido or vvssh."
)]
pub struct Config {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Attach to a named persistent desktop, starting it if necessary.
    #[arg(long, value_name = "NAME")]
    pub session: Option<String>,

    /// Replace the current presenter when attaching to a persistent desktop.
    #[arg(long, requires = "session")]
    pub replace: bool,

    /// Which compositor to run; `auto` probes the host.
    #[arg(long, value_enum, default_value_t = CompositorChoice::Auto, global = true)]
    pub compositor: CompositorChoice,

    /// Start a headless compositor running exactly one application.
    ///
    /// Known profiles: google-chrome, thunar.
    ///
    /// Composes with `--compositor` and with `--`: the profile's own arguments come first and the
    /// trailing program arguments are appended.
    #[arg(long, value_name = "PROFILE", global = true)]
    pub app: Option<String>,

    /// Weston output backend (Weston only).
    #[arg(long, value_enum, default_value_t = Backend::Auto)]
    pub backend: Backend,

    #[arg(long, default_value = "weston")]
    pub weston: PathBuf,

    #[arg(long, default_value = "sway")]
    pub sway: PathBuf,

    #[arg(long)]
    pub drm_device: Option<PathBuf>,

    #[arg(long)]
    pub drm_output: Option<String>,

    #[arg(long, value_enum, default_value_t = Renderer::Auto)]
    pub renderer: Renderer,

    #[arg(long)]
    pub width: Option<u32>,

    #[arg(long)]
    pub height: Option<u32>,

    #[arg(long, default_value_t = 30)]
    pub fps: u32,

    #[arg(long, default_value_t = 8_000_000)]
    pub bitrate: u64,

    #[arg(long, default_value_t = 2)]
    pub gop_seconds: u32,

    #[arg(long, default_value_t = 4_194_304)]
    pub max_access_unit_bytes: u32,

    #[arg(long)]
    pub no_audio: bool,

    #[arg(long, conflicts_with = "no_audio")]
    pub require_audio: bool,

    /// Report that the nested desktop is in secure-input mode from startup.
    #[arg(long)]
    pub secure_input: bool,

    /// Pulse-compatible server address used by capture and nested desktop clients.
    #[arg(long)]
    pub audio_capture_server: Option<String>,

    #[arg(long, value_enum, default_value_t = Xwayland::Auto)]
    pub xwayland: Xwayland,

    #[arg(long)]
    pub xkb_model: Option<String>,

    #[arg(long, default_value = "us")]
    pub xkb_layout: String,

    #[arg(long)]
    pub xkb_variant: Option<String>,

    #[arg(long)]
    pub xkb_options: Option<String>,

    #[arg(long)]
    pub doctor: bool,

    /// Present a desktop-surface-v1 Vivid target instead of the terminal target.
    ///
    /// The target profile is declared in HELLO, so the choice must match the presenter window:
    /// a Vivido window started with --desktop-target for the full-window desktop path, or the
    /// default terminal window (for example through vvssh) for the terminal anchor path.
    #[arg(long)]
    pub desktop_target: bool,

    /// Vivid control endpoint, inherited from Vivido or vvssh.
    #[arg(skip = std::env::var("VIVID_ENDPOINT_CONTROL").ok().map(Zeroizing::new))]
    pub endpoint_control: Option<Zeroizing<String>>,

    /// Optional interactive-lane endpoint (desktop target only).
    #[arg(skip = std::env::var("VIVID_ENDPOINT_INTERACTIVE").ok().map(Zeroizing::new))]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub endpoint_interactive: Option<Zeroizing<String>>,

    /// Optional realtime-lane endpoint, inherited from vvssh.
    #[arg(skip = std::env::var("VIVID_ENDPOINT_REALTIME").ok().map(Zeroizing::new))]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub endpoint_realtime: Option<Zeroizing<String>>,

    /// Optional bulk-lane endpoint, inherited from vvssh.
    #[arg(skip = std::env::var("VIVID_ENDPOINT_BULK").ok().map(Zeroizing::new))]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub endpoint_bulk: Option<Zeroizing<String>>,

    /// Program and arguments to launch once the compositor is ready.
    #[arg(last = true, allow_hyphen_values = true)]
    pub program: Vec<OsString>,
}

impl Config {
    pub fn validate(&self) -> io::Result<()> {
        if let Some(command) = &self.command {
            if self.session.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--session without a subcommand is the attach form",
                ));
            }
            if self.doctor {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--doctor cannot be combined with a subcommand",
                ));
            }
            if let Command::KillSession { target } = command {
                #[cfg(target_os = "linux")]
                crate::linux::runtime::validate_session_name(target)?;
                #[cfg(not(target_os = "linux"))]
                validate_session_name_portable(target)?;
            }
            match command {
                Command::Serve { session, .. } | Command::Server { session, .. } => {
                    #[cfg(target_os = "linux")]
                    crate::linux::runtime::validate_session_name(session)?;
                    #[cfg(not(target_os = "linux"))]
                    validate_session_name_portable(session)?;
                    if self.desktop_target || self.secure_input {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "serve cannot be combined with --desktop-target or --secure-input",
                        ));
                    }
                    self.validate_provisioning_bounds()?;
                }
                // Administrative and client commands provision no desktop and intentionally
                // ignore geometry, codec, compositor, keymap, and credential settings.
                Command::Msg(_) | Command::List | Command::KillSession { .. } => {}
            }
            return Ok(());
        }
        if let Some(session) = &self.session {
            if self.doctor {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--doctor cannot be combined with --session",
                ));
            }
            #[cfg(target_os = "linux")]
            crate::linux::runtime::validate_session_name(session)?;
            #[cfg(not(target_os = "linux"))]
            validate_session_name_portable(session)?;
        }
        self.validate_provisioning_bounds()?;
        // Only the credentials are waived for `--doctor`; the bounds still apply so a diagnostic
        // run reports the same geometry and keymap errors a real run would hit.
        if self.requires_vivid_credentials() {
            if self.endpoint_control.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "VIVID_ENDPOINT_CONTROL is not set; run Vvland inside Vivido or through vvssh",
                ));
            }
            let root_secret = std::env::var("VIVID_ROOT_SECRET").ok().map(Zeroizing::new);
            if root_secret.as_ref().is_none_or(|secret| secret.is_empty()) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "VIVID_ROOT_SECRET is not set",
                ));
            }
        }
        Ok(())
    }

    fn validate_provisioning_bounds(&self) -> io::Result<()> {
        self.validate_compositor_options()?;
        // An unknown `--app` profile is a typo whether or not this run is a diagnostic.
        #[cfg(target_os = "linux")]
        if let Some(app) = &self.app {
            crate::linux::app::profile(app)?;
        }
        crate::producer::cli::validate_common_bounds(
            self.width,
            self.height,
            self.fps,
            self.bitrate,
            self.gop_seconds,
            self.max_access_unit_bytes,
            self.audio_capture_server.as_deref(),
            &self.xkb_layout,
            self.xkb_model.as_deref(),
            self.xkb_variant.as_deref(),
            self.xkb_options.as_deref(),
        )?;
        if self
            .drm_output
            .as_deref()
            .is_some_and(|output| !crate::producer::cli::safe_config_value(output))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "DRM output values must be at most 128 single-line bytes",
            ));
        }
        Ok(())
    }

    fn requires_vivid_credentials(&self) -> bool {
        self.command.is_none() && !self.doctor
    }

    /// Reject flag combinations that name one compositor's capability while running the other.
    ///
    /// Only an explicit `--compositor` makes a combination unambiguous; under `auto` a
    /// Weston-only flag simply selects Weston (plan D5) and a renderer that does not apply to the
    /// chosen compositor degrades to automatic selection (plan D11).
    fn validate_compositor_options(&self) -> io::Result<()> {
        if self.compositor == CompositorChoice::Weston && self.renderer == Renderer::Gles2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--renderer=gles2 is wlroots' name for the GLES renderer; Weston uses gl",
            ));
        }
        if self.compositor != CompositorChoice::Sway {
            return Ok(());
        }
        if self.backend != Backend::Auto {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--backend applies only to Weston; Sway is always headless",
            ));
        }
        if self.drm_device.is_some() || self.drm_output.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--drm-device and --drm-output apply only to Weston",
            ));
        }
        if self.renderer == Renderer::Gl {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--renderer=gl is Weston's name for the GL renderer; Sway uses gles2",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Serializes every test that mutates the process environment; the SDK reads
    /// `VIVID_ROOT_SECRET` and the endpoints from the environment at connect time.
    pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Parse an argument vector that the test author knows is valid.
    pub(crate) fn parse<I, T>(arguments: I) -> Config
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        Config::try_parse_from(arguments).expect("valid test arguments")
    }

    fn base() -> Config {
        Config {
            command: None,
            session: None,
            replace: false,
            compositor: CompositorChoice::Auto,
            app: None,
            backend: Backend::Auto,
            weston: "weston".into(),
            sway: "sway".into(),
            drm_device: None,
            drm_output: None,
            renderer: Renderer::Auto,
            width: None,
            height: None,
            fps: 30,
            bitrate: 8_000_000,
            gop_seconds: 2,
            max_access_unit_bytes: 4_194_304,
            no_audio: false,
            require_audio: false,
            secure_input: false,
            audio_capture_server: None,
            xwayland: Xwayland::Auto,
            xkb_model: None,
            xkb_layout: "us".into(),
            xkb_variant: None,
            xkb_options: None,
            doctor: false,
            desktop_target: false,
            endpoint_control: Some(Zeroizing::new("unix:/tmp/vivid.sock".into())),
            endpoint_interactive: None,
            endpoint_realtime: None,
            endpoint_bulk: None,
            program: Vec::new(),
        }
    }

    #[test]
    fn doctor_does_not_require_vivid_credentials() {
        let mut config = base();
        config.doctor = true;
        config.endpoint_control = None;
        assert!(config.validate().is_ok());
        // Credentials are the only thing a diagnostic run waives; the bounds still apply.
        config.fps = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn unknown_app_profiles_are_rejected_and_known_ones_accepted() {
        let mut config = base();
        config.doctor = true;
        config.app = Some("emacs".into());
        assert!(config.validate().is_err());
        for name in ["google-chrome", "thunar"] {
            config.app = Some(name.into());
            assert!(config.validate().is_ok(), "{name}");
        }
    }

    #[test]
    fn app_mode_composes_with_the_compositor_and_the_passthrough() {
        // `--app` is not exclusive with anything: it names the profile, `--compositor` still
        // selects the session, and `--` arguments append to the profile's own (plan D4/D11).
        let config = parse([
            "vvland",
            "--doctor",
            "--compositor=sway",
            "--app=google-chrome",
            "--",
            "--user-data-dir=/tmp/x",
        ]);
        assert_eq!(config.app.as_deref(), Some("google-chrome"));
        assert_eq!(config.compositor, CompositorChoice::Sway);
        assert_eq!(config.program, [OsString::from("--user-data-dir=/tmp/x")]);
        assert!(config.validate().is_ok());

        // `--app` alone is the app-mode marker with no trailing program.
        let alone = parse(["vvland", "--doctor", "--app=thunar"]);
        assert_eq!(alone.app.as_deref(), Some("thunar"));
        assert!(alone.program.is_empty());
    }

    #[test]
    fn weston_only_flags_are_rejected_for_sway() {
        // Sway is headless-only and has no DRM leg; naming both is a mistake, not a preference.
        for arguments in [
            vec!["vvland", "--doctor", "--compositor=sway", "--backend=drm"],
            vec![
                "vvland",
                "--doctor",
                "--compositor=sway",
                "--backend=headless",
            ],
            vec![
                "vvland",
                "--doctor",
                "--compositor=sway",
                "--drm-device=/dev/dri/card0",
            ],
            vec![
                "vvland",
                "--doctor",
                "--compositor=sway",
                "--drm-output=HDMI-A-1",
            ],
        ] {
            let config = parse(arguments.clone());
            assert!(config.validate().is_err(), "{arguments:?}");
        }
        // The same flags are fine under auto, where they simply select Weston.
        assert!(
            parse(["vvland", "--doctor", "--backend=drm"])
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn renderer_names_are_rejected_for_the_wrong_compositor() {
        // `gl` is Weston's spelling and `gles2` is wlroots'; each is an error for the other.
        assert!(
            parse(["vvland", "--doctor", "--compositor=sway", "--renderer=gl"])
                .validate()
                .is_err()
        );
        assert!(
            parse([
                "vvland",
                "--doctor",
                "--compositor=weston",
                "--renderer=gles2"
            ])
            .validate()
            .is_err()
        );
        assert!(
            parse([
                "vvland",
                "--doctor",
                "--compositor=sway",
                "--renderer=gles2"
            ])
            .validate()
            .is_ok()
        );
        assert!(
            parse(["vvland", "--doctor", "--compositor=weston", "--renderer=gl"])
                .validate()
                .is_ok()
        );
        // Under `auto` neither name is an error: it degrades to automatic selection.
        assert!(
            parse(["vvland", "--doctor", "--renderer=gl"])
                .validate()
                .is_ok()
        );
        assert!(
            parse(["vvland", "--doctor", "--renderer=gles2"])
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn renderer_maps_to_each_compositors_own_name() {
        assert_eq!(Renderer::Gl.as_weston(), "gl");
        assert_eq!(Renderer::Auto.as_weston(), "auto");
        assert_eq!(Renderer::Pixman.as_weston(), "pixman");
        assert_eq!(Renderer::Vulkan.as_weston(), "vulkan");
        // wlroots' name degrades to Weston's automatic selection rather than a bogus value.
        assert_eq!(Renderer::Gles2.as_weston(), "auto");

        assert_eq!(Renderer::Gles2.as_wlroots(), Some("gles2"));
        assert_eq!(Renderer::Pixman.as_wlroots(), Some("pixman"));
        assert_eq!(Renderer::Vulkan.as_wlroots(), Some("vulkan"));
        assert_eq!(Renderer::Auto.as_wlroots(), None);
        assert_eq!(Renderer::Gl.as_wlroots(), None);
    }

    #[test]
    fn requires_the_1_5_endpoint_and_root_secret_names() {
        let _guard = TEST_ENV_LOCK.lock().unwrap();
        // SAFETY: test-only environment mutation, same value as the sibling test.
        unsafe { std::env::set_var("VIVID_ROOT_SECRET", "0123456789abcdef0123456789abcdef") };
        let config = base();
        assert!(config.validate().is_ok());
        let mut missing = base();
        missing.endpoint_control = None;
        assert!(missing.validate().is_err());
    }

    #[test]
    fn credentials_cannot_be_supplied_as_command_arguments() {
        assert!(Config::try_parse_from(["vvland", "--token", "secret"]).is_err());
        assert!(Config::try_parse_from(["vvland", "--endpoint", "unix:/tmp/socket"]).is_err());
        assert!(
            Config::try_parse_from(["vvland", "--endpoint-control", "unix:/tmp/socket"]).is_err()
        );
        // Nothing in the root or any subcommand help should suggest a secret can travel on the
        // command line.
        let command = Config::command();
        let mut commands = vec![command.clone()];
        commands.extend(command.get_subcommands().cloned());
        for mut command in commands {
            let help = command.render_long_help().to_string();
            assert!(!help.contains("--token"));
            assert!(!help.contains("--endpoint"));
            assert!(!help.contains("VIVID_TOKEN"));
            assert!(!help.contains("VIVID_ROOT_SECRET"));
        }
    }

    #[test]
    fn removed_verbose_option_is_rejected() {
        assert!(Config::try_parse_from(["vvland", "--verbose"]).is_err());
    }

    #[test]
    fn administrative_subcommands_need_no_credentials_or_streaming_bounds() {
        let list = parse(["vvland", "list"]);
        assert_eq!(list.command, Some(Command::List));
        assert!(list.validate().is_ok());

        let kill = parse(["vvland", "kill-session", "-t", "work"]);
        assert_eq!(
            kill.command,
            Some(Command::KillSession {
                target: "work".into()
            })
        );
        assert!(kill.validate().is_ok());

        let invalid = parse(["vvland", "kill-session", "-t", "../work"]);
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn subcommands_do_not_steal_the_legacy_trailing_program() {
        let config = parse(["vvland", "--doctor", "--", "list"]);
        assert!(config.command.is_none());
        assert_eq!(config.program, [OsString::from("list")]);
    }

    #[test]
    fn serve_accepts_app_and_trailing_program_after_the_subcommand() {
        let config = parse([
            "vvland",
            "serve",
            "--session",
            "work",
            "--foreground",
            "--app",
            "thunar",
            "--",
            "--new-window",
        ]);
        assert_eq!(config.app.as_deref(), Some("thunar"));
        let Some(Command::Serve { serve_program, .. }) = &config.command else {
            panic!("serve command expected");
        };
        assert_eq!(serve_program, &[OsString::from("--new-window")]);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn bare_session_is_the_attach_form_and_replace_is_explicit() {
        let attach = parse([
            "vvland",
            "--session",
            "work",
            "--replace",
            "--bitrate",
            "4000000",
            "--fps",
            "24",
        ]);
        assert_eq!(attach.session.as_deref(), Some("work"));
        assert!(attach.replace);
        assert!(attach.command.is_none());
        assert_eq!(attach.bitrate, 4_000_000);
        assert_eq!(attach.fps, 24);

        let serve = parse(["vvland", "serve", "--session", "work"]);
        assert!(serve.session.is_none());
        assert!(matches!(serve.command, Some(Command::Serve { .. })));
        assert!(Config::try_parse_from(["vvland", "--replace"]).is_err());
    }

    #[test]
    fn control_duration_parser_accepts_family_units() {
        assert_eq!(parse_control_duration("250ms").unwrap(), 250);
        assert_eq!(parse_control_duration("2s").unwrap(), 2_000);
        assert_eq!(parse_control_duration("3m").unwrap(), 180_000);
        assert_eq!(parse_control_duration("1h").unwrap(), 3_600_000);
        assert!(parse_control_duration("1.5s").is_err());
    }

    #[test]
    fn sway_window_observation_commands_have_stable_cli_shapes() {
        let list = parse(["vvland", "msg", "list-windows"]);
        assert!(matches!(
            list.command,
            Some(Command::Msg(MsgOptions {
                message: MsgCommand::ListWindows,
                ..
            }))
        ));

        let wait = parse([
            "vvland",
            "msg",
            "wait",
            "window",
            "--app-id",
            "org.example.Editor",
            "--timeout-ms",
            "5000",
        ]);
        assert!(matches!(
            wait.command,
            Some(Command::Msg(MsgOptions {
                message: MsgCommand::Wait {
                    condition: MsgWaitCondition::Window {
                        ref app_id,
                        timeout_ms: 5_000,
                    },
                },
                ..
            })) if app_id == "org.example.Editor"
        ));
    }
}

#[cfg(not(target_os = "linux"))]
fn validate_session_name_portable(name: &str) -> io::Result<()> {
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
