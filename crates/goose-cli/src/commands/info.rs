use anyhow::{anyhow, Result};
use console::style;
use goose::config::paths::Paths;
use goose::config::Config;
use goose::conversation::message::Message;
use goose::session::session_manager::{DB_NAME, SESSIONS_FOLDER};
use goose_providers::errors::ProviderError;
use serde_yaml;
use std::time::Duration;

fn print_aligned(label: &str, value: &str, width: usize) {
    println!("  {:<width$} {}", label, value, width = width);
}

use goose::config::base::CONFIG_YAML_NAME;
use std::fs;
use std::path::Path;

fn check_path_status(path: &Path) -> String {
    if path.exists() {
        "".to_string()
    } else {
        let mut current = path.parent();
        while let Some(parent) = current {
            if parent.exists() {
                return match fs::metadata(parent).map(|m| !m.permissions().readonly()) {
                    Ok(true) => style("no existe (se puede crear)").dim().to_string(),
                    Ok(false) => style("no existe (padre de sólo lectura)").red().to_string(),
                    Err(_) => style("no existe (no se pudo comprobar)").red().to_string(),
                };
            }
            current = parent.parent();
        }
        style("no existe (sin padre escribible)").red().to_string()
    }
}

struct ProviderCheckSuccess {
    provider: String,
    model: String,
    elapsed: Duration,
}

enum ProviderCheckError {
    NotConfigured {
        label: &'static str,
        error: String,
    },
    InvalidModel(String),
    ProviderCreate {
        error: String,
        show_api_key_hint: bool,
    },
    ProviderRequest(ProviderError),
}

async fn check_provider(
    config: &Config,
) -> std::result::Result<ProviderCheckSuccess, ProviderCheckError> {
    let (provider, model) = match (config.get_ghosty_provider(), config.get_ghosty_model()) {
        (Ok(provider), Ok(model)) => (provider, model),
        (Err(e), _) => {
            return Err(ProviderCheckError::NotConfigured {
                label: "Proveedor:",
                error: e.to_string(),
            });
        }
        (_, Err(e)) => {
            return Err(ProviderCheckError::NotConfigured {
                label: "Modelo:",
                error: e.to_string(),
            });
        }
    };

    let model_config = goose::model_config::model_config_from_user_config(&provider, &model)
        .map_err(|e| ProviderCheckError::InvalidModel(e.to_string()))?;

    let provider_client = goose::providers::create(&provider, Vec::new())
        .await
        .map_err(|e| {
            let error = e.to_string();
            ProviderCheckError::ProviderCreate {
                show_api_key_hint: error.contains("not found") || error.contains("API_KEY"),
                error,
            }
        })?;

    let test_msg = Message::user().with_text("Say 'ok'");
    let start = std::time::Instant::now();
    goose::session_context::with_session_id(
        Some("check".to_string()),
        provider_client.complete(&model_config, "", &[test_msg], &[]),
    )
    .await
    .map_err(ProviderCheckError::ProviderRequest)?;

    Ok(ProviderCheckSuccess {
        provider,
        model,
        elapsed: start.elapsed(),
    })
}

