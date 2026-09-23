#!/usr/bin/env python3
"""Offline DOM regression test with an installed Chromium/Chrome; no npm or keys."""
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
CHROME = next((p for name in ("chromium", "google-chrome", "google-chrome-stable")
               if (p := shutil.which(name))), None)
if not CHROME:
    raise SystemExit("Install Chromium/Chrome to run the optional Free-Router UI smoke test")

catalog = json.loads((ROOT / "crates/router/src/free_router/catalog.json").read_text())
provider = dict(id="groq", enabled=False, base_url="https://api.groq.com/openai/v1",
                api_key="", api_key_env="GROQ_API_KEY", model="test-model", requests_per_minute=30,
                requests_per_hour=None, requests_per_day=None, requests_per_month=None,
                tokens_per_minute=None, tokens_per_day=None, min_interval_ms=0,
                safety_buffer_requests=None, max_output_tokens=1024)
initial = dict(config=dict(use_when_all_offline=False, jumper=False, safety_buffer_requests=5,
                           providers=[provider, {**provider, "id": "second"}]),
               revision="initial", statuses=[], catalog=catalog)
markup = (ROOT / "crates/router/templates/free_router.html").read_text()
markup = markup.replace('<script src="/static/free-router.js" defer></script>', "")
script = (ROOT / "crates/router/static/free-router.js").read_text()
mock = f"window.fixture = {json.dumps(initial)};" + """
window.saved = [];
window.fetch = async (url, options = {}) => {
  if (url !== '/api/v1/free-router') throw new Error('Unexpected network request');
  if (options.method === 'PUT') {
    const input = JSON.parse(options.body);
    saved.push(input);
    fixture.config = input.config;
    fixture.revision += '-saved';
    return {ok: true, status: 200, json: async () => ({ok: true})};
  }
  return {ok: true, status: 200, json: async () => {
    const view = structuredClone(fixture);
    for (const provider of view.config.providers) provider.api_key = '';
    return view;
  }};
};
window.confirm = () => true;
"""
checks = """
setTimeout(async () => {
  const result = document.getElementById('free-router-ui-result');
  const assert = (ok, reason) => { if (!ok) throw new Error(reason); };
  try {
    const form = document.getElementById('free-router-form');
    const rows = document.getElementById('free-router-providers');
    assert(rows.children.length === 2, 'initial providers');
    // There are now THREE fields named safety_buffer_requests. The global
    // value must not accidentally be read from form.elements as a RadioNodeList.
    assert(form.querySelectorAll('[name=safety_buffer_requests]').length === 3, 'duplicate names fixture');
    form.querySelector('[name=jumper]').checked = true;
    form.querySelector('[name=use_when_all_offline]').checked = true;
    rows.children[0].querySelector('[name=model]').value = '<img src=x onerror=alert(1)>';
    rows.children[1].querySelector('[name=api_key]').value = 'NEW-SECRET';
    rows.children[1].querySelector('[name=clear_key]').checked = true;
    const up = [...rows.children[1].querySelectorAll('button')].find(b => b.textContent.includes('Früher'));
    up.click();
    assert(rows.children[0].querySelector('[name=id]').value === 'second', 'reordering');
    assert(!rows.querySelector('img'), 'unsafe HTML interpolation');
    await form.onsubmit({preventDefault() {}});
    assert(saved.length === 1, 'save request');
    const p = saved[0];
    assert(p.config.safety_buffer_requests === 5, 'global reserve changed silently');
    assert(p.config.jumper && p.config.use_when_all_offline, 'switches lost');
    assert(p.config.providers[0].id === 'second', 'saved order');
    assert(p.config.providers[0].api_key === 'NEW-SECRET', 'key lost on reorder');
    assert(p.config.providers[0].safety_buffer_requests === null, 'optional reserve');
    assert(!('_clearKey' in p.config.providers[0]), 'UI-only properties sent to strict schema');
    assert(p.clear_keys[0] === 'second', 'clear-key selection lost');
    assert(rows.children[0].querySelector('[name=api_key]').value === '', 'stored key shown after reload');
    const picker = document.getElementById('free-router-catalog');
    picker.value = 'replicate'; picker.onchange();
    assert(document.getElementById('free-router-add').disabled, 'native API falsely offered as compatible');
    picker.value = 'openrouter'; picker.onchange();
    document.getElementById('free-router-add').click();
    const added = rows.lastElementChild;
    assert(!added.querySelector('[name=enabled]').checked, 'preset automatically enabled');
    assert(added.querySelector('[name=requests_per_day]').value === '50', 'preset quota');
    await form.onsubmit({preventDefault() {}});
    assert(saved[1].config.safety_buffer_requests === 5, 'reserve after adding provider');
    result.textContent = 'PASS';
  } catch (error) { result.textContent = 'FAIL: ' + error.message; }
}, 100);
"""
with tempfile.TemporaryDirectory(prefix="pgpu-free-router-ui-") as directory:
    fixture = Path(directory) / "fixture.html"
    fixture.write_text(f'<!doctype html><meta charset="utf-8">{markup}'
                       '<pre id="free-router-ui-result">PENDING</pre>'
                       f'<script>{mock}</script><script>{script}</script><script>{checks}</script>')
    result = subprocess.run([CHROME, "--headless", "--disable-gpu", "--no-first-run",
                             "--disable-background-networking", "--disable-extensions",
                             f"--user-data-dir={directory}/profile", "--virtual-time-budget=2000",
                             "--dump-dom", fixture.as_uri()], capture_output=True, text=True, timeout=30)
    if result.returncode or 'id="free-router-ui-result">PASS</pre>' not in result.stdout:
        raise SystemExit(f"UI smoke failed:\n{result.stdout}\n{result.stderr}")
print("Free-Router UI smoke: reserve, switches, reorder, write-only key input, preset safeguards OK")
