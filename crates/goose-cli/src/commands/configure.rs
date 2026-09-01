use cliclack::spinner;
use console::style;
use goose::agents::extension::{ToolInfo, PLATFORM_EXTENSIONS};
use goose::agents::extension_manager::get_parameter_names;
use goose::agents::Agent;
use goose::agents::{extension::Envs, ExtensionConfig};
use goose::config::declarative_providers::{
    create_custom_provider, remove_custom_provider, CreateCustomProviderParams,
};
use goose::config::extensions::{
    get_all_extension_names, get_all_extensions, get_enabled_extensions, get_extension_by_name,
    name_to_key, remove_extension, set_extension, set_extension_enabled,
};
use goose::config::paths::Paths;
use goose::config::permission::PermissionLevel;
use goose::config::{
    Config, ConfigError, ExperimentManager, ExtensionEntry, GooseMode, PermissionManager,
};
use goose::providers::base::ConfigKey;
use goose::providers::provider_test::test_provider_configuration;
use goose::providers::{create, providers, retry_operation, RetryConfig};
use goose::session::SessionType;
use goose_providers::thinking::ThinkingEffort;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{IsTerminal, Write};

// useful for light themes where there is no discernible colour contrast between
// cursor-selected and cursor-unselected items.
const MULTISELECT_VISIBILITY_HINT: &str = "<";
const MAX_PROVIDER_ROWS: usize = 10;
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

type ProviderItem = (String, String, String);

#[derive(Clone, PartialEq, Eq)]
enum ProviderChoice {
    Provider(String),
    Search,
    SearchAgain,
}

fn provider_choice_items(items: &[ProviderItem]) -> Vec<(ProviderChoice, String, String)> {
    items
        .iter()
        .map(|(name, label, hint)| {
            (
                ProviderChoice::Provider(name.clone()),
                label.clone(),
                hint.clone(),
            )
        })
        .collect()
}

fn move_selected_item_into_view<T>(
    items: &mut Vec<T>,
    selected_index: Option<usize>,
    visible_rows: usize,
) {
    if let Some(index) = selected_index.filter(|&index| index >= visible_rows) {
        let selected = items.remove(index);
        items.insert(0, selected);
    }
}

fn fuzzy_filter_provider_items(items: &[ProviderItem], query: &str) -> Vec<ProviderItem> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return items.to_vec();
    }

    let query_words: Vec<_> = query.split_whitespace().collect();
    let mut scored_items: Vec<_> = items
        .iter()
        .filter_map(|item| {
            let label = item.1.to_lowercase();
            let similarity = strsim::jaro_winkler(&label, &query);
            let word_match_bonus =
                query_words.iter().all(|word| label.contains(*word)) as u8 as f64;
            let score = similarity + word_match_bonus;
            (score > 0.6).then_some((score, item))
        })
        .collect();

    scored_items.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored_items
        .into_iter()
        .map(|(_, item)| item.clone())
        .collect()
}

fn search_provider_dialog(provider_items: &[ProviderItem]) -> anyhow::Result<String> {
    let mut query = String::new();

    loop {
        let input: String = cliclack::input("Busca un proveedor")
            .placeholder("ej. OpenAI, Anthropic, local")
            .default_input(&query)
            .interact()?;
        query = input.trim().to_string();

        let filtered_items = fuzzy_filter_provider_items(provider_items, &query);
        if filtered_items.is_empty() {
            cliclack::log::warning("Ningún proveedor coincide. Prueba con otro término.")?;
            continue;
        }

        let mut items = provider_choice_items(&filtered_items);
        items.push((
            ProviderChoice::SearchAgain,
            "Buscar de nuevo…".to_string(),
            "escribe otro término".to_string(),
        ));

        match cliclack::select("¿Qué proveedor de modelos usamos?")
            .items(&items)
            .max_rows(MAX_PROVIDER_ROWS)
            .interact()?
        {
            ProviderChoice::SearchAgain => continue,
            ProviderChoice::Provider(name) => return Ok(name),
            ProviderChoice::Search => {
                unreachable!("Search entry is not added to the results list")
            }
        }
    }
}

struct CursorRestoreGuard;

impl Drop for CursorRestoreGuard {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(SHOW_CURSOR);
        let _ = stdout.flush();
    }
}

pub async fn handle_configure() -> anyhow::Result<()> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "`ghosty configure` necesita una terminal interactiva.\n\
             Si lo instalaste con 'curl ... | bash', corre 'ghosty configure' aparte cuando termine la instalación."
        );
    }

    let _cursor_restore = CursorRestoreGuard;
    let config = Config::global();

    if !config.exists() {
        handle_first_time_setup(config).await
    } else {
        handle_existing_config().await
    }
}

/// Los cinco proveedores del primer arranque. Un id vacío = lista completa con búsqueda.
const QUICK_PROVIDERS: &[(&str, &str, &str)] = &[
    (
        "easybits",
        "EasyBits (recomendado)",
        "una sola llave para modelos DeepSeek y el MCP de easybits",
    ),
    (
        "custom_deepseek",
        "DeepSeek",
        "llave directa de platform.deepseek.com",
    ),
    ("anthropic", "Anthropic", "Claude con tu API key"),
    ("openai", "OpenAI", "GPT con tu API key"),
    ("ollama", "Ollama", "modelos locales, sin llave"),
    ("", "Otro proveedor…", "la lista completa, con búsqueda"),
];

/// Clave de config con el consentimiento de telemetría.
const TELEMETRY_KEY: &str = "GHOSTY_TELEMETRY";

/// Aviso de telemetría del primer arranque. Default: sí.
fn telemetry_consent_dialog() -> anyhow::Result<()> {
    let config = Config::global();
    println!(
        "{}",
        style(ghosty_telemetry::notice::NOTICE_HEADLINE).bold()
    );
    println!();
    for line in ghosty_telemetry::notice::NOTICE_BODY.lines() {
        println!("  {}", style(line).dim());
    }
    println!();
    let consent = crate::commands::confirm_es("¿Compartir estos conteos anónimos?")
        .initial_value(true)
        .interact()?;
    config.set_param(TELEMETRY_KEY, consent)?;
    Ok(())
}

/// Ajustes → Telemetría: mismo aviso, con el valor actual como default.
fn configure_telemetry_dialog() -> anyhow::Result<()> {
    let config = Config::global();
    if std::env::var(TELEMETRY_KEY).is_ok() {
        let _ = cliclack::log::info(
            "Aviso: la variable de entorno GHOSTY_TELEMETRY está puesta y gana sobre lo que guardes aquí.",
        );
    }
    let current = config.get_param::<bool>(TELEMETRY_KEY).unwrap_or(true);
    let _ = cliclack::log::info(format!(
        "{}\n\n{}",
        ghosty_telemetry::notice::NOTICE_HEADLINE,
        ghosty_telemetry::notice::NOTICE_BODY
    ));
    let consent = crate::commands::confirm_es("¿Compartir estos conteos anónimos?")
        .initial_value(current)
        .interact()?;
    config.set_param(TELEMETRY_KEY, consent)?;
    cliclack::outro(if consent {
        "Telemetría activada."
    } else {
        "Telemetría desactivada."
    })?;
    Ok(())
}

async fn handle_first_time_setup(config: &Config) -> anyhow::Result<()> {
    println!();
    println!(
        "{}",
        style("👻  Bienvenido a Ghosty. Vamos a dejarlo listo.").dim()
    );
    println!(
        "{}",
        style("  puedes repetir esto cuando quieras con `ghosty configure`").dim()
    );
    println!();

    telemetry_consent_dialog()?;

    println!();
    cliclack::intro(style(" ghosty configure ").on_cyan().black())?;

    let mut pick = cliclack::select("¿Con qué proveedor quieres empezar?");
    for (id, label, hint) in QUICK_PROVIDERS {
        pick = pick.item(*id, *label, *hint);
    }
    let chosen = pick.interact()?;
    let preselected = if chosen.is_empty() {
        None
    } else {
        Some(chosen)
    };

    handle_manual_provider_setup(config, preselected).await;

    if config.exists() {
        let setup_serve = crate::commands::confirm_es("¿Dejar listo `ghosty serve` ahora?")
            .initial_value(false)
            .interact()?;
        if setup_serve {
            crate::commands::serve_setup::run_serve_setup().await?;
        } else {
            cliclack::outro(
                "Listo. Ejecuta `ghosty` para chatear o `ghosty serve --setup` para exponer el agente.",
            )?;
        }
    }
    Ok(())
}

