//! Avisos de subagentes hacia gs (ghosty.studio) y el registro para detenerlos.
//!
//! gs pinta la lista viva de subagentes en /c, iOS y Android con el mismo contrato que usa el
//! claude-worker (`reporter.ts`): `POST $GS_SUBAGENT_EVENTS_URL` con
//! `{sessionId, events: [{type: "task", task: {...}}]}` y `Authorization: Bearer $FLEET_TOKEN`.
//! Sin esas variables (uso local de ghosty) todo esto es un no-op.
//!
//! Además lleva el registro global `task_id → CancellationToken` que usa el método ACP
//! `_goose/unstable/subagent/cancel` para detener UN hijo desde la app.

use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::conversation::message::{Message, MessageContent};

/// Cada cuánto se manda el avance de un hijo (pasos), como mucho.
const PROGRESS_EVERY: Duration = Duration::from_secs(3);
/// Pasos que se guardan por hijo (gs se queda con los últimos 50).
const MAX_STEPS: usize = 50;
/// Latido: un hijo vivo avisa al menos cada tanto aunque no haga nada (esperando al modelo). gs
/// da por perdido al que pase `3 × HEARTBEAT` sin avisar (proceso muerto, caja reciclada).
pub const HEARTBEAT: Duration = Duration::from_secs(20);
/// Reintentos del aviso final: es el que cierra la fila en gs y no puede perderse en un deploy.
const FINAL_RETRY_DELAYS: [u64; 6] = [1, 2, 4, 8, 16, 32];

/// Lo más que corre un hijo antes de detenerlo y darlo por fallido
/// (`GHOSTY_SUBAGENT_MAX_SECS`, 20 min por defecto).
pub fn max_duration() -> Duration {
    let secs = std::env::var("GHOSTY_SUBAGENT_MAX_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1200);
    Duration::from_secs(secs)
}

/// Corre al hijo con tope de tiempo. Al vencer, el futuro se suelta (eso detiene al hijo) y
/// regresa un error, que se reporta como `failed` (no como `stopped`: nadie lo detuvo a mano).
pub async fn with_deadline<F>(fut: F) -> anyhow::Result<String>
where
    F: std::future::Future<Output = anyhow::Result<String>>,
{
    let limit = max_duration();
    match tokio::time::timeout(limit, fut).await {
        Ok(r) => r,
        Err(_) => {
            Err(anyhow::anyhow!(
                "Se pasó del tiempo límite ({} min) y lo detuve.",
                limit.as_secs() / 60
            ))
        }
    }
}

/// Late mientras el hijo vive. Se detiene con el token que regresa (al terminar el hijo).
pub fn spawn_heartbeat(report: std::sync::Arc<Mutex<TaskReport>>) -> CancellationToken {
    let stop = CancellationToken::new();
    if !reports_enabled() {
        return stop;
    }
    let s = stop.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(HEARTBEAT);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = s.cancelled() => break,
                _ = tick.tick() => {
                    if let Ok(mut r) = report.lock() {
                        r.heartbeat();
                    }
                }
            }
        }
    });
    stop
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Los hijos vivos, para poder detener uno por su id (`_goose/unstable/subagent/cancel`).
static RUNNING: Lazy<Mutex<HashMap<String, (String, CancellationToken)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub fn register(task_id: &str, parent_session: &str, token: CancellationToken) {
    if let Ok(mut m) = RUNNING.lock() {
        m.insert(task_id.to_string(), (parent_session.to_string(), token));
    }
}

pub fn unregister(task_id: &str) {
    if let Ok(mut m) = RUNNING.lock() {
        m.remove(task_id);
    }
}

/// Detiene un hijo. `false` si no existe o es de otra conversación.
pub fn cancel(parent_session: &str, task_id: &str) -> bool {
    let entry = RUNNING.lock().ok().and_then(|m| m.get(task_id).cloned());
    match entry {
        Some((parent, token)) if parent == parent_session => {
            token.cancel();
            true
        }
        _ => false,
    }
}

/// ¿Hay a quién avisar? (las variables las hornea gs en la caja).
pub fn reports_enabled() -> bool {
    std::env::var("GS_SUBAGENT_EVENTS_URL").is_ok() && std::env::var("FLEET_TOKEN").is_ok()
}

/// ¿Cuántos hijos siguen vivos? (para que el front no duerma la caja con hijos trabajando).
pub fn running_count() -> usize {
    RUNNING.lock().map(|m| m.len()).unwrap_or(0)
}

/// El estado de UN hijo tal como lo espera gs.
pub struct TaskReport {
    parent_session: String,
    id: String,
    title: String,
    model: Option<String>,
    started_at: u64,
    tool_uses: u32,
    tokens: u64,
    steps: Vec<Value>,
    last_sent: Option<Instant>,
    /// Ya mandó su aviso final: nada más (un latido tardío reabriría la fila).
    done: bool,
}

impl TaskReport {
    pub fn new(parent_session: &str, id: &str, title: &str, model: Option<String>) -> Self {
        Self {
            parent_session: parent_session.to_string(),
            id: id.to_string(),
            title: title.to_string(),
            model,
            started_at: now_millis(),
            tool_uses: 0,
            tokens: 0,
            steps: Vec::new(),
            last_sent: None,
            done: false,
        }
    }

