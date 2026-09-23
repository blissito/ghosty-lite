//! Aislamiento de cada conversación ACP dentro de la caja: un uid de Linux por
//! sesión.
//!
//! Un mismo `ghosty serve` (root) atiende a muchos clientes finales a la vez.
//! El relé manda en `_meta["ghosty/sandbox"]` de `session/new` y `session/load`
//! la identidad de la conversación:
//!
//! ```json
//! { "uid": 20001, "gid": 20001, "home": "/data/s/abc", "cwd": "/data/s/abc/work",
//!   "read": ["/data/kb"], "write": ["/data/s/abc"] }
//! ```
//!
//! Con eso:
//! - los procesos que nacen para la sesión (shell del developer, extensiones
//!   stdio) corren con ese uid/gid, sin grupos suplementarios, `HOME=<home>` y
//!   `umask 077` ([`SessionSandbox::apply_identity`]);
//! - las tools de archivos que corren DENTRO del proceso root sólo leen bajo
//!   `read ∪ write` y sólo escriben bajo `write`, resolviendo enlaces antes de
//!   comparar ([`SessionSandbox::readable`], [`SessionSandbox::writable`]), y
//!   hacen la E/S con la identidad de la sesión ([`SessionSandbox::run_as`]);
//! - `chatrecall` sólo ve la propia conversación y las tools que manejan otras
//!   sesiones (orchestrator, scheduler) se niegan.
//!
//! Una sesión sin `ghosty/sandbox` se comporta como siempre (Zed, relés viejos).

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};

use serde::Deserialize;

/// Clave de `_meta` con la identidad de la sesión.
pub const SESSION_SANDBOX_META_KEY: &str = "ghosty/sandbox";

/// uid/gid mínimos: por debajo están root, los usuarios del sistema y los de la
/// imagen; un error del relé no debe poder prestarle a una conversación uno de
/// ésos.
pub const MIN_SANDBOX_ID: u32 = 20_000;

const MAX_ROOTS: usize = 64;
const MAX_PATH_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSandbox {
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct RawSandbox {
    uid: u32,
    gid: u32,
    home: String,
    cwd: String,
    #[serde(default)]
    read: Vec<String>,
    #[serde(default)]
    write: Vec<String>,
}

static SESSION_SANDBOXES: LazyLock<RwLock<HashMap<String, Arc<SessionSandbox>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Ruta absoluta y normalizada: empieza en `/`, sin `.` ni `..`, sin NUL.
fn normalized_absolute(field: &str, raw: &str) -> Result<PathBuf, String> {
    if raw.len() > MAX_PATH_BYTES || raw.contains('\0') {
        return Err(format!("ghosty/sandbox.{field}: ruta inválida"));
    }
    let path = Path::new(raw);
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return Err(format!(
            "ghosty/sandbox.{field}: la ruta debe ser absoluta ({raw})"
        ));
    }
    let mut normalized = PathBuf::from("/");
    for component in components {
        match component {
            Component::Normal(part) => normalized.push(part),
            _ => {
                return Err(format!(
                    "ghosty/sandbox.{field}: la ruta no puede llevar '.' ni '..' ({raw})"
                ))
            }
        }
    }
    // `Path::components` se come los `.` intermedios; se rechazan igual.
    if raw.split('/').any(|part| part == "." || part == "..") {
        return Err(format!(
            "ghosty/sandbox.{field}: la ruta no puede llevar '.' ni '..' ({raw})"
        ));
    }
    Ok(normalized)
}

fn normalized_roots(field: &str, raw: &[String]) -> Result<Vec<PathBuf>, String> {
    if raw.len() > MAX_ROOTS {
        return Err(format!(
            "ghosty/sandbox.{field}: máximo {MAX_ROOTS} directorios"
        ));
    }
    raw.iter()
        .map(|root| normalized_absolute(field, root))
        .collect()
}

