// Modified from ghosty (Apache-2.0); see NOTICE.
//! `ghosty serve --setup` y `ghosty serve --check`.
//!
//! El servidor es la razón de ghosty-lite, así que dejarlo listo es parte del
//! onboarding: token, host, puerto, orígenes permitidos y builtins, guardados
//! donde `serve` los lee, más el bloque que dice cómo conectarse.

use anyhow::Result;
use cliclack::{confirm, input, intro, multiselect, outro, select};
use console::style;
use goose::config::Config;

/// Clave de config/env con el token del servidor (secreto).
pub const SERVER_TOKEN_KEY: &str = "GHOSTY_SERVER_TOKEN";
/// Host y puerto por defecto de `serve`.
pub const SERVE_HOST_KEY: &str = "GHOSTY_SERVE_HOST";
pub const SERVE_PORT_KEY: &str = "GHOSTY_SERVE_PORT";
/// Orígenes permitidos, separados por coma.
pub const SERVE_ALLOWED_ORIGINS_KEY: &str = "GHOSTY_SERVE_ALLOWED_ORIGINS";
/// Builtins que arranca `serve`, separados por coma.
pub const SERVE_BUILTINS_KEY: &str = "GHOSTY_SERVE_BUILTINS";

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 3284;

/// Un origen válido para `--allowed-origin`: no vacío, sin comodín, con
/// esquema http(s) y un `HeaderValue` legal. Es la misma regla que aplica
/// `serve` al arrancar.
pub fn validate_origin(raw: &str) -> Result<String, String> {
    let origin = raw.trim();
    if origin.is_empty() || origin == "*" {
        return Err("el origen no puede estar vacío ni ser `*`".to_string());
    }
    if !(origin.starts_with("http://") || origin.starts_with("https://")) {
        return Err("el origen debe empezar con http:// o https://".to_string());
    }
    axum::http::HeaderValue::from_str(origin)
        .map(|_| origin.to_string())
        .map_err(|e| format!("origen inválido: {e}"))
}

fn split_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Lo que `serve` necesita leer de config cuando no vienen flags.
pub struct ServeSettings {
    pub token: Option<String>,
    pub host: String,
    pub port: u16,
    pub allowed_origins: Vec<String>,
    pub builtins: Vec<String>,
}

impl ServeSettings {
    pub fn load(config: &Config) -> Self {
        let csv = |key: &str| -> Vec<String> {
            config
                .get_param::<String>(key)
                .ok()
                .map(|s| split_csv(&s))
                .unwrap_or_default()
        };
        Self {
            token: config
                .get_secret::<String>(SERVER_TOKEN_KEY)
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
            host: config
                .get_param::<String>(SERVE_HOST_KEY)
                .ok()
                .filter(|h| !h.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_HOST.to_string()),
            port: config
                .get_param::<u16>(SERVE_PORT_KEY)
                .unwrap_or(DEFAULT_PORT),
            allowed_origins: csv(SERVE_ALLOWED_ORIGINS_KEY),
            builtins: csv(SERVE_BUILTINS_KEY),
        }
    }
}