    /// Lo que hizo el hijo en un mensaje: texto y herramientas, como pasos.
    pub fn observe(&mut self, msg: &Message) {
        for block in &msg.content {
            match block {
                MessageContent::ToolRequest(req) => {
                    self.tool_uses += 1;
                    let name = req
                        .tool_call
                        .as_ref()
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|_| "herramienta".to_string());
                    let (label, class) = tool_label(&name);
                    self.push_step("tool", &label);
                    // La clase del paso (mismos `kind` que arma gs): la app elige el ícono con ella.
                    if let Some(last) = self.steps.last_mut() {
                        last["class"] = json!(class);
                    }
                }
                // El hijo transmite su texto en pedazos («En», «contr», «é»…): se pegan al paso
                // de texto anterior hasta que entre una herramienta. Antes cada pedazo era un
                // renglón y la hoja de la app salía una palabra cortada por línea.
                MessageContent::Text(t) if !t.text.is_empty() => self.append_text(&t.text),
                _ => {}
            }
        }
    }

    fn append_text(&mut self, chunk: &str) {
        if let Some(last) = self.steps.last_mut() {
            if last["kind"] == "text" {
                let prev = last["text"].as_str().unwrap_or("").to_string();
                if prev.chars().count() < 200 {
                    let joined: String = (prev + chunk).chars().take(200).collect();
                    last["text"] = json!(joined);
                }
                return;
            }
        }
        let start = chunk.trim_start();
        if !start.is_empty() {
            self.push_step("text", start);
        }
    }

    fn push_step(&mut self, kind: &str, text: &str) {
        let text: String = text.chars().take(200).collect();
        self.steps.push(json!({ "at": now_millis(), "kind": kind, "text": text }));
        if self.steps.len() > MAX_STEPS {
            self.steps.remove(0);
        }
    }

    /// Manda el avance si ya pasó `PROGRESS_EVERY` desde el último envío.
    pub fn progress(&mut self) {
        if self.last_sent.is_some_and(|t| t.elapsed() < PROGRESS_EVERY) {
            return;
        }
        self.send("running", None);
    }

    /// Los tokens del hijo (los de su sesión al terminar). Sin esto la hoja decía «0 tokens».
    pub fn set_tokens(&mut self, tokens: u64) {
        self.tokens = tokens;
    }

    /// Aviso de «sigo vivo» si nada se mandó en el último latido.
    pub fn heartbeat(&mut self) {
        if self.last_sent.is_some_and(|t| t.elapsed() < HEARTBEAT) {
            return;
        }
        self.send("running", None);
    }

    pub fn started(&mut self) {
        self.send("running", None);
    }

    /// `status`: `completed` | `failed` | `stopped`.
    pub fn finished(&mut self, status: &str, summary: Option<String>) {
        self.send(status, summary);
    }

    fn send(&mut self, status: &str, summary: Option<String>) {
        if self.done {
            return;
        }
        self.last_sent = Some(Instant::now());
        let mut task = json!({
            "id": self.id,
            "title": self.title,
            "status": status,
            "startedAt": self.started_at,
            "usage": { "toolUses": self.tool_uses, "tokens": self.tokens },
            "steps": self.steps,
            // Contrato del latido: este motor avisa al menos cada N s mientras vive; gs puede dar
            // por perdido al que pase 3 × N sin avisar. Un motor que no lo manda no se reapea.
            "heartbeatSecs": HEARTBEAT.as_secs(),
        });
        if let Some(model) = &self.model {
            task["model"] = json!(model);
        }
        if status != "running" {
            task["endedAt"] = json!(now_millis());
            if let Some(s) = summary {
                task["summary"] = json!(s.chars().take(4000).collect::<String>());
            }
        }
        let events = json!([{ "type": "task", "task": task }]);
        if status == "running" {
            post(&self.parent_session, events);
        } else {
            self.done = true;
            post_until_ok(&self.parent_session, events);
        }
    }
}

/// El padre recogió a este hijo con `load`: su resultado ya está en la conversación y gs no
/// debe despertar al padre para entregarlo de nuevo (entrega exactamente una vez). A diferencia
/// del resto de los avisos, éste se espera (con tope): tiene que llegar antes de que cierre el
/// turno del padre, que es cuando gs decide si despierta.
pub async fn collected(parent_session: &str, task_id: &str) {
    if !reports_enabled() {
        return;
    }
    let events = json!([{ "type": "task", "task": { "id": task_id, "collected": true } }]);
    let Some(req) = request(parent_session, events) else {
        return;
    };
    match tokio::time::timeout(Duration::from_secs(3), req.send()).await {
        Ok(Ok(r)) if !r.status().is_success() => {
            warn!("subagent events: gs contestó {} a collected", r.status())
        }
        Ok(Err(e)) => warn!("subagent events: collected no llegó a gs: {e}"),
        Err(_) => warn!("subagent events: collected tardó más de 3 s"),
        _ => {}
    }
}

