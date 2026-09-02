# Telemetría de ghosty-lite

**Estado: inerte hasta que se configura un endpoint.** El binario no trae ningún host
horneado. Sin `GHOSTY_TELEMETRY_ENDPOINT` no se construye un cliente HTTP, no se escribe
un buffer y no se manda nada a nadie. Quien despliega ghosty-lite decide si quiere
telemetría y a dónde va.

Cuando hay endpoint, la telemetría cuenta: versión, familia de sistema y de CPU, duración
y desenlace de la sesión, y contadores agregados de funciones y de errores. Nunca
conversaciones, código, prompts, archivos, nombres de repo o de rama, contenido del
modelo ni credenciales. Nunca una línea de tiempo por turno ni por herramienta.

Este documento **es** el esquema: un test en `crates/ghosty-telemetry/src/tests.rs`
extrae los nombres de campo de los bloques `jsonc` y de las tablas de abajo y los
compara con los structs que serializan de verdad. Un campo que esté aquí y no en el
código, o al revés, rompe el build.

## Encender

```sh
GHOSTY_TELEMETRY_ENDPOINT=https://tu-ingest.example/v1/telemetry ghosty
```

La clave también vale en `config.yaml`. Un endpoint que no sea `https://` (o `http://`
a loopback) se rechaza y la telemetría queda apagada para esa ejecución, con un aviso
en el log. Un endpoint **explícitamente vacío** (`GHOSTY_TELEMETRY_ENDPOINT=""`) es el
modo dry-run: los lotes se anexan a `$GHOSTY_PATH_ROOT/telemetry/dryrun.jsonl`, byte a
byte lo que recibiría el servidor, sin cliente HTTP. Ese archivo es la forma de auditar
este documento contra la realidad.

Con endpoint configurado y sin decisión previa, el REPL enseña un aviso al primer
arranque (`ghosty configure → Ajustes → Telemetría` lo repite). `serve` y `run` nunca
preguntan: siguen la configuración tal cual.

## Apagar

Hay dos apagados y hacen cosas distintas.

```sh
GHOSTY_TELEMETRY=0 ghosty                       # kill switch: esta ejecución, sin borrar nada
ghosty configure → Ajustes → Telemetría → No    # opt-out durable: escribe GHOSTY_TELEMETRY=false
```

**`GHOSTY_TELEMETRY=false` en `config.yaml` es el opt-out.** Es un piso: `GHOSTY_TELEMETRY=1`
en el entorno pierde contra él. Borra el install id, trunca el buffer y el dry-run, y
deja una lápida (`disabled`). Si algo de ese borrado falla, la lápida sigue ahí y el
buffer no se puede drenar: un borrado fallido falla cerrado. Cada ejecución posterior
reafirma la lápida mientras el ajuste siga.

**La variable de entorno es un kill switch, no un opt-out.** Apaga la recolección para
esa ejecución y no toca nada en disco. Un arnés que ponga `GHOSTY_TELEMETRY=0` para un
comando no debe borrar el estado de la persona dueña de la máquina.

`GHOSTY_TELEMETRY` acepta `0 1 true false yes no on off enabled disabled`. Un valor
que no se puede leer resuelve a apagado.

## Dónde vive y cuánto disco usa

Todo está en `$GHOSTY_PATH_ROOT/telemetry/`:

| Archivo | Qué |
|---|---|
| `buffer.jsonl` | eventos pendientes, un objeto JSON por línea |
| `buffer.jsonl.lock` | el lock de orden que comparten escritura, envío, armado y borrado |
| `dryrun.jsonl` | a dónde van los lotes con endpoint vacío |
| `state.json` | última versión vista y último intento de envío |
| `install_id.json` | el id aleatorio y cuándo se acuñó |
| `disabled` | la lápida; si existe no se anexa ni se manda nada |

Topes: 512 eventos o 256 KiB de buffer (`buffer.rs`); una línea de más de 4 KiB se
descarta. Un endpoint caído no reintenta ni hace backoff: el lote se descarta.

## Cuándo se manda algo

Un lote sale al cerrar la sesión (`session_end`) y en el panic hook. Nunca por turno,
nunca por herramienta. El POST lleva `Content-Type: application/json` y nada más: sin
cookies, sin `Authorization`, sin cabeceras de identidad.

## Esquema de eventos

### Batch envelope — sent on every POST

```jsonc
{
  "schema_version": 1,
  "sent_at":     "2026-09-02T18:04:11Z",   // RFC3339 UTC, precisión de segundos
  "install_id":  "3f2a…",                  // uuid v4, rota cada 90 días
  "app_version": "1.48.0",
  "git_sha":     null,                     // sólo en builds estampados con SHA
  "surface":     "serve",
  "os":          "linux",
  "arch":        "x86_64",
  "libc":        "musl",
  "tty":         false,
  "events":      [ … ]
}
```

| Field | Tipo | Regla |
|---|---|---|
| `schema_version` | `u32` | constante en `event.rs`. Sube con cualquier campo añadido, quitado o retipado. Fijada por un snapshot dorado. |
| `sent_at` | RFC3339 | por **lote**; los eventos no llevan timestamp. |
| `install_id` | uuid v4 | aleatorio, nunca derivado, rota cada 90 días. |
| `app_version` | string | `env!("CARGO_PKG_VERSION")`. `^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$`. |
| `git_sha` | string \| null | primeros 12 hex de `GHOSTY_BUILD_SHA` o `GITHUB_SHA` en el build. `null` si no se estampó. Nunca el commit del workspace del usuario. |
| `surface` | enum | `tui \| exec \| cli \| serve`, fijado por `argv[1]` en `main.rs`. |
| `os` | enum | `linux \| macos \| windows \| freebsd \| android \| other`. |
| `arch` | enum | `x86_64 \| aarch64 \| other`. |
| `libc` | enum | `gnu \| musl \| none`, en tiempo de compilación. |
| `tty` | bool | `stdin` y `stdout` son terminal. |
| `events` | array | el buffer drenado. Tope 200 eventos o 64 KiB por lote; el resto espera al siguiente. |

