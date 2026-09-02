//! The first-run notice copy.
//!
//! One string, owned by the crate that owns what is collected, so the TUI and
//! the CLI cannot drift into describing two different products. Every claim
//! below is checked against [`crate::event`] by a test: if the schema grows a
//! field this text does not cover, that test fails.
//!
//! Two properties of the wording are deliberate and load-bearing:
//!
//! 1. **The default is stated plainly and the opt-out is immediate.** The
//!    native TUI starts on the yes choice and makes the opt-out equally
//!    reachable.
//! 2. **The red lines are stated as "not collected", not as "anonymized".**
//!    Sampling and hashing are not the same promise, and a notice that implies
//!    them when neither is true is worse than no notice.

/// Headline shown above [`NOTICE_BODY`].
pub const NOTICE_HEADLINE: &str = "¿Ayudas a mejorar Ghosty?";

/// The notice itself.
///
/// Wrapped at 72 columns so it renders unchanged in the native responsive
/// modal and remains readable in an 80-column terminal.
pub const NOTICE_BODY: &str = "\
Ghosty cuenta: qué versión usas, la familia de sistema operativo y de
CPU, la duración y el desenlace de la sesión, y contadores agregados de
funciones y de errores.

Nunca recoge tus conversaciones, código, prompts, archivos, nombres de
repositorio o de rama, contenido del modelo ni credenciales — y nunca
envía una línea de tiempo por turno o por herramienta de la actividad
del agente.

Sólo te identifica un ID aleatorio guardado en esta máquina, que se
reemplaza cada 90 días. Puedes cambiar de opinión cuando quieras:
                        ghosty configure → Ajustes → Telemetría
o, sólo para una ejecución:  GHOSTY_TELEMETRY=0

Esquema completo, campo por campo, en el repo:
  crates/ghosty-telemetry/docs/TELEMETRY.md";
