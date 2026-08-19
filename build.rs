use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

const INPUT_SOURCE: &str = "src/linux/compositor/weston_input.c";
const INPUT_HEADER: &str = "src/linux/compositor/input_wire.h";

fn main() {
    println!("cargo:rerun-if-changed={INPUT_SOURCE}");
    println!("cargo:rerun-if-changed={INPUT_HEADER}");
    println!("cargo:rustc-check-cfg=cfg(no_weston_input)");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set"));
    let module = out.join("libveston-input.so");
    let compiler = env::var_os("CC").unwrap_or_else(|| OsString::from("cc"));

    // vvland supports Weston 13-16. libweston ships a per-release SONAME (libweston-13 .. -16) and
    // refactored its input-injection API in release 16, so select the newest installed version and
    // hand its major number to the C module via -DLIBWESTON_MAJOR.
    //
    // A Sway-only host has no libweston development files at all. That is not a build failure: the
    // input module is skipped, `no_weston_input` is set, and the Weston backend fails fast with a
    // documented message while `--compositor sway` stays fully functional (plan D12).
    let found = [
        "libweston-16",
        "libweston-15",
        "libweston-14",
        "libweston-13",
    ]
    .into_iter()
    .find_map(|name| {
        let found = Command::new("pkg-config")
            .args(["--exists", name])
            .status()
            .is_ok_and(|status| status.success());
        found.then(|| (name, name.rsplit('-').next().expect("suffixed name")))
    });
    let Some((libweston, major)) = found else {
        println!("cargo:rustc-cfg=no_weston_input");
        println!(
            "cargo:warning=no libweston-13..16 development files found; \
             vvland is being built without Weston input support (--compositor sway only)"
        );
        return;
    };

    let pkg = Command::new("pkg-config")
        .args(["--cflags", "--libs", libweston, "wayland-server"])
        .output()
        .unwrap_or_else(|error| panic!("failed to run pkg-config for {libweston}: {error}"));
    if !pkg.status.success() {
        panic!(
            "{libweston} development files are required: {}",
            String::from_utf8_lossy(&pkg.stderr).trim()
        );
    }

    let mut command = Command::new(compiler);
    command.args([
        "-std=c11",
        "-D_GNU_SOURCE",
        "-fPIC",
        "-fvisibility=hidden",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-shared",
    ]);
    command.arg(format!("-DLIBWESTON_MAJOR={major}"));
    command.args([INPUT_SOURCE, "-o"]);
    command.arg(&module);
    for argument in String::from_utf8_lossy(&pkg.stdout).split_whitespace() {
        command.arg(argument);
    }
    let status = command
        .status()
        .expect("failed to compile Weston input module");
    assert!(status.success(), "failed to compile Weston input module");
}