### Evento `install_or_upgrade`

```jsonc
{ "event": "install_or_upgrade", "kind": "upgrade", "previous_version": "1.47.0" }
```

`kind`: `install | upgrade | downgrade`. `previous_version` sale sólo de `state.json`.

### Evento `session_start`

```jsonc
{ "event": "session_start", "source": "interactive" }
```

### Evento `session_end`

```jsonc
{
  "event": "session_end",
  "duration_bucket": "1m_10m",
  "exit_class": "clean",
  "cold_start_bucket": "250_1000",
  "providers": ["deepseek", "custom"],
  "counters": { "turns": 14, "tool_calls": 61, "fleet_dispatch": 0, "workflow_run": 0,
                "subagent_spawn": 2, "mcp_server_connected": 0, "memory_search": 0,
                "approval_modal_shown": 0, "approval_auto_allowed": 0,
                "command_palette_open": 3 },
  "errors":   { "auth_preflight_failed": 0, "provider_http_4xx": 0, "provider_http_5xx": 1,
                "tool_denied_by_policy": 0, "tool_timeout": 0, "network_error": 0 },
  "turn_wall": { "lt_5s": 9, "5_30s": 4, "30_120s": 1, "gte_120s": 0 }
}
```

`counters` y `errors` son structs de campos `u32` con nombre, no mapas: el conjunto de
claves lo cierra el compilador y se serializa entero, ceros incluidos.

- `duration_bucket`: `lt_1m | 1m_10m | 10m_60m | gt_60m`.
- `exit_class`: `clean | signal | panic | error`, desde un atómico explícito, nunca desde
  el exit code.
- `cold_start_bucket`: `lt_250 | 250_1000 | 1000_3000 | gte_3000` ms. Sólo en el REPL.
- `providers`: nombres de proveedor de un enum cerrado, ordenados y sin repetir. **Nunca
  un id de modelo**: puede ser una ruta, una URL o un deployment id que es credencial.

El esquema de contadores viene de ghostycode y es más ancho que lo que ghosty-lite
instrumenta hoy. Lo que **sí** sube en este binario está en `crates/goose/src/telemetry.rs`;
lo demás se manda siempre en 0.

**`counters`** — conjunto cerrado. Cada incremento ocurre en el punto de llamada:

| field | en ghosty-lite |
|---|---|
| `turns` | sí, al cerrar cada turno del agente |
| `tool_calls` | sí, en la ejecución de herramientas |
| `fleet_dispatch` | siempre 0 |
| `workflow_run` | sí, trabajos del scheduler |
| `subagent_spawn` | siempre 0 |
| `mcp_server_connected` | siempre 0 |
| `memory_search` | siempre 0 |
| `approval_modal_shown` | siempre 0 |
| `approval_auto_allowed` | siempre 0 |
| `command_palette_open` | siempre 0 |

**`errors`** — conjunto cerrado. Cada valor es un discriminante de variante, nunca `err.to_string()`:

| field | en ghosty-lite |
|---|---|
| `auth_preflight_failed` | errores de proveedor clase `auth` / `not_configured` |
| `provider_http_4xx` | errores de proveedor con status 4xx |
| `provider_http_5xx` | clase `server` |
| `tool_denied_by_policy` | herramienta negada por permisos |
| `tool_timeout` | herramienta con timeout |
| `network_error` | clase `network` |

Por qué discriminantes y nada más: el `Display` de muchos errores lleva rutas absolutas,
fragmentos de código emitidos por el modelo o el cuerpo HTTP crudo del proveedor, y un
400 de un filtro de contenido suele devolver el prompt entero.

`turn_wall`: histograma por sesión, nunca serie por turno. `lt_5s | 5_30s | 30_120s | gte_120s`.

### Evento `panic`

Se anexa **de forma síncrona** desde el panic hook, porque puede no llegar a haber
`session_end`.

```jsonc
{ "event": "panic", "site": "crates/goose/src/agents/agent.rs:1582:5" }
```

`site` es `file:line:col` **sólo si** el archivo empieza por `crates/`; si no, el literal
`"<dep>"` (una dependencia de registry incluiría el usuario de la máquina que compiló).
**El mensaje del panic nunca se manda**: un panic de slicing embebe la cadena entera.

## Qué no se recoge nunca

Prompts; respuestas; argumentos de herramientas; diffs; parches; contenido o nombres de
archivos; rutas; remotos, nombres o ramas de git; SHAs del workspace; entradas de memoria;
historial de chat; API keys, tokens, cookies o cabeceras `Authorization` (ni un booleano
que diga que existe una llave); ids de modelo; nombres de proveedores custom; nombres,
comandos o URLs de servidores MCP; texto de reglas de permisos; cuerpos de mensajes de
error; texto de panics; timestamps por evento; teclas; portapapeles; capturas; micrófono;
cámara; ubicación; y ningún SDK de anuncios o analítica de terceros: no hay ninguno en el
binario y no se puede añadir.

**Regla para quien toque esto**: nunca `#[derive(Serialize)]` sobre un tipo de estado
existente. Cada struct de telemetría se construye desde cero con campos explícitos.
