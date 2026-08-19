pub mod app;
mod attach;
mod compositor;
mod control;
mod doctor;
mod host;
mod launcher;
mod pipeline;
pub(crate) mod runtime;
mod serve;
mod video;

/// The shared Linux producer modules (audio, scene, desktop input, terminal).
pub use crate::producer::{audio, desktop_input, scene, terminal};

// Product branding is resolved at runtime from the selected compositor rather than fixed at
// compile time: see `compositor::ResolvedCompositor::identity` (plan D2).

use std::error::Error;

use crate::cli::Config;

pub fn run(config: Config) -> Result<(), Box<dyn Error>> {
    if let Some(session) = config.session.clone() {
        return attach::run(&config, &session).map_err(Into::into);
    }
    match config.command.as_ref() {
        Some(crate::cli::Command::Serve {
            session,
            foreground,
            serve_program,
        }) => {
            let mut serve_config = config.clone();
            if !serve_program.is_empty() {
                serve_config.program.clone_from(serve_program);
            }
            return serve::launch(&serve_config, session, *foreground).map_err(Into::into);
        }
        Some(crate::cli::Command::Server {
            session,
            ready_handle,
            server_program,
        }) => {
            let mut server_config = config.clone();
            if !server_program.is_empty() {
                server_config.program.clone_from(server_program);
            }
            return serve::server(&server_config, session, *ready_handle).map_err(Into::into);
        }
        Some(crate::cli::Command::Msg(_)) => unreachable!("msg is dispatched before Linux"),
        Some(crate::cli::Command::List) => return runtime::print_sessions().map_err(Into::into),
        Some(crate::cli::Command::KillSession { target }) => {
            return runtime::terminate_session(target).map_err(Into::into);
        }
        None => {}
    }
    if config.doctor {
        return doctor::run(&config).map_err(Into::into);
    }
    pipeline::run(config)
}
