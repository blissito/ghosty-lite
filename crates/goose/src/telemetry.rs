//! Fachada de telemetría de ghosty sobre `ghosty-telemetry`.
//!
//! Los call sites del árbol llaman a estas funciones y NADA más: aquí se
//! clasifica cada señal en un contador cerrado y se descarta todo lo demás.
//! Nunca sale de aquí un prompt, un mensaje de error, un nombre de tool ni
//! una ruta. Con la feature `telemetry` apagada, todo esto compila a no-ops.
//!
//! Semántica de los interruptores (ver `crates/ghosty-telemetry/docs/TELEMETRY.md`):
//!
//! - `GHOSTY_TELEMETRY=0` en el entorno, o `DO_NOT_TRACK=1`: kill switch de la
//!   corrida. Apaga sin tocar disco.
//! - `GHOSTY_TELEMETRY: false` en `config.yaml`: opt-out durable. Apaga y borra.
//! - Ausente: encendido.

#[cfg(not(feature = "telemetry"))]
pub use disabled::*;
#[cfg(feature = "telemetry")]
pub use enabled::*;

/// Clave (env y config) del interruptor principal.
pub const TELEMETRY_KEY: &str = "GHOSTY_TELEMETRY";
/// Clave de config que registra que el usuario declinó el aviso.
pub const NOTICE_DECLINED_KEY: &str = "GHOSTY_TELEMETRY_NOTICE_DECLINED";
/// Clave (env y config) del endpoint. Vacío = dry-run local.
pub const ENDPOINT_KEY: &str = "GHOSTY_TELEMETRY_ENDPOINT";
/// Convención de la industria: puesto a algo que no sea `0`/`false`, apaga.
pub const DO_NOT_TRACK_KEY: &str = "DO_NOT_TRACK";

#[cfg(feature = "telemetry")]
mod enabled {
    use std::path::PathBuf;
    use std::sync::{Arc, OnceLock};
    use std::time::Instant;

    use ghosty_telemetry::{
        decide, init, record, record_blocking, session_counters, set_exit_class, Counter,
        DurationBucket, ErrorCounter, Event, ExitClass, Resolver, TelemetryDecision,
        TelemetryInputs, CLI_PERSIST_TIMEOUT, SHUTDOWN_FLUSH_TIMEOUT,
    };
    pub use ghosty_telemetry::{reduce_panic_site, SessionSource, Surface};

    use super::{DO_NOT_TRACK_KEY, ENDPOINT_KEY, NOTICE_DECLINED_KEY, TELEMETRY_KEY};
    use crate::config::paths::Paths;
    use crate::config::Config;

    /// Lo que `arm` fija una sola vez por proceso.
    struct Armed {
        surface: Surface,
        started: Instant,
    }

    static ARMED: OnceLock<Armed> = OnceLock::new();
    static SHUT_DOWN: OnceLock<()> = OnceLock::new();

    /// Resolver que relee `Config::global()` y el entorno en cada llamada.
    ///
    /// `ghosty-telemetry` lo vuelve a correr antes de cada flush para ver un
    /// opt-out escrito por otro proceso, por eso no cachea nada.
    pub fn resolver() -> Resolver {
        Arc::new(|| resolve_inputs(Config::global()))
    }

    /// Construye las entradas de la decisión desde un `Config` concreto.
    ///
    /// Separado de [`resolver`] para poder probarlo contra un config temporal
    /// sin pasar por el singleton global.
    pub fn resolve_inputs(config: &Config) -> TelemetryInputs {
        let (enabled, explicit_off) = resolve_switch(config);
        let notice_declined = config
            .get_param::<serde_yaml::Value>(NOTICE_DECLINED_KEY)
            .ok()
            .and_then(|value| yaml_bool(&value))
            .unwrap_or(false);
        let endpoint = std::env::var(ENDPOINT_KEY)
            .ok()
            .or_else(|| config.get_param::<String>(ENDPOINT_KEY).ok());
        TelemetryInputs {
            home: Some(Paths::state_dir()),
            enabled,
            explicit_off,
            notice_declined,
            endpoint,
            config_path: Some(PathBuf::from(config.path())),
        }
    }