/// El aviso final, con reintentos hasta que gs conteste 2xx (gs lo aplica idempotente: es un
/// upsert por id). Sin esto, un aviso que caía en un deploy dejaba al hijo «running» para siempre.
fn post_until_ok(parent_session: &str, events: Value) {
    let parent = parent_session.to_string();
    tokio::spawn(async move {
        for (i, delay) in std::iter::once(0).chain(FINAL_RETRY_DELAYS).enumerate() {
            tokio::time::sleep(Duration::from_secs(delay)).await;
            let Some(req) = request(&parent, events.clone()) else {
                return;
            };
            match req.send().await {
                Ok(r) if r.status().is_success() => return,
                // 4xx que no sea 408/429: reintentar no lo arregla.
                Ok(r) if r.status().is_client_error()
                    && r.status().as_u16() != 408
                    && r.status().as_u16() != 429 =>
                {
                    warn!("subagent events: gs rechazó el aviso final ({})", r.status());
                    return;
                }
                Ok(r) => warn!("subagent events: aviso final intento {}: {}", i + 1, r.status()),
                Err(e) => warn!("subagent events: aviso final intento {}: {e}", i + 1),
            }
        }
        warn!("subagent events: el aviso final no llegó a gs tras varios intentos");
    });
}

/// Nombre de herramienta legible para la hoja de la app («ghosty__web_buscar» → «Buscando en
/// la web») y su clase, con los mismos `kind` que arma gs (`web_search`, `read`…). Lo que no se
/// conoce sale sin la extensión, sin guiones bajos y con clase `other`.
fn tool_label(name: &str) -> (String, &'static str) {
    let tool = name.split_once("__").map_or(name, |(_, t)| t);
    let t = tool.to_lowercase();
    let known = match t.as_str() {
        "web_buscar" | "web_search" | "search" => Some(("Buscando en la web", "web_search")),
        "web_leer" | "fetch" | "web_fetch" | "leer_pagina" => Some(("Leyendo una página", "fetch")),
        "shell" => Some(("Usando la terminal", "execute")),
        "text_editor" | "edit" | "write" => Some(("Editando un archivo", "edit")),
        "read" | "read_file" | "tree" | "analyze" => Some(("Leyendo archivos", "read")),
        "entregar_archivo" => Some(("Preparando un archivo", "deliver")),
        "todo_write" | "todowrite" => Some(("Organizando los pasos", "todo")),
        "load" => Some(("Revisando lo que llevo", "collect")),
        _ => None,
    };
    if let Some((label, class)) = known {
        return (label.to_string(), class);
    }
    let human = tool.replace('_', " ");
    let mut c = human.chars();
    let label = match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => "Herramienta".to_string(),
    };
    (label, "other")
}

/// Fuego y olvido: un aviso que no llega no puede frenar al agente.
fn request(parent_session: &str, events: Value) -> Option<reqwest::RequestBuilder> {
    let (Ok(url), Ok(token)) = (
        std::env::var("GS_SUBAGENT_EVENTS_URL"),
        std::env::var("FLEET_TOKEN"),
    ) else {
        return None;
    };
    let body = json!({ "sessionId": parent_session, "events": events });
    Some(
        reqwest::Client::new()
            .post(&url)
            .bearer_auth(token)
            .timeout(Duration::from_secs(10))
            .json(&body),
    )
}

fn post(parent_session: &str, events: Value) {
    let Some(req) = request(parent_session, events) else {
        return;
    };
    tokio::spawn(async move {
        match req.send().await {
            Ok(r) if !r.status().is_success() => {
                warn!("subagent events: gs contestó {}", r.status())
            }
            Err(e) => warn!("subagent events: no llegó a gs: {e}"),
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_only_from_its_own_conversation() {
        let token = CancellationToken::new();
        register("t1", "parent-a", token.clone());
        assert!(!cancel("parent-b", "t1"));
        assert!(!token.is_cancelled());
        assert!(cancel("parent-a", "t1"));
        assert!(token.is_cancelled());
        unregister("t1");
        assert!(!cancel("parent-a", "t1"));
    }

    #[test]
    fn streamed_text_joins_into_one_step() {
        let mut r = TaskReport::new("p", "t", "título", None);
        for c in ["En", "contr", "é", " señales"] {
            r.append_text(c);
        }
        assert_eq!(r.steps.len(), 1);
        assert_eq!(r.steps[0]["text"], "Encontré señales");
        assert_eq!(tool_label("ghosty__web_buscar"), ("Buscando en la web".to_string(), "web_search"));
        assert_eq!(tool_label("ghosty__otra_cosa"), ("Otra cosa".to_string(), "other"));
    }

    #[test]
    fn nothing_is_sent_after_the_final_report() {
        let mut r = TaskReport::new("p", "t", "título", None);
        r.done = true;
        r.last_sent = None;
        r.heartbeat();
        assert!(r.last_sent.is_none());
    }

    #[test]
    fn steps_are_capped() {
        let mut r = TaskReport::new("p", "t", "título", None);
        for i in 0..80 {
            r.push_step("text", &format!("paso {i}"));
        }
        assert_eq!(r.steps.len(), MAX_STEPS);
    }
}
