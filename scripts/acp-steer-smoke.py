#!/usr/bin/env python3
"""Humo de steer y cancel por ACP contra `ghosty serve` con un proveedor REAL.

1. Un prompt largo (6 × `sleep 4`, narrando cada paso) con la instrucción de que, si
   llega una palabra clave, pare y la conteste en MAYÚSCULAS.
2. A los ~6 s, `_goose/unstable/session/steer` con `expectedRunId` (sale de
   `session_info_update._meta.goose.activeRunId`) → la respuesta debe traer la palabra en
   mayúsculas ANTES del último paso (mismo turno, misma burbuja).
3. Otro prompt largo y `session/cancel` a los ~3 s → `stopReason == "cancelled"` en < 10 s.

Uso: DEEPSEEK_API_KEY=… scripts/acp-steer-smoke.py [ruta/al/binario]
"""
import asyncio, json, os, subprocess, sys, tempfile, time
BIN = next((a for a in sys.argv[1:] if not a.startswith("--")), "target/release/ghosty")
PORT = 3298
TOKEN = "ghl-steer-" + "x" * 22
PALABRA = "banderilla"

async def main():
    import websockets
    root = tempfile.mkdtemp(prefix="gl-steer-")
    env = dict(os.environ, GHOSTY_PATH_ROOT=root, GHOSTY_SERVER_TOKEN=TOKEN, GHOSTY_TELEMETRY="0",
               GHOSTY_DISABLE_KEYRING="1", GHOSTY_MODE="auto")
    env.setdefault("GHOSTY_PROVIDER", "custom_deepseek"); env.setdefault("GHOSTY_MODEL", "deepseek-v4-flash")  # override: GHOSTY_PROVIDER=claude-code GHOSTY_MODEL=sonnet
    proc = subprocess.Popen([BIN, "serve", "--port", str(PORT)], env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    try:
        for _ in range(200):
            try:
                import urllib.request
                if urllib.request.urlopen(f"http://127.0.0.1:{PORT}/status", timeout=1).status == 200: break
            except Exception: pass
            time.sleep(0.05)
        async with websockets.connect(f"ws://127.0.0.1:{PORT}/acp?token={TOKEN}") as ws:
            pending, texto, run_id, eventos = {}, [], {"v": None}, []
            async def reader():
                async for raw in ws:
                    m = json.loads(raw)
                    if "id" in m and m.get("method") is None:
                        fut = pending.pop(m["id"], None)
                        if fut: fut.set_result(m)
                    elif m.get("method") == "session/update":
                        u = m["params"]["update"]; eventos.append(u.get("sessionUpdate"))
                        if u.get("sessionUpdate") == "agent_message_chunk":
                            texto.append(u["content"].get("text", ""))
                        if u.get("sessionUpdate") == "session_info_update":
                            g = (u.get("_meta") or {}).get("goose") or {}
                            if "activeRunId" in g: run_id["v"] = g["activeRunId"]
                    elif m.get("method") == "session/request_permission":
                        opts = m["params"]["options"]; ok = next(o for o in opts if o["kind"].startswith("allow"))
                        await ws.send(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": {"outcome": {"outcome": "selected", "optionId": ok["optionId"]}}}))
            asyncio.create_task(reader())
            nid = [0]
            async def call(method, params, timeout=180):
                nid[0] += 1; fut = asyncio.get_event_loop().create_future(); pending[nid[0]] = fut
                await ws.send(json.dumps({"jsonrpc": "2.0", "id": nid[0], "method": method, "params": params}))
                return await asyncio.wait_for(fut, timeout)
            async def notify(method, params):
                await ws.send(json.dumps({"jsonrpc": "2.0", "method": method, "params": params}))
            await call("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
            sid = (await call("session/new", {"cwd": root, "mcpServers": []}))["result"]["sessionId"]
            print(f"✓ sesión {sid}")

            # ── 1+2: steer a mitad del turno ──
            largo = ("Ejecuta SEIS veces, una por una, el comando `sleep 4 && echo paso N` con N de 1 a 6, "
                     "y después de cada uno escribe 'Listo paso N'. "
                     f"Si en algún momento recibes un mensaje mío con una palabra clave, DETENTE y responde sólo esa palabra en MAYÚSCULAS.")
            t0 = time.time()
            turno = asyncio.create_task(call("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": largo}]}))
            for _ in range(100):
                if run_id["v"]: break
                await asyncio.sleep(0.1)
            assert run_id["v"], "no llegó activeRunId en session_info_update"
            print(f"✓ activeRunId={run_id['v']} a los {time.time()-t0:.1f}s")
            await asyncio.sleep(6)
            r = await call("_goose/unstable/session/steer", {"sessionId": sid, "prompt": [{"type": "text", "text": f"palabra clave: {PALABRA}"}], "expectedRunId": run_id["v"]})
            assert "result" in r, r; print(f"✓ steer aceptado → {r['result']}  ({time.time()-t0:.1f}s)")
            r = await turno
            cuerpo = "".join(texto); dur = time.time()-t0
            print(f"  stopReason={r.get('result',{}).get('stopReason')} en {dur:.1f}s; eventos={sorted(set(eventos))}")
            print("  texto:", cuerpo[-300:].replace("\n", " | "))
            assert PALABRA.upper() in cuerpo, "el modelo no contestó la palabra en mayúsculas"
            assert "Listo paso 6" not in cuerpo, "no paró: llegó al paso 6"
            print("✓ STEER OK: paró antes del paso 6 y contestó en mayúsculas en el mismo turno")

            # ── 3: cancel ──
            texto.clear(); run_id["v"] = None
            t0 = time.time()
            turno = asyncio.create_task(call("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": largo}]}))
            await asyncio.sleep(3)
            await notify("session/cancel", {"sessionId": sid})
            r = await asyncio.wait_for(turno, 20)
            stop = r.get("result", {}).get("stopReason"); dur = time.time()-t0
            print(f"  stopReason={stop} en {dur:.1f}s")
            assert stop == "cancelled", r
            print("✓ CANCEL OK")
        print("HUMO STEER/CANCEL OK")
    finally:
        proc.terminate()
        try: proc.wait(5)
        except subprocess.TimeoutExpired: proc.kill()
        out = proc.stdout.read()
        err = [l for l in out.splitlines() if "ERROR" in l or "WARN" in l]
        if err: print("--- serve avisos ---"); print("\n".join(err[:10]))

asyncio.run(main())