    /// `(enabled, explicit_off)`. El entorno gana y nunca es durable; el
    /// archivo es lo único que cuenta como opt-out.
    fn resolve_switch(config: &Config) -> (bool, bool) {
        if let Ok(raw) = std::env::var(DO_NOT_TRACK_KEY) {
            // `DO_NOT_TRACK=1` (o cualquier valor ilegible) apaga la corrida.
            if parse_switch(&raw) != Some(false) {
                return (false, false);
            }
        }
        if let Ok(raw) = std::env::var(TELEMETRY_KEY) {
            // Un valor que no se entiende resuelve a apagado: un typo en un
            // kill switch nunca puede resolver a "encendido".
            return (parse_switch(&raw).unwrap_or(false), false);
        }
        match config.get_param::<serde_yaml::Value>(TELEMETRY_KEY) {
            Ok(value) => match yaml_bool(&value) {
                Some(true) => (true, false),
                Some(false) => (false, true),
                None => (false, false),
            },
            Err(_) => (true, false),
        }
    }

    /// Valores aceptados por el interruptor. `None` = ilegible.
    fn parse_switch(raw: &str) -> Option<bool> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "enabled" => Some(true),
            "0" | "false" | "no" | "off" | "disabled" => Some(false),
            _ => None,
        }
    }

    fn yaml_bool(value: &serde_yaml::Value) -> Option<bool> {
        match value {
            serde_yaml::Value::Bool(b) => Some(*b),
            serde_yaml::Value::String(s) => parse_switch(s),
            serde_yaml::Value::Number(n) => n.as_i64().map(|n| n != 0),
            _ => None,
        }
    }

    /// Decide y arma la telemetría para esta superficie. Idempotente; nunca
    /// falla ni bloquea. Devuelve `true` sólo la vez que armó.
    pub fn arm(surface: Surface) -> bool {
        arm_with(resolver(), surface)
    }

    /// Como [`arm`], con un resolver propio (tests, hosts embebidos).
    pub fn arm_with(resolver: Resolver, surface: Surface) -> bool {
        if ARMED
            .set(Armed {
                surface,
                started: Instant::now(),
            })
            .is_err()
        {
            return false;
        }
        // Un pánico dentro de la decisión no puede tumbar el proceso del
        // usuario: la telemetría se queda desarmada y ya.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            if let TelemetryDecision::Enabled(consent) = decide(resolver, surface) {
                init(consent);
            }
        }));
        true
    }

    /// Arma mirando el primer argumento posicional: sin subcomando o `session`
    /// es el REPL (`Tui`), `serve` es `Serve`, `run` es `Exec`, lo demás `Cli`.
    pub fn arm_from_args(args: &[String]) -> bool {
        arm(surface_for_args(args))
    }

    /// Superficie para una línea de comandos. Las banderas antes del
    /// subcomando se saltan.
    pub fn surface_for_args(args: &[String]) -> Surface {
        let subcommand = args
            .iter()
            .skip(1)
            .find(|arg| !arg.starts_with('-'))
            .map(String::as_str);
        match subcommand {
            None | Some("session") => Surface::Tui,
            Some("serve") => Surface::Serve,
            Some("run") => Surface::Exec,
            Some(_) => Surface::Cli,
        }
    }

    fn armed_surface() -> Option<Surface> {
        ARMED.get().map(|armed| armed.surface)
    }

    // ---- fachada con los nombres que tenía posthog ----

    /// Una sesión arrancó.
    pub fn emit_session_started(source: SessionSource) {
        record(Event::SessionStart { source });
    }

    /// Clasifica un error en un contador cerrado. `message` se usa SÓLO para
    /// distinguir timeout de denegación en tools y nunca se guarda.
    pub fn emit_error(kind: &str, message: &str) {
        if let Some(counter) = classify_error(kind, message) {
            session_counters().bump_error(counter);
        }
    }

    /// El mapa de `ProviderError::telemetry_type()` y de los tipos propios de
    /// ghosty a los seis contadores de error del esquema. Lo que no cabe en
    /// ninguno (recetas, scheduler, compactación, reintentos) se descarta.
    pub fn classify_error(kind: &str, message: &str) -> Option<ErrorCounter> {
        match kind {
            "auth" | "not_configured" => Some(ErrorCounter::AuthPreflightFailed),
            "network" => Some(ErrorCounter::NetworkError),
            "server" => Some(ErrorCounter::ProviderHttp5xx),
            "rate_limit" | "credits_exhausted" | "refusal" | "context_length" | "request"
            | "endpoint_not_found" | "invalid_value" | "usage" => {
                Some(ErrorCounter::ProviderHttp4xx)
            }
            "tool_execution_failed" => {
                let lowered = message.to_ascii_lowercase();
                if lowered.contains("timed out") || lowered.contains("timeout") {
                    Some(ErrorCounter::ToolTimeout)
                } else if lowered.contains("denied")
                    || lowered.contains("not allowed")
                    || lowered.contains("permission")
                {
                    Some(ErrorCounter::ToolDeniedByPolicy)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Una tool se ejecutó.
    pub fn emit_tool_call() {
        session_counters().bump(Counter::ToolCalls);
    }

    /// Un turno del modelo terminó, con su duración de pared.
    pub fn emit_turn(secs: u64) {
        let counters = session_counters();
        counters.bump(Counter::Turns);
        counters.observe_turn_secs(secs);
    }

    /// Un job del scheduler corrió una receta.
    pub fn emit_workflow_run() {
        session_counters().bump(Counter::WorkflowRun);
    }

    /// Se enrutó a un proveedor. El nombre se canonicaliza a la lista cerrada
    /// antes de guardarse; uno desconocido queda como `"other"`.
    pub fn record_provider(name: &str) {
        session_counters().record_provider(name);
    }

    /// Emite el `session_end` con todo lo acumulado. La llama [`shutdown`].
    pub fn emit_session_ended() {
        let Some(armed) = ARMED.get() else {
            return;
        };
        let counters = session_counters();
        record(Event::SessionEnd {
            duration_bucket: DurationBucket::from_secs(armed.started.elapsed().as_secs()),
            exit_class: ghosty_telemetry::exit_class(),
            cold_start_bucket: None,
            providers: counters.providers(),
            counters: counters.counters(),
            errors: counters.errors(),
            turn_wall: counters.turn_wall(),
        });
    }

    /// El proceso reventó. `site` ya viene reducido por
    /// [`reduce_panic_site`]; nunca el mensaje del pánico.
    pub fn emit_panic(site: String) {
        set_exit_class(ExitClass::Panic);
        record_blocking(Event::Panic { site });
    }

    /// Cierra: `session_end` + flush acotado. Los comandos cortos (`Cli`) sólo
    /// sellan al buffer local sin tocar la red; el resto intenta un POST con
    /// tope de `SHUTDOWN_FLUSH_TIMEOUT`. Idempotente.
    pub fn shutdown() {
        if SHUT_DOWN.set(()).is_err() {
            return;
        }
        emit_session_ended();
        match armed_surface() {
            Some(Surface::Cli) => {
                let _ = ghosty_telemetry::persist_local_blocking(CLI_PERSIST_TIMEOUT);
            }
            Some(_) => {
                let _ = ghosty_telemetry::shutdown_blocking(SHUTDOWN_FLUSH_TIMEOUT);
            }
            None => {}
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn config_in(dir: &std::path::Path) -> Config {
            let config_path = dir.join("config").join("config.yaml");
            std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
            Config::new_with_file_secrets(&config_path, dir.join("secrets.yaml")).unwrap()
        }

        fn as_resolver(inputs: TelemetryInputs) -> Resolver {
            Arc::new(move || inputs.clone())
        }

        #[test]
        fn kill_switch_from_env_vs_opt_out_from_config() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().to_str().unwrap().to_string();
            let config = config_in(temp.path());

            // Ausente → encendido.
            {
                let _guard = env_lock::lock_env([
                    ("GHOSTY_PATH_ROOT", Some(root.as_str())),
                    (TELEMETRY_KEY, None),
                    ("GOOSE_TELEMETRY", None),
                    (DO_NOT_TRACK_KEY, None),
                    (ENDPOINT_KEY, Some("")),
                ]);
                let inputs = resolve_inputs(&config);
                assert!(inputs.enabled);
                assert!(!inputs.explicit_off);
                assert_eq!(inputs.home, Some(temp.path().join("state")));
                assert_eq!(inputs.endpoint.as_deref(), Some(""));
                assert!(decide(as_resolver(inputs), Surface::Cli).is_enabled());
            }

            // Env `0` → kill switch: apagado pero NO durable.
            {
                let _guard = env_lock::lock_env([
                    ("GHOSTY_PATH_ROOT", Some(root.as_str())),
                    (TELEMETRY_KEY, Some("0")),
                    ("GOOSE_TELEMETRY", None),
                    (DO_NOT_TRACK_KEY, None),
                ]);
                let inputs = resolve_inputs(&config);
                assert!(!inputs.enabled);
                assert!(!inputs.explicit_off);
                let decision = decide(as_resolver(inputs), Surface::Cli);
                assert!(matches!(decision, TelemetryDecision::ForcedOff));
            }

            // DO_NOT_TRACK=1 → mismo kill switch.
            {
                let _guard = env_lock::lock_env([
                    ("GHOSTY_PATH_ROOT", Some(root.as_str())),
                    (TELEMETRY_KEY, None),
                    ("GOOSE_TELEMETRY", None),
                    (DO_NOT_TRACK_KEY, Some("1")),
                ]);
                let inputs = resolve_inputs(&config);
                assert!(!inputs.enabled);
                assert!(!inputs.explicit_off);
            }

            // config.yaml `false` → opt-out durable.
            config.set_param(TELEMETRY_KEY, false).unwrap();
            {
                let _guard = env_lock::lock_env([
                    ("GHOSTY_PATH_ROOT", Some(root.as_str())),
                    (TELEMETRY_KEY, None),
                    ("GOOSE_TELEMETRY", None),
                    (DO_NOT_TRACK_KEY, None),
                ]);
                let inputs = resolve_inputs(&config);
                assert!(!inputs.enabled);
                assert!(inputs.explicit_off);
                let decision = decide(as_resolver(inputs), Surface::Cli);
                assert!(matches!(decision, TelemetryDecision::OptedOut));
            }
        }

        #[test]
        fn arm_is_idempotent() {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("state");
            let resolver: Resolver = Arc::new(move || TelemetryInputs {
                home: Some(home.clone()),
                enabled: true,
                explicit_off: false,
                notice_declined: false,
                endpoint: Some(String::new()),
                config_path: None,
            });
            let first = arm_with(resolver.clone(), Surface::Cli);
            let second = arm_with(resolver, Surface::Tui);
            // La segunda llamada nunca arma ni cambia la superficie.
            assert!(!second || !first);
            assert!(ARMED.get().is_some());
            if first {
                assert_eq!(armed_surface(), Some(Surface::Cli));
            }
            // Todas las señales son seguras después de armar.
            emit_tool_call();
            emit_turn(3);
            emit_error("network", "ignored");
            emit_error("tool_execution_failed", "tool timed out after 30s");
            record_provider("anthropic");
            record_provider("mi-proveedor-privado");
            let providers = session_counters().providers();
            assert!(providers.contains(&"anthropic".to_string()));
            assert!(providers.contains(&"other".to_string()));
            assert!(!providers.iter().any(|p| p.contains("privado")));
        }

        #[test]
        fn surface_from_argv() {
            let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            assert_eq!(surface_for_args(&args(&["ghosty"])), Surface::Tui);
            assert_eq!(
                surface_for_args(&args(&["ghosty", "session"])),
                Surface::Tui
            );
            assert_eq!(
                surface_for_args(&args(&["ghosty", "serve", "--port", "1"])),
                Surface::Serve
            );
            assert_eq!(
                surface_for_args(&args(&["ghosty", "run", "-t", "x"])),
                Surface::Exec
            );
            assert_eq!(
                surface_for_args(&args(&["ghosty", "--version"])),
                Surface::Tui
            );
            assert_eq!(
                surface_for_args(&args(&["ghosty", "configure"])),
                Surface::Cli
            );
        }

        #[test]
        fn errors_classify_without_keeping_text() {
            assert_eq!(
                classify_error("tool_execution_failed", "shell: command timed out"),
                Some(ErrorCounter::ToolTimeout)
            );
            assert_eq!(
                classify_error("tool_execution_failed", "permission denied by policy"),
                Some(ErrorCounter::ToolDeniedByPolicy)
            );
            assert_eq!(classify_error("tool_execution_failed", "boom"), None);
            assert_eq!(classify_error("recipe_encode_failed", "x"), None);
            assert_eq!(
                classify_error("server", ""),
                Some(ErrorCounter::ProviderHttp5xx)
            );
        }
    }
}

#[cfg(not(feature = "telemetry"))]
mod disabled {
    //! No-ops con la misma firma, para que los call sites no lleven `cfg`.

    /// Superficie del proceso (espejo mínimo del enum del crate real).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Surface {
        /// REPL interactivo.
        Tui,
        /// `ghosty run`.
        Exec,
        /// Comandos cortos.
        Cli,
        /// `ghosty serve`.
        Serve,
    }

    /// Origen de una sesión (espejo mínimo).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SessionSource {
        /// Abierta por una persona.
        Interactive,
        /// Reanudada.
        Resume,
        /// Bifurcada.
        Fork,
        /// Programática.
        Api,
        /// Sin decir.
        Unknown,
    }

    pub fn reduce_panic_site(_file: &str, _line: u32, _column: u32) -> String {
        String::new()
    }
    pub fn arm(_surface: Surface) -> bool {
        false
    }
    pub fn arm_from_args(_args: &[String]) -> bool {
        false
    }
    pub fn emit_session_started(_source: SessionSource) {}
    pub fn emit_error(_kind: &str, _message: &str) {}
    pub fn emit_tool_call() {}
    pub fn emit_turn(_secs: u64) {}
    pub fn emit_workflow_run() {}
    pub fn record_provider(_name: &str) {}
    pub fn emit_session_ended() {}
    pub fn emit_panic(_site: String) {}
    pub fn shutdown() {}
}
