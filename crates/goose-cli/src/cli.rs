use anyhow::Result;
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell as ClapShell};
use clap_complete_nushell::Nushell as ClapNushell;
use goose::agents::GoosePlatform;
use goose::builtin_extension::register_builtin_extensions;
use goose::config::{Config, GooseMode};
use goose::recipe::Recipe;
use goose::source_roots::SourceRoot;
use goose_mcp::mcp_server_runner::{serve, McpCommand};
use goose_mcp::{ComputerControllerServer, MemoryServer, TutorialServer};

use crate::commands::configure::handle_configure;
use crate::commands::info::handle_info;
use crate::commands::plugin::{handle_plugin_install, handle_plugin_update};
use crate::commands::recipe::{handle_deeplink, handle_list, handle_open, handle_validate};
use crate::commands::term::{
    handle_term_info, handle_term_init, handle_term_log, handle_term_run, Shell,
};

use crate::commands::schedule::{
    handle_schedule_add, handle_schedule_cron_help, handle_schedule_list, handle_schedule_remove,
    handle_schedule_run_now, handle_schedule_services_status, handle_schedule_services_stop,
    handle_schedule_sessions,
};
use crate::commands::session::{handle_session_list, handle_session_remove};
use crate::commands::skills::handle_skills_list;
use crate::recipes::extract_from_cli::extract_recipe_info_from_cli;
use crate::recipes::recipe::{explain_recipe, render_recipe_as_yaml};
use crate::session::{build_session, SessionBuilderConfig};
use goose::agents::Container;
use goose::session::session_manager::SessionType;
use goose::session::SessionManager;
use std::io::Read;
use std::path::PathBuf;
const GHOSTY_SERVER_TOKEN_ENV: &str = "GHOSTY_SERVER_TOKEN";

pub(crate) fn generate_serve_secret_key() -> String {
    use rand::distr::{Alphanumeric, SampleString};

    format!("ghl-{}", Alphanumeric.sample_string(&mut rand::rng(), 32))
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ServePlatform {
    #[default]
    Cli,
    Desktop,
}

impl From<ServePlatform> for GoosePlatform {
    fn from(platform: ServePlatform) -> Self {
        match platform {
            ServePlatform::Cli => GoosePlatform::GooseCli,
            ServePlatform::Desktop => GoosePlatform::GooseDesktop,
        }
    }
}

#[derive(Parser)]
#[command(name = "ghosty", author, version, display_name = "", about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Args, Debug, Clone)]
#[group(required = false, multiple = false)]
pub struct Identifier {
    #[arg(
        short = 'n',
        long,
        value_name = "NAME",
        help = "Nombre de la sesión (ej. 'project-x')",
        long_help = "Nombre de la sesión de chat. Con --resume, reanuda esa sesión si existe."
    )]
    pub name: Option<String>,

    #[arg(
        long = "session-id",
        alias = "id",
        value_name = "SESSION_ID",
        help = "ID de sesión (ej. '20250921_143022')",
        long_help = "ID de la sesión a reanudar. Requiere --resume."
    )]
    pub session_id: Option<String>,

    #[arg(
        long,
        value_name = "PATH",
        help = "Legado: ruta de la sesión de chat",
        long_help = "Parámetro de compatibilidad. Saca el ID de sesión de la ruta del archivo (ej. '/ruta/a/20250325_200615.jsonl' -> '20250325_200615')."
    )]
    pub path: Option<PathBuf>,
}

/// Session behavior options shared between Session and Run commands
#[derive(Args, Debug, Clone, Default)]
pub struct SessionOptions {
    #[arg(
        long,
        help = "Modo debug: contenido completo, sin truncar",
        long_help = "Muestra las respuestas de las herramientas completas, sin truncar, y las rutas enteras."
    )]
    pub debug: bool,

    #[arg(
        long = "max-tool-repetitions",
        value_name = "NUMBER",
        help = "Máximo de llamadas idénticas y consecutivas a una herramienta",
        long_help = "Cuántas veces seguidas se puede llamar a la misma herramienta con los mismos parámetros. Evita bucles infinitos."
    )]
    pub max_tool_repetitions: Option<u32>,

    #[arg(
        long = "max-turns",
        value_name = "NUMBER",
        help = "Turnos máximos sin intervención del usuario (default: 1000)",
        long_help = "Cuántos turnos (iteraciones) puede dar el agente sin pedirte nada para continuar."
    )]
    pub max_turns: Option<u32>,

    #[arg(
        long = "container",
        value_name = "CONTAINER_ID",
        help = "ID del contenedor Docker donde correr las extensiones",
        long_help = "Corre las extensiones (stdio y builtin) dentro del contenedor indicado. La extensión debe existir ahí. Para las builtin, ghosty debe estar instalado dentro del contenedor."
    )]
    pub container: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StreamableHttpOptions {
    pub url: String,
    pub timeout: u64,
}

fn parse_streamable_http_extension(input: &str) -> Result<StreamableHttpOptions, String> {
    let mut input_iter = input.split_whitespace();
    let (mut url, mut timeout) = (String::new(), goose::config::DEFAULT_EXTENSION_TIMEOUT);

    if let Some(url_str) = input_iter.next() {
        url.push_str(url_str);
    }

    for kv_pair in input_iter {
        if !kv_pair.contains('=') {
            continue;
        }

        let (key, value) = kv_pair.split_once('=').unwrap();

        // We Can have more keys here for setting other properties
        if key == "timeout" {
            if let Ok(seconds) = value.parse::<u64>() {
                timeout = seconds;
            }
        }
    }

    Ok(StreamableHttpOptions { url, timeout })
}

/// Extension configuration options shared between Session and Run commands
#[derive(Args, Debug, Clone, Default)]
pub struct ExtensionOptions {
    #[arg(
        long = "with-extension",
        value_name = "COMMAND",
        help = "Agrega extensiones stdio (se puede repetir)",
        long_help = "Agrega extensiones stdio a partir del comando completo con variables de entorno. Se puede repetir. Formato: '[nombre:]ENV1=val1 ENV2=val2 comando args...'. Sin el nombre opcional, la extensión se llama como el comando, que es el lanzador cuando se arranca a través de uno ('npx', 'python', 'uvx', ...); las que acabarían con el mismo nombre se llaman por su línea de comando completa.",
        action = clap::ArgAction::Append
    )]
    pub extensions: Vec<String>,

    #[arg(
        long = "with-streamable-http-extension",
        value_name = "URL",
        help = "Agrega extensiones Streamable HTTP (se puede repetir)",
        long_help = "Agrega extensiones Streamable HTTP desde una URL. Se puede repetir. Formato: 'url...' o 'url... timeout=100' para cambiar el timeout",
        action = clap::ArgAction::Append,
        value_parser = parse_streamable_http_extension
    )]
    pub streamable_http_extensions: Vec<StreamableHttpOptions>,

    #[arg(
        long = "with-builtin",
        value_name = "NAME",
        help = "Agrega extensiones builtin por nombre (ej. 'developer' o varias: 'developer,memory')",
        long_help = "Una o más extensiones builtin que vienen con ghosty, por nombre y separadas por coma",
        value_delimiter = ','
    )]
    pub builtins: Vec<String>,

    #[arg(
        long = "no-profile",
        help = "No cargues las extensiones por defecto; sólo las indicadas en la línea de comandos"
    )]
    pub no_profile: bool,
}

/// Input source and recipe options for the run command
#[derive(Args, Debug, Clone, Default)]
pub struct InputOptions {
    /// Path to instruction file containing commands
    #[arg(
        short,
        long,
        value_name = "FILE",
        help = "Ruta al archivo de instrucciones. Usa - para stdin.",
        conflicts_with = "input_text",
        conflicts_with = "recipe"
    )]
    pub instructions: Option<String>,

    /// Input text containing commands
    #[arg(
        short = 't',
        long = "text",
        value_name = "TEXT",
        help = "Texto de entrada para ghosty",
        long_help = "Texto con las instrucciones para ghosty. Sustituye al argumento del archivo de instrucciones.",
        conflicts_with = "instructions",
        conflicts_with = "recipe"
    )]
    pub input_text: Option<String>,

    /// Recipe name or full path to the recipe file
    #[arg(
        short = None,
        long = "recipe",
        value_name = "RECIPE_NAME or FULL_PATH_TO_RECIPE_FILE",
        help = "Nombre o ruta completa de la receta (usa --explain para ver sus detalles)",
        long_help = "Nombre o ruta completa del archivo de receta que define la configuración del agente. Con --explain se ven título, descripción y parámetros.",
        conflicts_with = "instructions",
        conflicts_with = "input_text"
    )]
    pub recipe: Option<String>,

    /// Additional system prompt to customize agent behavior
    #[arg(
        long = "system",
        value_name = "TEXT",
        help = "System prompt adicional para ajustar al agente",
        long_help = "Instrucciones de sistema adicionales para ajustar el comportamiento del agente",
        conflicts_with = "recipe"
    )]
    pub system: Option<String>,

    #[arg(
        long,
        value_name = "KEY=VALUE",
        help = "Parámetros dinámicos (ej. --params username=alice --params channel_name=general)",
        long_help = "Parámetros clave=valor para la receta. Se puede repetir.",
        action = clap::ArgAction::Append,
        value_parser = parse_key_val,
    )]
    pub params: Vec<(String, String)>,

    /// Additional sub-recipe file paths
    #[arg(
        long = "sub-recipe",
        value_name = "RECIPE",
        help = "Nombre o ruta de sub-receta (se puede repetir)",
        long_help = "Sub-recetas que acompañan a la principal. Pueden ser:\n  - Nombres de receta en GitHub (si GHOSTY_RECIPE_GITHUB_REPO está configurado)\n  - Rutas locales a archivos YAML\nSe puede repetir para incluir varias.",
        action = clap::ArgAction::Append
    )]
    pub additional_sub_recipes: Vec<String>,

    /// Show the recipe title, description, and parameters
    #[arg(
        long = "explain",
        help = "Muestra título, descripción y parámetros de la receta"
    )]
    pub explain: bool,

    /// Print the rendered recipe instead of running it
    #[arg(
        long = "render-recipe",
        help = "Imprime la receta renderizada en vez de correrla."
    )]
    pub render_recipe: bool,
}

