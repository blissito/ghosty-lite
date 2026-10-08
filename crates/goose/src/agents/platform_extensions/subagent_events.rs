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
                    self.push_step("tool", &tool_label(&name));
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

    pub fn started(&mut self) {
        self.send("running", None);
    }

    /// `status`: `completed` | `failed` | `stopped`.
    pub fn finished(&mut self, status: &str, summary: Option<String>) {
        self.send(status, summary);
    }

    fn send(&mut self, status: &str, summary: Option<String>) {
        self.last_sent = Some(Instant::now());
        let mut task = json!({
            "id": self.id,
            "title": self.title,
            "status": status,
            "startedAt": self.started_at,
            "usage": { "toolUses": self.tool_uses, "tokens": self.tokens },
            "steps": self.steps,
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
        post(&self.parent_session, json!([{ "type": "task", "task": task }]));
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

/// Nombre de herramienta legible para la hoja de la app («ghosty__web_buscar» → «Buscando en
/// la web»). Lo que no se conoce sale sin la extensión y sin guiones bajos.
fn tool_label(name: &str) -> String {
    let tool = name.split_once("__").map_or(name, |(_, t)| t);
    let t = tool.to_lowercase();
    let known = match t.as_str() {
        "web_buscar" | "web_search" | "search" => Some("Buscando en la web"),
        "web_leer" | "fetch" | "web_fetch" | "leer_pagina" => Some("Leyendo una página"),
        "shell" => Some("Usando la terminal"),
        "text_editor" | "edit" | "write" => Some("Editando un archivo"),
        "read" | "read_file" | "tree" | "analyze" => Some("Leyendo archivos"),
        "entregar_archivo" => Some("Preparando un archivo"),
        "todo_write" | "todowrite" => Some("Organizando los pasos"),
        "load" => Some("Revisando lo que llevo"),
        _ => None,
    };
    if let Some(k) = known {
        return k.to_string();
    }
    let human = tool.replace('_', " ");
    let mut c = human.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => "Herramienta".to_string(),
    }
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
        assert_eq!(tool_label("ghosty__web_buscar"), "Buscando en la web");
        assert_eq!(tool_label("ghosty__otra_cosa"), "Otra cosa");
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