async fn handle_manual_provider_setup(config: &Config, preselected: Option<&str>) {
    match configure_provider_dialog_for(preselected).await {
        Ok(true) => {
            set_extension(ExtensionEntry {
                enabled: true,
                config: ExtensionConfig::default(),
            });
            if preselected == Some("easybits") {
                offer_easybits_mcp(config);
            }
        }
        Ok(false) => {
            let _ = config.clear();
            println!(
                "\n  {}: no guardamos la configuración. Revisa la llave y vuelve a correr '{}'",
                style("Aviso").yellow().italic(),
                style("ghosty configure").cyan()
            );
        }
        Err(e) => {
            let _ = config.clear();
            print_manual_config_error(&e);
        }
    }
}

/// Con la misma llave de EasyBits se puede montar su MCP (archivos, imágenes,
/// documentos, +100 tools). Se ofrece una vez, justo después de guardar la llave.
fn offer_easybits_mcp(config: &Config) {
    let Ok(key) = config.get_secret::<String>("EASYBITS_API_KEY") else {
        return;
    };
    let wants = crate::commands::confirm_es(
        "¿Activar también el MCP de easybits (archivos, imágenes, documentos)?",
    )
    .initial_value(true)
    .interact()
    .unwrap_or(false);
    if !wants {
        return;
    }
    let mut ext = ExtensionConfig::streamable_http(
        "easybits",
        "https://www.easybits.cloud/api/mcp?tools=core",
        "Herramientas de easybits.cloud",
        goose::config::DEFAULT_EXTENSION_TIMEOUT,
    );
    if let ExtensionConfig::StreamableHttp { headers, .. } = &mut ext {
        headers.insert("Authorization".to_string(), format!("Bearer {key}"));
    }
    set_extension(ExtensionEntry {
        enabled: true,
        config: ext,
    });
    let _ = cliclack::log::success("MCP de easybits activado.");
}

fn print_manual_config_error(e: &anyhow::Error) {
    let rerun = style("ghosty configure").cyan();
    let err = style("Error").red().italic();
    match e.downcast_ref::<ConfigError>() {
        Some(ConfigError::NotFound(key)) => {
            println!(
                "\n  {err} Falta la clave de configuración '{key}' \n  Dale un valor y vuelve a correr '{rerun}'"
            );
        }
        Some(ConfigError::KeyringError(msg)) => {
            print_keyring_error(msg);
        }
        Some(ConfigError::DeserializeError(msg)) => {
            println!(
                "\n  {err} Valor de configuración inválido: {msg} \n  Revisa lo que escribiste y vuelve a correr '{rerun}'"
            );
        }
        Some(ConfigError::FileError(e)) => {
            println!(
                "\n  {err} No se pudo acceder al archivo de configuración: {e} \n  Revisa los permisos del archivo y vuelve a correr '{rerun}'"
            );
        }
        Some(ConfigError::DirectoryError(msg)) => {
            println!(
                "\n  {err} No se pudo acceder al directorio de configuración: {msg} \n  Revisa los permisos del directorio y vuelve a correr '{rerun}'"
            );
        }
        _ => {
            println!(
                "\n  {err} {e} \n  No guardamos la configuración. Revisa la llave y vuelve a correr '{rerun}'"
            );
        }
    }
}

#[cfg(target_os = "macos")]
fn print_keyring_error(msg: &str) {
    println!(
        "\n  {} No se pudo acceder al llavero del sistema (keyring): {} \n  Revisa tu llavero y vuelve a correr '{}'. \n  Si tu sistema no puede usar el llavero, pon los secretos como variables de entorno.",
        style("Error").red().italic(),
        msg,
        style("ghosty configure").cyan()
    );
}