/// Output configuration options for the run command
#[derive(Args, Debug, Clone)]
pub struct OutputOptions {
    /// Quiet mode - suppress non-response output
    #[arg(
        short = 'q',
        long = "quiet",
        help = "Modo silencioso: sólo la respuesta del modelo a stdout"
    )]
    pub quiet: bool,

    /// Output format (text, json, stream-json)
    #[arg(
        long = "output-format",
        value_name = "FORMAT",
        help = "Formato de salida (text, json, stream-json)",
        default_value = "text",
        value_parser = clap::builder::PossibleValuesParser::new(["text", "json", "stream-json"])
    )]
    pub output_format: String,
}

impl Default for OutputOptions {
    fn default() -> Self {
        Self {
            quiet: false,
            output_format: "text".to_string(),
        }
    }
}

/// Model/provider override options
#[derive(Args, Debug, Clone, Default)]
pub struct ModelOptions {
    /// Provider to use for this run (overrides environment variable)
    #[arg(
        long = "provider",
        value_name = "PROVIDER",
        help = "Proveedor de LLM a usar (ej. 'openai', 'anthropic')",
        long_help = "Pisa GHOSTY_PROVIDER sólo para esta ejecución. Hay easybits, anthropic, openai, ollama y otros."
    )]
    pub provider: Option<String>,

    /// Model to use for this run (overrides environment variable)
    #[arg(
        long = "model",
        value_name = "MODEL",
        help = "Modelo a usar (ej. 'deepseek-v4-flash', 'claude-sonnet-4-20250514')",
        long_help = "Pisa GHOSTY_MODEL sólo para esta ejecución. El proveedor debe soportar el modelo."
    )]
    pub model: Option<String>,
}

/// Run execution behavior options
#[derive(Args, Debug, Clone, Default)]
pub struct RunBehavior {
    /// Continue in interactive mode after processing input
    #[arg(
        short = 's',
        long = "interactive",
        help = "Sigue en modo interactivo tras procesar la entrada inicial"
    )]
    pub interactive: bool,

    /// Run without storing a session file
    #[arg(
        long = "no-session",
        help = "Corre sin guardar sesión",
        long_help = "Ejecuta sin crear ni usar sesión. Útil para corridas automatizadas.",
        conflicts_with_all = ["resume", "name", "path"]
    )]
    pub no_session: bool,

    /// Resume a previous run
    #[arg(
        short,
        long,
        action = clap::ArgAction::SetTrue,
        help = "Reanuda una corrida anterior",
        long_help = "Continúa una corrida anterior conservando estado y contexto."
    )]
    pub resume: bool,

    /// Print generation statistics after completion
    #[arg(
        long = "stats",
        help = "Imprime estadísticas de generación al terminar"
    )]
    pub stats: bool,

    /// Scheduled job ID (used internally for scheduled executions)
    #[arg(
        long = "scheduled-job-id",
        value_name = "ID",
        help = "ID del trabajo programado que disparó esta ejecución (uso interno)",
        long_help = "Parámetro interno cuando este run lo dispara un trabajo programado. Asocia la sesión al schedule.",
        hide = true
    )]
    pub scheduled_job_id: Option<String>,
}

async fn get_or_create_session_id(
    identifier: Option<Identifier>,
    resume: bool,
    no_session: bool,
    goose_mode: GooseMode,
) -> Result<Option<String>> {
    if no_session {
        return Ok(None);
    }

    let session_manager = SessionManager::instance();

    let resolved_id = if resume {
        let Some(id) = identifier else {
            let sessions = session_manager
                .list_sessions_by_types(&[SessionType::User])
                .await?;
            let session_id = sessions
                .first()
                .map(|s| s.id.clone())
                .ok_or_else(|| anyhow::anyhow!("No session found to resume"))?;
            return Ok(Some(session_id));
        };

        if let Some(session_id) = id.session_id {
            session_id
        } else if let Some(name) = id.name {
            let sessions = session_manager.list_sessions().await?;
            sessions
                .into_iter()
                .find(|s| s.name == name || s.id == name)
                .map(|s| s.id)
                .ok_or_else(|| anyhow::anyhow!("No session found with name '{}'", name))?
        } else if let Some(path) = id.path {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    anyhow::anyhow!("Could not extract session ID from path: {:?}", path)
                })?
        } else {
            return Err(anyhow::anyhow!("Invalid identifier"));
        }
    } else {
        let Some(id) = identifier else {
            let session = session_manager
                .create_session(
                    std::env::current_dir()?,
                    "CLI Session".to_string(),
                    SessionType::User,
                    goose_mode,
                )
                .await?;
            return Ok(Some(session.id));
        };

        if id.session_id.is_some() {
            return Err(anyhow::anyhow!("Cannot use --session-id without --resume"));
        }

        let has_user_provided_name = id.name.is_some();
        let name = id.name.unwrap_or_else(|| "CLI Session".to_string());
        let session = session_manager
            .create_session(
                std::env::current_dir()?,
                name.clone(),
                SessionType::User,
                goose_mode,
            )
            .await?;

        if has_user_provided_name {
            session_manager
                .update(&session.id)
                .user_provided_name(name)
                .apply()
                .await?;
        }

        return Ok(Some(session.id));
    };

    Ok(Some(resolved_id))
}

async fn lookup_session_id(identifier: Identifier) -> Result<String> {
    let session_manager = SessionManager::instance();

    if let Some(session_id) = identifier.session_id {
        Ok(session_id)
    } else if let Some(name) = identifier.name {
        let sessions = session_manager.list_sessions().await?;
        sessions
            .into_iter()
            .find(|s| s.name == name || s.id == name)
            .map(|s| s.id)
            .ok_or_else(|| anyhow::anyhow!("No session found with name '{}'", name))
    } else if let Some(path) = identifier.path {
        path.file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("Could not extract session ID from path: {:?}", path))
    } else {
        Err(anyhow::anyhow!("No identifier provided"))
    }
}

fn parse_key_val(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((key, value)) => Ok((key.to_string(), value.to_string())),
        None => Err(format!("invalid KEY=VALUE: {}", s)),
    }
}

#[derive(Subcommand)]
enum SessionCommand {
    #[command(about = "Lista las sesiones")]
    List {
        #[arg(
            short,
            long,
            help = "Formato de salida (text, json)",
            default_value = "text"
        )]
        format: String,

        #[arg(
            long = "ascending",
            help = "Ordena por fecha ascendente (la más vieja primero)",
            long_help = "Ordena por fecha ascendente (la más vieja primero). Por defecto es descendente."
        )]
        ascending: bool,

        #[arg(
            short = 'w',
            short_alias = 'p',
            long = "working_dir",
            help = "Filtra por directorio de trabajo"
        )]
        working_dir: Option<PathBuf>,

        #[arg(short = 'l', long = "limit", help = "Limita el número de resultados")]
        limit: Option<usize>,
    },
    #[command(about = "Borra sesiones. Interactivo si no das ID, nombre ni regex.")]
    Remove {
        #[command(flatten)]
        identifier: Option<Identifier>,
        #[arg(short = 'r', long, help = "Regex de las sesiones a borrar (opcional)")]
        regex: Option<String>,
    },
    #[command(about = "Exporta una sesión")]
    Export {
        #[command(flatten)]
        identifier: Option<Identifier>,

        #[arg(
            short,
            long,
            help = "Archivo de salida (default: stdout)",
            long_help = "Ruta donde guardar el export. Sin ella, sale por stdout"
        )]
        output: Option<PathBuf>,

        #[arg(
            long = "format",
            value_name = "FORMAT",
            help = "Formato de salida (markdown, json, yaml)",
            default_value = "markdown"
        )]
        format: String,
    },
    #[command(about = "Importa una sesión desde JSON o un .jsonl de Claude Code / Codex / Pi")]
    Import {
        #[arg(
            help = "Ruta a un export de sesión de ghosty o a un .jsonl de Claude Code, Codex o Pi"
        )]
        input: String,
    },
    #[command(name = "diagnostics")]
    Diagnostics {
        #[command(flatten)]
        identifier: Option<Identifier>,

        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum SchedulerCommand {
    #[command(about = "Agrega un trabajo programado")]
    Add {
        #[arg(
            long = "schedule-id",
            alias = "id",
            help = "ID único del trabajo recurrente"
        )]
        schedule_id: String,
        #[arg(
            long,
            help = "Expresión cron",
            long_help = "Expresión cron de cuándo correr. Ejemplos:\n  '0 * * * *'     - cada hora, al minuto 0\n  '0 */2 * * *'   - cada 2 horas\n  '@hourly'       - cada hora (atajo)\n  '0 9 * * *'     - todos los días a las 9:00\n  '0 9 * * 1'     - los lunes a las 9:00\n  '0 0 1 * *'     - el primer día de cada mes a medianoche"
        )]
        cron: String,
        #[arg(long, help = "Receta (ruta al archivo o receta en base64)")]
        recipe_source: String,
        #[arg(
            long,
            value_name = "KEY=VALUE",
            help = "Parámetro de la receta en formato CLAVE=VALOR (se puede repetir)",
            action = clap::ArgAction::Append,
            value_parser = parse_key_val,
        )]
        params: Vec<(String, String)>,
    },
    #[command(about = "Lista los trabajos programados")]
    List {},
    #[command(about = "Quita un trabajo programado por ID")]
    Remove {
        #[arg(
            long = "schedule-id",
            alias = "id",
            help = "ID del trabajo a quitar (elimina la recurrencia)"
        )]
        schedule_id: String,
    },
    /// List sessions created by a specific schedule
    #[command(about = "Lista las sesiones creadas por un schedule")]
    Sessions {
        /// ID of the schedule
        #[arg(long = "schedule-id", alias = "id", help = "ID del schedule")]
        schedule_id: String,
        #[arg(short = 'l', long, help = "Máximo de sesiones a devolver")]
        limit: Option<usize>,
    },
    #[command(about = "Corre un trabajo programado ahora")]
    RunNow {
        /// ID of the schedule to run
        #[arg(long = "schedule-id", alias = "id", help = "ID del schedule a correr")]
        schedule_id: String,
    },
    /// Check status of scheduler services (deprecated - no external services needed)
    #[command(about = "[Obsoleto] Estado de los servicios del scheduler")]
    ServicesStatus {},
    /// Stop scheduler services (deprecated - no external services needed)
    #[command(about = "[Obsoleto] Detiene los servicios del scheduler")]
    ServicesStop {},
    /// Show cron expression examples and help
    #[command(about = "Ejemplos y ayuda de expresiones cron")]
    CronHelp {},
}

