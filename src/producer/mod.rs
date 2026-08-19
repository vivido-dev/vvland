//! Compositor-independent producer modules for the isolated desktop.
//!
//! These modules were extracted from the former `veston` and `vvsway` producers after being
//! proven byte-identical (or near-identical, differing only in product branding); the branding
//! differences are parameterized through [`ProductIdentity`] instead of duplicated strings. The
//! per-compositor backends keep their capture, input injection, compositor supervision, doctor,
//! and native build logic; this module holds exactly what they share.

pub mod cli;
pub mod desktop_input;
pub mod scene;

#[cfg(target_os = "linux")]
pub mod audio;
#[cfg(target_os = "linux")]
pub mod keysynth;
#[cfg(target_os = "linux")]
pub mod terminal;

/// Product branding for the shared modules.
///
/// The modules were duplicated for years with every difference being one of these three
/// strings; passing them in keeps the shared code free of any product name (asserted by a test).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProductIdentity {
    /// Machine name: Pulse sink names, thread names, and test fixture paths.
    pub slug: &'static str,
    /// Human product name: Pulse device descriptions.
    pub display_name: &'static str,
    /// The compositor this producer supervises: status-line and leader-help strings.
    pub compositor_name: &'static str,
}

/// The injector surface the shared terminal and desktop-input translation needs.
///
/// Each backend's injector — the libweston input module's IPC for Weston, Wayland virtual
/// devices for Sway — implements this trait; the shared code never names either type.
pub trait TerminalInjector {
    fn key(&mut self, code: u32, pressed: bool) -> io::Result<()>;
    fn pointer_absolute(&mut self, x: u32, y: u32) -> io::Result<()>;
    fn pointer_button(&mut self, button: u32, pressed: bool) -> io::Result<()>;
    fn pointer_axis(&mut self, axis: u32, delta: i32) -> io::Result<()>;
    fn release_all(&mut self) -> io::Result<()>;
}

use std::io;

#[cfg(test)]
mod branding {
    /// D3: the shared modules must contain no product name; every branding string is
    /// parameterized through [`ProductIdentity`]. Duplicated strings are how the two trees
    /// drifted from byte-identical to near-identical in the first place.
    #[test]
    fn shared_modules_contain_no_product_name() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        for file in [
            "src/producer/audio.rs",
            "src/producer/cli.rs",
            "src/producer/desktop_input.rs",
            "src/producer/keysynth.rs",
            "src/producer/scene.rs",
            "src/producer/terminal.rs",
        ] {
            let source = std::fs::read_to_string(format!("{manifest}/{file}")).unwrap();
            for name in [
                "veston", "vvsway", "vvland", "Veston", "Vvsway", "Vvland", "Weston", "Sway",
            ] {
                assert!(
                    !source.contains(name),
                    "{file} contains the product name {name:?}"
                );
            }
        }
    }
}