/// Extrae la identidad de `_meta["ghosty/sandbox"]`.
///
/// `Ok(None)` si la clave no viene (la sesión conserva lo que tenía). Si viene
/// y es inválida es `Err`: una conversación que pidió aislamiento no puede
/// arrancar sin él.
pub fn session_sandbox_from_meta(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Result<Option<SessionSandbox>, String> {
    let Some(value) = meta.and_then(|meta| meta.get(SESSION_SANDBOX_META_KEY)) else {
        return Ok(None);
    };
    let raw: RawSandbox = serde_json::from_value(value.clone())
        .map_err(|error| format!("ghosty/sandbox inválido: {error}"))?;
    if raw.uid < MIN_SANDBOX_ID || raw.gid < MIN_SANDBOX_ID {
        return Err(format!(
            "ghosty/sandbox: uid y gid deben ser ≥ {MIN_SANDBOX_ID}"
        ));
    }
    Ok(Some(SessionSandbox {
        uid: raw.uid,
        gid: raw.gid,
        home: normalized_absolute("home", &raw.home)?,
        cwd: normalized_absolute("cwd", &raw.cwd)?,
        read: normalized_roots("read", &raw.read)?,
        write: normalized_roots("write", &raw.write)?,
    }))
}

/// Reemplaza la identidad de la sesión.
pub fn set_session_sandbox(session_id: &str, sandbox: SessionSandbox) {
    if let Ok(mut sandboxes) = SESSION_SANDBOXES.write() {
        tracing::info!(
            session_id,
            uid = sandbox.uid,
            gid = sandbox.gid,
            "ghosty/sandbox guardado"
        );
        sandboxes.insert(session_id.to_string(), Arc::new(sandbox));
    }
}

pub fn remove_session_sandbox(session_id: &str) {
    if let Ok(mut sandboxes) = SESSION_SANDBOXES.write() {
        sandboxes.remove(session_id);
    }
}

/// Identidad de la sesión, si tiene.
pub fn session_sandbox(session_id: &str) -> Option<Arc<SessionSandbox>> {
    SESSION_SANDBOXES
        .read()
        .ok()
        .and_then(|sandboxes| sandboxes.get(session_id).cloned())
}

/// Un subagente hereda la identidad de su padre: si no, su shell correría como
/// root.
pub fn inherit_session_sandbox(parent_session_id: &str, child_session_id: &str) {
    if let Some(sandbox) = session_sandbox(parent_session_id) {
        if let Ok(mut sandboxes) = SESSION_SANDBOXES.write() {
            sandboxes.insert(child_session_id.to_string(), sandbox);
        }
    }
}

fn denied(path: &Path, what: &str) -> String {
    format!(
        "Acceso denegado: {} está fuera de los directorios que esta conversación puede {what}.",
        path.display()
    )
}

/// `canonicalize` que acepta rutas que todavía no existen: resuelve el ancestro
/// más profundo que sí existe y le pega el resto. El resto no puede llevar `..`
/// (se rechaza) ni pasar por un enlace roto (seguirlo al escribir crearía el
/// destino fuera de las raíces).
fn canonicalize_lenient(path: &Path) -> Result<PathBuf, String> {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        match std::fs::canonicalize(&existing) {
            Ok(real) => {
                let mut real = real;
                for part in rest.iter().rev() {
                    real.push(part);
                }
                return Ok(real);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(&existing).is_ok() {
                    return Err(format!(
                        "Acceso denegado: {} es un enlace a algo que no existe.",
                        existing.display()
                    ));
                }
                match (existing.file_name(), existing.parent()) {
                    (Some(name), Some(parent)) => {
                        rest.push(name.to_os_string());
                        existing = parent.to_path_buf();
                    }
                    _ => return Err(format!("Ruta no válida: {}", path.display())),
                }
            }
            Err(error) => return Err(format!("No se pudo resolver {}: {error}", path.display())),
        }
    }
}

fn canonical_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

