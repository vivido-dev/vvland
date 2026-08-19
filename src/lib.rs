//! vvland: run one Wayland app or compositor inside Vivido over Vivid.
//!
//! The binary is a two-line call into [`main_entry`]. The same entry point is what the
//! deprecated `veston` and `vvsway` wrappers call, each prepending its own `--compositor`
//! argument so an old command line keeps its old behavior.

pub mod cli;
#[cfg(unix)]
mod control_cli;
pub mod producer;

#[cfg(target_os = "linux")]
pub mod linux;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::Config;

/// Parse `arguments` and run a session.
///
/// `arguments` is a full argv including the program name, so a wrapper can inject flags ahead of
/// the user's own. Injected flags come first, which means an explicit user flag still wins.
/// Errors are reported on stderr under the `vvland:` prefix — the wrappers deliberately do not
/// re-label them, so the message names the binary that actually ran.
pub fn main_entry<I, T>(arguments: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    // `parse_from` handles --help and --version itself and exits with clap's own formatting.
    let config = Config::parse_from(arguments);
    if let Err(error) = config.validate() {
        eprintln!("vvland: {error}");
        return ExitCode::FAILURE;
    }

    #[cfg(unix)]
    if let Some(crate::cli::Command::Msg(options)) = &config.command {
        return match control_cli::run(options) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("vvland: {error}");
                ExitCode::FAILURE
            }
        };
    }

    #[cfg(target_os = "linux")]
    {
        match linux::run(config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("vvland: {error}");
                ExitCode::FAILURE
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = config;
        eprintln!("vvland: this application is supported only on Linux");
        ExitCode::FAILURE
    }
}

/// Print a wrapper's deprecation notice, but only to a terminal.
///
/// Scripts, `vvssh` sessions, and vvmux panes must stay byte-for-byte quiet: the notice is for a
/// human at a prompt, and anything else would corrupt output that a caller is parsing.
pub fn print_deprecation_notice(old_name: &str, replacement: &str) {
    use std::io::IsTerminal;

    if std::io::stderr().is_terminal() {
        eprintln!("{old_name}: deprecated; this is now a wrapper for `{replacement}`");
    }
}
