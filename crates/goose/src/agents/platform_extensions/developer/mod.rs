pub mod edit;
pub mod image;
pub mod shell;
mod shell_output_streaming;
pub mod tree;

use crate::agents::extension::PlatformExtensionContext;
use crate::agents::mcp_client::{Error, McpClientTrait};
use crate::agents::ToolCallContext;
use crate::session::session_sandbox::{session_sandbox, SessionSandbox};
use anyhow::Result;
use async_trait::async_trait;
use edit::{EditTools, FileEditParams, FileWriteParams};
use image::{ImageReadParams, ImageTool};
use indoc::indoc;
use rmcp::model::{
    Annotations, CallToolResult, ContentBlock, Implementation, InitializeResult, JsonObject,
    ListToolsResult, ServerCapabilities, TextContent, Tool, ToolAnnotations,
};
use schemars::{schema_for, JsonSchema};
use serde_json::Value;
use shell::{shell_display_name, ShellOutput, ShellParams, ShellTool};
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tree::{TreeParams, TreeTool};

pub static EXTENSION_NAME: &str = "developer";

fn visible_text(text: impl Into<String>) -> ContentBlock {
    ContentBlock::Text(
        TextContent::new(text).with_annotations(Annotations::default().with_priority(0.0)),
    )
}

pub struct DeveloperClient {
    info: InitializeResult,
    shell_tool: Arc<ShellTool>,
    edit_tools: Arc<EditTools>,
    tree_tool: Arc<TreeTool>,
    image_tool: Arc<ImageTool>,
}

fn developer_instructions() -> &'static str {
    if cfg!(windows) {
        indoc! {"
            Use the developer extension to build software and operate a terminal.

            Make sure to use the tools *efficiently* - reading all the content you need in as few
            iterations as possible and then making the requested edits or running commands. You are
            responsible for managing your context window, and to minimize unnecessary turns which
            cost the user money.

            For editing software, prefer the flow of using tree to understand the codebase structure
            and file sizes. When you need to search, prefer findstr or Select-String (via shell).
            Then use type or Get-Content to gather the context you need, always reading before
            editing. Use write and edit to efficiently make changes. Test and verify as appropriate.
        "}
    } else {
        indoc! {"
            Use the developer extension to build software and operate a terminal.

            Make sure to use the tools *efficiently* - reading all the content you need in as few
            iterations as possible and then making the requested edits or running commands. You are
            responsible for managing your context window, and to minimize unnecessary turns which
            cost the user money.

            For editing software, prefer the flow of using tree to understand the codebase structure
            and file sizes. When you need to search, prefer rg which correctly respects gitignored
            content. Then use cat or sed to gather the context you need, always reading before editing.
            Use write and edit to efficiently make changes. Test and verify as appropriate.

            When running Python scripts or commands, always use `python3` instead of `python`.
        "}
    }
}

impl DeveloperClient {
    pub fn new(context: PlatformExtensionContext) -> Result<Self> {
        let info = InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(EXTENSION_NAME, "1.0.0").with_title("Developer"))
            .with_instructions(developer_instructions());

        Ok(Self {
            info,
            shell_tool: Arc::new(ShellTool::new(context.use_login_shell_path)?),
            edit_tools: Arc::new(EditTools::new()),
            tree_tool: Arc::new(TreeTool::new()),
            image_tool: Arc::new(ImageTool::new()),
        })
    }

    fn schema<T: JsonSchema>() -> JsonObject {
        serde_json::to_value(schema_for!(T))
            .expect("schema serialization should succeed")
            .as_object()
            .expect("schema should serialize to an object")
            .clone()
    }

    pub fn parse_args<T: serde::de::DeserializeOwned>(
        arguments: Option<JsonObject>,
    ) -> Result<T, String> {
        let value = arguments
            .map(Value::Object)
            .ok_or_else(|| "Missing arguments".to_string())?;
        serde_json::from_value(value).map_err(|e| format!("Failed to parse arguments: {e}"))
    }

    /// `write`/`edit`/`tree` de una conversación aislada: la ruta se comprueba
    /// contra sus raíces (enlaces resueltos) y la E/S corre con su identidad.
    /// A la tool se le pasa la ruta real, así que ya no depende del cwd.
    fn confined(
        sandbox: &SessionSandbox,
        path: &str,
        write: bool,
        run: impl FnOnce(String, &Path) -> CallToolResult,
    ) -> CallToolResult {
        let checked = if write {
            sandbox.writable(Path::new(path))
        } else {
            sandbox.readable(Path::new(path))
        };
        let result = checked.and_then(|real| {
            sandbox.run_as(|| run(real.to_string_lossy().into_owned(), &sandbox.cwd))
        });
        result.unwrap_or_else(|error| CallToolResult::error(vec![visible_text(error)]))
    }

    pub(crate) fn get_tools() -> Vec<Tool> {
        vec![
            Tool::new(
                "write".to_string(),
                "Create a new file or overwrite an existing file. Creates parent directories if needed.".to_string(),
                Self::schema::<FileWriteParams>(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Write".to_string()),
                Some(false),
                Some(true),
                Some(false),
                Some(false),
            )),
            Tool::new(
                "edit".to_string(),
                "Edit a file by finding and replacing text. The before text must match exactly and uniquely. Use empty after text to delete.".to_string(),
                Self::schema::<FileEditParams>(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Edit".to_string()),
                Some(false),
                Some(true),
                Some(false),
                Some(false),
            )),
            {
                let shell = shell_display_name();
                let newline_note = if shell == "cmd" {
                    " Commands must be on a single line — cmd.exe silently truncates at the \
                     first newline. Use `&` to chain (e.g. `echo a & echo b`) or set \
                     GHOSTY_SHELL=powershell for multi-line support."
                } else {
                    ""
                };
                let description = format!(
                    "Execute a shell command in the current dir. Commands run under `{shell}` \
                     (set GHOSTY_SHELL to override) - write command strings in that shell's \
                     syntax.{newline_note} Returns an object with stdout and stderr as separate \
                     fields. The output of each stream is limited to up to 2000 lines, and \
                     longer outputs will be saved to a temporary file.",
                );
                Tool::new("shell".to_string(), description, Self::schema::<ShellParams>())
            }
            .with_output_schema::<ShellOutput>()
            .annotate(ToolAnnotations::from_raw(
                Some("Shell".to_string()),
                Some(false),
                Some(true),
                Some(false),
                Some(true),
            )),
            Tool::new(
                "tree".to_string(),
                "List a directory tree with line counts. Traversal respects .gitignore rules.".to_string(),
                Self::schema::<TreeParams>(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Tree".to_string()),
                Some(true),
                Some(false),
                Some(true),
                Some(false),
            )),
            Tool::new(
                "read_image".to_string(),
                "Read an image from a local file path or http(s) URL and return it as image content for the model to inspect. Supports png, jpeg, gif, and webp.".to_string(),
                Self::schema::<ImageReadParams>(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Read Image".to_string()),
                Some(false),
                Some(false),
                Some(true),
                Some(true),
            )),
        ]
    }
}

