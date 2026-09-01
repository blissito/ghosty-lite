# Guía para agentes

ghosty-lite es un fork de goose (Apache 2.0) podado y rebrandeado por ghosty.studio.
Un agente de terminal ligero para cajas efímeras, con servidor ACP por WebSocket.

## Reglas

- Identificadores en inglés; comentarios, commits y copy de UI en español.
- Los nombres de crate `goose*` y el namespace ACP `_goose/unstable/*` se conservan a
  propósito (compatibilidad y `git merge upstream`). No los renombres.
- Las claves de config son `GHOSTY_*` y son también variables de entorno. Nada nuevo
  con prefijo `GOOSE_`.
- La home es `~/.ghosty-lite` o `GHOSTY_PATH_ROOT`. Nunca `~/.ghosty` (es de ghostycode).
- Sin `!important`, sin telemetría de terceros, sin V8 en el default.

## Comandos

```sh
just check      # cargo check --workspace --all-targets
just lint       # fmt --check + clippy -D warnings
just test       # cargo test --workspace
just release    # binario ghosty en target/release
bash scripts/check-branding.sh   # cero marca de upstream fuera de la atribución
```

## Estructura

- `crates/goose` — núcleo: agente, providers, ACP, sesiones, config.
- `crates/goose-cli` — el binario `ghosty`: REPL, `configure`, `serve`.
- `crates/goose-providers` — providers nativos y declarativos (`declarative/definitions/*.json`).
- `crates/ghosty-telemetry` — telemetría propia (derivada de ghostycode, MIT).
- `Dockerfile` — `serve` en una VM: musl estático, sin keyring.