impl SessionSandbox {
    /// Las rutas relativas cuelgan de `cwd`.
    pub fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }

    fn check<'a>(
        &self,
        path: &Path,
        roots: impl Iterator<Item = &'a PathBuf>,
        what: &str,
    ) -> Result<PathBuf, String> {
        let requested = self.resolve(path);
        let real = canonicalize_lenient(&requested)?;
        let mut roots = roots;
        if roots.any(|root| real.starts_with(canonical_root(root))) && !self.in_sibling(&real) {
            Ok(real)
        } else {
            Err(denied(&requested, what))
        }
    }

    /// ¿Cae `real` en el directorio de OTRA conversación?
    ///
    /// Las raíces de escritura de cada sesión viven lado a lado
    /// (`/data/work/s/<uid>`, `/data/tmp/<uid>`) y una raíz de lectura común
    /// como `/data/work` contiene a todas. Regla: el padre de cada raíz de
    /// escritura propia es un contenedor privado; dentro de él sólo vale lo que
    /// cuelga de una raíz de escritura propia o de una raíz de lectura que viva
    /// dentro del contenedor. (En la caja la E/S además corre
    /// con el uid de la sesión y esos directorios son 0700; esto es la segunda
    /// barrera, y la única fuera de Linux/root.)
    fn in_sibling(&self, real: &Path) -> bool {
        let own: Vec<PathBuf> = self.write.iter().map(|root| canonical_root(root)).collect();
        if own.iter().any(|root| real.starts_with(root)) {
            return false;
        }
        let read: Vec<PathBuf> = self.read.iter().map(|root| canonical_root(root)).collect();
        own.iter()
            .filter_map(|root| root.parent())
            // Con una raíz colgada de `/` la regla lo negaría todo.
            .filter(|container| container.parent().is_some())
            .any(|container| {
                real.starts_with(container)
                    && real != container
                    // Una raíz de lectura que vive DENTRO del contenedor (p. ej.
                    // `<padre>/kb` junto a la home) se concedió a propósito.
                    && !read.iter().any(|root| {
                        root.starts_with(container) && root != container && real.starts_with(root)
                    })
            })
    }

    /// Ruta real (enlaces resueltos) si está bajo `read ∪ write`.
    pub fn readable(&self, path: &Path) -> Result<PathBuf, String> {
        self.check(path, self.read.iter().chain(&self.write), "leer")
    }

    /// Ruta real (enlaces resueltos) si está bajo `write`.
    pub fn writable(&self, path: &Path) -> Result<PathBuf, String> {
        self.check(path, self.write.iter(), "escribir")
    }

    /// Corre `f` —síncrona, sin `await` dentro— con la identidad de la sesión
    /// en el sistema de archivos del hilo actual (ver [`fs_identity`]). Así el
    /// kernel también vigila la E/S (cierra la carrera entre la comprobación y
    /// el `open`) y lo que se crea queda a nombre del uid de la sesión, que es
    /// quien luego lo edita desde su shell.
    pub fn run_as<T>(&self, f: impl FnOnce() -> T) -> Result<T, String> {
        let _guard = fs_identity::FsIdentityGuard::enter(self.uid, self.gid)?;
        Ok(f())
    }

    /// uid/gid, `HOME`, `umask 077` y el env de la caja FILTRADO para un
    /// proceso que nace para la sesión. Borra el env que ya tuviera el comando:
    /// hay que llamarla ANTES de agregarle el de la sesión (`ghosty/env`,
    /// `AGENT_SESSION_ID`) o el de la extensión. El cwd lo pone quien llama.
    ///
    /// Los grupos suplementarios los limpia la propia std: al cambiar de uid
    /// siendo root sin `groups` explícitos llama `setgroups(0, NULL)` antes del
    /// `setuid`. Un `pre_exec` no serviría para eso: corre ya sin privilegios.
    #[cfg(unix)]
    pub fn apply_identity(&self, command: &mut tokio::process::Command) {
        command.uid(self.uid);
        command.gid(self.gid);
        command.env_clear();
        command.envs(
            std::env::vars_os().filter(|(name, _)| name.to_str().is_some_and(is_inheritable_env)),
        );
        command.env("HOME", &self.home);
        // SAFETY: `umask` es async-signal-safe y no toca memoria compartida.
        unsafe {
            command.pre_exec(|| {
                libc::umask(0o077);
                Ok(())
            });
        }
    }
}

/// Qué variables del proceso root hereda un proceso de una conversación
/// aislada. El resto NO pasa: ahí viven los secretos de la caja
/// (`CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
/// `DEEPSEEK_API_KEY`, `GHOSTY_SERVER_TOKEN`, `REPORT_TOKEN`, `ACP_*`…).
///
/// Lista blanca:
/// - `PATH`, `LANG`, `LC_*`, `TERM`, `TZ`, `NODE_PATH`;
/// - config no secreta de la plataforma: `GS_*` (`GS_TOOLS_URL`,
///   `GS_RENDER_URL`, `GS_TTS_URL`, `GS_STT_URL`, `GS_VIDEO_URL`,
///   `GS_SVC_MESH`…) y `QUOTE_*`;
/// - aun dentro de la lista, nada con cara de secreto (`TOKEN`, `SECRET`,
///   `PASSWORD`, `API_KEY`, `*_KEY`): el `GS_TOOLS_TOKEN` de la caja no pasa;
///   el sub-token propio llega por el `ghosty/env` de la sesión, igual que
///   `GS_AGENT_GROUP_ID` y `TMPDIR`.
pub fn is_inheritable_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let allowed = matches!(
        upper.as_str(),
        "PATH" | "LANG" | "TERM" | "TZ" | "NODE_PATH"
    ) || ["LC_", "GS_", "QUOTE_"]
        .iter()
        .any(|prefix| upper.starts_with(prefix));
    let secret = ["TOKEN", "SECRET", "PASSWORD", "API_KEY"]
        .iter()
        .any(|word| upper.contains(word))
        || upper.ends_with("_KEY");
    allowed && !secret
}