#[derive(Subcommand)]
enum PluginCommand {
    /// Install a plugin from a git repository URL
    #[command(about = "Instala un plugin desde la URL de un repo git")]
    Install {
        #[arg(
            long,
            help = "Actualiza el plugin automáticamente antes de cargar sus skills"
        )]
        auto_update: bool,

        #[arg(help = "URL de un repo git con un plugin soportado")]
        url: String,
    },

    /// Update an installed git-backed plugin
    #[command(about = "Actualiza un plugin instalado desde git")]
    Update {
        #[arg(help = "Nombre del plugin a actualizar")]
        name: String,
    },
}

#[derive(Subcommand)]
enum SkillsCommand {
    /// Lista las skills disponibles para el agente
    #[command(about = "Lista las skills disponibles para el agente")]
    List,
}

#[derive(Subcommand)]
enum RecipeCommand {
    /// Validate a recipe file
    #[command(about = "Valida una receta")]
    Validate {
        /// Recipe name to get recipe file to validate
        #[arg(help = "nombre o ruta completa de la receta a validar")]
        recipe_name: String,
    },

    /// Generate a deeplink for a recipe file
    #[command(about = "Genera un deeplink de una receta")]
    Deeplink {
        /// Recipe name to get recipe file to generate deeplink
        #[arg(help = "nombre o ruta completa de la receta")]
        recipe_name: String,
        /// Recipe parameters in key=value format (can be specified multiple times)
        #[arg(
            short = 'p',
            long = "param",
            value_name = "KEY=VALUE",
            help = "Parámetro de la receta en formato clave=valor (se puede repetir)"
        )]
        params: Vec<String>,
    },

    /// Abre una receta en la app de escritorio
    #[command(about = "Abre una receta en la app de escritorio")]
    Open {
        /// Recipe name to get recipe file to open
        #[arg(help = "nombre o ruta completa de la receta")]
        recipe_name: String,
        /// Recipe parameters in key=value format (can be specified multiple times)
        #[arg(
            short = 'p',
            long = "param",
            value_name = "KEY=VALUE",
            help = "Parámetro de la receta en formato clave=valor (se puede repetir)"
        )]
        params: Vec<String>,
    },

    /// List available recipes
    #[command(about = "Lista las recetas disponibles")]
    List {
        /// Output format (text, json)
        #[arg(
            long = "format",
            value_name = "FORMAT",
            help = "Formato de salida (text, json)",
            default_value = "text"
        )]
        format: String,

        /// Show verbose information including recipe descriptions
        #[arg(short, long, help = "Información detallada, con descripciones")]
        verbose: bool,
    },
}

#[derive(Subcommand)]
enum Command {
    /// Configura ghosty
    #[command(about = "Configura ghosty: proveedor, extensiones, servidor y ajustes")]
    Configure {},

    /// Muestra la información de ghosty
    #[command(about = "Muestra versión, rutas y estado de ghosty")]
    Info {
        /// Show verbose information including current configuration
        #[arg(short, long, help = "Información detallada, incluido config.yaml")]
        verbose: bool,
        #[arg(long, help = "Prueba la conexión con el proveedor")]
        check: bool,
    },

    #[command(about = "Comprueba que tu instalación de ghosty funciona")]
    Doctor {},

    /// Manage system prompts and behaviors
    #[command(about = "Corre uno de los servidores MCP que vienen con ghosty")]
    Mcp {
        #[arg(value_parser = clap::value_parser!(McpCommand))]
        server: McpCommand,
    },