#[cfg(target_os = "windows")]
fn print_keyring_error(msg: &str) {
    println!(
        "\n  {} No se pudo acceder al Administrador de credenciales de Windows: {} \n  Revísalo y vuelve a correr '{}'. \n  Si tu sistema no puede usarlo, pon los secretos como variables de entorno.",
        style("Error").red().italic(),
        msg,
        style("ghosty configure").cyan()
    );
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn print_keyring_error(msg: &str) {
    println!(
        "\n  {} No se pudo acceder al almacén seguro del sistema: {} \n  Revísalo y vuelve a correr '{}'. \n  Si tu sistema no puede usarlo, pon los secretos como variables de entorno.",
        style("Error").red().italic(),
        msg,
        style("ghosty configure").cyan()
    );
}

async fn handle_existing_config() -> anyhow::Result<()> {
    let config_dir = Paths::config_dir().display().to_string();

    println!();
    println!("{}", style("Esto actualiza tu configuración actual").dim());
    println!(
        "{} {}",
        style("  si prefieres, edítala directo en").dim(),
        config_dir
    );
    println!();

    cliclack::intro(style(" ghosty configure ").on_cyan().black())?;
    let action = cliclack::select("¿Qué quieres configurar?")
        .item(
            "providers",
            "Proveedores",
            "cambiar de proveedor o actualizar llaves",
        )
        .item(
            "custom_providers",
            "Proveedores custom",
            "una API compatible con OpenAI, Anthropic u Ollama",
        )
        .item(
            "add",
            "Agregar extensión",
            "conectar un servidor MCP: builtin, stdio o HTTP",
        )
        .item(
            "toggle",
            "Activar / desactivar extensiones",
            "las que ya están conectadas",
        )
        .item("remove", "Quitar extensión", "")
        .item(
            "serve",
            "Servidor (serve)",
            "token, host y puerto, orígenes permitidos",
        )
        .item(
            "settings",
            "Ajustes",
            "modo, permisos de herramientas, salida, turnos, secretos, telemetría",
        )
        .interact()?;

    match action {
        "toggle" => toggle_extensions_dialog(),
        "add" => configure_extensions_dialog(),
        "remove" => remove_extension_dialog(),
        "serve" => crate::commands::serve_setup::run_serve_setup().await,
        "settings" => configure_settings_dialog().await,
        "providers" => configure_provider_dialog().await.map(|_| ()),
        "custom_providers" => configure_custom_provider_dialog().await,
        _ => unreachable!(),
    }
}

/// Helper function to handle OAuth configuration for a provider
async fn handle_oauth_configuration(provider_name: &str, key_name: &str) -> anyhow::Result<()> {
    let _ = cliclack::log::info(format!(
        "Configurando {} con el flujo OAuth de código de dispositivo…",
        key_name
    ));

    // Create a temporary provider instance to handle OAuth
    match create(provider_name, Vec::new()).await {
        Ok(provider) => match provider.configure_oauth().await {
            Ok(_) => {
                let _ = cliclack::log::success("Autenticación OAuth completada.");
                Ok(())
            }
            Err(e) => {
                let _ = cliclack::log::error(format!("No se pudo autenticar: {}", e));
                Err(anyhow::anyhow!(
                    "Falló la autenticación OAuth de {}: {}",
                    key_name,
                    e
                ))
            }
        },
        Err(e) => {
            let _ =
                cliclack::log::error(format!("No se pudo crear el proveedor para OAuth: {}", e));
            Err(anyhow::anyhow!(
                "No se pudo crear el proveedor para OAuth: {}",
                e
            ))
        }
    }
}

const UNLISTED_MODEL_KEY: &str = "__unlisted__";

fn interactive_model_search(
    models: &[String],
    provider_meta: &goose::providers::base::ProviderMetadata,
) -> anyhow::Result<String> {
    const MAX_VISIBLE: usize = 30;
    let mut query = String::new();

    loop {
        let _ = cliclack::clear_screen();

        let _ = cliclack::log::info(format!(
            "🔍 {} modelos disponibles. Escribe para filtrar.",
            models.len()
        ));

        let input: String = cliclack::input("Filtra modelos y presiona Enter para buscar")
            .placeholder("ej. gpt, sonnet, llama, qwen")
            .default_input(&query)
            .interact::<String>()?;
        query = input.trim().to_string();

        let filtered: Vec<String> = if query.is_empty() {
            models.to_vec()
        } else {
            let q = query.to_lowercase();
            models
                .iter()
                .filter(|m| m.to_lowercase().contains(&q))
                .cloned()
                .collect()
        };

        if filtered.is_empty() {
            let selection = cliclack::select("Ningún modelo coincide. ¿Qué quieres hacer?")
                .item("__new_search__", "Nueva búsqueda…", "escribe otro término")
                .item(
                    UNLISTED_MODEL_KEY,
                    "Escribir un modelo que no está en la lista…",
                    "",
                )
                .interact()?;

            if selection == UNLISTED_MODEL_KEY {
                return prompt_unlisted_model(provider_meta);
            }

            query.clear();
            continue;
        }

        let mut items: Vec<(String, String, &str)> = filtered
            .iter()
            .take(MAX_VISIBLE)
            .map(|m| (m.clone(), m.clone(), ""))
            .collect();

        if filtered.len() > MAX_VISIBLE {
            items.insert(
                0,
                (
                    "__refine__".to_string(),
                    format!(
                        "Afina la búsqueda para ver más (mostrando {} de {})",
                        MAX_VISIBLE,
                        filtered.len()
                    ),
                    "demasiadas coincidencias",
                ),
            );
        } else {
            items.insert(
                0,
                (
                    "__new_search__".to_string(),
                    "Nueva búsqueda…".to_string(),
                    "escribe otro término",
                ),
            );
        }

        items.push((
            UNLISTED_MODEL_KEY.to_string(),
            "Escribir un modelo que no está en la lista…".to_string(),
            "",
        ));

        let selection = cliclack::select("Elige un modelo:")
            .items(&items)
            .interact()?;

        if selection == "__refine__" {
            continue;
        } else if selection == "__new_search__" {
            query.clear();
            continue;
        } else if selection == UNLISTED_MODEL_KEY {
            return prompt_unlisted_model(provider_meta);
        } else {
            return Ok(selection);
        }
    }
}

fn select_model_from_list(
    models: &[String],
    provider_meta: &goose::providers::base::ProviderMetadata,
) -> anyhow::Result<String> {
    const MAX_MODELS: usize = 10;

    // Smart model selection:
    // If we have more than MAX_MODELS models, show the recommended models with additional search option.
    // Otherwise, show all models without search.
    if models.len() > MAX_MODELS {
        let recommended_models: Vec<String> = provider_meta
            .known_models
            .iter()
            .map(|m| m.name.clone())
            .filter(|name| models.contains(name))
            .collect();

        if !recommended_models.is_empty() {
            let mut model_items: Vec<(String, String, &str)> = recommended_models
                .iter()
                .map(|m| (m.clone(), m.clone(), "recomendado"))
                .collect();

            model_items.insert(
                0,
                (
                    "search_all".to_string(),
                    "Buscar en todos los modelos…".to_string(),
                    "filtrar la lista completa",
                ),
            );

            model_items.push((
                UNLISTED_MODEL_KEY.to_string(),
                "Escribir un modelo que no está en la lista…".to_string(),
                "",
            ));

            let selection = cliclack::select("Elige un modelo:")
                .items(&model_items)
                .interact()?;

            if selection == "search_all" {
                interactive_model_search(models, provider_meta)
            } else if selection == UNLISTED_MODEL_KEY {
                prompt_unlisted_model(provider_meta)
            } else {
                Ok(selection)
            }
        } else {
            interactive_model_search(models, provider_meta)
        }
    } else {
        let mut model_items: Vec<(String, String, &str)> =
            models.iter().map(|m| (m.clone(), m.clone(), "")).collect();

        model_items.push((
            UNLISTED_MODEL_KEY.to_string(),
            "Escribir un modelo que no está en la lista…".to_string(),
            "",
        ));

        let selection = cliclack::select("Elige un modelo:")
            .items(&model_items)
            .interact()?;

        if selection == UNLISTED_MODEL_KEY {
            prompt_unlisted_model(provider_meta)
        } else {
            Ok(selection)
        }
    }
}

fn prompt_unlisted_model(
    provider_meta: &goose::providers::base::ProviderMetadata,
) -> anyhow::Result<String> {
    let model: String = cliclack::input("Nombre del modelo:")
        .placeholder(&provider_meta.default_model)
        .validate(|input: &String| {
            if input.trim().is_empty() {
                Err("Escribe el nombre de un modelo")
            } else {
                Ok(())
            }
        })
        .interact()?;
    Ok(model.trim().to_string())
}

fn try_store_secret(config: &Config, key_name: &str, value: String) -> anyhow::Result<bool> {
    match config.set_secret(key_name, &value) {
        Ok(_) => Ok(true),
        Err(ConfigError::FallbackToFileStorage) => Ok(true),
        Err(e) => {
            cliclack::outro(style(format!(
                "No se pudo guardar {} de forma segura: {}. Revisa que el almacén seguro del sistema esté accesible. También puedes correr con GHOSTY_DISABLE_KEYRING=true o poner la llave como variable de entorno",
                key_name, e
            )).on_red().white())?;
            Ok(false)
        }
    }
}

async fn configure_single_key(
    config: &Config,
    provider_name: &str,
    display_name: &str,
    key: &ConfigKey,
) -> anyhow::Result<bool> {
    let from_env = std::env::var(&key.name).ok();

    match from_env {
        Some(env_value) => {
            let _ = cliclack::log::info(format!("{} viene de una variable de entorno", key.name));
            if crate::commands::confirm_es("¿Guardar este valor en tu llavero?")
                .initial_value(true)
                .interact()?
            {
                if key.secret {
                    if !try_store_secret(config, &key.name, env_value)? {
                        return Ok(false);
                    }
                } else {
                    config.set_param(&key.name, &env_value)?;
                }
                let _ = cliclack::log::info(format!("{} guardado en {}", key.name, config.path()));
            }
        }
        None => {
            let existing: Result<String, _> = if key.secret {
                config.get_secret(&key.name)
            } else {
                config.get_param(&key.name)
            };

            match existing {
                Ok(_) => {
                    let _ = cliclack::log::info(format!("{} ya está configurado", key.name));
                    if crate::commands::confirm_es("¿Actualizar este valor?").interact()? {
                        if key.oauth_flow {
                            handle_oauth_configuration(provider_name, &key.name).await?;
                        } else {
                            let value: String = if key.secret {
                                cliclack::password(format!("Nuevo valor de {}", key.name))
                                    .mask('▪')
                                    .interact()?
                            } else {
                                let mut input =
                                    cliclack::input(format!("Nuevo valor de {}", key.name));
                                if key.default.is_some() {
                                    input = input.default_input(&key.default.clone().unwrap());
                                }
                                input.interact()?
                            };

                            if key.secret {
                                if !try_store_secret(config, &key.name, value)? {
                                    return Ok(false);
                                }
                            } else {
                                config.set_param(&key.name, &value)?;
                            }
                        }
                    }
                }
                Err(_) => {
                    if key.oauth_flow {
                        handle_oauth_configuration(provider_name, &key.name).await?;
                    } else if !key.required && key.secret {
                        if crate::commands::confirm_es(format!(
                            "¿Quieres poner {}? (opcional)",
                            key.name
                        ))
                        .initial_value(true)
                        .interact()?
                        {
                            let value: String =
                                cliclack::password(format!("Valor de {}", key.name))
                                    .mask('▪')
                                    .interact()?;
                            if !try_store_secret(config, &key.name, value)? {
                                return Ok(false);
                            }
                        }
                    } else {
                        let prompt = if key.required {
                            format!(
                                "El proveedor {} necesita {}, escribe un valor",
                                display_name, key.name
                            )
                        } else {
                            format!("{} (opcional, Enter para saltar)", key.name)
                        };

                        let value: String = if key.secret {
                            cliclack::password(&prompt).mask('▪').interact()?
                        } else {
                            let mut input = cliclack::input(&prompt);
                            if key.default.is_some() {
                                input = input.default_input(&key.default.clone().unwrap());
                            }
                            if !key.required {
                                input = input.required(false);
                            }
                            input.interact()?
                        };

                        if value.is_empty() {
                            return Ok(true);
                        }

                        if key.secret {
                            if !try_store_secret(config, &key.name, value)? {
                                return Ok(false);
                            }
                        } else {
                            config.set_param(&key.name, &value)?;
                        }
                    }
                }
            }
        }
    }
    Ok(true)
}

pub async fn configure_provider_dialog() -> anyhow::Result<bool> {
    configure_provider_dialog_for(None).await
}

/// Configura un proveedor. Con `preselected` se salta el select (primer
/// arranque con proveedor rápido); con `None` muestra la lista completa.
pub async fn configure_provider_dialog_for(preselected: Option<&str>) -> anyhow::Result<bool> {
    let config = Config::global();

    let current_provider: Option<String> = config.get_ghosty_provider().ok();
    let mut available_providers = providers().await;
    available_providers.retain(|(provider, _)| {
        provider.deprecated.is_none() || current_provider.as_deref() == Some(&provider.name)
    });

    // Sort providers alphabetically by display name
    available_providers.sort_by(|a, b| a.0.display_name.cmp(&b.0.display_name));

    // Get current default provider if it exists
    let current_provider_index = current_provider.as_ref().and_then(|current_provider| {
        available_providers
            .iter()
            .position(|(provider, _)| &provider.name == current_provider)
    });
    let visible_provider_rows = if available_providers.len() > MAX_PROVIDER_ROWS {
        MAX_PROVIDER_ROWS - 1
    } else {
        MAX_PROVIDER_ROWS
    };
    move_selected_item_into_view(
        &mut available_providers,
        current_provider_index,
        visible_provider_rows,
    );

    // Create selection items from provider metadata
    let provider_items: Vec<ProviderItem> = available_providers
        .iter()
        .map(|(p, _)| {
            let description = match p
                .deprecated
                .as_ref()
                .and_then(|deprecated| deprecated.replacement.as_deref())
            {
                Some(replacement) => {
                    format!("{} Obsoleto; usa {replacement}.", p.description)
                }
                None => p.description.clone(),
            };
            (p.name.clone(), p.display_name.clone(), description)
        })
        .collect();

    let default_provider = current_provider
        .filter(|current_provider| {
            available_providers
                .iter()
                .any(|(provider, _)| &provider.name == current_provider)
        })
        .unwrap_or_default();

    // cliclack 0.5.5 does not reset its private list offset when filtering a
    // paginated select, so use a separate fuzzy-search step for long lists.
    let provider_name = if let Some(name) = preselected {
        if !available_providers.iter().any(|(p, _)| p.name == name) {
            anyhow::bail!("el proveedor '{name}' no está disponible en esta build");
        }
        name.to_string()
    } else if provider_items.len() > MAX_PROVIDER_ROWS {
        let mut paginated_items = provider_choice_items(&provider_items);
        paginated_items.insert(
            MAX_PROVIDER_ROWS - 1,
            (
                ProviderChoice::Search,
                "Buscar en todos los proveedores…".to_string(),
                "filtrar la lista completa".to_string(),
            ),
        );

        match cliclack::select("¿Qué proveedor de modelos usamos?")
            .initial_value(ProviderChoice::Provider(default_provider.clone()))
            .items(&paginated_items)
            .max_rows(MAX_PROVIDER_ROWS)
            .interact()?
        {
            ProviderChoice::Search => search_provider_dialog(&provider_items)?,
            ProviderChoice::Provider(name) => name,
            ProviderChoice::SearchAgain => {
                unreachable!("SearchAgain entry is not added to the paginated list")
            }
        }
    } else {
        cliclack::select("¿Qué proveedor de modelos usamos?")
            .initial_value(default_provider.clone())
            .items(&provider_items)
            .filter_mode()
            .interact()?
    };

    // Get the selected provider's metadata
    let (provider_meta, _) = available_providers
        .iter()
        .find(|(p, _)| p.name == provider_name.as_str())
        .expect("Selected provider must exist in metadata");

    for key in provider_meta
        .config_keys
        .iter()
        .filter(|k| k.primary || k.oauth_flow)
    {
        if !configure_single_key(config, &provider_name, &provider_meta.display_name, key).await? {
            return Ok(false);
        }
    }

    let non_primary_keys: Vec<_> = provider_meta
        .config_keys
        .iter()
        .filter(|k| !k.primary && !k.oauth_flow)
        .collect();
    if !non_primary_keys.is_empty()
        && crate::commands::confirm_es("¿Configurar ajustes avanzados?")
            .initial_value(false)
            .interact()?
    {
        for key in non_primary_keys {
            if !configure_single_key(config, &provider_name, &provider_meta.display_name, key)
                .await?
            {
                return Ok(false);
            }
        }
    }

    let spin = spinner();
    spin.start("Consultando los modelos disponibles…");
    let temp_provider = create(&provider_name, Vec::new()).await?;
    let models_res = retry_operation(&RetryConfig::default(), || async {
        temp_provider
            .fetch_recommended_models(goose::model_config::global_toolshim())
            .await
    })
    .await;
    spin.stop(style("Modelos consultados").green());

    // Select a model: on fetch error show styled error and abort; if models available, show list; otherwise free-text input
    let model: String = match models_res {
        Err(e) => {
            // Provider hook error
            cliclack::outro(style(e.to_string()).on_red().white())?;
            return Ok(false);
        }
        Ok(models) if !models.is_empty() => select_model_from_list(&models, provider_meta)?,
        Ok(_) => {
            let default_model =
                std::env::var("GHOSTY_MODEL").unwrap_or(provider_meta.default_model.clone());
            cliclack::input("Escribe un modelo de ese proveedor:")
                .default_input(&default_model)
                .interact()?
        }
    };

    {
        let supports_thinking = match temp_provider.fetch_model_info(&model).await {
            Ok(model_info) => model_info.reasoning,
            Err(_) => goose_providers::model::ModelConfig::new(&model).is_reasoning_model(),
        };

        if supports_thinking {
            let effort: ThinkingEffort = cliclack::select("Esfuerzo de razonamiento:")
                .item("off", "Apagado - sin razonamiento extendido", "")
                .item("low", "Bajo - más rápido, razona menos", "")
                .item("medium", "Medio - razonamiento moderado", "")
                .item("high", "Alto - razonamiento profundo", "")
                .item("max", "Máximo - sin límite de profundidad", "")
                .initial_value("off")
                .interact()?
                .parse()
                .map_err(|_| anyhow::anyhow!("esfuerzo de razonamiento inválido"))?;
            config.set_ghosty_thinking_effort(effort)?;
        }
    }

    // Test the configuration
    let spin = spinner();
    spin.start("Probando la configuración…");

    let toolshim_enabled = std::env::var("GHOSTY_TOOLSHIM")
        .map(|val| val == "1" || val.to_lowercase() == "true")
        .unwrap_or(false);
    let toolshim_model = std::env::var("GHOSTY_TOOLSHIM_OLLAMA_MODEL").ok();

    match test_provider_configuration(&provider_name, &model, toolshim_enabled, toolshim_model)
        .await
    {
        Ok(()) => {
            goose::config::set_active_provider(config, &provider_name, &model)?;
            print_config_file_saved()?;
            Ok(true)
        }
        Err(e) => {
            spin.stop(style(e.to_string()).red());
            cliclack::outro(
                style(format!("No se pudo configurar el proveedor: {e}"))
                    .on_red()
                    .white(),
            )?;
            Ok(false)
        }
    }
}

/// Extensiones que ghosty puede usar
/// Dialog for toggling which extensions are enabled/disabled
pub fn toggle_extensions_dialog() -> anyhow::Result<()> {
    for warning in goose::config::get_warnings() {
        eprintln!("{}", style(format!("Aviso: {}", warning)).yellow());
    }

    let extensions = get_all_extensions();

    if extensions.is_empty() {
        cliclack::outro("Todavía no hay extensiones. Corre configure y agrega alguna primero.")?;
        return Ok(());
    }

    // Create a list of extension names and their enabled status
    let mut extension_status: Vec<(String, bool)> = extensions
        .iter()
        .map(|entry| (entry.config.name().to_string(), entry.enabled))
        .collect();

    // Sort extensions alphabetically by name
    extension_status.sort_by(|a, b| a.0.cmp(&b.0));

    // Get currently enabled extensions for the selection
    let enabled_extensions: Vec<&String> = extension_status
        .iter()
        .filter(|(_, enabled)| *enabled)
        .map(|(name, _)| name)
        .collect();

    // Let user toggle extensions
    let selected =
        cliclack::multiselect("activa extensiones: (\"espacio\" alterna, \"enter\" confirma)")
            .required(false)
            .items(
                &extension_status
                    .iter()
                    .map(|(name, _)| (name, name.as_str(), MULTISELECT_VISIBILITY_HINT))
                    .collect::<Vec<_>>(),
            )
            .initial_values(enabled_extensions)
            .filter_mode()
            .interact()?;

    // Update enabled status for each extension
    for name in extension_status.iter().map(|(name, _)| name) {
        set_extension_enabled(
            &name_to_key(name),
            selected.iter().any(|s| s.as_str() == name),
        );
    }

    let config = Config::global();
    cliclack::outro(format!("Extensiones guardadas en {}", config.path()))?;
    Ok(())
}

fn prompt_extension_timeout() -> anyhow::Result<u64> {
    Ok(cliclack::input("Timeout de esta extensión (en segundos):")
        .placeholder(&goose::config::DEFAULT_EXTENSION_TIMEOUT.to_string())
        .validate(|input: &String| match input.parse::<u64>() {
            Ok(_) => Ok(()),
            Err(_) => Err("Escribe un timeout válido"),
        })
        .interact()?)
}

fn prompt_extension_description() -> anyhow::Result<String> {
    Ok(cliclack::input("Descripción de esta extensión:")
        .placeholder("Descripción")
        .validate(|input: &String| {
            if input.trim().is_empty() {
                Err("Escribe una descripción")
            } else {
                Ok(())
            }
        })
        .interact()?)
}

fn prompt_extension_name(placeholder: &str) -> anyhow::Result<String> {
    let extensions = get_all_extension_names();
    Ok(cliclack::input("¿Cómo se llama esta extensión?")
        .placeholder(placeholder)
        .validate(move |input: &String| {
            if input.is_empty() {
                Err("Escribe un nombre")
            } else if extensions.contains(input) {
                Err("Ya existe una extensión con ese nombre")
            } else {
                Ok(())
            }
        })
        .interact()?)
}

fn collect_env_vars() -> anyhow::Result<(HashMap<String, String>, Vec<String>)> {
    let envs = HashMap::new();
    let mut env_keys = Vec::new();
    let config = Config::global();

    if !crate::commands::confirm_es("¿Agregar variables de entorno?").interact()? {
        return Ok((envs, env_keys));
    }

    loop {
        let key: String = cliclack::input("Nombre de la variable:")
            .placeholder("API_KEY")
            .interact()?;

        let value: String = cliclack::password("Valor de la variable:")
            .mask('▪')
            .interact()?;

        if !try_store_secret(config, &key, value)? {
            return Err(anyhow::anyhow!("No se pudo guardar el secreto"));
        }
        env_keys.push(key);

        if !crate::commands::confirm_es("¿Otra variable de entorno?").interact()? {
            break;
        }
    }

    Ok((envs, env_keys))
}

fn collect_headers() -> anyhow::Result<HashMap<String, String>> {
    let mut headers = HashMap::new();

    if !crate::commands::confirm_es("¿Agregar headers personalizados?").interact()? {
        return Ok(headers);
    }

    loop {
        let key: String = cliclack::input("Nombre del header:")
            .placeholder("Authorization")
            .interact()?;

        let value: String = cliclack::input("Valor del header:")
            .placeholder("Bearer token123")
            .interact()?;

        headers.insert(key, value);

        if !crate::commands::confirm_es("¿Otro header?").interact()? {
            break;
        }
    }

    Ok(headers)
}

fn configure_builtin_extension() -> anyhow::Result<()> {
    let extensions = vec![
        (
            "computercontroller",
            "Computer Controller",
            "web scraping, caché de archivos y automatizaciones",
        ),
        (
            "developer",
            "Herramientas de desarrollo",
            "editar código y correr shell",
        ),
        (
            "memory",
            "Memoria",
            "guardar y recuperar recuerdos duraderos",
        ),
        ("tutorial", "Tutorial", "tutoriales y guías interactivas"),
    ];

    let mut select = cliclack::select("¿Qué extensión builtin quieres activar?");
    for (id, name, desc) in &extensions {
        select = select.item(id, name, desc);
    }
    let extension = select.interact()?.to_string();
    let (display_name, description) = extensions
        .iter()
        .find(|(id, _, _)| id == &extension)
        .map(|(_, name, desc)| (name.to_string(), desc.to_string()))
        .unwrap_or_else(|| (extension.clone(), extension.clone()));

    let config = if PLATFORM_EXTENSIONS.contains_key(extension.as_str()) {
        ExtensionConfig::Platform {
            name: extension.clone(),
            description,
            display_name: Some(display_name),
            bundled: Some(true),
            available_tools: Vec::new(),
        }
    } else {
        let timeout = prompt_extension_timeout()?;
        ExtensionConfig::Builtin {
            name: extension.clone(),
            display_name: Some(display_name),
            timeout: Some(timeout),
            bundled: Some(true),
            description,
            available_tools: Vec::new(),
        }
    };

    set_extension(ExtensionEntry {
        enabled: true,
        config,
    });

    cliclack::outro(format!("Extensión {} activada", style(extension).green()))?;
    Ok(())
}

fn configure_stdio_extension() -> anyhow::Result<()> {
    let name = prompt_extension_name("my-extension")?;

    let command_str: String = cliclack::input("¿Qué comando se ejecuta?")
        .placeholder("npx -y @block/gdrive")
        .validate(|input: &String| {
            if input.is_empty() {
                Err("Escribe un comando")
            } else {
                Ok(())
            }
        })
        .interact()?;

    let timeout = prompt_extension_timeout()?;

    let mut parts = goose::utils::split_command_args(&command_str)?;
    let cmd = if parts.is_empty() {
        String::new()
    } else {
        parts.remove(0)
    };
    let args = parts;

    let description = prompt_extension_description()?;
    let (envs, env_keys) = collect_env_vars()?;

    set_extension(ExtensionEntry {
        enabled: true,
        config: ExtensionConfig::Stdio {
            name: name.clone(),
            cmd,
            args,
            envs: Envs::new(envs),
            env_keys,
            description,
            timeout: Some(timeout),
            cwd: None,
            bundled: None,
            available_tools: Vec::new(),
        },
    });

    cliclack::outro(format!("Extensión {} agregada", style(name).green()))?;
    Ok(())
}

fn configure_streamable_http_extension() -> anyhow::Result<()> {
    let name = prompt_extension_name("my-remote-extension")?;

    let uri: String = cliclack::input("URI del endpoint Streamable HTTP:")
        .placeholder("http://localhost:8000/messages")
        .validate(|input: &String| {
            if input.is_empty() {
                Err("Escribe una URI")
            } else if !(input.starts_with("http://") || input.starts_with("https://")) {
                Err("La URI debe empezar con http:// o https://")
            } else {
                Ok(())
            }
        })
        .interact()?;

    let timeout = prompt_extension_timeout()?;
    let description = prompt_extension_description()?;
    let headers = collect_headers()?;

    // Original behavior: no env var collection for Streamable HTTP
    let envs = HashMap::new();
    let env_keys = Vec::new();

    set_extension(ExtensionEntry {
        enabled: true,
        config: ExtensionConfig::StreamableHttp {
            name: name.clone(),
            uri,
            envs: Envs::new(envs),
            env_keys,
            headers,
            description,
            timeout: Some(timeout),
            socket: None,
            client_id: None,
            client_secret_key: None,
            scopes: vec![],
            bundled: None,
            available_tools: Vec::new(),
        },
    });

    cliclack::outro(format!("Extensión {} agregada", style(name).green()))?;
    Ok(())
}

pub fn configure_extensions_dialog() -> anyhow::Result<()> {
    let extension_type = cliclack::select("¿Qué tipo de extensión quieres agregar?")
        .item("built-in", "Extensión builtin", "una que viene con ghosty")
        .item(
            "stdio",
            "Extensión por línea de comandos",
            "correr un comando o script local",
        )
        .item(
            "streamable_http",
            "Extensión remota (Streamable HTTP)",
            "conectar a un servidor MCP por Streamable HTTP",
        )
        .interact()?;

    match extension_type {
        "built-in" => configure_builtin_extension()?,
        "stdio" => configure_stdio_extension()?,
        "streamable_http" => configure_streamable_http_extension()?,
        _ => unreachable!(),
    };

    print_config_file_saved()?;
    Ok(())
}

pub fn remove_extension_dialog() -> anyhow::Result<()> {
    for warning in goose::config::get_warnings() {
        eprintln!("{}", style(format!("Aviso: {}", warning)).yellow());
    }

    let extensions = get_all_extensions();

    // Create a list of extension names and their enabled status
    let mut extension_status: Vec<(String, bool)> = extensions
        .iter()
        .map(|entry| (entry.config.name().to_string(), entry.enabled))
        .collect();

    // Sort extensions alphabetically by name
    extension_status.sort_by(|a, b| a.0.cmp(&b.0));

    if extensions.is_empty() {
        cliclack::outro("Todavía no hay extensiones. Corre configure y agrega alguna primero.")?;
        return Ok(());
    }

    // Check if all extensions are enabled
    if extension_status.iter().all(|(_, enabled)| *enabled) {
        cliclack::outro(
            "Todas las extensiones están activas. Desactiva primero la que quieras quitar.",
        )?;
        return Ok(());
    }

    // Filter out only disabled extensions
    let disabled_extensions: Vec<_> = extensions
        .iter()
        .filter(|entry| !entry.enabled)
        .map(|entry| (entry.config.name().to_string(), entry.enabled))
        .collect();

    let selected = cliclack::multiselect("Elige las extensiones a quitar (sólo las desactivadas; \"espacio\" alterna, \"enter\" confirma)")
        .required(false)
        .items(
            &disabled_extensions
                .iter()
                .filter(|(_, enabled)| !enabled)
                .map(|(name, _)| (name, name.as_str(), MULTISELECT_VISIBILITY_HINT))
                .collect::<Vec<_>>(),
        )
        .filter_mode()
        .interact()?;

    for name in selected {
        remove_extension(&name_to_key(name));
        PermissionManager::instance().remove_extension(&name_to_key(name));
        cliclack::outro(format!("Extensión {} quitada", style(name).green()))?;
    }

    print_config_file_saved()?;

    Ok(())
}

pub async fn configure_settings_dialog() -> anyhow::Result<()> {
    let setting_type = cliclack::select("¿Qué ajuste quieres cambiar?")
        .item(
            "goose_mode",
            "Modo",
            "cuánto puede hacer el agente sin preguntar",
        )
        .item(
            "tool_permission",
            "Permisos de herramientas",
            "permiso por herramienta de las extensiones activas",
        )
        .item(
            "tool_output",
            "Salida de herramientas",
            "mostrar más o menos salida de las herramientas",
        )
        .item(
            "max_turns",
            "Turnos máximos",
            "cuántos turnos seguidos sin pedirte nada",
        )
        .item(
            "keyring",
            "Almacén de secretos",
            "llavero del sistema o archivo",
        )
        .item(
            "experiment",
            "Experimentos",
            "activar o desactivar funciones experimentales",
        )
        .item(
            "telemetry",
            "Telemetría",
            "conteos anónimos de uso, sin contenido",
        )
        .interact()?;

    let mut should_print_config_path = true;

    match setting_type {
        "goose_mode" => {
            configure_goose_mode_dialog()?;
        }
        "tool_permission" => {
            configure_tool_permissions_dialog().await.and(Ok(()))?;
            // No need to print config file path since it's already handled.
            should_print_config_path = false;
        }
        "tool_output" => {
            configure_tool_output_dialog()?;
        }
        "max_turns" => {
            configure_max_turns_dialog()?;
        }
        "keyring" => {
            configure_keyring_dialog()?;
        }
        "experiment" => {
            toggle_experiments_dialog()?;
        }
        "telemetry" => {
            configure_telemetry_dialog()?;
        }
        _ => unreachable!(),
    };

    if should_print_config_path {
        print_config_file_saved()?;
    }

    Ok(())
}

pub fn configure_goose_mode_dialog() -> anyhow::Result<()> {
    let config = Config::global();

    if std::env::var("GHOSTY_MODE").is_ok() {
        let _ = cliclack::log::info(
            "Aviso: la variable de entorno GHOSTY_MODE está puesta y gana sobre lo que guardes aquí.",
        );
    }

    let mode = cliclack::select("¿Qué modo quieres?")
        .item(
            GooseMode::Auto,
            "Automático",
            "edita, crea y borra archivos y usa extensiones sin preguntar",
        )
        .item(
            GooseMode::Approve,
            "Aprobar",
            "toda herramienta, extensión y cambio de archivo pide aprobación",
        )
        .item(
            GooseMode::SmartApprove,
            "Aprobar con criterio",
            "editar, crear, borrar archivos y usar extensiones pide aprobación",
        )
        .item(
            GooseMode::Chat,
            "Chat",
            "sólo conversar: sin herramientas, extensiones ni cambios de archivos",
        )
        .interact()?;

    config.set_ghosty_mode(mode)?;
    let msg = match mode {
        GooseMode::Auto => "Modo automático: cambios de archivos sin preguntar",
        GooseMode::Approve => "Modo aprobar: toda herramienta y cambio pide aprobación",
        GooseMode::SmartApprove => "Modo aprobar con criterio: los cambios piden aprobación",
        GooseMode::Chat => "Modo chat: sin herramientas ni cambios",
    };
    cliclack::outro(msg)?;
    Ok(())
}

pub fn configure_tool_output_dialog() -> anyhow::Result<()> {
    let config = Config::global();

    if std::env::var("GHOSTY_CLI_MIN_PRIORITY").is_ok() {
        let _ = cliclack::log::info(
            "Aviso: la variable de entorno GHOSTY_CLI_MIN_PRIORITY está puesta y gana sobre lo que guardes aquí.",
        );
    }
    let tool_log_level = cliclack::select("¿Cuánta salida de herramientas mostrar?")
        .item("high", "Sólo lo importante", "")
        .item(
            "medium",
            "Importancia media",
            "ej. resultados de escrituras de archivos",
        )
        .item("all", "Todo (default)", "ej. salida de comandos de shell")
        .interact()?;

    match tool_log_level {
        "high" => {
            config.set_param("GHOSTY_CLI_MIN_PRIORITY", 0.8)?;
            cliclack::outro("Se muestra sólo la salida importante.")?;
        }
        "medium" => {
            config.set_param("GHOSTY_CLI_MIN_PRIORITY", 0.2)?;
            cliclack::outro("Se muestra la salida de importancia media.")?;
        }
        "all" => {
            config.set_param("GHOSTY_CLI_MIN_PRIORITY", 0.0)?;
            cliclack::outro("Se muestra toda la salida.")?;
        }
        _ => unreachable!(),
    };

    Ok(())
}

pub fn configure_keyring_dialog() -> anyhow::Result<()> {
    let config = Config::global();

    if std::env::var("GHOSTY_DISABLE_KEYRING").is_ok() {
        let _ = cliclack::log::info(
            "Aviso: la variable de entorno GHOSTY_DISABLE_KEYRING está puesta y gana sobre lo que guardes aquí.",
        );
    }

    let currently_disabled = config.get_param::<String>("GHOSTY_DISABLE_KEYRING").is_ok();

    let current_status = if currently_disabled {
        "Desactivado (archivo)"
    } else {
        "Activado (llavero del sistema)"
    };

    let _ = cliclack::log::info(format!("Almacén de secretos actual: {}", current_status));
    let secrets_path = Paths::config_dir().join("secrets.yaml");
    let _ = cliclack::log::warning(format!(
        "Ojo: sin llavero, los secretos van a un archivo en texto plano ({})",
        secrets_path.display()
    ));

    let storage_option = cliclack::select("¿Dónde guardar los secretos?")
        .item(
            "keyring",
            "Llavero del sistema (recomendado)",
            "el almacén seguro del sistema para llaves y secretos",
        )
        .item(
            "file",
            "Archivo",
            "un archivo local (útil cuando el llavero no está disponible)",
        )
        .interact()?;

    match storage_option {
        "keyring" => {
            // Set to empty string to enable keyring (absence or empty = enabled)
            config.set_param("GHOSTY_DISABLE_KEYRING", Value::String("".to_string()))?;
            cliclack::outro("Secretos en el llavero del sistema (seguro)")?;
            let _ = cliclack::log::info("Puede que haga falta reiniciar ghosty para que aplique");
        }
        "file" => {
            // Set the disable flag to use file storage
            config.set_param("GHOSTY_DISABLE_KEYRING", Value::String("true".to_string()))?;
            cliclack::outro(format!(
                "Secretos en archivo ({}). ¡Cuídalo!",
                secrets_path.display(),
            ))?;
            let _ = cliclack::log::info("Puede que haga falta reiniciar ghosty para que aplique");
        }
        _ => unreachable!(),
    };

    Ok(())
}

/// Funciones experimentales
/// Dialog for toggling which experiments are enabled/disabled
pub fn toggle_experiments_dialog() -> anyhow::Result<()> {
    let experiments = ExperimentManager::get_all()?;

    if experiments.is_empty() {
        cliclack::outro("Todavía no hay experimentos.")?;
        return Ok(());
    }

    // Get currently enabled experiments for the selection
    let enabled_experiments: Vec<&String> = experiments
        .iter()
        .filter(|(_, enabled)| *enabled)
        .map(|(name, _)| name)
        .collect();

    // Let user toggle experiments
    let selected =
        cliclack::multiselect("activa experimentos: (\"espacio\" alterna, \"enter\" confirma)")
            .required(false)
            .items(
                &experiments
                    .iter()
                    .map(|(name, _)| (name, name.as_str(), MULTISELECT_VISIBILITY_HINT))
                    .collect::<Vec<_>>(),
            )
            .initial_values(enabled_experiments)
            .interact()?;

    // Update enabled status for each experiments
    for name in experiments.iter().map(|(name, _)| name) {
        ExperimentManager::set_enabled(name, selected.iter().any(|&s| s.as_str() == name))?;
    }

    cliclack::outro("Experimentos actualizados")?;
    Ok(())
}

pub async fn configure_tool_permissions_dialog() -> anyhow::Result<()> {
    let mut extensions: Vec<String> = get_enabled_extensions()
        .into_iter()
        .map(|ext| ext.name().clone())
        .collect();
    extensions.push("platform".to_string());

    extensions.sort();

    let selected_extension_name =
        cliclack::select("Elige la extensión cuyas herramientas quieres configurar")
            .items(
                &extensions
                    .iter()
                    .map(|ext| (ext.clone(), ext.clone(), ""))
                    .collect::<Vec<_>>(),
            )
            .filter_mode()
            .interact()?;

    let config = Config::global();

    let provider_name: String = config
        .get_ghosty_provider()
        .expect("No hay proveedor configurado. Configura uno primero");

    let model: String = config
        .get_ghosty_model()
        .expect("No hay modelo configurado. Configura uno primero");
    let model_config = goose::model_config::model_config_from_user_config(&provider_name, &model)?;

    let agent = Agent::new();

    let session = agent
        .config
        .session_manager
        .create_session(
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            "Tool Permission Configuration".to_string(),
            SessionType::Hidden,
            agent.config.goose_mode,
        )
        .await?;

    let extension_config = get_extension_by_name(&selected_extension_name);
    if let Some(config) = extension_config.as_ref() {
        agent
            .add_extension(config.clone(), &session.id)
            .await
            .unwrap_or_else(|_| {
                println!(
                    "{} No se pudo revisar la extensión: {}",
                    style("Error").red().italic(),
                    config.name()
                );
            });
    } else {
        println!(
            "{} No hay configuración para la extensión: {}",
            style("Aviso").yellow().italic(),
            selected_extension_name
        );
        return Ok(());
    }

    let extensions = extension_config.into_iter().collect::<Vec<_>>();
    let new_provider = create(&provider_name, extensions).await?;
    agent
        .update_provider(new_provider, model_config, &session.id)
        .await?;

    let permission_manager = PermissionManager::instance();
    let selected_tools = agent
        .list_tools(&session.id, Some(selected_extension_name.clone()))
        .await
        .into_iter()
        .map(|tool| {
            ToolInfo::new(
                &tool.name,
                tool.description
                    .as_ref()
                    .map(|d| d.as_ref())
                    .unwrap_or_default(),
                get_parameter_names(&tool),
                permission_manager.get_user_permission(&tool.name),
            )
        })
        .collect::<Vec<ToolInfo>>();

    let tool_name = cliclack::select("Elige la herramienta cuyo permiso quieres cambiar")
        .items(
            &selected_tools
                .iter()
                .map(|tool| {
                    let first_description = tool
                        .description
                        .split('.')
                        .next()
                        .unwrap_or("sin descripción")
                        .trim();
                    (tool.name.clone(), tool.name.clone(), first_description)
                })
                .collect::<Vec<_>>(),
        )
        .filter_mode()
        .interact()?;

    // Find the selected tool
    let tool = selected_tools
        .iter()
        .find(|tool| tool.name == tool_name)
        .unwrap();

    // Display tool description and current permission level
    let current_permission = match tool.permission {
        Some(PermissionLevel::AlwaysAllow) => "Siempre permitir",
        Some(PermissionLevel::AskBefore) => "Preguntar antes",
        Some(PermissionLevel::NeverAllow) => "Nunca permitir",
        None => "sin definir",
    };

    // Allow user to set the permission level
    let permission = cliclack::select(format!(
        "Permiso para la herramienta {} (actual: {})",
        tool.name, current_permission
    ))
    .item(
        "always_allow",
        "Siempre permitir",
        "se ejecuta sin preguntar",
    )
    .item(
        "ask_before",
        "Preguntar antes",
        "pide confirmación antes de ejecutarse",
    )
    .item("never_allow", "Nunca permitir", "no se ejecuta")
    .interact()?;

    let permission_label = match permission {
        "always_allow" => "Siempre permitir",
        "ask_before" => "Preguntar antes",
        "never_allow" => "Nunca permitir",
        _ => unreachable!(),
    };

    // Update the permission level in the configuration
    let new_permission = match permission {
        "always_allow" => PermissionLevel::AlwaysAllow,
        "ask_before" => PermissionLevel::AskBefore,
        "never_allow" => PermissionLevel::NeverAllow,
        _ => unreachable!(),
    };

    permission_manager.update_user_permission(&tool.name, new_permission);

    cliclack::outro(format!(
        "Permiso de la herramienta {} actualizado a {}.",
        tool.name, permission_label
    ))?;

    cliclack::outro(format!(
        "Cambios guardados en {}",
        permission_manager.get_config_path().display()
    ))?;

    Ok(())
}

pub fn configure_max_turns_dialog() -> anyhow::Result<()> {
    let config = Config::global();

    let current_max_turns: u32 = config.get_param("GHOSTY_MAX_TURNS").unwrap_or(1000);

    let max_turns_input: String = cliclack::input("Turnos máximos del agente sin pedirte nada:")
        .placeholder(&current_max_turns.to_string())
        .default_input(&current_max_turns.to_string())
        .validate(|input: &String| match input.parse::<u32>() {
            Ok(value) => {
                if value < 1 {
                    Err("Mínimo 1")
                } else {
                    Ok(())
                }
            }
            Err(_) => Err("Escribe un número válido"),
        })
        .interact()?;

    let max_turns: u32 = max_turns_input.parse()?;
    config.set_param("GHOSTY_MAX_TURNS", max_turns)?;

    cliclack::outro(format!(
        "Turnos máximos: {}. Ghosty te preguntará tras {} acciones seguidas",
        max_turns, max_turns
    ))?;

    Ok(())
}

/// Prompts the user to collect custom HTTP headers for a provider.
fn collect_custom_headers() -> anyhow::Result<Option<std::collections::HashMap<String, String>>> {
    let use_custom_headers =
        crate::commands::confirm_es("¿Este proveedor necesita headers personalizados?")
            .initial_value(false)
            .interact()?;

    if !use_custom_headers {
        return Ok(None);
    }

    let mut custom_headers = std::collections::HashMap::new();

    loop {
        let header_name: String = cliclack::input("Nombre del header:")
            .placeholder("ej. x-origin-client-id")
            .required(false)
            .interact()?;

        if header_name.is_empty() {
            break;
        }

        let header_value: String = cliclack::password(format!("Valor de '{}':", header_name))
            .mask('▪')
            .interact()?;

        custom_headers.insert(header_name, header_value);

        let add_more = crate::commands::confirm_es("¿Otro header?")
            .initial_value(false)
            .interact()?;

        if !add_more {
            break;
        }
    }

    if custom_headers.is_empty() {
        Ok(None)
    } else {
        Ok(Some(custom_headers))
    }
}

fn add_provider() -> anyhow::Result<()> {
    let config = Config::global();
    let provider_type = cliclack::select("¿Qué tipo de API es?")
        .item(
            "openai_compatible",
            "Compatible con OpenAI",
            "usa el formato de la API de OpenAI",
        )
        .item(
            "anthropic_compatible",
            "Compatible con Anthropic",
            "usa el formato de la API de Anthropic",
        )
        .item(
            "ollama_compatible",
            "Compatible con Ollama",
            "usa el formato de la API de Ollama",
        )
        .interact()?;

    let display_name: String = cliclack::input("¿Cómo se llama este proveedor?")
        .placeholder("Nombre del proveedor")
        .validate(|input: &String| {
            if input.is_empty() {
                Err("Escribe un nombre")
            } else {
                Ok(())
            }
        })
        .interact()?;

    let api_url: String = cliclack::input("URL de la API del proveedor:")
        .placeholder("https://api.example.com/v1")
        .validate(|input: &String| {
            if !input.starts_with("http://") && !input.starts_with("https://") {
                Err("La URL debe empezar con http:// o https://")
            } else {
                Ok(())
            }
        })
        .interact()?;

    let requires_auth = crate::commands::confirm_es("¿Este proveedor pide autenticación?")
        .initial_value(true)
        .interact()?;

    let api_key: String = if requires_auth {
        cliclack::password("API key:").mask('▪').interact()?
    } else {
        String::new()
    };

    let models_input: String = cliclack::input("Modelos disponibles (separados por coma):")
        .placeholder("model-a, model-b, model-c")
        .validate(|input: &String| {
            if input.trim().is_empty() {
                Err("Escribe al menos un modelo")
            } else {
                Ok(())
            }
        })
        .interact()?;

    let models: Vec<String> = models_input
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let supports_streaming =
        crate::commands::confirm_es("¿Este proveedor soporta respuestas en streaming?")
            .initial_value(true)
            .interact()?;

    let base_path_input: String =
        cliclack::input("Ruta base de la API (opcional, Enter para saltar):")
            .placeholder("ej. v1/chat/completions o project_id/v1")
            .required(false)
            .interact()?;

    let base_path = if base_path_input.trim().is_empty() {
        None
    } else {
        Some(base_path_input)
    };

    let headers = collect_custom_headers()?;

    let provider_config = create_custom_provider(CreateCustomProviderParams {
        engine: provider_type.to_string(),
        display_name: display_name.clone(),
        api_url,
        api_key: requires_auth.then_some(api_key),
        models,
        supports_streaming: Some(supports_streaming),
        headers,
        requires_auth,
        catalog_provider_id: None,
        base_path,
        preserves_thinking: None,
    })?;

    if !provider_config.models.is_empty() {
        let model_items: Vec<_> = provider_config
            .models
            .iter()
            .map(|m| (m.name.as_str(), m.name.as_str(), ""))
            .collect();
        if let Ok(model) = cliclack::select("¿Qué modelo va por defecto?")
            .items(&model_items)
            .interact()
        {
            config.set_ghosty_provider(&provider_config.name)?;
            config.set_ghosty_model(model)?;
        }
    }

    cliclack::outro(format!("Proveedor custom agregado: {}", display_name))?;
    Ok(())
}

async fn remove_provider() -> anyhow::Result<()> {
    let custom_providers_dir = goose::config::declarative_providers::custom_providers_dir();
    let custom_providers = if custom_providers_dir.exists() {
        goose::config::declarative_providers::load_custom_providers(&custom_providers_dir)?
    } else {
        Vec::new()
    };

    if custom_providers.is_empty() {
        cliclack::outro("Todavía no hay proveedores custom.")?;
        return Ok(());
    }

    let provider_items: Vec<_> = custom_providers
        .iter()
        .map(|p| (p.name.as_str(), p.display_name.as_str(), "proveedor custom"))
        .collect();

    let selected_id = cliclack::select("¿Qué proveedor custom quieres quitar?")
        .items(&provider_items)
        .filter_mode()
        .interact()?;

    // Clean up provider-specific cache files (e.g., OAuth tokens) before removing config
    if let Err(e) = goose::providers::cleanup_provider(selected_id).await {
        tracing::warn!("Failed to clean up provider cache: {}", e);
    }

    remove_custom_provider(selected_id)?;
    cliclack::outro(format!("Proveedor custom quitado: {}", selected_id))?;
    Ok(())
}

pub async fn configure_custom_provider_dialog() -> anyhow::Result<()> {
    let action = cliclack::select("¿Qué quieres hacer?")
        .item(
            "add",
            "Agregar proveedor custom",
            "una API compatible con OpenAI, Anthropic u Ollama",
        )
        .item("remove", "Quitar proveedor custom", "uno que ya agregaste")
        .interact()?;

    match action {
        "add" => add_provider(),
        "remove" => remove_provider().await,
        _ => unreachable!(),
    }?;

    print_config_file_saved()?;

    Ok(())
}

fn print_config_file_saved() -> anyhow::Result<()> {
    let config = Config::global();
    cliclack::outro(format!("Configuración guardada en {}", config.path()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_item_inside_visible_window_keeps_order() {
        let mut items: Vec<_> = (0..MAX_PROVIDER_ROWS + 1).collect();
        let expected = items.clone();

        move_selected_item_into_view(
            &mut items,
            Some(MAX_PROVIDER_ROWS - 2),
            MAX_PROVIDER_ROWS - 1,
        );

        assert_eq!(items, expected);
    }

    #[test]
    fn selected_item_outside_visible_window_moves_to_front() {
        let mut items: Vec<_> = (0..MAX_PROVIDER_ROWS + 2).collect();

        move_selected_item_into_view(
            &mut items,
            Some(MAX_PROVIDER_ROWS - 1),
            MAX_PROVIDER_ROWS - 1,
        );

        assert_eq!(items[0], MAX_PROVIDER_ROWS - 1);
        assert_eq!(
            items[1..MAX_PROVIDER_ROWS],
            (0..MAX_PROVIDER_ROWS - 1).collect::<Vec<_>>()
        );
    }

    #[test]
    fn fuzzy_provider_filter_keeps_relevant_matches_ranked_first() {
        let items = vec![
            (
                "anthropic".to_string(),
                "Anthropic".to_string(),
                String::new(),
            ),
            (
                "openrouter".to_string(),
                "OpenRouter".to_string(),
                String::new(),
            ),
            ("openai".to_string(), "OpenAI".to_string(), String::new()),
        ];

        let filtered = fuzzy_filter_provider_items(&items, "open ai");

        assert_eq!(filtered.first().map(|item| item.0.as_str()), Some("openai"));
    }
}
