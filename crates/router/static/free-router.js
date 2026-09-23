/* Write-only API keys; DOM APIs only, never interpolate submitted HTML. */
(() => {
  const form = document.getElementById('free-router-form');
  if (!form) return;
  const rows = document.getElementById('free-router-providers');
  const picker = document.getElementById('free-router-catalog');
  const message = document.getElementById('free-router-status');
  const note = document.getElementById('free-router-catalog-note');
  let config, revision, catalog = [], statuses = [], busy = false;
  // Provider rows reuse these field names; do not read a RadioNodeList from
  // form.elements (that would silently turn the global reserve into zero).
  const field = name => form.querySelector(`:scope > p input[name="${name}"]`);
  const text = (tag, value, parent) => {
    const node = document.createElement(tag);
    node.textContent = value;
    if (parent) parent.append(node);
    return node;
  };
  async function request(options) {
    const r = await fetch('/api/v1/free-router', {credentials: 'same-origin', cache: 'no-store', ...options});
    if (r.status === 401) throw new Error('Session abgelaufen. Bitte erneut anmelden.');
    const data = await r.json();
    if (!r.ok) throw new Error(data.error || 'Free-Router nicht erreichbar');
    return data;
  }
  function input(parent, label, name, value, type = 'text', optional = false) {
    const wrap = text('label', label, parent);
    const control = document.createElement('input');
    control.name = name;
    control.type = type;
    if (type === 'checkbox') control.checked = !!value;
    else control.value = value ?? '';
    if (type === 'number') { control.min = '0'; control.step = '1'; }
    if (type === 'password') { control.autocomplete = 'new-password'; control.placeholder = 'Leer = gespeichert beibehalten'; }
    if (optional) control.placeholder = 'Nicht gesetzt';
    if (type !== 'checkbox') {
      control.style.cssText = 'width:100%;box-sizing:border-box;background:#0d1116;color:var(--fg);border:1px solid var(--line);padding:6px';
    }
    wrap.style.cssText = 'display:block;margin:8px 0';
    wrap.append(control);
    return control;
  }
  const numbers = [
    ['requests_per_minute', 'Requests / Minute (Pflicht)', false],
    ['max_output_tokens', 'Max. Ausgabe-Tokens je Request', false],
    ['requests_per_hour', 'Requests / Stunde', true],
    ['requests_per_day', 'Requests / 24 Stunden', true],
    ['requests_per_month', 'Requests / 31 Tage', true],
    ['tokens_per_minute', 'Tokens / Minute', true],
    ['tokens_per_day', 'Tokens / 24 Stunden', true],
    ['min_interval_ms', 'Mindestabstand / ms (z. B. 1000 bei 1 RPS)', false],
    ['safety_buffer_requests', 'Eigene Reserve (leer = globale Reserve)', true],
  ];
  function collect() {
    config.use_when_all_offline = field('use_when_all_offline').checked;
    config.jumper = field('jumper').checked;
    config.safety_buffer_requests = Number(field('safety_buffer_requests').value);
    config.providers = [...rows.children].map(row => {
      const get = name => row.querySelector(`[name="${name}"]`);
      const p = {};
      for (const name of ['id', 'base_url', 'api_key', 'api_key_env', 'model']) p[name] = get(name).value.trim();
      p.enabled = get('enabled').checked;
      for (const [name, , optional] of numbers) p[name] = optional && !get(name).value ? null : Number(get(name).value);
      p._clearKey = get('clear_key').checked;
      return p;
    });
  }
  function button(parent, label, click, disabled = false) {
    const b = text('button', label, parent);
    b.type = 'button'; b.disabled = disabled; b.onclick = click;
  }
  function showStatuses() {
    [...rows.children].forEach((row, index) => {
      const id = row.querySelector('[name="id"]').value;
      const s = statuses.find(s => s.id === id);
      const target = row.querySelector('[data-quota]');
      if (!s) { target.textContent = `${index + 1}. Noch nicht gespeichert`; return; }
      const q = s.quota;
      target.textContent = `${index + 1}. ${s.enabled ? 'Aktiv' : 'Aus'} · Key ${s.key_configured ? 'vorhanden' : 'fehlt'} · ` +
        (!q ? 'Quota-Ledger nicht verfügbar' : `nutzbare Requests: ${q.remaining_requests} · ${q.retry_after_s ? `Wartezeit ${q.retry_after_s}s (${q.reason})` : 'bereit (Tokenprüfung je Request)'}`);
    });
  }
  function render() {
    rows.replaceChildren();
    config.providers.forEach((p, index) => {
      const card = document.createElement('div'); card.className = 'card'; rows.append(card);
      const status = text('p', '', card); status.dataset.quota = '';
      input(card, ' Aktiviert ', 'enabled', p.enabled, 'checkbox');
      const grid = document.createElement('div'); grid.className = 'grid'; card.append(grid);
      for (const [name, label, type] of [
        ['id', 'Eindeutige ID', 'text'], ['base_url', 'OpenAI-Base-URL (inkl. /v1, ohne /chat/completions)', 'url'],
        ['model', 'Exakte kostenlose Modell-ID', 'text'], ['api_key', 'API-Key (neu/ersetzen)', 'password'],
        ['api_key_env', 'Alternativ: Env-Variablenname (hat Vorrang)', 'text'],
      ]) input(grid, label, name, p[name], type);
      input(card, ' Gespeicherten Key löschen ', 'clear_key', p._clearKey, 'checkbox');
      const limits = document.createElement('div'); limits.className = 'grid'; card.append(limits);
      for (const [name, label, optional] of numbers) input(limits, label, name, p[name], 'number', optional);
      const move = delta => { collect(); const other = index + delta; [config.providers[index], config.providers[other]] = [config.providers[other], config.providers[index]]; render(); };
      button(card, '↑ Früher', () => move(-1), index === 0);
      button(card, '↓ Später', () => move(1), index === config.providers.length - 1);
      button(card, 'Entfernen', () => { collect(); config.providers.splice(index, 1); render(); });
    });
    showStatuses();
  }
  function showCatalogNote() {
    const p = catalog.find(p => p.id === picker.value);
    note.replaceChildren();
    if (!p) { note.textContent = 'Eigener OpenAI-kompatibler Anbieter/Adapter. Modell und Limits selbst eintragen.'; return; }
    text('span', p.note + ' ', note);
    const a = text('a', 'API-Key / Account öffnen', note); a.href = p.key_url; a.target = '_blank'; a.rel = 'noopener noreferrer';
    document.getElementById('free-router-add').disabled = p.tier === 'adapter';
  }
  async function load() {
    const data = await request();
    config = data.config; revision = data.revision; statuses = data.statuses; catalog = data.catalog;
    for (const name of ['use_when_all_offline', 'jumper']) field(name).checked = config[name];
    field('safety_buffer_requests').value = config.safety_buffer_requests;
    picker.replaceChildren();
    for (const p of [...catalog, {id: 'custom', name: 'Eigener Anbieter', tier: 'custom'}]) {
      const option = text('option', `${p.name} [${p.tier}]`, picker); option.value = p.id;
    }
    render(); showCatalogNote(); message.textContent = 'Gespeicherte Einstellungen geladen.';
  }
  picker.onchange = () => { document.getElementById('free-router-add').disabled = false; showCatalogNote(); };
  document.getElementById('free-router-add').onclick = () => {
    if (!config) return;
    collect();
    const preset = catalog.find(p => p.id === picker.value) || {id: 'custom'};
    let id = preset.id, suffix = 2;
    while (config.providers.some(p => p.id === id)) id = `${preset.id}-${suffix++}`;
    config.providers.push({id, enabled: false, base_url: preset.base_url || 'https://', api_key: '', api_key_env: '', model: '',
      requests_per_minute: preset.rpm || 0, requests_per_hour: preset.rph ?? null, requests_per_day: preset.rpd ?? null,
      requests_per_month: preset.rpmth ?? null, tokens_per_minute: null, tokens_per_day: null,
      min_interval_ms: preset.interval_ms || 0, safety_buffer_requests: null, max_output_tokens: 1024});
    render(); message.textContent = 'Anbieter hinzugefügt, noch nicht gespeichert. Modell, Key und Limits prüfen; dann aktivieren.';
  };
  form.onsubmit = async event => {
    event.preventDefault(); if (!config || busy) return;
    busy = true;
    try {
      collect();
      const clear_keys = config.providers.filter(p => p._clearKey).map(p => p.id);
      const clean = {...config, providers: config.providers.map(({_clearKey, ...p}) => p)};
      await request({method: 'PUT', headers: {'content-type': 'application/json'}, body: JSON.stringify({config: clean, revision, clear_keys})});
      await load(); message.textContent = 'Free-Router gespeichert und live angewendet. Kontingentzähler wurden nicht zurückgesetzt.';
    } catch (error) { message.textContent = error.message; }
    finally { busy = false; }
  };
  document.getElementById('free-router-reload').onclick = () => {
    if (confirm('Ungespeicherte Änderungen verwerfen?')) load().catch(e => { message.textContent = e.message; });
  };
  load().catch(e => { message.textContent = e.message; });
  setInterval(async () => {
    if (!config || busy || document.hidden) return;
    try { statuses = (await request()).statuses; showStatuses(); }
    catch (e) { message.textContent = e.message; }
  }, 5000);
})();