/// El asistente. Se llama desde `ghosty serve --setup` y desde el menú de
/// `ghosty configure` → Servidor.
pub async fn run_serve_setup() -> Result<()> {
    let config = Config::global();
    let current = ServeSettings::load(config);

    intro(style(" ghosty serve ").on_cyan().black())?;

    // 1. Token
    let token_choice = select("¿Cómo autenticar a los clientes?")
        .item(
            "generate",
            "Generar un token nuevo (recomendado)",
            "se guarda como secreto",
        )
        .item("manual", "Escribir uno", "si ya tienes uno repartido")
        .item(
            "none",
            "Sin autenticación",
            "sólo loopback; peligroso fuera de tu máquina",
        )
        .initial_value("generate")
        .interact()?;
    let token: Option<String> = match token_choice {
        "generate" => Some(crate::cli::generate_serve_secret_key()),
        "manual" => Some(
            cliclack::password("Token")
                .mask('▪')
                .validate(|t: &String| {
                    if t.trim().len() < 16 {
                        Err("mínimo 16 caracteres")
                    } else {
                        Ok(())
                    }
                })
                .interact()?
                .trim()
                .to_string(),
        ),
        _ => None,
    };

    // 2. Host y puerto
    let host: String = input("Host")
        .default_input(&current.host)
        .interact::<String>()?
        .trim()
        .to_string();
    if host == "0.0.0.0" && token.is_none() {
        cliclack::log::warning(
            "Escuchar en 0.0.0.0 sin token deja el agente abierto a la red. Ponle token.",
        )?;
    }
    let port: u16 = input("Puerto")
        .default_input(&current.port.to_string())
        .validate(|s: &String| {
            s.trim()
                .parse::<u16>()
                .ok()
                .filter(|p| *p > 0)
                .map(|_| ())
                .ok_or("tiene que ser un número entre 1 y 65535")
        })
        .interact::<String>()?
        .trim()
        .parse()?;

    // 3. Orígenes permitidos
    cliclack::log::info(
        "Un navegador sólo puede conectarse desde un origen de esta lista. \
         Vacía = sólo loopback. Al poner uno, los de loopback dejan de valer solos.",
    )?;
    let mut origins: Vec<String> = current.allowed_origins.clone();
    if !origins.is_empty() {
        let keep = confirm(format!(
            "Orígenes guardados: {}. ¿Conservarlos?",
            origins.join(", ")
        ))
        .initial_value(true)
        .interact()?;
        if !keep {
            origins.clear();
        }
    }
    loop {
        let shown = if origins.is_empty() {
            "(ninguno)".to_string()
        } else {
            origins.join(", ")
        };
        let add = confirm(format!("Orígenes permitidos: {shown}. ¿Agregar uno?"))
            .initial_value(false)
            .interact()?;
        if !add {
            break;
        }
        let origin: String = input("Origen (ej. https://app.ghosty.studio)")
            .validate(|s: &String| validate_origin(s).map(|_| ()))
            .interact()?;
        let origin = validate_origin(&origin).unwrap_or(origin);
        if !origins.contains(&origin) {
            origins.push(origin);
        }
    }

    // 4. Builtins
    let builtin_items = [
        ("developer", "developer", "shell y editor de archivos"),
        ("memory", "memory", "memoria persistente"),
        (
            "computercontroller",
            "computercontroller",
            "automatizar apps y documentos",
        ),
    ];
    let initial: Vec<&str> = if current.builtins.is_empty() {
        vec!["developer"]
    } else {
        current.builtins.iter().map(String::as_str).collect()
    };
    let builtins: Vec<String> = multiselect("Extensiones builtin que arranca el servidor")
        .items(&builtin_items)
        .initial_values(initial)
        .required(false)
        .interact()?
        .into_iter()
        .map(str::to_string)
        .collect();

    // Guardar
    match &token {
        Some(t) => config.set_secret(SERVER_TOKEN_KEY, t)?,
        None => {
            let _ = config.delete_secret(SERVER_TOKEN_KEY);
        }
    }
    config.set_param(SERVE_HOST_KEY, &host)?;
    config.set_param(SERVE_PORT_KEY, port)?;
    config.set_param(SERVE_ALLOWED_ORIGINS_KEY, origins.join(","))?;
    config.set_param(SERVE_BUILTINS_KEY, builtins.join(","))?;

    print_connect_block(&host, port, token.as_deref(), &origins);
    outro("Listo. Arranca con `ghosty serve`.")?;
    Ok(())
}

