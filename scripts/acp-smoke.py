#!/usr/bin/env python3
"""Humo del servidor: arranca `ghosty serve`, conecta por WebSocket con ?token=,
hace initialize → session/new (→ session/prompt si hay proveedor) y comprueba
/health sin token, 401 con token malo y 403 con un Origin ajeno.

Uso: scripts/acp-smoke.py [ruta/al/binario] [--prompt]
Requiere `pip install websockets`.
"""
import asyncio, json, os, subprocess, sys, tempfile, time, urllib.request, urllib.error

BIN = next((a for a in sys.argv[1:] if not a.startswith("--")), "target/release/ghosty")
WANT_PROMPT = "--prompt" in sys.argv
PORT = 3299
TOKEN = "ghl-smoke-" + "x" * 22

def http(path, headers=None, method="GET", body=None):
    req = urllib.request.Request(f"http://127.0.0.1:{PORT}{path}", data=body, method=method, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()

async def main():
    import websockets
    root = tempfile.mkdtemp(prefix="gl-smoke-")
    env = dict(os.environ, GHOSTY_PATH_ROOT=root, GHOSTY_SERVER_TOKEN=TOKEN,
               GHOSTY_TELEMETRY="0", GHOSTY_DISABLE_KEYRING="1", GHOSTY_MODE="auto")
    if not WANT_PROMPT:
        env.setdefault("GHOSTY_PROVIDER", "ollama"); env.setdefault("GHOSTY_MODEL", "llama3.2")
    proc = subprocess.Popen([BIN, "serve", "--port", str(PORT), "--allowed-origin", "https://app.ghosty.studio"],
                            env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    t0 = time.time()
    try:
        for _ in range(200):
            try:
                if http("/status")[0] == 200: break
            except Exception: pass
            time.sleep(0.05)
        else:
            raise SystemExit("serve no levantó")
        print(f"✓ /status ok a los {int((time.time()-t0)*1000)} ms")
        s, _ = http("/health"); assert s == 200, s; print("✓ /health sin token → 200")
        s, _ = http("/acp", {"X-Secret-Key": "malo", "Content-Type": "application/json"}, "POST", b"{}")
        assert s == 401, s; print("✓ token malo → 401")
        try:
            await websockets.connect(f"ws://127.0.0.1:{PORT}/acp?token={TOKEN}", additional_headers={"Origin": "https://otro.example"})
            raise SystemExit("✗ un Origin ajeno debió dar 403")
        except websockets.exceptions.InvalidStatus as e:
            assert e.response.status_code == 403, e.response.status_code; print("✓ Origin ajeno → 403")
        async with websockets.connect(f"ws://127.0.0.1:{PORT}/acp?token={TOKEN}",
                                      additional_headers={"Origin": "https://app.ghosty.studio"}) as ws:
            print("✓ WebSocket upgrade con ?token= y Origin permitido")
            async def call(id_, method, params):
                await ws.send(json.dumps({"jsonrpc": "2.0", "id": id_, "method": method, "params": params}))
                while True:
                    m = json.loads(await asyncio.wait_for(ws.recv(), 60))
                    if m.get("id") == id_: return m
                    yield_notification(m)
            notes = []
            def yield_notification(m): notes.append(m.get("method"))
            r = await call(1, "initialize", {"protocolVersion": 1, "clientCapabilities": {}})
            info = r["result"].get("agentInfo", {})
            assert info.get("name") == "ghosty-lite", info; print(f"✓ initialize → agentInfo {info.get('name')} {info.get('version')}")
            r = await call(2, "session/new", {"cwd": root, "mcpServers": []})
            sid = r.get("result", {}).get("sessionId"); assert sid, r; print(f"✓ session/new → {sid}")
            if WANT_PROMPT:
                r = await call(3, "session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": "Ejecuta `echo hola-ghosty` con la herramienta de shell y dime qué imprimió."}]})
                stop = r.get("result", {}).get("stopReason"); print(f"✓ session/prompt → stopReason={stop}; notificaciones: {sorted(set(notes))}")
                assert stop == "end_turn", r
        print("HUMO OK")
    finally:
        proc.terminate()
        try: proc.wait(5)
        except subprocess.TimeoutExpired: proc.kill()
        out = proc.stdout.read()
        print("--- salida de serve (primeras líneas) ---"); print("\n".join(out.splitlines()[:12]))

asyncio.run(main())