/// Identidad de sistema de archivos por HILO (Linux).
///
/// `setfsuid`/`setfsgid` cambian sólo el hilo que llama, y al pasar el fsuid de
/// 0 a otro el kernel le quita a ese hilo `CAP_DAC_OVERRIDE` y compañía; al
/// volver a 0 se las devuelve. Los grupos suplementarios se vacían con la
/// syscall cruda: el `setgroups` de la libc los cambiaría en TODOS los hilos.
#[cfg(target_os = "linux")]
mod fs_identity {
    pub struct FsIdentityGuard {
        saved_groups: Option<Vec<libc::gid_t>>,
    }

    fn current_fsuid() -> u32 {
        // Una llamada que falla devuelve el fsuid actual sin cambiarlo.
        unsafe { libc::setfsuid(u32::MAX) as u32 }
    }

    fn current_fsgid() -> u32 {
        unsafe { libc::setfsgid(u32::MAX) as u32 }
    }

    impl FsIdentityGuard {
        pub fn enter(uid: u32, gid: u32) -> Result<Self, String> {
            // Sin root no hay a quién bajar: el proceso ya no puede más de lo
            // que su uid permite (tests, desarrollo).
            if unsafe { libc::geteuid() } != 0 {
                return Ok(Self { saved_groups: None });
            }
            let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
            let mut groups = vec![0 as libc::gid_t; count.max(0) as usize];
            if count > 0 {
                let got = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
                groups.truncate(got.max(0) as usize);
            }
            let guard = Self {
                saved_groups: Some(groups),
            };
            unsafe {
                libc::syscall(
                    libc::SYS_setgroups,
                    0 as libc::size_t,
                    std::ptr::null::<libc::gid_t>(),
                );
                libc::setfsgid(gid);
                libc::setfsuid(uid);
            }
            if current_fsuid() != uid || current_fsgid() != gid {
                // `guard` se suelta aquí y restaura.
                return Err(
                    "No se pudo adoptar la identidad de la conversación; operación cancelada."
                        .to_string(),
                );
            }
            Ok(guard)
        }
    }

    impl Drop for FsIdentityGuard {
        fn drop(&mut self) {
            let Some(groups) = self.saved_groups.take() else {
                return;
            };
            unsafe {
                libc::setfsuid(0);
                libc::setfsgid(0);
                libc::syscall(
                    libc::SYS_setgroups,
                    groups.len() as libc::size_t,
                    groups.as_ptr(),
                );
            }
            if current_fsuid() != 0 || current_fsgid() != 0 {
                tracing::error!("no se pudo restaurar la identidad root del hilo");
            }
        }
    }
}

/// Fuera de Linux no hay identidad por hilo: sólo quedan las comprobaciones de
/// ruta. La caja siempre es Linux.
#[cfg(not(target_os = "linux"))]
mod fs_identity {
    pub struct FsIdentityGuard;

    impl FsIdentityGuard {
        pub fn enter(_uid: u32, _gid: u32) -> Result<Self, String> {
            Ok(Self)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        value.as_object().unwrap().clone()
    }

    fn sandbox_meta(sandbox: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        meta(json!({ "ghosty/sandbox": sandbox }))
    }

    #[test]
    fn sandbox_meta_absent_is_none() {
        assert_eq!(session_sandbox_from_meta(None), Ok(None));
        assert_eq!(
            session_sandbox_from_meta(Some(&meta(json!({ "other": 1 })))),
            Ok(None)
        );
    }

    #[test]
    fn sandbox_meta_valid_is_normalized() {
        let sandbox = session_sandbox_from_meta(Some(&sandbox_meta(json!({
            "uid": 20001,
            "gid": 20002,
            "home": "/data/s/a/",
            "cwd": "/data/s/a/work",
            "read": ["/data/kb"],
            "write": ["/data/s/a"],
        }))))
        .unwrap()
        .unwrap();
        assert_eq!(sandbox.uid, 20001);
        assert_eq!(sandbox.gid, 20002);
        assert_eq!(sandbox.home, PathBuf::from("/data/s/a"));
        assert_eq!(sandbox.read, vec![PathBuf::from("/data/kb")]);
        assert_eq!(sandbox.write, vec![PathBuf::from("/data/s/a")]);
    }