/// El bloque de "cómo conectarse". Lo imprime el asistente (con token) y
/// `serve` al arrancar (sin token).
pub fn print_connect_block(host: &str, port: u16, token: Option<&str>, origins: &[String]) {
    let shown_host = if host == "0.0.0.0" || host == "::" {
        "127.0.0.1"
    } else {
        host
    };
    let http = format!("http://{shown_host}:{port}/acp");
    let ws = format!("ws://{shown_host}:{port}/acp");
    let t = token.unwrap_or("<GHOSTY_SERVER_TOKEN>");
    println!();
    println!("{}", style("👻  Servidor listo.").bold());
    println!();
    println!("  Arrancar:     ghosty serve");
    println!("  HTTP / WS:    {http}   ·   {ws}");
    println!("  Salud:        curl http://{shown_host}:{port}/health");
    if let Some(token) = token {
        println!();
        println!("  Token (guárdalo, no se vuelve a mostrar):");
        println!("    {}", style(token).cyan());
    }
    println!();
    println!("  Prueba con curl (initialize):");
    println!("    curl -s {http} \\");
    println!("      -H 'Content-Type: application/json' \\");
    println!("      -H 'X-Secret-Key: {t}' \\");
    println!(
        "      -d '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1,\"clientCapabilities\":{{}}}}}}'"
    );
    println!();
    println!("  Desde el navegador (WebSocket):");
    println!("    new WebSocket(\"{ws}?token={t}\")");
    if origins.is_empty() {
        println!(
            "    {}",
            style("(sólo desde loopback; agrega tu origen con `ghosty serve --setup`)").dim()
        );
    } else {
        println!("    orígenes permitidos: {}", origins.join(", "));
    }
    println!();
}

/// `ghosty serve --check`: ¿puede arrancar tal como está? Devuelve `true` si
/// todo pasa. El token de env gana sobre el guardado, igual que en `serve`.
pub fn run_serve_check() -> Result<bool> {
    let config = Config::global();
    let s = ServeSettings::load(config);
    let mark = |good: bool| {
        if good {
            style("✓").green()
        } else {
            style("✗").red()
        }
    };

    let env_token = std::env::var(SERVER_TOKEN_KEY)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let token_ok = env_token.is_some() || s.token.is_some();
    println!(
        "{} token: {}",
        mark(token_ok),
        if env_token.is_some() {
            "configurado (variable de entorno)"
        } else if s.token.is_some() {
            "configurado (secreto guardado)"
        } else {
            "falta (GHOSTY_SERVER_TOKEN o `ghosty serve --setup`)"
        }
    );

    let bind = std::net::TcpListener::bind((s.host.as_str(), s.port));
    let bind_ok = bind.is_ok();
    println!(
        "{} bind {}:{}: {}",
        mark(bind_ok),
        s.host,
        s.port,
        match &bind {
            Ok(_) => "libre".to_string(),
            Err(e) => e.to_string(),
        }
    );
    drop(bind);

    let bad: Vec<&String> = s
        .allowed_origins
        .iter()
        .filter(|o| validate_origin(o).is_err())
        .collect();
    println!(
        "{} orígenes: {}",
        mark(bad.is_empty()),
        if s.allowed_origins.is_empty() {
            "ninguno (sólo loopback)".to_string()
        } else {
            s.allowed_origins.join(", ")
        }
    );
    for o in &bad {
        println!("    origen inválido: {o}");
    }

    println!(
        "{} builtins: {}",
        mark(true),
        if s.builtins.is_empty() {
            "developer (por defecto)".to_string()
        } else {
            s.builtins.join(", ")
        }
    );
    Ok(token_ok && bind_ok && bad.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_rules() {
        assert!(validate_origin("").is_err());
        assert!(validate_origin("*").is_err());
        assert!(validate_origin("app.ghosty.studio").is_err());
        assert_eq!(
            validate_origin(" https://app.ghosty.studio ").unwrap(),
            "https://app.ghosty.studio"
        );
    }

    #[test]
    fn csv_splits_and_trims() {
        assert_eq!(split_csv(" a, b ,,c "), vec!["a", "b", "c"]);
        assert!(split_csv("").is_empty());
    }
}