    /// Corre ghosty como agente ACP (Agent Client Protocol)
    #[command(about = "Corre ghosty como agente ACP por stdio")]
    Acp {
        /// Add builtin extensions by name
        #[arg(
            long = "with-builtin",
            value_name = "NAME",
            help = "Agrega extensiones builtin por nombre (ej. 'developer' o varias: 'developer,memory')",
            long_help = "Una o más extensiones builtin que vienen con ghosty, por nombre y separadas por coma",
            value_delimiter = ','
        )]
        builtins: Vec<String>,

        #[arg(long, help = "Activa la ejecución programada de recetas")]
        enable_scheduler: bool,
    },

    /// Share or connect to agents peer-to-peer over iroh

    /// Start ACP server over HTTP and WebSocket
    #[command(about = "Arranca el servidor ACP por HTTP y WebSocket")]
    Serve {
        #[arg(long, help = "Host donde escuchar (default: el guardado, o 127.0.0.1)")]
        host: Option<String>,

        #[arg(long, help = "Puerto (default: el guardado, o 3284)")]
        port: Option<u16>,

        #[arg(
            long,
            help = "Asistente interactivo: token, host, puerto, orígenes y builtins"
        )]
        setup: bool,

        #[arg(
            long,
            help = "Comprueba que el servidor puede arrancar tal como está (sale 0 o 1)"
        )]
        check: bool,

        #[arg(long, help = "Sirve ACP con TLS")]
        tls: bool,

        #[arg(long = "tls-cert-path", value_name = "PATH")]
        tls_cert_path: Option<String>,

        #[arg(long = "tls-key-path", value_name = "PATH")]
        tls_key_path: Option<String>,

        #[arg(long, value_enum, default_value_t = ServePlatform::Cli)]
        platform: ServePlatform,

        #[arg(
            long = "with-builtin",
            value_name = "NAME",
            help = "Agrega extensiones builtin por nombre (ej. 'developer' o varias: 'developer,memory')",
            long_help = "Una o más extensiones builtin que vienen con ghosty, por nombre y separadas por coma",
            value_delimiter = ',',
            action = clap::ArgAction::Append
        )]
        builtins: Vec<String>,

        #[arg(
            long = "dangerously-unauthenticated",
            help = "Arranca el endpoint ACP sin exigir GHOSTY_SERVER_TOKEN"
        )]
        dangerously_unauthenticated: bool,

        #[arg(
            long = "allowed-origin",
            value_name = "ORIGIN",
            action = clap::ArgAction::Append,
            help = "Permite un Origin exacto para CORS de ACP; se puede repetir y REEMPLAZA los orígenes loopback por defecto"
        )]
        allowed_origins: Vec<String>,

        #[arg(long, help = "Activa la ejecución programada de recetas")]
        enable_scheduler: bool,
    },

    /// Start or resume interactive chat sessions
    #[command(
        about = "Inicia o reanuda una sesión de chat interactiva",
        visible_alias = "s"
    )]
    Session {
        #[command(subcommand)]
        command: Option<SessionCommand>,

        #[command(flatten)]
        identifier: Option<Identifier>,

        /// Resume a previous session
        #[arg(
            short,
            long,
            help = "Reanuda una sesión anterior (la última, o la de --name/--session-id)",
            long_help = "Continúa una sesión anterior. Con --name o --session-id reanuda ésa; si no, la más reciente."
        )]
        resume: bool,

        /// Fork a previous session (creates new session with copied history)
        #[arg(
            long,
            requires = "resume",
            help = "Bifurca una sesión anterior (nueva sesión con el historial copiado)",
            long_help = "Crea una sesión nueva copiando los mensajes de otra. Requiere --resume. Con --name o --session-id bifurca ésa; si no, la más reciente."
        )]
        fork: bool,

        /// Open the session's conversation in $EDITOR before starting
        #[arg(
            long,
            requires = "resume",
            help = "Edita la conversación en $EDITOR antes de empezar",
            long_help = "Abre la conversación en tu editor ($VISUAL / $EDITOR / vi) antes de reanudar. Con --fork, crea una sesión nueva a partir del resultado editado."
        )]
        edit: bool,

        /// Show message history when resuming
        #[arg(
            long,
            help = "Muestra los mensajes anteriores al reanudar",
            requires = "resume"
        )]
        history: bool,

        #[command(flatten)]
        session_opts: SessionOptions,

        #[command(flatten)]
        extension_opts: ExtensionOptions,

        #[command(flatten)]
        model_opts: ModelOptions,
    },

    /// Execute commands from an instruction file
    #[command(about = "Ejecuta instrucciones desde un archivo o stdin")]
    Run {
        #[command(flatten)]
        input_opts: InputOptions,

        #[command(flatten)]
        identifier: Option<Identifier>,

        #[command(flatten)]
        run_behavior: RunBehavior,

        #[command(flatten)]
        session_opts: SessionOptions,

        #[command(flatten)]
        extension_opts: ExtensionOptions,

        #[command(flatten)]
        output_opts: OutputOptions,

        #[command(flatten)]
        model_opts: ModelOptions,
    },

    /// Recipe utilities for validation and deeplinking
    #[command(about = "Utilidades de recetas: validar y deeplinks")]
    Recipe {
        #[command(subcommand)]
        command: RecipeCommand,
    },

    /// Skill utilities
    #[command(about = "Utilidades de skills")]
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },

    /// Manage plugins
    #[command(about = "Administra plugins")]
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },

    /// Manage scheduled jobs
    #[command(about = "Administra trabajos programados", visible_alias = "sched")]
    Schedule {
        #[command(subcommand)]
        command: SchedulerCommand,
    },

    /// Terminal-integrated session (one session per terminal)
    #[command(
        about = "Sesión de ghosty integrada a la terminal",
        long_about = "Corre una sesión de ghosty atada a tu ventana de terminal.\n\
                      Cada terminal mantiene su propia sesión persistente, que se reanuda sola.\n\n\
                      Instalación:\n  \
                        eval \"$(ghosty term init zsh)\"  # zsh/bash\n  \
                        let init = ($nu.cache-dir | path join \"ghosty-term-init.nu\"); ^ghosty term init nu | save --force $init; source $init\n\n\
                      Uso:\n  \
                        ghosty term run \"lista los archivos de este directorio\"\n  \
                        @ghosty \"crea un script de python\"  # con el alias\n  \
                        @g \"pregunta rápida\"  # alias corto"
    )]
    Term {
        #[command(subcommand)]
        command: TermCommand,
    },

    /// Generate completions for various shells
    #[command(
        about = "Genera el script de autocompletado o el módulo de Nushell para el shell indicado"
    )]
    Completion {
        #[arg(value_enum)]
        shell: CompletionShell,

        #[arg(long, default_value = "ghosty", help = "Nombre del binario")]
        bin_name: String,
    },

    /// Local code review.
    ///
    /// Discovers `**/.agents/checks/*.md` subagent reviewers and
    /// `**/.agents/REVIEW.md` scoped prompt overrides, builds a review
    /// request from the working tree (or an explicit diff range), and
    /// runs the review through ghosty.
    #[command(about = "Revisa el diff actual con ghosty")]
    Review {
        /// Diff range to review (e.g. "main...HEAD"). Defaults to the working
        /// tree vs HEAD.
        #[arg(value_name = "RANGE")]
        range: Option<String>,

        /// Path to a Markdown file with a custom base review prompt. Replaces
        /// the embedded default prompt.
        #[arg(long = "prompt", value_name = "FILE")]
        prompt: Option<PathBuf>,

        /// Default model used for the main review agent and for any check
        /// that does not declare its own `model:` in frontmatter.
        #[arg(long = "model", value_name = "MODEL")]
        model: Option<String>,

        /// Provider for the main review agent.
        #[arg(long = "provider", value_name = "PROVIDER")]
        provider: Option<String>,

        /// Force every discovered check to use this model, regardless of
        /// the check's own `model:` field.
        #[arg(long = "override-model", value_name = "MODEL")]
        override_model: Option<String>,

        /// Default `turn-limit` for orchestrated main-pass subprocesses and
        /// for checks that do not declare their own. Does not cap the legacy
        /// `--no-orchestrate` in-process main agent.
        #[arg(long = "turn-limit", value_name = "N")]
        turn_limit: Option<usize>,

        /// Print the assembled review prompt and discovered checks instead of
        /// running the review.
        #[arg(long = "dry-run")]
        dry_run: bool,

        /// Suppress non-result output from the underlying agent.
        #[arg(long, short = 'q')]
        quiet: bool,

        /// Disable the Rust-driven parallel orchestrator and fall back to
        /// the single-prompt path that asks the main agent to delegate
        /// each check via `delegate(... async: true ...)`. The default
        /// orchestrator dispatches one `goose run` subprocess per check
        /// (capped at 4 concurrent), bounding wall-clock to the slowest
        /// single check rather than waiting on the model to issue
        /// dispatches.
        /// Checks with an explicit tool allowlist require the default orchestrator.
        #[arg(long = "no-orchestrate")]
        no_orchestrate: bool,

        /// Additional free-form instructions to prepend to the review
        /// (e.g. PR intent, commit-message context, "this is a refactor,
        /// flag any behavior change"). Mirrors `amp review --instructions`
        /// for drop-in compatibility with existing reviewer wrappers.
        #[arg(long = "instructions", short = 'i', value_name = "TEXT")]
        instructions: Option<String>,

        /// Restrict the review to a specific set of files. Other files in
        /// the diff are still passed to the agent for context but are
        /// excluded from the assembled diff sent to checks. Mirrors
        /// `amp review --files`.
        #[arg(long = "files", short = 'f', value_name = "FILE", num_args = 1..)]
        files: Vec<String>,

        /// Only run checks whose `name` matches one of these. Other
        /// discovered checks are skipped. Mirrors `amp review --check-filter`.
        #[arg(long = "check-filter", short = 'c', value_name = "NAME", num_args = 1..)]
        check_filter: Vec<String>,

        /// Alternate directory to search for `.agents/checks/*.md` instead
        /// of the repo root. Mirrors `amp review --check-scope`.
        #[arg(long = "check-scope", short = 's', value_name = "DIR")]
        check_scope: Option<PathBuf>,

        /// Skip the main correctness pass and only run check subagents.
        /// Mirrors `amp review --checks-only`.
        #[arg(long = "checks-only")]
        checks_only: bool,

        /// Print only the diff summary; skip the full review.
        /// Mirrors `amp review --summary-only`.
        #[arg(long = "summary-only")]
        summary_only: bool,

        /// Minimum severity to display. Findings below this rank are
        /// dropped from the output. Default is `medium`, matching
        /// Amp's CLI which hides `low` from review output. Pass
        /// `--severity low` to surface every finding.
        #[arg(long = "severity", value_name = "LEVEL", default_value = "medium")]
        severity: String,
    },
    #[command(
        name = "validate-extensions",
        about = "Valida un archivo bundled-extensions.json",
        hide = true
    )]
    ValidateExtensions {
        #[arg(help = "Ruta al archivo bundled-extensions.json")]
        file: PathBuf,
    },

    #[command(
        name = "mcp-probe",
        about = "Inspecciona un servidor MCP stdio sin usar un LLM",
        hide = true
    )]
    McpProbe {
        #[arg(help = "Comando del servidor MCP stdio a inspeccionar")]
        extension: String,

        #[arg(
            long,
            value_name = "PATH|-",
            help = "Script JSON de sondeo; usa - para stdin"
        )]
        script: Option<String>,
    },
}

