//! Deprecated wrapper: `vvsway ARGS` is `vvland --compositor sway ARGS`.
//!
//! The crate keeps its name and command line so existing invocations, scripts, and `vvssh`
//! instructions keep working; everything it used to implement now lives in `vvland`.

use std::process::ExitCode;

fn main() -> ExitCode {
    vvland::print_deprecation_notice("vvsway", "vvland --compositor sway");

    // The forced flags come first so an explicit `--compositor` from the caller still wins.
    let mut arguments = vec![
        std::ffi::OsString::from("vvsway"),
        std::ffi::OsString::from("--compositor"),
        std::ffi::OsString::from("sway"),
    ];
    arguments.extend(std::env::args_os().skip(1));
    vvland::main_entry(arguments)
}
