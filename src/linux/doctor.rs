//! Host diagnostics.
//!
//! The body is shared — pkg-config set, FFmpeg encoders and inputs, Pulse resolution, the private
//! sink, the idle-monitor probe, the Opus pipeline, Xwayland — and each compositor contributes its
//! own section. Under `--compositor auto` both sections run and a missing compositor is a
//! reportable line rather than a hard failure of the other one (plan D9).

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::cli::{Backend, CompositorChoice, Config, Renderer};

use super::audio::{AudioPipeline, PulseSink, idle_monitor_produces_data, resolve_pulse_server};
use super::compositor::{
    CompositorEnvironment, ResolvedCompositor, capture::ScreencopyCapture, sway, weston,
};
use super::video::{CaptureSource, H264Encoder};

pub fn run(config: &Config) -> io::Result<()> {
    let mut failures = Vec::new();
    println!("Vvland host diagnostics");

    let weston_section = matches!(
        config.compositor,
        CompositorChoice::Auto | CompositorChoice::Weston
    );
    let sway_section = matches!(
        config.compositor,
        CompositorChoice::Auto | CompositorChoice::Sway
    );
    // Under `auto` neither compositor is required: only a host with no usable compositor at all
    // is a failure. Under an explicit choice, that compositor's section is mandatory.
    let mut compositor_failures = Vec::new();

    if weston_section {
        let mut section = Vec::new();
        check_weston(config, &mut section);
        if config.compositor == CompositorChoice::Weston {
            failures.append(&mut section);
        } else {
            compositor_failures.push(("weston", section));
        }
    }
    if sway_section {
        let mut section = Vec::new();
        check_sway(config, &mut section);
        if config.compositor == CompositorChoice::Sway {
            failures.append(&mut section);
        } else {
            compositor_failures.push(("sway", section));
        }
    }
    if config.compositor == CompositorChoice::Auto {
        let usable = compositor_failures
            .iter()
            .filter(|(_, section)| section.is_empty())
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        for (name, section) in &compositor_failures {
            for failure in section {
                println!("  warning  vvland {name}: {failure}");
            }
        }
        if usable.is_empty() {
            failures.push("no usable compositor for --compositor auto".into());
        } else {
            println!("  ok       vvland auto will use {}", usable[0]);
        }
    }

    // Only the packages every path needs. A compositor's own libraries belong to its section:
    // a Sway-only host has no reason to carry libpipewire, and a Weston-only host none to carry
    // wayland-client, exactly as the two pre-consolidation doctors reported it.
    for package in [
        "xkbcommon",
        "libavcodec",
        "libavdevice",
        "libavformat",
        "libavutil",
        "libswscale",
        "libswresample",
    ] {
        check_package(package, &mut failures);
    }

    match command_output(
        Path::new("ffmpeg"),
        [OsStr::new("-hide_banner"), OsStr::new("-encoders")],
    ) {
        Ok(encoders) => {
            for encoder in ["libx264", "libopus", "png", "mjpeg"] {
                if encoders.split_whitespace().any(|item| item == encoder) {
                    println!("  ok       FFmpeg encoder: {encoder}");
                } else {
                    failures.push(format!("FFmpeg encoder {encoder} is unavailable"));
                }
            }
        }
        Err(error) => failures.push(format!("could not inspect FFmpeg encoders: {error}")),
    }

    match command_output(
        Path::new("ffmpeg"),
        [OsStr::new("-hide_banner"), OsStr::new("-devices")],
    ) {
        Ok(devices) if devices.split_whitespace().any(|item| item == "pulse") => {
            println!("  ok       FFmpeg input: pulse")
        }
        Ok(_) => failures.push("FFmpeg Pulse input device is unavailable".into()),
        Err(error) => failures.push(format!("could not inspect FFmpeg devices: {error}")),
    }

    match H264Encoder::new(640, 360, 30, 2_000_000, 2, 4_194_304) {
        Ok(_) => println!("  ok       FFmpeg libx264 H.264 encoder is ready"),
        Err(error) => failures.push(format!("FFmpeg libx264 encoder is unavailable: {error}")),
    }
    match super::control::screenshot::check_encoders() {
        Ok(()) => println!("  ok       FFmpeg PNG and MJPEG screenshot encoders are ready"),
        Err(error) => failures.push(format!(
            "FFmpeg screenshot encoders are unavailable: {error}"
        )),
    }

    if weston_section {
        // Only Weston's capture path goes through PipeWire; Sway uses wlroots screencopy.
        match require_pipewire_server() {
            Ok(()) => println!("  ok       PipeWire server is reachable"),
            Err(error) if config.compositor == CompositorChoice::Weston => {
                failures.push(error.to_string())
            }
            Err(error) => println!("  warning  vvland weston: {error}"),
        }
    }

    if !config.no_audio {
        check_audio(config, &mut failures);
    } else {
        println!("  ok       audio explicitly disabled");
    }

    let xwayland = command_output(Path::new("Xwayland"), [OsStr::new("-version")]).is_ok();
    match config.xwayland {
        crate::cli::Xwayland::On if !xwayland => {
            failures.push("--xwayland=on was requested but Xwayland is unavailable".into())
        }
        crate::cli::Xwayland::Auto if !xwayland => {
            println!("  warning  Xwayland unavailable; X11 applications will be disabled")
        }
        crate::cli::Xwayland::Off => println!("  ok       Xwayland explicitly disabled"),
        _ => println!("  ok       Xwayland is installed"),
    }

    if weston_section {
        check_drm(config, &mut failures);
    }

    if failures.is_empty() {
        println!("  result   ready");
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("  missing  {failure}");
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} required diagnostic checks failed", failures.len()),
        ))
    }
}

