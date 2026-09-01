#![recursion_limit = "256"]

use anyhow::Result;
use goose_cli::cli::cli;

/// Enable ANSI/VT escape sequence processing on Windows Console Host.
///
/// Without this, spinners and progress bars from cliclack/indicatif render as
/// repeated new lines instead of updating in place, because Windows Console Host
/// does not process ANSI escapes by default.
#[cfg(windows)]
fn enable_windows_vt_processing() {
    // colors_supported() has the side effect of calling SetConsoleMode with
    // ENABLE_VIRTUAL_TERMINAL_PROCESSING on the underlying console handle.
    let _ = console::Term::stdout().features().colors_supported();
    let _ = console::Term::stderr().features().colors_supported();
}

async fn run() -> Result<()> {
    if let Err(e) = goose_cli::logging::setup_logging(None) {
        eprintln!("Warning: Failed to initialize logging: {}", e);
    }

    let result = cli().await;

    #[cfg(feature = "otel")]
    if goose::otel::otlp::is_otlp_initialized() {
        goose::otel::otlp::shutdown_otlp();
    }

    result
}

/// Registra el pánico en la telemetría (sólo el sitio reducido, nunca el
/// mensaje) y luego deja correr el hook por defecto. Se instala antes de
/// parsear argv: mientras la telemetría no está armada, esto es un no-op.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let site = info
            .location()
            .map(|loc| goose::telemetry::reduce_panic_site(loc.file(), loc.line(), loc.column()))
            .unwrap_or_default();
        goose::telemetry::emit_panic(site);
        default_hook(info);
    }));
}

fn main() -> Result<()> {
    #[cfg(windows)]
    enable_windows_vt_processing();

    install_panic_hook();
    // La superficie sale de argv[1]: REPL → Tui, `serve` → Serve, `run` →
    // Exec, lo demás → Cli. `serve` no pregunta nunca; sigue el default
    // documentado y los kill switches.
    let args: Vec<String> = std::env::args().collect();
    goose::telemetry::arm_from_args(&args);

    let handle = std::thread::Builder::new()
        .name("goose-cli-main".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Failed to build Tokio runtime");
            runtime.block_on(run())
        })
        .map_err(|e| anyhow::anyhow!("Failed to spawn goose-cli main thread: {}", e))?;

    let result = handle
        .join()
        .map_err(|_| anyhow::anyhow!("goose-cli main thread panicked"));
    // Cierre acotado: session_end + flush con timeout. Nunca bloquea la salida
    // más allá de los topes del crate.
    goose::telemetry::shutdown();
    result?
}