#[derive(Subcommand)]
enum TermCommand {
    /// Print shell initialization script
    #[command(
        about = "Imprime el script de inicialización del shell",
        long_about = "Imprime la configuración de shell para las sesiones integradas a la terminal.\n\
                      Cada terminal tiene una sesión persistente de ghosty que se reanuda sola.\n\n\
                      Instalación:\n  \
                        echo 'eval \"$(ghosty term init zsh)\"' >> ~/.zshrc\n  \
                        source ~/.zshrc\n\n\
                        Nushell:\n  \
                        let init = ($nu.cache-dir | path join \"ghosty-term-init.nu\")\n  \
                        ^ghosty term init nu | save --force $init\n  \
                        source $init\n\n\
                      Con --default (todo lo que no sea un comando va a ghosty):\n  \
                        echo 'eval \"$(ghosty term init zsh --default)\"' >> ~/.zshrc\n  \
                        ^ghosty term init nu --default | save --force $init"
    )]
    Init {
        /// Shell type (bash, zsh, fish, nu, powershell)
        #[arg(value_enum)]
        shell: Shell,

        #[arg(short, long, help = "Nombre de la sesión de terminal")]
        name: Option<String>,

        /// Hace a ghosty el manejador por defecto de comandos desconocidos
        #[arg(
            long = "default",
            help = "Hace a ghosty el manejador por defecto de comandos desconocidos",
            long_help = "Todo lo que escribas que no sea un comando válido se manda a ghosty. Soportado en zsh, bash y nu."
        )]
        default: bool,
    },

    /// Log a shell command (called by shell hook)
    #[command(about = "Registra un comando de shell en la sesión", hide = true)]
    Log {
        /// The command that was executed
        command: String,
    },

    /// Run a prompt in the terminal session
    #[command(
        about = "Manda un prompt a la sesión de terminal",
        long_about = "Manda un prompt a la sesión integrada a la terminal.\n\n\
                      Ejemplos:\n  \
                        ghosty term run lista los archivos de este directorio\n  \
                        @ghosty lista archivos  # con el alias\n  \
                        @g por qué falló eso  # alias corto"
    )]
    Run {
        /// The prompt to send to goose (multiple words allowed without quotes)
        #[arg(required = true, num_args = 1..)]
        prompt: Vec<String>,
    },

    /// Print session info for prompt integration
    #[command(
        about = "Imprime info compacta de la sesión para el prompt del shell",
        long_about = "Imprime info compacta de la sesión (tokens, modelo) para integrarla al prompt del shell.\n\
                      Ejemplo de salida: ●○○○○ sonnet"
    )]
    Info,
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum CliProviderVariant {
    OpenAi,
    Databricks,
    Ollama,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum CompletionShell {
    Bash,
    Elvish,
    Fish,
    #[value(alias = "pwsh")]
    Powershell,
    #[value(alias = "nushell")]
    Nu,
    Zsh,
}

impl CompletionShell {
    fn generate(self, cmd: &mut clap::Command, bin_name: &str, writer: &mut dyn std::io::Write) {
        match self {
            CompletionShell::Bash => generate(ClapShell::Bash, cmd, bin_name, writer),
            CompletionShell::Elvish => generate(ClapShell::Elvish, cmd, bin_name, writer),
            CompletionShell::Fish => generate(ClapShell::Fish, cmd, bin_name, writer),
            CompletionShell::Powershell => generate(ClapShell::PowerShell, cmd, bin_name, writer),
            CompletionShell::Nu => generate(ClapNushell, cmd, bin_name, writer),
            CompletionShell::Zsh => generate(ClapShell::Zsh, cmd, bin_name, writer),
        }
    }
}

#[derive(Debug)]
pub struct InputConfig {
    pub contents: Option<String>,
    pub additional_system_prompt: Option<String>,
}

fn get_command_name(command: &Option<Command>) -> &'static str {
    match command {
        Some(Command::Configure {}) => "configure",
        Some(Command::Doctor {}) => "doctor",
        Some(Command::Info { .. }) => "info",
        Some(Command::Mcp { .. }) => "mcp",
        Some(Command::Acp { .. }) => "acp",
        Some(Command::Serve { .. }) => "serve",
        Some(Command::Session { .. }) => "session",
        Some(Command::Run { .. }) => "run",
        Some(Command::Schedule { .. }) => "schedule",
        Some(Command::Recipe { .. }) => "recipe",
        Some(Command::Skills { .. }) => "skills",
        Some(Command::Plugin { .. }) => "plugin",
        Some(Command::Term { .. }) => "term",
        Some(Command::Completion { .. }) => "completion",
        Some(Command::Review { .. }) => "review",
        Some(Command::ValidateExtensions { .. }) => "validate-extensions",
        Some(Command::McpProbe { .. }) => "mcp-probe",
        None => "default_session",
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpProbeScript {
    #[serde(default)]
    steps: Vec<McpProbeStep>,
    elicitation: Option<McpProbeElicitation>,
    #[serde(default)]
    oauth: goose::oauth::OAuthFlowConfig,
    protocol_version: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
enum McpProbeStep {
    ListTools,
    ListPrompts,
    ListResources,
    CallTool {
        name: String,
        #[serde(default)]
        arguments: serde_json::Map<String, serde_json::Value>,
    },
}

#[derive(Clone, serde::Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
enum McpProbeElicitation {
    Accept { content: serde_json::Value },
    AcceptSchemaDefaults,
    Decline,
    Cancel,
}

async fn handle_mcp_probe(extension_command: String, script_path: Option<String>) -> Result<()> {
    use goose::agents::{Agent, AgentConfig, ToolCallContext};
    use goose::config::ExtensionConfig;
    use rmcp::model::{ElicitRequestParams, ElicitResult, ElicitationAction};
    use tokio_util::sync::CancellationToken;

    let script = if let Some(path) = script_path {
        let json = if path == "-" {
            let mut json = String::new();
            std::io::stdin().read_to_string(&mut json)?;
            json
        } else {
            std::fs::read_to_string(path)?
        };
        serde_json::from_str::<McpProbeScript>(&json)?
    } else {
        McpProbeScript {
            steps: vec![
                McpProbeStep::ListTools,
                McpProbeStep::ListPrompts,
                McpProbeStep::ListResources,
            ],
            elicitation: None,
            oauth: goose::oauth::OAuthFlowConfig::default(),
            protocol_version: None,
        }
    };

    let mut extension = if url::Url::parse(&extension_command)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
    {
        crate::session::CliSession::parse_streamable_http_extension(
            &extension_command,
            goose::config::DEFAULT_EXTENSION_TIMEOUT,
        )
    } else {
        crate::session::CliSession::parse_stdio_extension(&extension_command)?
    };
    match &mut extension {
        ExtensionConfig::Stdio { name, .. } | ExtensionConfig::StreamableHttp { name, .. } => {
            *name = "probe".to_string();
        }
        _ => unreachable!("MCP probe only creates stdio or streamable HTTP extensions"),
    }

    if let Some(client_id) = &script.oauth.client_id {
        std::env::set_var("GHOSTY_MCP_OAUTH_CLIENT_ID", client_id);
    }
    if let Some(client_secret) = &script.oauth.client_secret {
        std::env::set_var("GHOSTY_MCP_OAUTH_CLIENT_SECRET", client_secret);
    }
    if let Some(client_metadata_url) = &script.oauth.client_metadata_url {
        std::env::set_var("GHOSTY_MCP_OAUTH_CLIENT_METADATA_URL", client_metadata_url);
    }

    let config = goose::config::Config::global();
    let mut agent_config = AgentConfig::new(
        std::sync::Arc::new(SessionManager::instance()),
        goose::config::permission::PermissionManager::instance(),
        None,
        config.get_ghosty_mode().unwrap_or_default(),
        true,
        GoosePlatform::GooseCli,
    );
    if let Some(protocol_version) = script.protocol_version.as_deref() {
        agent_config.mcp_protocol_version = Some(serde_json::from_value(
            serde_json::Value::String(protocol_version.to_string()),
        )?);
    }
    if let Some(action) = script.elicitation.clone() {
        agent_config.elicitation_handler =
            Some(std::sync::Arc::new(move |request| match &action {
                McpProbeElicitation::Accept { content } => {
                    ElicitResult::new(ElicitationAction::Accept).with_content(content.clone())
                }
                McpProbeElicitation::AcceptSchemaDefaults => {
                    let content = match request {
                        ElicitRequestParams::FormElicitationParams {
                            requested_schema, ..
                        } => serde_json::to_value(requested_schema)
                            .ok()
                            .and_then(|schema| schema.get("properties").cloned())
                            .and_then(|properties| properties.as_object().cloned())
                            .map(|properties| {
                                properties
                                    .into_iter()
                                    .filter_map(|(name, schema)| {
                                        schema.get("default").cloned().map(|value| (name, value))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        _ => serde_json::Map::new(),
                    };
                    ElicitResult::new(ElicitationAction::Accept)
                        .with_content(serde_json::Value::Object(content))
                }
                McpProbeElicitation::Decline => ElicitResult::new(ElicitationAction::Decline),
                McpProbeElicitation::Cancel => ElicitResult::new(ElicitationAction::Cancel),
            }));
    }
    let agent = Agent::with_config(agent_config);
    let session = agent
        .config
        .session_manager
        .create_session(
            std::env::current_dir()?,
            "MCP Probe".to_string(),
            goose::session::session_manager::SessionType::Hidden,
            agent.config.goose_mode,
        )
        .await?;
    let session_id = session.id.as_str();
    agent.add_extension(extension, session_id).await?;

    let mut results = Vec::new();
    for step in script.steps {
        let result = match step {
            McpProbeStep::ListTools => serde_json::json!({
                "action": "listTools",
                "result": agent.extension_manager.list_tools_from_extension(
                    session_id,
                    "probe",
                    CancellationToken::new(),
                ).await?,
            }),
            McpProbeStep::ListPrompts => serde_json::json!({
                "action": "listPrompts",
                "result": agent.extension_manager.list_prompts_from_extension(
                    session_id,
                    "probe",
                    CancellationToken::new(),
                ).await?,
            }),
            McpProbeStep::ListResources => serde_json::json!({
                "action": "listResources",
                "result": agent.extension_manager.list_resources_result_from_extension(
                    session_id,
                    "probe",
                    CancellationToken::new(),
                ).await?,
            }),
            McpProbeStep::CallTool { name, arguments } => {
                let scoped_name = format!("probe__{name}");
                let ctx = ToolCallContext::new(
                    session_id.to_string(),
                    Some(std::env::current_dir()?),
                    Some("mcp-probe-tool-call".to_string()),
                );
                let result = agent
                    .extension_manager
                    .dispatch_tool_call(
                        &ctx,
                        rmcp::model::CallToolRequestParams::new(scoped_name)
                            .with_arguments(arguments),
                        CancellationToken::new(),
                    )
                    .await?
                    .result
                    .await?;
                serde_json::json!({ "action": "callTool", "name": name, "result": result })
            }
        };
        results.push(result);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({ "results": results }))?
    );
    Ok(())
}

async fn handle_mcp_command(server: McpCommand) -> Result<()> {
    let name = server.name();
    let _ = crate::logging::setup_logging(Some(&format!("mcp-{name}")));
    match server {
        McpCommand::ComputerController => serve(ComputerControllerServer::new()).await?,
        McpCommand::Memory => serve(MemoryServer::new()).await?,
        McpCommand::Tutorial => serve(TutorialServer::new()).await?,
    }
    Ok(())
}

struct ServeCommandArgs {
    host: Option<String>,
    port: Option<u16>,
    setup: bool,
    check: bool,
    tls: bool,
    tls_cert_path: Option<String>,
    tls_key_path: Option<String>,
    platform: ServePlatform,
    builtins: Vec<String>,
    dangerously_unauthenticated: bool,
    allowed_origins: Vec<String>,
    enable_scheduler: bool,
}

async fn handle_serve_command(args: ServeCommandArgs) -> Result<()> {
    use axum::http::HeaderValue;
    use goose::acp::server::AcpBuiltinSelection;
    use goose::acp::server_factory::{AcpServer, AcpServerFactoryConfig};
    use goose::acp::transport::create_router;
    use goose::config::paths::Paths;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tracing::{info, warn};

    let ServeCommandArgs {
        host,
        port,
        setup,
        check,
        tls,
        tls_cert_path,
        tls_key_path,
        platform,
        builtins,
        dangerously_unauthenticated,
        allowed_origins,
        enable_scheduler,
    } = args;

    if setup {
        return crate::commands::serve_setup::run_serve_setup().await;
    }
    if check {
        let ok = crate::commands::serve_setup::run_serve_check()?;
        if !ok {
            std::process::exit(1);
        }
        return Ok(());
    }

    // Flag > config guardada (`ghosty serve --setup`) > default.
    let saved = crate::commands::serve_setup::ServeSettings::load(Config::global());
    let host = host.unwrap_or(saved.host);
    let port = port.unwrap_or(saved.port);
    let builtins = if builtins.is_empty() {
        saved.builtins
    } else {
        builtins
    };
    let allowed_origins = if allowed_origins.is_empty() {
        saved.allowed_origins
    } else {
        allowed_origins
    };
    let builtins = AcpBuiltinSelection::from_requested(builtins);

    let additional_source_roots = Config::global()
        .get_param::<String>("ADDITIONAL_AGENT_SOURCE_ROOTS")
        .ok()
        .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|path| {
            let path = path.canonicalize().unwrap_or(path);
            SourceRoot::read_only(path)
        })
        .collect();

    let server = Arc::new(AcpServer::new(AcpServerFactoryConfig {
        builtins,
        data_dir: Paths::data_dir(),
        config_dir: Paths::config_dir(),
        goose_platform: platform.into(),
        additional_source_roots,
        session_cwd: None,
        enable_scheduler,
    }));
    // La variable de entorno gana; si no está, el secreto guardado por `--setup`.
    let env_secret = std::env::var(GHOSTY_SERVER_TOKEN_ENV)
        .ok()
        .map(|secret| secret.trim().to_string())
        .filter(|secret| !secret.is_empty())
        .or(saved.token);
    let require_token = env_secret.is_some();
    if !require_token && !dangerously_unauthenticated {
        anyhow::bail!(
            "Falta {GHOSTY_SERVER_TOKEN_ENV} para arrancar `ghosty serve`. Corre `ghosty serve --setup`, o pasa --dangerously-unauthenticated para correr sin autenticación ACP"
        );
    }
    if dangerously_unauthenticated && !require_token {
        warn!(
            "{GHOSTY_SERVER_TOKEN_ENV} no está y se pasó --dangerously-unauthenticated; el endpoint ACP aceptará conexiones sin autenticar"
        );
    }
    let additional_allowed_origins = allowed_origins
        .iter()
        .map(|origin| {
            let origin = crate::commands::serve_setup::validate_origin(origin)
                .map_err(|e| anyhow::anyhow!("--allowed-origin `{origin}`: {e}"))?;
            HeaderValue::from_str(&origin).map_err(|error| {
                anyhow::anyhow!("valor inválido de --allowed-origin `{origin}`: {error}")
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let secret_key = env_secret.unwrap_or_else(generate_serve_secret_key);
    if let Err(error) = server.start_scheduler().await {
        warn!("El scheduler no arrancó; los trabajos programados no correrán hasta que un cliente se conecte: {error}");
    }
    let router = create_router(
        server,
        secret_key,
        require_token,
        additional_allowed_origins,
    );

    let config = Config::global();
    let tls_cert_path =
        tls_cert_path.or_else(|| config.get_param::<String>("GHOSTY_TLS_CERT_PATH").ok());
    let tls_key_path =
        tls_key_path.or_else(|| config.get_param::<String>("GHOSTY_TLS_KEY_PATH").ok());
    let tls = tls
        || config.get_param::<bool>("GHOSTY_TLS").unwrap_or(false)
        || tls_cert_path.is_some()
        || tls_key_path.is_some();

    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;
    if tls {
        #[cfg(feature = "rustls-tls")]
        {
            let tls_setup = goose::acp::transport::tls::setup_tls(
                tls_cert_path.as_deref(),
                tls_key_path.as_deref(),
            )
            .await?;
            info!("Starting ACP server on https://{}", addr);
            crate::commands::serve_setup::print_connect_block(&host, port, None, &allowed_origins);

            axum_server::bind_rustls(addr, tls_setup.config)
                .serve(router.into_make_service_with_connect_info::<SocketAddr>())
                .await?;
        }

        #[cfg(not(feature = "rustls-tls"))]
        {
            let _ = (tls_cert_path, tls_key_path);
            anyhow::bail!(
                "TLS was requested but no TLS backend is enabled. \
                 Enable the `rustls-tls` feature."
            );
        }
    } else {
        info!("Starting ACP server on http://{}", addr);
        let listener = tokio::net::TcpListener::bind(addr).await?;
        crate::commands::serve_setup::print_connect_block(&host, port, None, &allowed_origins);
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await?;
    }

    Ok(())
}

async fn handle_session_subcommand(command: SessionCommand) -> Result<()> {
    match command {
        SessionCommand::List {
            format,
            ascending,
            working_dir,
            limit,
        } => {
            handle_session_list(format, ascending, working_dir, limit).await?;
        }
        SessionCommand::Remove { identifier, regex } => {
            let (session_id, name) = if let Some(id) = identifier {
                (id.session_id, id.name)
            } else {
                (None, None)
            };
            handle_session_remove(session_id, name, regex).await?;
        }
        SessionCommand::Export {
            identifier,
            output,
            format,
        } => {
            let session_manager = SessionManager::instance();
            let session_identifier = if let Some(id) = identifier {
                lookup_session_id(id).await?
            } else {
                match crate::commands::session::prompt_interactive_session_selection(
                    &session_manager,
                )
                .await
                {
                    Ok(id) => id,
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        return Ok(());
                    }
                }
            };
            crate::commands::session::handle_session_export(session_identifier, output, format)
                .await?;
        }
        SessionCommand::Import { input } => {
            crate::commands::session::handle_session_import(input).await?;
        }
        SessionCommand::Diagnostics { identifier, output } => {
            let session_manager = SessionManager::instance();
            let session_id = if let Some(id) = identifier {
                lookup_session_id(id).await?
            } else {
                match crate::commands::session::prompt_interactive_session_selection(
                    &session_manager,
                )
                .await
                {
                    Ok(id) => id,
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        return Ok(());
                    }
                }
            };
            crate::commands::session::handle_diagnostics(&session_id, output).await?;
        }
    }
    Ok(())
}

struct InteractiveSessionArgs {
    identifier: Option<Identifier>,
    resume: bool,
    fork: bool,
    edit: bool,
    history: bool,
    session_opts: SessionOptions,
    extension_opts: ExtensionOptions,
    model_opts: ModelOptions,
}

async fn handle_interactive_session(args: InteractiveSessionArgs) -> Result<()> {
    let InteractiveSessionArgs {
        identifier,
        resume,
        fork,
        edit,
        history,
        session_opts,
        extension_opts,
        model_opts,
    } = args;

    let session_start = std::time::Instant::now();
    let session_type = if fork {
        "forked"
    } else if resume {
        "resumed"
    } else {
        "new"
    };

    tracing::info!(
        monotonic_counter.goose.session_starts = 1,
        session_type,
        interactive = true,
        "Session started"
    );

    if let Some(Identifier {
        session_id: Some(_),
        ..
    }) = &identifier
    {
        if !resume {
            eprintln!("Error: --session-id can only be used with --resume flag");
            std::process::exit(1);
        }
    }

    let goose_mode = Config::global().get_ghosty_mode().unwrap_or_default();
    let mut session_id = get_or_create_session_id(identifier, resume, false, goose_mode).await?;

    if edit || fork {
        if let Some(ref id) = session_id {
            let session_manager = SessionManager::instance();
            let original = session_manager.get_session(id, true).await?;

            let target_id = if fork {
                let copied = session_manager
                    .copy_session(id, original.name.clone())
                    .await?;
                let copied_id = copied.id.clone();
                session_id = Some(copied.id);
                copied_id
            } else {
                id.clone()
            };

            if edit {
                let conversation = original
                    .conversation
                    .ok_or_else(|| anyhow::anyhow!("session has no messages to edit"))?;
                let edited = crate::session::editor::edit_conversation(&conversation)?;
                session_manager
                    .replace_conversation(&target_id, &edited)
                    .await?;
            }
        }
    }

    let mut session: crate::CliSession = build_session(SessionBuilderConfig {
        session_id,
        resume,
        fork,
        no_session: false,
        extensions: extension_opts.extensions,
        streamable_http_extensions: extension_opts.streamable_http_extensions,
        builtins: extension_opts.builtins,
        no_profile: extension_opts.no_profile,
        recipe: None,
        additional_system_prompt: None,
        provider: model_opts.provider,
        model: model_opts.model,
        debug: session_opts.debug,
        max_tool_repetitions: session_opts.max_tool_repetitions,
        max_turns: session_opts.max_turns,
        scheduled_job_id: None,
        interactive: true,
        quiet: false,
        output_format: "text".to_string(),
        container: session_opts.container.map(Container::new),
        stats: false,
    })
    .await;

    if (resume || fork) && history {
        session.render_message_history();
    }

    let result = session.interactive(None).await;
    log_session_completion(&session, session_start, session_type, result.is_ok()).await;
    result
}

async fn log_session_completion(
    session: &crate::CliSession,
    session_start: std::time::Instant,
    session_type: &str,
    success: bool,
) {
    let session_duration = session_start.elapsed();
    let exit_type = if success { "normal" } else { "error" };

    let (total_tokens, message_count) = session
        .get_session()
        .await
        .map(|m| (m.usage.total_tokens.unwrap_or(0), m.message_count))
        .unwrap_or((0, 0));

    tracing::info!(
        monotonic_counter.goose.session_completions = 1,
        session_type,
        exit_type,
        duration_ms = session_duration.as_millis() as u64,
        total_tokens,
        message_count,
        "Session completed"
    );

    tracing::info!(
        monotonic_counter.goose.session_duration_ms = session_duration.as_millis() as u64,
        session_type,
        "Session duration"
    );

    if total_tokens > 0 {
        tracing::info!(
            monotonic_counter.goose.session_tokens = total_tokens,
            session_type,
            "Session tokens"
        );
    }
}

fn parse_run_input(
    input_opts: &InputOptions,
    quiet: bool,
) -> Result<Option<(InputConfig, Option<Recipe>)>> {
    match (
        &input_opts.instructions,
        &input_opts.input_text,
        &input_opts.recipe,
    ) {
        (Some(file), _, _) if file == "-" => {
            let mut contents = String::new();
            std::io::stdin()
                .read_to_string(&mut contents)
                .expect("Failed to read from stdin");
            Ok(Some((
                InputConfig {
                    contents: Some(contents),
                    additional_system_prompt: input_opts.system.clone(),
                },
                None,
            )))
        }
        (Some(file), _, _) => {
            let contents = std::fs::read_to_string(file).unwrap_or_else(|err| {
                eprintln!(
                    "No se encontró el archivo de instrucciones. ¿Querías `ghosty run --text`?\n{}",
                    err
                );
                std::process::exit(1);
            });
            Ok(Some((
                InputConfig {
                    contents: Some(contents),
                    additional_system_prompt: None,
                },
                None,
            )))
        }
        (_, Some(text), _) => Ok(Some((
            InputConfig {
                contents: Some(text.clone()),
                additional_system_prompt: input_opts.system.clone(),
            },
            None,
        ))),
        (_, _, Some(recipe_name)) => {
            let recipe_display_name = std::path::Path::new(recipe_name)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(recipe_name);

            let recipe_version = crate::recipes::search_recipe::load_recipe_file(recipe_name)
                .ok()
                .and_then(|rf| {
                    goose::recipe::template_recipe::parse_recipe_content(
                        &rf.content,
                        Some(rf.parent_dir.display().to_string()),
                    )
                    .ok()
                    .map(|(r, _)| r.version)
                })
                .unwrap_or_else(|| "unknown".to_string());

            if input_opts.explain {
                explain_recipe(recipe_name, input_opts.params.clone())?;
                return Ok(None);
            }
            if input_opts.render_recipe {
                if let Err(err) = render_recipe_as_yaml(recipe_name, input_opts.params.clone()) {
                    eprintln!("{}: {}", console::style("Error").red().bold(), err);
                    std::process::exit(1);
                }
                return Ok(None);
            }

            tracing::info!(
                monotonic_counter.goose.recipe_runs = 1,
                recipe_name = %recipe_display_name,
                recipe_version = %recipe_version,
                session_type = "recipe",
                interface = "cli",
                "Recipe execution started"
            );

            let (input_config, recipe) = extract_recipe_info_from_cli(
                recipe_name.clone(),
                input_opts.params.clone(),
                input_opts.additional_sub_recipes.clone(),
                quiet,
            )?;
            Ok(Some((input_config, Some(recipe))))
        }
        (None, None, None) => {
            eprintln!(
                "Error: Must provide either --instructions (-i), --text (-t), or --recipe. Use -i - for stdin."
            );
            std::process::exit(1);
        }
    }
}

async fn handle_run_command(
    input_opts: InputOptions,
    identifier: Option<Identifier>,
    run_behavior: RunBehavior,
    session_opts: SessionOptions,
    extension_opts: ExtensionOptions,
    output_opts: OutputOptions,
    model_opts: ModelOptions,
) -> Result<()> {
    let parsed = parse_run_input(&input_opts, output_opts.quiet)?;

    let Some((input_config, recipe)) = parsed else {
        return Ok(());
    };

    if let Some(Identifier {
        session_id: Some(_),
        ..
    }) = &identifier
    {
        if !run_behavior.resume {
            eprintln!("Error: --session-id can only be used with --resume flag");
            std::process::exit(1);
        }
    }

    let goose_mode = Config::global().get_ghosty_mode().unwrap_or_default();
    let session_id = get_or_create_session_id(
        identifier,
        run_behavior.resume,
        run_behavior.no_session,
        goose_mode,
    )
    .await?;

    let mut session = build_session(SessionBuilderConfig {
        session_id,
        resume: run_behavior.resume,
        fork: false,
        no_session: run_behavior.no_session,
        extensions: extension_opts.extensions,
        streamable_http_extensions: extension_opts.streamable_http_extensions,
        builtins: extension_opts.builtins,
        no_profile: extension_opts.no_profile,
        recipe: recipe.clone(),
        additional_system_prompt: input_config.additional_system_prompt,
        provider: model_opts.provider,
        model: model_opts.model,
        debug: session_opts.debug,
        max_tool_repetitions: session_opts.max_tool_repetitions,
        max_turns: session_opts.max_turns,
        scheduled_job_id: run_behavior.scheduled_job_id,
        interactive: run_behavior.interactive,
        quiet: output_opts.quiet,
        output_format: output_opts.output_format,
        container: session_opts.container.map(Container::new),
        stats: run_behavior.stats,
    })
    .await;

    if run_behavior.interactive {
        session.interactive(input_config.contents).await
    } else if let Some(contents) = input_config.contents {
        let session_start = std::time::Instant::now();
        let session_type = if recipe.is_some() { "recipe" } else { "run" };

        tracing::info!(
            monotonic_counter.goose.session_starts = 1,
            session_type,
            interactive = false,
            "Headless session started"
        );

        let result = session.headless(contents).await;
        log_session_completion(&session, session_start, session_type, result.is_ok()).await;
        result
    } else {
        Err(anyhow::anyhow!(
            "no text provided for prompt in headless mode"
        ))
    }
}

async fn handle_schedule_command(command: SchedulerCommand) -> Result<()> {
    match command {
        SchedulerCommand::Add {
            schedule_id,
            cron,
            recipe_source,
            params,
        } => handle_schedule_add(schedule_id, cron, recipe_source, params).await,
        SchedulerCommand::List {} => handle_schedule_list().await,
        SchedulerCommand::Remove { schedule_id } => handle_schedule_remove(schedule_id).await,
        SchedulerCommand::Sessions { schedule_id, limit } => {
            handle_schedule_sessions(schedule_id, limit).await
        }
        SchedulerCommand::RunNow { schedule_id } => handle_schedule_run_now(schedule_id).await,
        SchedulerCommand::ServicesStatus {} => handle_schedule_services_status().await,
        SchedulerCommand::ServicesStop {} => handle_schedule_services_stop().await,
        SchedulerCommand::CronHelp {} => handle_schedule_cron_help().await,
    }
}

fn handle_plugin_subcommand(command: PluginCommand) -> Result<()> {
    match command {
        PluginCommand::Install { url, auto_update } => handle_plugin_install(&url, auto_update),
        PluginCommand::Update { name } => handle_plugin_update(&name),
    }
}

fn handle_recipe_subcommand(command: RecipeCommand) -> Result<()> {
    match command {
        RecipeCommand::Validate { recipe_name } => handle_validate(&recipe_name),
        RecipeCommand::Deeplink {
            recipe_name,
            params,
        } => {
            handle_deeplink(&recipe_name, &params)?;
            Ok(())
        }
        RecipeCommand::Open {
            recipe_name,
            params,
        } => handle_open(&recipe_name, &params),
        RecipeCommand::List { format, verbose } => handle_list(&format, verbose),
    }
}

async fn handle_skills_subcommand(command: SkillsCommand) -> Result<()> {
    match command {
        SkillsCommand::List => handle_skills_list().await,
    }
}

async fn handle_term_subcommand(command: TermCommand) -> Result<()> {
    match command {
        TermCommand::Init {
            shell,
            name,
            default,
        } => handle_term_init(shell, name, default).await,
        TermCommand::Log { command } => handle_term_log(command).await,
        TermCommand::Run { prompt } => handle_term_run(prompt).await,
        TermCommand::Info => handle_term_info().await,
    }
}

async fn handle_default_session() -> Result<()> {
    if !Config::global().exists() {
        return handle_configure().await;
    }

    let goose_mode = Config::global().get_ghosty_mode().unwrap_or_default();
    let session_id = get_or_create_session_id(None, false, false, goose_mode).await?;

    let mut session = build_session(SessionBuilderConfig {
        session_id,
        resume: false,
        fork: false,
        no_session: false,
        extensions: Vec::new(),
        streamable_http_extensions: Vec::new(),
        builtins: Vec::new(),
        no_profile: false,
        recipe: None,
        additional_system_prompt: None,
        provider: None,
        model: None,
        debug: false,
        max_tool_repetitions: None,
        max_turns: None,
        scheduled_job_id: None,
        interactive: true,
        quiet: false,
        output_format: "text".to_string(),
        container: None,
        stats: false,
    })
    .await;
    session.interactive(None).await
}

pub async fn cli() -> anyhow::Result<()> {
    register_builtin_extensions(goose_mcp::BUILTIN_EXTENSIONS.clone());

    let cli = Cli::parse();

    let command_name = get_command_name(&cli.command);
    tracing::info!(
        monotonic_counter.goose.cli_commands = 1,
        command = command_name,
        "CLI command executed"
    );

    match cli.command {
        Some(Command::Completion { shell, bin_name }) => {
            let mut cmd = Cli::command();
            shell.generate(&mut cmd, &bin_name, &mut std::io::stdout());
            Ok(())
        }
        Some(Command::Configure {}) => handle_configure().await,
        Some(Command::Doctor {}) => crate::commands::doctor::handle_doctor().await,
        Some(Command::Info { verbose, check }) => handle_info(verbose, check).await,
        Some(Command::Mcp { server }) => handle_mcp_command(server).await,
        Some(Command::Acp {
            builtins,
            enable_scheduler,
        }) => goose::acp::server::run(builtins, enable_scheduler).await,
        Some(Command::Serve {
            host,
            port,
            setup,
            check,
            tls,
            tls_cert_path,
            tls_key_path,
            platform,
            builtins,
            dangerously_unauthenticated,
            allowed_origins,
            enable_scheduler,
        }) => {
            handle_serve_command(ServeCommandArgs {
                host,
                port,
                setup,
                check,
                tls,
                tls_cert_path,
                tls_key_path,
                platform,
                builtins,
                dangerously_unauthenticated,
                allowed_origins,
                enable_scheduler,
            })
            .await
        }
        Some(Command::Session {
            command: Some(cmd), ..
        }) => handle_session_subcommand(cmd).await,
        Some(Command::Session {
            command: None,
            identifier,
            resume,
            fork,
            edit,
            history,
            session_opts,
            extension_opts,
            model_opts,
        }) => {
            handle_interactive_session(InteractiveSessionArgs {
                identifier,
                resume,
                fork,
                edit,
                history,
                session_opts,
                extension_opts,
                model_opts,
            })
            .await
        }
        Some(Command::Run {
            input_opts,
            identifier,
            run_behavior,
            session_opts,
            extension_opts,
            output_opts,
            model_opts,
        }) => {
            handle_run_command(
                input_opts,
                identifier,
                run_behavior,
                session_opts,
                extension_opts,
                output_opts,
                model_opts,
            )
            .await
        }
        Some(Command::Schedule { command }) => handle_schedule_command(command).await,
        Some(Command::Recipe { command }) => handle_recipe_subcommand(command),
        Some(Command::Skills { command }) => handle_skills_subcommand(command).await,
        Some(Command::Plugin { command }) => handle_plugin_subcommand(command),
        Some(Command::Term { command }) => handle_term_subcommand(command).await,
        Some(Command::Review {
            range,
            prompt,
            model,
            provider,
            override_model,
            turn_limit,
            dry_run,
            quiet,
            no_orchestrate,
            instructions,
            files,
            check_filter,
            check_scope,
            checks_only,
            summary_only,
            severity,
        }) => {
            use crate::commands::review::{handle_review, ReviewOptions};
            handle_review(ReviewOptions {
                range,
                prompt_file: prompt,
                default_model: model,
                provider,
                override_model,
                default_turn_limit: turn_limit,
                dry_run,
                quiet,
                no_orchestrate,
                instructions,
                files,
                check_filter,
                check_scope,
                checks_only,
                summary_only,
                severity,
            })
            .await
        }
        Some(Command::ValidateExtensions { file }) => {
            use goose::agents::validate_extensions::validate_bundled_extensions;
            match validate_bundled_extensions(&file) {
                Ok(msg) => {
                    println!("{msg}");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        Some(Command::McpProbe { extension, script }) => handle_mcp_probe(extension, script).await,
        None => handle_default_session().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_command_accepts_nushell_alias() {
        let cli = Cli::try_parse_from(["goose", "completion", "nushell"]).expect("parse failed");

        match cli.command {
            Some(Command::Completion {
                shell: CompletionShell::Nu,
                ..
            }) => {}
            _ => panic!("expected nu completion shell"),
        }
    }

    #[test]
    fn session_resume_accepts_provider_and_model_overrides() {
        let cli = Cli::try_parse_from([
            "goose",
            "session",
            "--resume",
            "--provider",
            "openai",
            "--model",
            "gpt-5.4",
        ])
        .expect("parse failed");

        match cli.command {
            Some(Command::Session {
                resume, model_opts, ..
            }) => {
                assert!(resume);
                assert_eq!(model_opts.provider.as_deref(), Some("openai"));
                assert_eq!(model_opts.model.as_deref(), Some("gpt-5.4"));
            }
            _ => panic!("expected session command"),
        }
    }

    #[test]
    fn session_accepts_provider_override_without_resume() {
        let cli = Cli::try_parse_from(["goose", "session", "--provider", "openai"])
            .expect("provider override should work for a new session");

        match cli.command {
            Some(Command::Session {
                resume, model_opts, ..
            }) => {
                assert!(!resume);
                assert_eq!(model_opts.provider.as_deref(), Some("openai"));
            }
            _ => panic!("expected session command"),
        }
    }

    #[test]
    fn session_accepts_model_override_without_resume() {
        let cli = Cli::try_parse_from(["goose", "session", "--model", "gpt-5.4"])
            .expect("model override should work for a new session");

        match cli.command {
            Some(Command::Session {
                resume, model_opts, ..
            }) => {
                assert!(!resume);
                assert_eq!(model_opts.model.as_deref(), Some("gpt-5.4"));
            }
            _ => panic!("expected session command"),
        }
    }

    #[test]
    fn nushell_completion_generation_emits_module() {
        let mut cmd = Cli::command();
        let mut buffer = Vec::new();

        CompletionShell::Nu.generate(&mut cmd, "goose", &mut buffer);

        let script = String::from_utf8(buffer).expect("utf8");
        assert!(script.contains("module completions"));
        assert!(script.contains("export extern goose"));
        assert!(script.contains("export use completions *"));
    }

    #[test]
    fn term_init_help_mentions_nushell() {
        let mut cmd = Cli::command();
        let term = cmd.find_subcommand_mut("term").expect("term command");
        let init = term.find_subcommand_mut("init").expect("init command");
        let mut buffer = Vec::new();

        init.write_long_help(&mut buffer).expect("write help");

        let help = String::from_utf8(buffer).expect("utf8");
        assert!(help.contains("ghosty term init nu"));
        assert!(help.contains("Soportado en zsh, bash y nu"));
    }

    #[test]
    fn completion_help_lists_nu() {
        let mut cmd = Cli::command();
        let completion = cmd
            .find_subcommand_mut("completion")
            .expect("completion command");
        let mut buffer = Vec::new();

        completion.write_long_help(&mut buffer).expect("write help");

        let help = String::from_utf8(buffer).expect("utf8");
        assert!(help.contains("nu"));
    }

    #[test]
    fn skills_command_accepts_list_subcommand() {
        let cli = Cli::try_parse_from(["goose", "skills", "list"]).expect("parse failed");

        match cli.command {
            Some(Command::Skills {
                command: SkillsCommand::List,
            }) => {}
            _ => panic!("expected skills list command"),
        }
    }

    #[test]
    fn serve_command_accepts_dangerously_unauthenticated_flag() {
        let cli = Cli::try_parse_from([
            "goose",
            "serve",
            "--dangerously-unauthenticated",
            "--allowed-origin",
            "app://localhost",
            "--allowed-origin",
            "https://app.example",
        ])
        .expect("parse failed");

        match cli.command {
            Some(Command::Serve {
                dangerously_unauthenticated,
                allowed_origins,
                ..
            }) => {
                assert!(dangerously_unauthenticated);
                assert_eq!(
                    allowed_origins,
                    vec!["app://localhost", "https://app.example"]
                );
            }
            _ => panic!("expected serve command"),
        }
    }

    #[test]
    fn review_command_accepts_options() {
        let cli = Cli::try_parse_from([
            "goose",
            "review",
            "origin/main...HEAD",
            "--prompt",
            "REVIEW.md",
            "--model",
            "test-model",
            "--provider",
            "openai",
            "--override-model",
            "check-model",
            "--turn-limit",
            "4",
            "--dry-run",
            "--quiet",
            "--no-orchestrate",
            "--instructions",
            "focus on correctness",
            "--files",
            "src/lib.rs",
            "--check-filter",
            "security",
            "--check-scope",
            ".agents",
            "--checks-only",
            "--summary-only",
            "--severity",
            "low",
        ])
        .expect("parse failed");

        match cli.command {
            Some(Command::Review {
                range,
                prompt,
                model,
                provider,
                override_model,
                turn_limit,
                dry_run,
                quiet,
                no_orchestrate,
                instructions,
                files,
                check_filter,
                check_scope,
                checks_only,
                summary_only,
                severity,
            }) => {
                assert_eq!(range.as_deref(), Some("origin/main...HEAD"));
                assert_eq!(prompt.as_deref(), Some(std::path::Path::new("REVIEW.md")));
                assert_eq!(model.as_deref(), Some("test-model"));
                assert_eq!(provider.as_deref(), Some("openai"));
                assert_eq!(override_model.as_deref(), Some("check-model"));
                assert_eq!(turn_limit, Some(4));
                assert!(dry_run);
                assert!(quiet);
                assert!(no_orchestrate);
                assert_eq!(instructions.as_deref(), Some("focus on correctness"));
                assert_eq!(files, vec!["src/lib.rs"]);
                assert_eq!(check_filter, vec!["security"]);
                assert_eq!(
                    check_scope.as_deref(),
                    Some(std::path::Path::new(".agents"))
                );
                assert!(checks_only);
                assert!(summary_only);
                assert_eq!(severity, "low");
            }
            _ => panic!("expected review command"),
        }
    }
}