pub async fn handle_info(verbose: bool, check: bool) -> Result<()> {
    let logs_dir = Paths::in_state_dir("logs");
    let sessions_dir = Paths::in_data_dir(SESSIONS_FOLDER);
    let sessions_db = sessions_dir.join(DB_NAME);
    let config = Config::global();
    let config_dir = Paths::config_dir();
    let config_yaml_file = config_dir.join(CONFIG_YAML_NAME);

    let paths = [
        ("Directorio de config:", &config_dir),
        ("Config yaml:", &config_yaml_file),
        ("DB de sesiones (sqlite):", &sessions_db),
        ("Directorio de logs:", &logs_dir),
    ];

    let label_padding = paths.iter().map(|(l, _)| l.len()).max().unwrap_or(0) + 4;
    let path_padding = paths
        .iter()
        .map(|(_, p)| p.display().to_string().len())
        .max()
        .unwrap_or(0)
        + 4;

    println!("{}", style("Versión:").cyan().bold());
    print_aligned("ghosty", env!("CARGO_PKG_VERSION"), label_padding);
    println!();

    println!("{}", style("Rutas:").cyan().bold());
    for (label, path) in &paths {
        println!(
            "{:<label_padding$}{:<path_padding$}{}",
            label,
            path.display(),
            check_path_status(path)
        );
    }

    println!("\n{}", style("Estado:").cyan().bold());
    {
        use crate::commands::serve_setup::{ServeSettings, SERVER_TOKEN_KEY};
        let serve = ServeSettings::load(config);
        let env_token = std::env::var(SERVER_TOKEN_KEY)
            .ok()
            .filter(|t| !t.trim().is_empty());
        let token_status = if env_token.is_some() {
            style("configurado (variable de entorno)")
                .green()
                .to_string()
        } else if serve.token.is_some() {
            style("configurado (secreto guardado)").green().to_string()
        } else {
            style("falta — `ghosty serve --setup`").yellow().to_string()
        };
        print_aligned("Token de serve:", &token_status, label_padding);

        let enabled: Vec<String> = goose::config::extensions::get_enabled_extensions()
            .into_iter()
            .map(|e| e.name().clone())
            .collect();
        let enabled = if enabled.is_empty() {
            style("ninguna").dim().to_string()
        } else {
            enabled.join(", ")
        };
        print_aligned("Extensiones activas:", &enabled, label_padding);

        let telemetry = match std::env::var("GHOSTY_TELEMETRY") {
            Ok(v) if v == "0" || v.eq_ignore_ascii_case("false") => {
                "apagada (variable de entorno)".to_string()
            }
            _ if std::env::var("DO_NOT_TRACK").is_ok_and(|v| v == "1") => {
                "apagada (DO_NOT_TRACK)".to_string()
            }
            _ => match config.get_param::<bool>("GHOSTY_TELEMETRY") {
                Ok(false) => "apagada".to_string(),
                Ok(true) => "encendida".to_string(),
                Err(_) => "encendida (default)".to_string(),
            },
        };
        print_aligned("Telemetría:", &telemetry, label_padding);
    }

    if verbose {
        println!("\n{}", style("Configuración:").cyan().bold());
        let values = config.all_values()?;
        if values.is_empty() {
            println!("  No hay valores de configuración");
            println!(
                "  Corre '{}' para configurar ghosty",
                style("ghosty configure").cyan()
            );
        } else {
            let sorted_values: std::collections::BTreeMap<_, _> =
                values.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

            if let Ok(yaml) = serde_yaml::to_string(&sorted_values) {
                for line in yaml.lines() {
                    println!("  {}", line);
                }
            }
        }
    }

    if check {
        println!("\n{}", style("Prueba del proveedor:").cyan().bold());

        let result = check_provider(config).await;
        match &result {
            Ok(success) => {
                print_aligned("Proveedor:", &success.provider, label_padding);
                print_aligned("Modelo:", &success.model, label_padding);
                print_aligned("Auth:", &style("ok").green().to_string(), label_padding);
                print_aligned(
                    "Conexión:",
                    &format!(
                        "{} (verificado en {:.1}s)",
                        style("ok").green(),
                        success.elapsed.as_secs_f64()
                    ),
                    label_padding,
                );
            }
            Err(ProviderCheckError::NotConfigured { label, error }) => {
                print_aligned(
                    label,
                    &format!("{} {}", style("sin configurar:").red(), error),
                    label_padding,
                );
                print_aligned(
                    "Pista:",
                    &format!("Corre '{}'", style("ghosty configure").cyan()),
                    label_padding,
                );
            }
            Err(ProviderCheckError::InvalidModel(error)) => {
                print_aligned(
                    "Modelo:",
                    &format!("{} {}", style("inválido:").red(), error),
                    label_padding,
                );
            }
            Err(ProviderCheckError::ProviderCreate {
                error,
                show_api_key_hint,
            }) => {
                // Split auth failures (missing/invalid credential) from provider
                // construction failures (unknown provider, malformed provider
                // config). Labeling the latter as "Auth: FAILED" misdirects
                // troubleshooting toward rotating API keys.
                if *show_api_key_hint {
                    print_aligned(
                        "Auth:",
                        &format!("{} {}", style("FALLÓ").red().bold(), error),
                        label_padding,
                    );
                    print_aligned(
                        "Pista:",
                        &format!(
                            "Pon la API key en tu entorno o corre '{}'",
                            style("ghosty configure").cyan()
                        ),
                        label_padding,
                    );
                } else {
                    print_aligned(
                        "Proveedor:",
                        &format!("{} {}", style("FALLÓ").red().bold(), error),
                        label_padding,
                    );
                    print_aligned(
                        "Pista:",
                        &format!(
                            "Revisa el nombre y la config del proveedor, o corre '{}'",
                            style("ghosty configure").cyan()
                        ),
                        label_padding,
                    );
                }
            }
            Err(ProviderCheckError::ProviderRequest(error)) => match error {
                ProviderError::Authentication(_) => {
                    print_aligned(
                        "Auth:",
                        &format!("{} {}", style("FALLÓ").red().bold(), error),
                        label_padding,
                    );
                    print_aligned(
                        "Pista:",
                        &format!(
                            "Revisa tu API key o corre '{}'",
                            style("ghosty configure").cyan()
                        ),
                        label_padding,
                    );
                }
                _ => {
                    print_aligned(
                        "Prueba:",
                        &format!("{} {}", style("FALLÓ").red().bold(), error),
                        label_padding,
                    );
                }
            },
        }

        // Propagate non-zero exit status so automation (CI scripts, install
        // checks, health probes) can rely on `ghosty info --check` as a
        // pre-flight verifier.
        if result.is_err() {
            return Err(anyhow!("la prueba del proveedor falló"));
        }
    }

    Ok(())
}