fn check_package(package: &str, failures: &mut Vec<String>) {
    match command_output(
        Path::new("pkg-config"),
        [OsStr::new("--modversion"), OsStr::new(package)],
    ) {
        Ok(version) => println!("  ok       {package}: {version}"),
        Err(error) => failures.push(format!("missing {package}: {error}")),
    }
}

/// The Weston section: binary version, dev files, the PipeWire backend, and the input module.
fn check_weston(config: &Config, failures: &mut Vec<String>) {
    match weston_version(&config.weston) {
        Ok(version) if weston_supported(&version) => println!("  ok       Weston: {version}"),
        Ok(version) => failures.push(format!(
            "Weston 13.x through 16.x is required; found {}",
            version.trim()
        )),
        Err(error) => failures.push(format!("could not execute Weston: {error}")),
    }

    match installed_libweston() {
        Some((name, version)) => println!("  ok       {name}: {version}"),
        None => failures.push(
            "no supported libweston dev files found (need libweston-13, -14, -15, or -16)".into(),
        ),
    }
    if weston::weston_input_compiled_in() {
        println!("  ok       Weston input module is compiled in");
    } else {
        failures.push(weston::WESTON_INPUT_MISSING.to_owned());
    }
    if weston_advertises_pipewire(&config.weston) {
        println!("  ok       Weston PipeWire backend is advertised");
    } else {
        failures.push("Weston does not advertise its PipeWire backend".into());
    }
    check_package("libpipewire-0.3", failures);
}

/// The Sway section: binary version plus a live screencopy/virtual-device probe.
fn check_sway(config: &Config, failures: &mut Vec<String>) {
    check_package("wayland-client", failures);
    match sway::sway_version(&config.sway) {
        Ok(version) if sway::sway_supported(&version) => println!("  ok       Sway: {version}"),
        Ok(version) => failures.push(format!(
            "Sway 1.9 or newer is required; found {}",
            version.trim()
        )),
        Err(error) => failures.push(format!("could not execute Sway: {error}")),
    }
    if failures.is_empty() {
        match check_sway_protocols(config) {
            Ok(()) => println!(
                "  ok       isolated Sway, screencopy, virtual keyboard, and virtual pointer are ready"
            ),
            Err(error) => failures.push(format!("isolated Sway protocol probe failed: {error}")),
        }
    }
}

