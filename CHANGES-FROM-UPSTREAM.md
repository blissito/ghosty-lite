# Cambios respecto a goose (upstream)

ghosty-lite parte de [goose](https://github.com/block/goose) **v1.48.0** (tag
`fork-point-v1.48.0`, commit `25021517f`). Este archivo cumple la sección 4(b) de la
licencia Apache 2.0: deja constancia de que los archivos fueron modificados. El detalle
línea a línea está en `git log fork-point-v1.48.0..HEAD`.

## Qué se quitó

| Componente | Por qué |
|---|---|
| `ui/` (desktop Electron), `documentation/`, `evals/`, `oidc-proxy/`, `services/` | no participan en el binario |
| `goose-roaming` (p2p por iroh) | un agente headless no lo usa |
| `goose-local-inference`, `goose-download-manager`, dictado (whisper) | fuerzan builds por acelerador y engordan el binario |
| PostHog | telemetría de Block; se sustituye por la propia (`crates/ghosty-telemetry`) |
| Compartir sesiones por Nostr | sin uso en cajas efímeras |
| Gateway (Telegram) | sin uso en cajas efímeras |
| Goose Apps y el proxy de MCP Apps | sólo el desktop los pinta |
| Autovisualiser | sólo el desktop lo pinta |
| `goose-sdk` (uniffi) | sin consumidores |
| `vendor/v8` y el shim de V8 | `code_execution` queda como feature opcional `code-mode` |
| Self-update (sigstore), `native-tls`, arboard, manpages | imagen inmutable; sólo rustls |
| AWS Bedrock/SageMaker | feature opcional `aws-providers`, apagada por defecto |

Cargo.lock: 1,339 → 955 paquetes. Miembros del workspace: 16 → 12 (11 de goose + `ghosty-telemetry`).

## Qué se renombró

| Superficie | goose | ghosty-lite |
|---|---|---|
| Binario | `goose` | `ghosty` |
| Claves de env/config | `GOOSE_*` | `GHOSTY_*` (se leen las viejas como respaldo y se migran en `config.yaml`) |
| Home | etcetera `Block/goose` + `GOOSE_PATH_ROOT` | `~/.ghosty-lite` + `GHOSTY_PATH_ROOT` |
| Servicio de keyring | `goose` | `ghosty-lite` |
| Secreto de `serve` | `GOOSE_SERVER__SECRET_KEY`, tokens `goose-acp-…` | `GHOSTY_SERVER_TOKEN`, tokens `ghl-…` |
| Nombre del agente ACP | `goose-acp` | `ghosty-lite` |

Se conservan a propósito: los nombres de crate `goose*` (internos; permiten mergear
upstream) y el namespace de métodos ACP `_goose/unstable/*` (compatibilidad con clientes).

## Qué se añadió

- Onboarding en español: `ghosty configure` con proveedores rápidos (EasyBits, DeepSeek,
  Anthropic, OpenAI, Ollama), `ghosty serve --setup` / `--check`.
- Provider declarativo `easybits`.
- Telemetría propia, con aviso al primer arranque y `GHOSTY_TELEMETRY=0`.
- La mascota: un fantasma en bloques con ojos animados, portado de ghostycode.
- `Dockerfile` para `serve` en VMs (musl estático, sin keyring).
