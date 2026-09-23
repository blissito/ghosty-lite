//! Variables de entorno propias de cada sesión ACP.
//!
//! El cliente (el relé de Node) las manda en `_meta["ghosty/env"]` de
//! `session/new` y `session/load`; aquí se guardan por id de sesión y las lee
//! quien lanza procesos para esa sesión (el shell del developer y las
//! extensiones stdio), igual que `AGENT_SESSION_ID`.
//!
//! Los valores son secretos (p. ej. `GS_TOOLS_TOKEN`): nunca se loguean.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

/// Clave de `_meta` con el mapa de variables.
pub const SESSION_ENV_META_KEY: &str = "ghosty/env";

const MAX_NAME_LEN: usize = 64;
const MAX_VALUE_BYTES: usize = 8 * 1024;

pub type SessionEnv = HashMap<String, String>;

static SESSION_ENVS: LazyLock<RwLock<HashMap<String, SessionEnv>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// `^[A-Z][A-Z0-9_]{0,63}$`
fn is_valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_NAME_LEN
        && bytes[0].is_ascii_uppercase()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
}

/// Extrae el mapa de `_meta["ghosty/env"]`. `None` si la clave no viene o no es
/// un objeto (el mapa guardado se conserva); `Some` —aunque quede vacío— si
/// viene, porque entonces reemplaza al anterior. Las entradas inválidas se
/// descartan en silencio.
pub fn session_env_from_meta(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<SessionEnv> {
    let value = meta?.get(SESSION_ENV_META_KEY)?;
    let Some(object) = value.as_object() else {
        tracing::debug!("ghosty/env no es un objeto; se ignora");
        return None;
    };
    let mut env = SessionEnv::new();
    for (name, value) in object {
        if !is_valid_name(name) {
            tracing::debug!(name = %name, "ghosty/env: nombre inválido; se descarta");
            continue;
        }
        match value.as_str() {
            Some(value) if value.len() <= MAX_VALUE_BYTES => {
                env.insert(name.clone(), value.to_string());
            }
            _ => {
                tracing::debug!(name = %name, "ghosty/env: valor no string o > 8 KiB; se descarta");
            }
        }
    }
    // Tampoco se aceptan las que secuestran ejecución (PATH, LD_PRELOAD…): la
    // misma lista que aplica a las extensiones.
    Some(crate::agents::extension::Envs::new(env).get_env())
}

/// Reemplaza el mapa de la sesión.
pub fn set_session_env(session_id: &str, env: SessionEnv) {
    if let Ok(mut envs) = SESSION_ENVS.write() {
        tracing::debug!(
            session_id,
            names = ?env.keys().collect::<Vec<_>>(),
            "ghosty/env guardado"
        );
        envs.insert(session_id.to_string(), env);
    }
}

pub fn remove_session_env(session_id: &str) {
    if let Ok(mut envs) = SESSION_ENVS.write() {
        envs.remove(session_id);
    }
}

/// Copia del mapa de la sesión (vacío si no tiene).
pub fn session_env(session_id: &str) -> SessionEnv {
    SESSION_ENVS
        .read()
        .ok()
        .and_then(|envs| envs.get(session_id).cloned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn session_env_keeps_only_valid_entries() {
        let long_name = format!("A{}", "B".repeat(64));
        let big_value = "x".repeat(MAX_VALUE_BYTES + 1);
        let meta = meta(json!({
            "ghosty/env": {
                "GS_TOOLS_TOKEN": "secret",
                "A": "ok",
                "lower": "no",
                "_LEADING": "no",
                "1DIGIT": "no",
                "BAD-DASH": "no",
                long_name: "no",
                "NUMBER": 5,
                "BIG": big_value,
                "LD_PRELOAD": "no",
            }
        }));
        let env = session_env_from_meta(Some(&meta)).unwrap();
        let mut names: Vec<_> = env.keys().cloned().collect();
        names.sort();
        assert_eq!(names, vec!["A", "GS_TOOLS_TOKEN"]);
        assert_eq!(env["GS_TOOLS_TOKEN"], "secret");
    }

    #[test]
    fn session_env_absent_vs_present() {
        assert!(session_env_from_meta(None).is_none());
        assert!(session_env_from_meta(Some(&meta(json!({ "other": 1 })))).is_none());
        assert!(session_env_from_meta(Some(&meta(json!({ "ghosty/env": "nope" })))).is_none());
        assert_eq!(
            session_env_from_meta(Some(&meta(json!({ "ghosty/env": {} })))),
            Some(SessionEnv::new())
        );
    }

    #[test]
    fn session_env_registry_replace_and_remove() {
        let id = "session-env-registry-test";
        set_session_env(id, HashMap::from([("GS_TOOLS_TOKEN".into(), "one".into())]));
        assert_eq!(session_env(id)["GS_TOOLS_TOKEN"], "one");
        set_session_env(id, HashMap::from([("OTHER".into(), "two".into())]));
        let env = session_env(id);
        assert!(!env.contains_key("GS_TOOLS_TOKEN"));
        assert_eq!(env["OTHER"], "two");
        remove_session_env(id);
        assert!(session_env(id).is_empty());
    }
}
