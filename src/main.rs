use std::process::ExitCode;

fn main() -> ExitCode {
    vvland::main_entry(std::env::args_os())
}