#[async_trait]
impl McpClientTrait for DeveloperClient {
    async fn list_tools(
        &self,
        _session_id: &str,
        _next_cursor: Option<String>,
        _cancellation_token: CancellationToken,
    ) -> Result<ListToolsResult, Error> {
        Ok(ListToolsResult {
            tools: Self::get_tools(),
            next_cursor: None,
            meta: None,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        ctx: &ToolCallContext,
        name: &str,
        arguments: Option<JsonObject>,
        cancel_token: CancellationToken,
    ) -> Result<CallToolResult, Error> {
        let working_dir = ctx.working_dir.as_deref();
        let sandbox = session_sandbox(&ctx.session_id);
        match name {
            "shell" => match Self::parse_args::<ShellParams>(arguments) {
                Ok(params) => Ok(self
                    .shell_tool
                    .shell_with_cwd_and_emitter(
                        params,
                        working_dir,
                        Some(&ctx.session_id),
                        ctx.notification_emitter().cloned(),
                        cancel_token,
                    )
                    .await),
                Err(error) => Ok(ShellTool::error_result(&format!("Error: {error}"), None)),
            },
            "write" => match Self::parse_args::<FileWriteParams>(arguments) {
                Ok(params) => Ok(match sandbox.as_deref() {
                    Some(sandbox) => {
                        let FileWriteParams { path, content } = params;
                        Self::confined(sandbox, &path, true, |path, cwd| {
                            self.edit_tools
                                .file_write_with_cwd(FileWriteParams { path, content }, Some(cwd))
                        })
                    }
                    None => self.edit_tools.file_write_with_cwd(params, working_dir),
                }),
                Err(error) => Ok(CallToolResult::error(vec![visible_text(format!(
                    "Error: {error}"
                ))])),
            },
            "edit" => match Self::parse_args::<FileEditParams>(arguments) {
                Ok(params) => Ok(match sandbox.as_deref() {
                    Some(sandbox) => {
                        let FileEditParams {
                            path,
                            before,
                            after,
                        } = params;
                        Self::confined(sandbox, &path, true, |path, cwd| {
                            self.edit_tools.file_edit_with_cwd(
                                FileEditParams {
                                    path,
                                    before,
                                    after,
                                },
                                Some(cwd),
                            )
                        })
                    }
                    None => self.edit_tools.file_edit_with_cwd(params, working_dir),
                }),
                Err(error) => Ok(CallToolResult::error(vec![visible_text(format!(
                    "Error: {error}"
                ))])),
            },
            "tree" => match Self::parse_args::<TreeParams>(arguments) {
                Ok(params) => Ok(match sandbox.as_deref() {
                    Some(sandbox) => {
                        let TreeParams { path, depth } = params;
                        Self::confined(sandbox, &path, false, |path, cwd| {
                            self.tree_tool
                                .tree_with_cwd(TreeParams { path, depth }, Some(cwd))
                        })
                    }
                    None => self.tree_tool.tree_with_cwd(params, working_dir),
                }),
                Err(error) => Ok(CallToolResult::error(vec![visible_text(format!(
                    "Error: {error}"
                ))])),
            },
            "read_image" => match Self::parse_args::<ImageReadParams>(arguments) {
                Ok(params) => Ok(self
                    .image_tool
                    .image_read_with_cwd(params, working_dir, sandbox.as_deref())
                    .await),
                Err(error) => Ok(CallToolResult::error(vec![visible_text(format!(
                    "Error: {error}"
                ))])),
            },
            _ => Ok(CallToolResult::error(vec![visible_text(format!(
                "Error: Unknown tool: {name}"
            ))])),
        }
    }

    fn get_info(&self) -> Option<&InitializeResult> {
        Some(&self.info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionManager;
    use rmcp::model::ContentBlock;
    use rmcp::object;
    use std::fs;

    #[test]
    fn developer_tools_are_flat() {
        let names: Vec<String> = DeveloperClient::get_tools()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();

        assert_eq!(names, vec!["write", "edit", "shell", "tree", "read_image"]);
    }

    #[test]
    fn read_image_annotations_reflect_network_access() {
        let read_image = DeveloperClient::get_tools()
            .into_iter()
            .find(|tool| tool.name == "read_image")
            .unwrap();
        let annotations = read_image.annotations.unwrap();

        assert_eq!(annotations.read_only_hint, Some(false));
        assert_eq!(annotations.open_world_hint, Some(true));
    }

    fn test_context(data_dir: std::path::PathBuf) -> PlatformExtensionContext {
        PlatformExtensionContext {
            extension_manager: None,
            session_manager: Arc::new(SessionManager::new(data_dir)),
            scheduler: None,
            session: None,
            use_login_shell_path: false,
        }
    }

    fn first_text(result: &CallToolResult) -> &str {
        match &result.content[0] {
            ContentBlock::Text(text) => &text.text,
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn developer_client_uses_working_dir_for_file_tools() {
        let temp = tempfile::tempdir().unwrap();
        let client = DeveloperClient::new(test_context(temp.path().join("sessions"))).unwrap();
        let cwd = temp.path().join("workspace");
        fs::create_dir_all(&cwd).unwrap();

        let ctx = ToolCallContext::new("session".to_owned(), Some(cwd.clone()), None);
        let write = client
            .call_tool(
                &ctx,
                "write",
                Some(object!({
                    "path": "notes.txt",
                    "content": "first line"
                })),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(write.is_error, Some(false));
        assert_eq!(
            fs::read_to_string(cwd.join("notes.txt")).unwrap(),
            "first line"
        );

        let edit = client
            .call_tool(
                &ctx,
                "edit",
                Some(object!({
                    "path": "notes.txt",
                    "before": "first",
                    "after": "updated"
                })),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(edit.is_error, Some(false));
        assert_eq!(
            fs::read_to_string(cwd.join("notes.txt")).unwrap(),
            "updated line"
        );
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn developer_client_passes_session_id_to_shell_tool() {
        let temp = tempfile::tempdir().unwrap();
        let client = DeveloperClient::new(test_context(temp.path().join("sessions"))).unwrap();
        let ctx = ToolCallContext::new("session-789".to_owned(), None, None);

        let result = client
            .call_tool(
                &ctx,
                "shell",
                Some(object!({
                    "command": "printenv AGENT_SESSION_ID"
                })),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(false));
        assert_eq!(first_text(&result), "session-789");
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn developer_client_uses_working_dir_for_shell_tool() {
        let temp = tempfile::tempdir().unwrap();
        let client = DeveloperClient::new(test_context(temp.path().join("sessions"))).unwrap();
        let cwd = temp.path().join("workspace");
        fs::create_dir_all(&cwd).unwrap();

        let ctx = ToolCallContext::new("session".to_owned(), Some(cwd.clone()), None);
        let result = client
            .call_tool(
                &ctx,
                "shell",
                Some(object!({
                    "command": "pwd"
                })),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));
        let observed = std::fs::canonicalize(first_text(&result)).unwrap();
        let expected = std::fs::canonicalize(&cwd).unwrap();
        assert_eq!(observed, expected);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sandboxed_file_tools_stay_inside_their_roots() {
        use crate::session::session_sandbox::{
            remove_session_sandbox, set_session_sandbox, SessionSandbox,
        };

        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        for dir in ["own/work", "kb", "other"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join("kb/faq.md"), "faq").unwrap();
        fs::write(root.join("other/secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(root.join("other"), root.join("own/escape")).unwrap();

        let session_id = "developer-sandbox-session";
        set_session_sandbox(
            session_id,
            SessionSandbox {
                uid: 20001,
                gid: 20001,
                home: root.join("own"),
                cwd: root.join("own/work"),
                read: vec![root.join("kb")],
                write: vec![root.join("own")],
            },
        );
        let client = DeveloperClient::new(test_context(root.join("sessions"))).unwrap();
        // El cwd de la sesión apunta al directorio compartido; no debe usarse.
        let ctx = ToolCallContext::new(session_id.to_owned(), Some(root.join("other")), None);
        let call = |name: &'static str, args: JsonObject| {
            let client = &client;
            let ctx = &ctx;
            async move {
                client
                    .call_tool(ctx, name, Some(args), CancellationToken::new())
                    .await
                    .unwrap()
            }
        };

        // Dentro de `write`: se escribe y se edita, relativo al cwd de la identidad.
        let write = call("write", object!({ "path": "notes.txt", "content": "hola" })).await;
        assert_eq!(write.is_error, Some(false), "{}", first_text(&write));
        assert_eq!(
            fs::read_to_string(root.join("own/work/notes.txt")).unwrap(),
            "hola"
        );
        let edit = call(
            "edit",
            object!({ "path": "notes.txt", "before": "hola", "after": "adiós" }),
        )
        .await;
        assert_eq!(edit.is_error, Some(false), "{}", first_text(&edit));

        // Fuera de `write`, aunque sea legible: negado.
        for path in [
            root.join("kb/faq.md"),
            root.join("other/new.txt"),
            root.join("own/escape/secret.txt"),
        ] {
            let path = path.to_string_lossy().into_owned();
            let result = call("write", object!({ "path": path.clone(), "content": "x" })).await;
            assert_eq!(result.is_error, Some(true), "{path}");
            assert!(first_text(&result).starts_with("Acceso denegado"), "{path}");
        }
        assert_eq!(fs::read_to_string(root.join("kb/faq.md")).unwrap(), "faq");
        assert!(!root.join("other/new.txt").exists());
        let edit = call(
            "edit",
            object!({
                "path": root.join("other/secret.txt").to_string_lossy(),
                "before": "secret",
                "after": "pwned"
            }),
        )
        .await;
        assert_eq!(edit.is_error, Some(true));
        assert_eq!(
            fs::read_to_string(root.join("other/secret.txt")).unwrap(),
            "secret"
        );

        // Lectura: `read ∪ write` sí; otra conversación, ni directo ni por enlace.
        let tree = call(
            "tree",
            object!({ "path": root.join("kb").to_string_lossy() }),
        )
        .await;
        assert_eq!(tree.is_error, Some(false), "{}", first_text(&tree));
        for path in [root.join("other"), root.join("own/escape"), "/etc".into()] {
            let result = call("tree", object!({ "path": path.to_string_lossy() })).await;
            assert_eq!(result.is_error, Some(true), "{}", path.display());
        }
        let image = call(
            "read_image",
            object!({ "source": root.join("own/escape/secret.txt").to_string_lossy() }),
        )
        .await;
        assert!(
            first_text(&image).contains("Acceso denegado"),
            "{}",
            first_text(&image)
        );

        remove_session_sandbox(session_id);
    }
}
