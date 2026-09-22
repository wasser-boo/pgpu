/* Local-only map preview. Country selection itself is calculated by the server. */
(() => {
  'use strict';
  const NS = 'http://www.w3.org/2000/svg';
  const R = 6371.0088;
  const wrap = lon => ((lon + 180) % 360 + 360) % 360 - 180;
  const codes = value => value.split(/[\s,]+/).filter(Boolean).map(v => v.toUpperCase());
  function circle(origin, radius) {
    if (radius === null || radius >= Math.PI * R) return '';
    const [lon, lat] = origin.map(v => v * Math.PI / 180);
    const angle = radius / R;
    let path = '', previous;
    for (let i = 0; i <= 180; i++) {
      const bearing = i * Math.PI / 90;
      const y = Math.asin(Math.max(-1, Math.min(1,
        Math.sin(lat) * Math.cos(angle) + Math.cos(lat) * Math.sin(angle) * Math.cos(bearing))));
      const x = wrap((lon + Math.atan2(Math.sin(bearing) * Math.sin(angle) * Math.cos(lat),
        Math.cos(angle) - Math.sin(lat) * Math.sin(y))) * 180 / Math.PI);
      path += (previous === undefined || Math.abs(previous - x) > 180 ? 'M' : 'L') + `${x},${-y * 180 / Math.PI} `;
      previous = x;
    }
    return path;
  }
  if (typeof document === 'undefined') { module.exports = { circle, codes, wrap }; return; }
  const element = (tag, attrs) => {
    const e = document.createElementNS(NS, tag);
    for (const [key, value] of Object.entries(attrs)) e.setAttribute(key, value);
    return e;
  };
  async function initialize() {
    const response = await fetch('/static/countries.json', { credentials: 'same-origin' });
    if (!response.ok) throw new Error('Kartendaten konnten nicht geladen werden');
    const countries = await response.json();
    const outlines = countries.map(c => ({ ...c, path: c.polygons.flatMap(p => p.map(r =>
      r.map(([x, y], i) => (i ? 'L' : 'M') + `${x},${-y}`).join(' ') + 'Z')).join(' ') }));
    for (const form of document.querySelectorAll('form.selection')) {
      const input = name => form.elements.namedItem(name);
      const svg = form.querySelector('svg');
      const status = form.querySelector('.selection-status');
      const select = input('origin_country');
      const selected = select.dataset.selected.toUpperCase();
      select.replaceChildren();
      for (const c of [...countries].sort((a,b) => a.name.localeCompare(b.name))) {
        const option = document.createElement('option'); option.value = c.code;
        option.textContent = `${c.code === 'DE' ? 'Deutschland' : c.name} (${c.code})`;
        option.selected = c.code === selected; select.append(option);
      }
      let timer, controller, sequence = 0, world = false, preview;
      function payload() {
        const numeric = name => input(name).value.trim() === '' ? null : Number(input(name).value);
        return { slot_id: Number(form.dataset.slot), gpu_names: input('gpu_names').value.split(/\r?\n/).map(v => v.trim()).filter(Boolean),
          location: { countries: codes(input('countries').value), excluded_countries: codes(input('excluded_countries').value),
            origin_country: select.value, radius_km: numeric('radius_km'), latitude: numeric('latitude'), longitude: numeric('longitude') } };
      }
      function draw() {
        if (!preview) return;
        svg.replaceChildren();
        const origin = preview.origin;
        svg.setAttribute('viewBox', world ? '-180 -90 360 180' : `${origin[0]-30} ${-origin[1]-20} 60 40`);
        const all = countries.map(c => c.code);
        const allowed = new Set([...(preview.bid_countries || all), ...(preview.on_demand_countries || all)]);
        const excluded = new Set(codes(input('excluded_countries').value));
        for (const c of outlines) {
          const p = element('path', { d: c.path, fill: allowed.has(c.code) ? '#2d7753' : excluded.has(c.code) ? '#7b3e47' : '#263445',
            stroke: '#a2b0ba', 'stroke-width': '0.4', 'vector-effect': 'non-scaling-stroke', 'fill-rule': 'evenodd' });
          const title = element('title', {}); title.textContent = `${c.name} (${c.code})`; p.append(title); svg.append(p);
          if (allowed.has(c.code)) svg.append(element('circle', { cx:c.center[0], cy:-c.center[1], r:world ? 0.6 : 0.18, fill:'#b8f4c9' }));
        }
        svg.append(element('path', { d:circle(origin,preview.radius_km), fill:'none', stroke:'#6ec9ff', 'stroke-width':'1.8', 'vector-effect':'non-scaling-stroke' }));
        svg.append(element('circle', { cx:origin[0], cy:-origin[1], r:world ? 1.3 : 0.4, fill:'#f5be61' }));
      }
      async function refresh(id) {
        controller = new AbortController();
        try {
          const response = await fetch('/api/v1/selection/preview', { method:'POST', credentials:'same-origin',
            headers:{'Content-Type':'application/json'}, body:JSON.stringify(payload()), signal:controller.signal });
          if (response.status === 401) throw new Error('Bitte neu anmelden');
          const data = await response.json();
          if (!response.ok) throw new Error(data.error || `HTTP ${response.status}`);
          if (id !== sequence) return;
          preview = data; draw();
          const names = list => list === null ? 'alle Länder' : list.join(', ');
          status.textContent = `Vorschau — Interruptible: ${names(data.bid_countries)}. On-demand: ${names(data.on_demand_countries)}. Änderungen werden erst durch Speichern aktiv.`;
        } catch (e) { if (e.name !== 'AbortError' && id === sequence) status.textContent = `Vorschau ungültig: ${e.message}`; }
      }
      function schedule() {
        clearTimeout(timer); controller?.abort();
        const id = ++sequence; status.textContent = 'Vorschau wird aktualisiert …';
        timer = setTimeout(() => refresh(id), 180);
      }
      select.addEventListener('change', () => { input('latitude').value = ''; input('longitude').value = ''; world = false; schedule(); });
      form.addEventListener('input', schedule);
      form.querySelector('[data-country-origin]').addEventListener('click', () => {
        input('latitude').value = ''; input('longitude').value = ''; world = false; schedule();
      });
      form.querySelector('[data-map-world]').addEventListener('click', () => { world = !world; draw(); });
      svg.addEventListener('click', event => {
        const matrix = svg.getScreenCTM(); if (!matrix) return;
        const point = svg.createSVGPoint(); point.x = event.clientX; point.y = event.clientY;
        const coord = point.matrixTransform(matrix.inverse());
        input('longitude').value = wrap(coord.x).toFixed(5);
        input('latitude').value = Math.max(-90, Math.min(90, -coord.y)).toFixed(5);
        schedule();
      });
      schedule();
    }
  }
  initialize().catch(error => {
    for (const status of document.querySelectorAll('.selection-status')) status.textContent = `${error.message}. TOML-Editor weiterhin verfügbar.`;
  });
})();