fn check_sway_protocols(config: &Config) -> io::Result<()> {
    let width = config.width.unwrap_or(640) & !1;
    let height = config.height.unwrap_or(360) & !1;
    let session = sway::SwaySession::start(
        config,
        CompositorEnvironment {
            width,
            height,
            pulse_server: None,
            pulse_sink: None,
            app_window: None,
        },
    )?;
    let capture = ScreencopyCapture::start(
        session.wayland_socket(),
        width,
        height,
        config.fps.clamp(1, 240),
        Instant::now(),
    )?;
    if capture.latest().snapshot()?.is_none() {
        return Err(io::Error::other(
            "Sway screencopy succeeded without retaining an initial frame",
        ));
    }
    drop(capture);
    drop(session);
    Ok(())
}

fn check_audio(config: &Config, failures: &mut Vec<String>) {
    // The sink and its description are named from the slug, which does not depend on the
    // compositor; any resolved identity produces the same audio probe.
    let identity = ResolvedCompositor::Weston.identity();
    match resolve_pulse_server(config.audio_capture_server.as_deref()) {
        Ok(server) => match PulseSink::create(&server, &identity) {
            Ok(sink) => {
                println!("  ok       Pulse compatibility server and private sink are ready");
                match idle_monitor_produces_data(
                    sink.monitor_name(),
                    sink.server(),
                    Duration::from_millis(750),
                ) {
                    Ok(true) => println!(
                        "  ok       audio: idle sink monitor produces silence — pre-roll will work"
                    ),
                    Ok(false) => println!(
                        "  warning  audio: idle sink monitor produced no data within 750 ms — this host needs the silence-filled pipeline; older releases will disable audio at start"
                    ),
                    Err(error) => println!(
                        "  warning  audio: idle sink monitor probe could not complete ({error})"
                    ),
                }
                match AudioPipeline::start(
                    sink.monitor_name(),
                    sink.server(),
                    Instant::now(),
                    &identity,
                ) {
                    Ok(pipeline) => {
                        println!("  ok       Pulse-to-Opus capture pipeline is ready");
                        drop(pipeline);
                    }
                    Err(error) => failures.push(format!(
                        "Pulse-to-Opus capture pipeline could not start: {error}"
                    )),
                }
                drop(sink);
            }
            Err(error) => failures.push(format!(
                "Pulse compatibility server is reachable but the private sink could not be created: {error}"
            )),
        },
        Err(error) => failures.push(format!(
            "Pulse compatibility server could not be resolved through pactl: {error}"
        )),
    }
}

