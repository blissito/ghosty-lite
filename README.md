<div align="center">

# 👻 ghosty-lite

**Un agente de terminal ligero, hecho para correr en cajas efímeras y exponerse por WebSocket.**

</div>

ghosty-lite es un fork de [goose](https://github.com/block/goose) (Apache 2.0) podado y
rebrandeado por [ghosty.studio](https://ghosty.studio). Se queda con lo que importa dentro de
una VM chica —el loop del agente, las herramientas, MCP, permisos, recetas, el servidor ACP— y
deja fuera el desktop, la inferencia local y todo lo que engorda el binario sin servir headless.

## Instalar

```bash
cargo install --git https://github.com/blissito/ghosty-lite ghosty-lite --bin ghosty
```

## Primer uso

```bash
ghosty            # el primer arranque abre el asistente de configuración
ghosty configure  # volver a abrirlo cuando quieras
ghosty doctor     # comprobar proveedor, extensiones y rutas
```

## Como servidor (la razón de este proyecto)

```bash
ghosty serve --setup   # genera el token, elige host/puerto/orígenes e imprime cómo conectar
ghosty serve           # ACP por WebSocket y HTTP en /acp, salud en /health
```

Desde un navegador:

```js
new WebSocket("ws://127.0.0.1:3284/acp?token=ghl-…")
```

Desde cualquier otro cliente, el token va en la cabecera `X-Secret-Key`. Habla
[ACP](https://agentclientprotocol.com) estándar: `initialize` → `session/new` →
`session/prompt`, y recibe `session/update`. Conserva el namespace de extensiones
`_goose/unstable/*` para que los clientes ACP que ya existen sigan funcionando.

Tres cosas que conviene saber antes de exponerlo:

- Sin `GHOSTY_SERVER_TOKEN` el servidor no arranca (salvo `--dangerously-unauthenticated`).
- `--allowed-origin` **reemplaza** los orígenes por defecto (loopback); una página servida desde
  otro origen necesita el suyo en esa lista o recibe 403 en el upgrade.
- `/health` y `/status` responden sin token. TLS conviene terminarlo en el balanceador.

## En una VM o contenedor

```bash
docker build -t ghosty-lite .
docker run -p 3284:3284 -e GHOSTY_SERVER_TOKEN=… -e EASYBITS_API_KEY=… -v datos:/data ghosty-lite
```

La imagen corre `ghosty serve --host 0.0.0.0`, guarda config y sesiones en `/data`
(`GHOSTY_PATH_ROOT`) y no usa el keyring del sistema. Todo el estado que importa está en ese
volumen; sin él, la caja nace limpia cada vez.

## Configuración

Vive en `~/.ghosty-lite/` (o donde apunte `GHOSTY_PATH_ROOT`). Cualquier clave se puede fijar
por entorno con el mismo nombre: `GHOSTY_PROVIDER`, `GHOSTY_MODEL`, `GHOSTY_MODE`… Las
variables `GOOSE_*` de una instalación de goose se siguen leyendo.

## Telemetría

Cuenta versión, sistema, duración de la sesión y contadores de uso y errores. Nunca
conversaciones, código, prompts ni credenciales. Se apaga con `GHOSTY_TELEMETRY=0` o desde
`ghosty configure` → Ajustes → Telemetría.

## Licencia

Apache 2.0. Ver `LICENSE`, `NOTICE` y `CHANGES-FROM-UPSTREAM.md`.
