use once_cell::sync::Lazy;
use rmcp::{ServerHandler, ServiceExt};
use std::collections::HashMap;

/// Raíz de estado de ghosty-lite para los servidores MCP builtin: la misma
/// que resuelve `goose::config::paths::Paths` (GHOSTY_PATH_ROOT, o
/// `~/.ghosty-lite`). No dependemos del crate goose para no crear un ciclo.
pub fn ghosty_home() -> std::path::PathBuf {
    if let Some(root) = std::env::var_os("GHOSTY_PATH_ROOT") {
        let root = std::path::PathBuf::from(root);
        if root.is_absolute() {
            return root;
        }
    }
    etcetera::home_dir()
        .map(|h| h.join(".ghosty-lite"))
        .unwrap_or_else(|_| std::path::PathBuf::from(".ghosty-lite"))
}

pub mod computercontroller;
pub mod mcp_server_runner;
mod memory;
#[cfg(target_os = "macos")]
pub mod peekaboo;
pub mod subprocess;
pub mod tutorial;

pub use computercontroller::ComputerControllerServer;
pub use memory::MemoryServer;
pub use tutorial::TutorialServer;

/// Type definition for a function that spawns and serves a builtin extension server
pub type SpawnServerFn = fn(tokio::io::DuplexStream, tokio::io::DuplexStream);

fn spawn_and_serve<S>(
    name: &'static str,
    server: S,
    transport: (tokio::io::DuplexStream, tokio::io::DuplexStream),
) where
    S: ServerHandler + Send + 'static,
{
    tokio::spawn(async move {
        match server.serve(transport).await {
            Ok(running) => {
                let _ = running.waiting().await;
            }
            Err(e) => tracing::error!(builtin = name, error = %e, "server error"),
        }
    });
}

macro_rules! builtin {
    ($name:ident, $server_ty:ty) => {{
        fn spawn(r: tokio::io::DuplexStream, w: tokio::io::DuplexStream) {
            spawn_and_serve(stringify!($name), <$server_ty>::new(), (r, w));
        }
        (stringify!($name), spawn as SpawnServerFn)
    }};
}

pub static BUILTIN_EXTENSIONS: Lazy<HashMap<&'static str, SpawnServerFn>> = Lazy::new(|| {
    HashMap::from([
        builtin!(computercontroller, ComputerControllerServer),
        builtin!(memory, MemoryServer),
        builtin!(tutorial, TutorialServer),
    ])
});