    #[test]
    fn sandbox_meta_invalid_is_rejected() {
        let base = json!({
            "uid": 20001, "gid": 20001, "home": "/h", "cwd": "/h", "read": [], "write": ["/h"]
        });
        let with = |key: &str, value: serde_json::Value| {
            let mut sandbox = base.clone();
            sandbox[key] = value;
            session_sandbox_from_meta(Some(&sandbox_meta(sandbox)))
        };
        assert!(with("uid", json!(0)).is_err());
        assert!(with("uid", json!(19999)).is_err());
        assert!(with("gid", json!(1000)).is_err());
        assert!(with("uid", json!(-1)).is_err());
        assert!(with("uid", json!("20001")).is_err());
        assert!(with("home", json!("relative/dir")).is_err());
        assert!(with("cwd", json!("/h/../etc")).is_err());
        assert!(with("cwd", json!("/h/./w")).is_err());
        assert!(with("write", json!(["/h", "../x"])).is_err());
        assert!(with("read", json!(["/a\u{0}b"])).is_err());
        assert!(with("read", json!("/h")).is_err());
        assert!(session_sandbox_from_meta(Some(&sandbox_meta(json!("nope")))).is_err());
        assert!(with("uid", json!(20001)).unwrap().is_some());
    }

    #[test]
    fn sandbox_registry_replace_inherit_and_remove() {
        let id = "session-sandbox-registry-test";
        let child = "session-sandbox-registry-test-child";
        let sandbox = |uid| SessionSandbox {
            uid,
            gid: uid,
            home: "/h".into(),
            cwd: "/h".into(),
            read: vec![],
            write: vec!["/h".into()],
        };
        assert!(session_sandbox(id).is_none());
        set_session_sandbox(id, sandbox(20001));
        set_session_sandbox(id, sandbox(20002));
        assert_eq!(session_sandbox(id).unwrap().uid, 20002);
        inherit_session_sandbox(id, child);
        assert_eq!(session_sandbox(child).unwrap().uid, 20002);
        remove_session_sandbox(id);
        remove_session_sandbox(child);
        assert!(session_sandbox(id).is_none());
        assert!(session_sandbox(child).is_none());
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        sandbox: SessionSandbox,
    }

    /// `own/` es de escritura, `kb/` de sólo lectura y `other/` (otra
    /// conversación) no está en ninguna raíz.
    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        for dir in ["own/work", "kb", "other"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("kb/faq.md"), "faq").unwrap();
        std::fs::write(root.join("other/secret.txt"), "secret").unwrap();
        let sandbox = SessionSandbox {
            uid: 20001,
            gid: 20001,
            home: root.join("own"),
            cwd: root.join("own/work"),
            read: vec![root.join("kb")],
            write: vec![root.join("own")],
        };
        Fixture {
            _temp: temp,
            root,
            sandbox,
        }
    }