fn check_drm(config: &Config, failures: &mut Vec<String>) {
    let drm_sys_root = Path::new("/sys/class/drm");
    let drm_dev_root = Path::new("/dev/dri");
    let connectors = weston::connected_drm_outputs(drm_sys_root);
    if connectors.is_empty() {
        if config.backend == Backend::Drm {
            failures.push("--backend=drm requires a connected DRM output".into());
        } else {
            println!("  warning  no connected DRM output; automatic mode will use headless");
        }
        return;
    }
    println!(
        "  ok       connected DRM outputs: {}",
        connectors.join(", ")
    );
    let selected_output = config
        .drm_output
        .as_deref()
        .or_else(|| connectors.first().map(String::as_str));
    let device = config
        .drm_device
        .clone()
        .or_else(|| {
            selected_output.and_then(|output| {
                weston::drm_device_for_output(drm_sys_root, drm_dev_root, output)
            })
        })
        .or_else(first_drm_card)
        .unwrap_or_else(|| PathBuf::from("/dev/dri/card0"));
    let device_available = match OpenOptions::new().read(true).write(true).open(&device) {
        Ok(_) => {
            println!("  ok       DRM device can be opened: {}", device.display());
            true
        }
        Err(error) if config.backend == Backend::Drm => {
            failures.push(format!(
                "cannot open DRM device {} read/write: {error}",
                device.display()
            ));
            false
        }
        Err(error) => {
            println!(
                "  warning  DRM device {} is unavailable ({error}); auto mode will use headless",
                device.display()
            );
            false
        }
    };
    if device_available
        && config.backend != Backend::Headless
        && config.renderer != Renderer::Pixman
    {
        if let Some(render_node) = weston::drm_render_node(drm_sys_root, drm_dev_root, &device) {
            match OpenOptions::new().read(true).write(true).open(&render_node) {
                Ok(_) => println!(
                    "  ok       GPU render node can be opened: {}",
                    render_node.display()
                ),
                Err(error)
                    if config.backend == Backend::Drm && config.renderer != Renderer::Auto =>
                {
                    failures.push(format!(
                        "cannot open GPU render node {} read/write for {} renderer: {error}",
                        render_node.display(),
                        config.renderer.as_weston()
                    ));
                }
                Err(error) if config.renderer == Renderer::Auto => println!(
                    "  warning  GPU render node {} is unavailable ({error}); automatic renderer will retry Pixman",
                    render_node.display()
                ),
                Err(error) => println!(
                    "  warning  GPU render node {} is unavailable ({error}); auto backend may use headless",
                    render_node.display()
                ),
            }
        }
    }
}

pub fn require_pipewire_server() -> io::Result<()> {
    command_output(Path::new("pw-cli"), [OsStr::new("info"), OsStr::new("0")])
        .map(|_| ())
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "PipeWire server is not reachable through pw-cli ({error}); install and start the user PipeWire service"
                ),
            )
        })
}

fn first_drm_card() -> Option<PathBuf> {
    let mut cards = std::fs::read_dir("/dev/dri")
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with("card"))
        })
        .collect::<Vec<_>>();
    cards.sort();
    cards.into_iter().next()
}

fn command_output<const N: usize>(program: &Path, args: [&OsStr; N]) -> io::Result<String> {
    let output = Command::new(program).args(args).output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{} exited with {}",
            program.display(),
            output.status
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let value = if stdout.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };
    Ok(value.to_owned())
}

fn installed_libweston() -> Option<(&'static str, String)> {
    [
        "libweston-16",
        "libweston-15",
        "libweston-14",
        "libweston-13",
    ]
    .into_iter()
    .find_map(|name| {
        command_output(
            Path::new("pkg-config"),
            [OsStr::new("--modversion"), OsStr::new(name)],
        )
        .ok()
        .map(|version| (name, version))
    })
}

pub fn weston_version(program: &Path) -> io::Result<String> {
    command_output(program, [OsStr::new("--version")])
}

pub fn weston_advertises_pipewire(program: &Path) -> bool {
    command_output(program, [OsStr::new("--help")]).is_ok_and(|help| help.contains("pipewire"))
}

pub fn weston_supported(version: &str) -> bool {
    version.split_whitespace().any(|word| {
        word.split('.')
            .next()
            .and_then(|major| major.parse::<u32>().ok())
            .is_some_and(|major| (13..=16).contains(&major))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_supported_weston_versions() {
        assert!(weston_supported("weston 13.0.0"));
        assert!(weston_supported("14.0.2"));
        assert!(weston_supported("weston 16.0.90"));
        assert!(!weston_supported("weston 12.0.1"));
        assert!(!weston_supported("weston 17.0.0"));
    }

    #[test]
    fn parses_supported_sway_versions() {
        assert!(sway::sway_supported("sway version 1.9"));
        assert!(sway::sway_supported("sway version 1.10-dev"));
        assert!(sway::sway_supported("sway version 2.0"));
        assert!(!sway::sway_supported("sway version 1.8.1"));
        assert!(!sway::sway_supported("unknown"));
    }
}
