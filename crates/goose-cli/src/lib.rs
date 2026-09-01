#![recursion_limit = "256"]

#[cfg(not(feature = "rustls-tls"))]
compile_error!("The `rustls-tls` feature must be enabled");

pub mod cli;
pub mod commands;
pub mod logging;
pub mod recipes;
pub mod scenario_tests;
pub mod session;
pub mod signal;

// Re-export commonly used types
pub use cli::Cli;
pub use session::CliSession;