    #[test]
    fn readable_and_writable_follow_the_roots() {
        let Fixture {
            _temp,
            root,
            sandbox,
        } = fixture();
        assert!(sandbox.readable(&root.join("kb/faq.md")).is_ok());
        assert!(sandbox.readable(Path::new("notes.txt")).is_ok());
        assert_eq!(
            sandbox.writable(Path::new("notes.txt")).unwrap(),
            root.join("own/work/notes.txt")
        );
        assert!(sandbox.writable(&root.join("own/new/dir/file.txt")).is_ok());

        let denied = sandbox
            .readable(&root.join("other/secret.txt"))
            .unwrap_err();
        assert!(denied.starts_with("Acceso denegado"), "{denied}");
        assert!(sandbox.writable(&root.join("kb/faq.md")).is_err());
        assert!(sandbox.writable(&root.join("kb/new.md")).is_err());
        assert!(sandbox.readable(Path::new("/etc/passwd")).is_err());
        assert!(sandbox
            .readable(Path::new("../../other/secret.txt"))
            .is_err());
        assert!(sandbox
            .writable(Path::new("missing/../../../other/x.txt"))
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_escape_the_roots() {
        let Fixture {
            _temp,
            root,
            sandbox,
        } = fixture();
        let own = root.join("own");
        std::os::unix::fs::symlink(root.join("other/secret.txt"), own.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(root.join("other"), own.join("dir-link")).unwrap();
        std::os::unix::fs::symlink(root.join("other/nuevo.txt"), own.join("dangling")).unwrap();
        std::os::unix::fs::symlink(root.join("own/work"), own.join("inside")).unwrap();

        assert!(sandbox.readable(&own.join("link.txt")).is_err());
        assert!(sandbox.writable(&own.join("link.txt")).is_err());
        assert!(sandbox.readable(&own.join("dir-link/secret.txt")).is_err());
        assert!(sandbox.writable(&own.join("dir-link/nuevo.txt")).is_err());
        assert!(sandbox.writable(&own.join("dangling")).is_err());
        assert_eq!(
            sandbox.writable(&own.join("inside/a.txt")).unwrap(),
            root.join("own/work/a.txt")
        );
    }

    /// El layout del relé: homes lado a lado en `/data/work/s/<uid>`, tmp en
    /// `/data/tmp/<uid>` y `/data/work` entera como raíz de lectura.
    #[cfg(unix)]
    #[test]
    fn other_sessions_homes_are_private_even_under_a_read_root() {
        let temp = tempfile::tempdir().unwrap();
        let data = std::fs::canonicalize(temp.path()).unwrap();
        for dir in ["work/s/20001", "work/s/20002", "tmp/20001", "tmp/20002"] {
            std::fs::create_dir_all(data.join(dir)).unwrap();
        }
        std::fs::write(data.join("work/faq.md"), "faq").unwrap();
        std::fs::write(data.join("work/s/20002/x"), "ajeno").unwrap();
        std::fs::write(data.join("tmp/20002/y"), "ajeno").unwrap();
        let home = data.join("work/s/20001");
        std::os::unix::fs::symlink(data.join("work/s/20002/x"), home.join("link")).unwrap();
        std::os::unix::fs::symlink(data.join("work/s/20002"), home.join("dir-link")).unwrap();
        let sandbox = SessionSandbox {
            uid: 20001,
            gid: 20001,
            home: home.clone(),
            cwd: home.clone(),
            read: vec![data.join("work")],
            write: vec![home.clone(), data.join("tmp/20001")],
        };

        assert!(sandbox.readable(&data.join("work/faq.md")).is_ok());
        assert!(sandbox.readable(&home.join("notas.txt")).is_ok());
        assert!(sandbox.writable(&data.join("tmp/20001/z")).is_ok());
        for path in [
            data.join("work/s/20002/x"),
            data.join("work/s/20002"),
            data.join("work/s/20003/nuevo"),
            data.join("tmp/20002/y"),
            home.join("link"),
            home.join("dir-link/x"),
            home.join("../20002/x"),
        ] {
            assert!(sandbox.readable(&path).is_err(), "{}", path.display());
        }
        assert!(sandbox.writable(&home.join("dir-link/nuevo")).is_err());
    }

    #[test]
    fn inheritable_env_is_an_allowlist_without_secrets() {
        for name in [
            "PATH",
            "LANG",
            "LC_ALL",
            "TERM",
            "TZ",
            "NODE_PATH",
            "GS_TOOLS_URL",
            "GS_RENDER_URL",
            "GS_SVC_MESH",
            "QUOTE_CURRENCY",
        ] {
            assert!(is_inheritable_env(name), "{name}");
        }
        for name in [
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "DEEPSEEK_API_KEY",
            "GHOSTY_SERVER_TOKEN",
            "REPORT_TOKEN",
            "ACP_TENANT",
            "GS_TOOLS_TOKEN",
            "GS_SIGNING_SECRET",
            "QUOTE_API_KEY",
            "GS_PRIVATE_KEY",
            "HOME",
            "USER",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(!is_inheritable_env(name), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn apply_identity_sets_home_on_the_command() {
        let Fixture { sandbox, .. } = fixture();
        let mut command = tokio::process::Command::new("true");
        command.env("ANTHROPIC_API_KEY", "secreto");
        sandbox.apply_identity(&mut command);
        let envs: Vec<_> = command.as_std().get_envs().collect();
        assert!(envs.iter().all(|(key, _)| *key != "ANTHROPIC_API_KEY"));
        let home = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == "HOME")
            .and_then(|(_, value)| value.map(PathBuf::from));
        assert_eq!(home, Some(sandbox.home.clone()));
    }
}
