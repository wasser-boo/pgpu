"""Provisioniert Vosk-Modelle (de/ja) und startet dann den Server.

Idempotent: bereits vorhandene Modelle werden übersprungen. Alphacephei-URLs
sind gepinnt (keine Latest-Downloads, kein Egress über Dritte).
"""
import os
import subprocess
import sys
from pathlib import Path
import zipfile

MODELS = {
    "de": ("vosk-model-de-0.21", "https://alphacephei.com/vosk/models/vosk-model-de-0.21.zip"),
    "ja": ("vosk-model-ja-0.22", "https://alphacephei.com/vosk/models/vosk-model-ja-0.22.zip"),
    "fr": ("vosk-model-fr-0.22", "https://alphacephei.com/vosk/models/vosk-model-fr-0.22.zip"),
}
SMALL = {
    "de": ("vosk-model-small-de-0.15", "https://alphacephei.com/vosk/models/vosk-model-small-de-0.15.zip"),
}

root = Path(os.environ.get("VOSK_MODEL_ROOT", "/data/vosk"))
languages = [x.strip() for x in os.environ.get("STT_LANGUAGES", "de,ja").split(",") if x.strip()]
use_small = os.environ.get("STT_SMALL_MODELS", "") == "1"
root.mkdir(parents=True, exist_ok=True)

for lang in languages:
    if lang not in MODELS:
        print(f"unknown language {lang}, skipping", file=sys.stderr)
        continue
    name, url = (SMALL.get(lang) if use_small else None) or MODELS[lang]
    target = root / lang
    marker = target / ".complete"
    if marker.is_file():
        print(f"model {lang} already provisioned")
        continue
    print(f"provisioning {name} ...")
    archive = root / f"{name}.zip"
    # -c: Resume nach Abbruch (1,4-GB-Download über NAS-Egress);
    # --tries/--timeout/--waitretry: Flaky-Firewall nicht sofort fatal —
    # docker restartet den Container ohnehin, -c macht dann weiter.
    subprocess.run(
        ["wget", "-q", "-c", "--tries=10", "--timeout=45", "--waitretry=5", "-O", str(archive), url],
        check=True,
    )
    with zipfile.ZipFile(archive) as zf:
        zf.extractall(root / ".tmp")
    extracted = next((root / ".tmp").iterdir())
    if target.exists():
        import shutil

        shutil.rmtree(target)
    extracted.rename(target)
    (root / ".tmp").rmdir()
    archive.unlink()
    marker.write_text(name)
    print(f"model {lang} provisioned ({name})")

os.execv(sys.executable, [sys.executable, "/app/vosk_server.py"])