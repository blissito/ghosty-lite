use anyhow::Result;
use console::style;
use goose::config::Config;

use crate::session::build_session;
use crate::session::SessionBuilderConfig;

pub async fn handle_doctor() -> Result<()> {
    if !Config::global().exists() {
        println!(
            "No hay configuración. Ejecuta `{}`.",
            style("ghosty configure").cyan()
        );
        return Ok(());
    }

    let mut session = build_session(SessionBuilderConfig {
        no_session: true,
        interactive: true,
        ..Default::default()
    })
    .await;

    session.interactive(Some("/doctor".to_string())).await
}
