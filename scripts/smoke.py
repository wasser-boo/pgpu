#!/usr/bin/env python3
"""Offline process smoke test: isolated SQLite, ephemeral loopback port, no cloud keys."""
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/praxis-router"
TOKEN = "offline-smoke-test-token-not-a-secret-123456789"


def status(url, **headers):
    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=1) as response:
            return response.status
    except urllib.error.HTTPError as error:
        return error.code


with tempfile.TemporaryDirectory(prefix="pgpu-smoke-") as directory:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    config = Path(directory) / "config.toml"
    config.write_text(f'''[router]
bind_ip = "127.0.0.1"
dashboard_port = {port}
data_dir = "{directory}/data"
tz = "UTC"
[stt]
mode = "media_slot"
[[slots]]
id = 1
name = "offline"
role = "llm"
''')
    # Explicit allowlist, NOT the user's environment with real cloud credentials.
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "ROUTER_TOKEN": TOKEN,
           "VAST_API_KEY": "", "NB_API_TOKEN": "", "RUST_LOG": "warn"}
    command = [str(BINARY), "--config", str(config)]
    rejected = subprocess.run(command, env={**env, "ROUTER_TOKEN": ""}, capture_output=True, timeout=5)
    assert rejected.returncode != 0, "empty admin token must fail startup"
    process = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        base = f"http://127.0.0.1:{port}"
        deadline = time.monotonic() + 10
        while True:
            if process.poll() is not None:
                raise AssertionError(process.communicate()[0].decode())
            try:
                if status(base + "/readyz") == 200:
                    break
            except (OSError, urllib.error.URLError):
                pass
            assert time.monotonic() < deadline, "readiness timed out"
            time.sleep(0.05)
        assert status(base + "/healthz") == 200
        assert status(base + "/api/v1/state") == 401
        assert status(base + "/api/v1/state", Authorization=f"Bearer {TOKEN}") == 200
        assert status(base + "/api/v1/events/stream", Cookie=f"pgpu_session={TOKEN}", Origin="http://other-host") == 401
        for path in ("/api/v1/performance", "/api/v1/performance/summary"):
            assert status(base + path) == 401
            assert status(base + path, Authorization=f"Bearer {TOKEN}") == 200
        for path in ("/performance", "/offers"):
            assert status(base + path, Cookie=f"pgpu_session={TOKEN}") == 200
        process.send_signal(signal.SIGTERM)
        output = process.communicate(timeout=6)[0]
        assert process.returncode == 0, output.decode()
        print("offline process smoke: health, readiness, auth, performance API/pages, cross-origin guard, SIGTERM OK")
    finally:
        if process.poll() is None:
            process.kill()
            process.communicate()
