#!/usr/bin/env node
// Isolated loopback-only browser test. No real cloud credentials, rentals or notifications.
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const {spawn} = require('node:child_process');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const {circle, codes} = require('../crates/router/static/selection.js');

const listen = server => new Promise(resolve => server.listen(0,'127.0.0.1', () => resolve(server.address().port)));
const delay = ms => new Promise(resolve => setTimeout(resolve,ms));
(async () => {
  assert.deepEqual(codes('de, AT\nCH'), ['DE','AT','CH']);
  for (const origin of [[9.678348,50.961733],[179.9,0],[0,90]]) {
    const d = circle(origin,1000); assert(d.length>100); assert(!/NaN|Infinity/.test(d));
  }
  const root = path.resolve(__dirname,'..');
  const binary = path.resolve(process.argv[2] || path.join(root,'target/debug/praxis-router'));
  const work = await fs.mkdtemp(path.join(os.tmpdir(),'pgpu-settings-'));
  const token = 'offline-browser-test-token-not-a-secret-123456789';
  const deliveries = [];
  const sink = http.createServer(async (req,res) => {
    let raw = ''; for await (const part of req) raw += part;
    deliveries.push({url:req.url, body:JSON.parse(raw)});
    res.writeHead(req.url.startsWith('/good') ? 200 : 404, {'Content-Type':'application/json'});
    res.end(req.url.startsWith('/good') ? '{"id":"fake-message"}' : '{"error":"SECRET-MOCK-ERROR"}');
  });
  const sinkPort = await listen(sink);
  const reserve = http.createServer(); const port = await listen(reserve); await new Promise(r => reserve.close(r));
  const base = `http://127.0.0.1:${port}`;
  const config = path.join(work,'config.toml');
  await fs.writeFile(config, `# KEEP ME: unrelated comments/settings must survive UI saves\n[router]\nbind_ip = "127.0.0.1"\ndashboard_port = ${port}\ndata_dir = "${work}/data"\ntz = "Europe/Berlin"\n[stt]\nmode = "media_slot"\n[budget]\ndaily_soft_eur = 3.0\ndaily_hard_eur = 4.0\nmonthly_eur = 10.0\nusd_per_eur = 1.08\n[alerts]\nwebhook_urls = ["http://127.0.0.1:${sinkPort}/good/SECRET-1", "http://127.0.0.1:${sinkPort}/bad/SECRET-2"]\nwebhook_format = "discord"\n[[slots]]\nid = 1\nrole = "llm"\nname = "Offline browser test"\nsearch_query = 'gpu_ram>=16 geolocation!=FR'\n[slots.env]\nLLAMA_CONTEXT_SIZE = "32768"\n`, {mode:0o600});
  const child = spawn(binary,['--config',config], {cwd:path.join(root,'crates/router'),
    env:{...process.env, ROUTER_TOKEN:token, VAST_API_KEY:'', NB_API_TOKEN:'', RUST_LOG:'warn', HTTP_PROXY:'', HTTPS_PROXY:'', ALL_PROXY:'', NO_PROXY:'127.0.0.1,localhost,::1'}, stdio:['ignore','pipe','pipe']});
  let log = ''; child.stdout.on('data',v => log += v); child.stderr.on('data',v => log += v);
  let browser;
  try {
    let ready = false;
    for (let i=0;i<120;i++) {
      if (child.exitCode !== null) throw new Error(`router exited: ${log}`);
      try { if ((await fetch(`${base}/readyz`)).status===200) { ready=true; break; } } catch {}
      await delay(100);
    }
    assert(ready,`router not ready: ${log}`);
    assert.equal((await fetch(`${base}/api/v1/alerts/test`,{method:'POST'})).status,401);
    browser = await chromium.launch({headless:true,executablePath:process.env.CHROMIUM || '/usr/bin/chromium'});
    const page = await browser.newPage({viewport:{width:1440,height:1000}});
    const errors = []; page.on('pageerror', e => errors.push(e.message));
    await page.route('**/*', route => {
      const host = new URL(route.request().url()).hostname;
      return ['127.0.0.1','localhost'].includes(host) ? route.continue() : route.abort();
    });
    await page.goto(`${base}/settings`);
    assert(page.url().endsWith('/login'));
    await page.locator('input[name=token]').fill(token);
    await Promise.all([page.waitForURL(`${base}/`),page.getByRole('button',{name:'Login',exact:true}).click()]);
    await page.goto(`${base}/settings`);
    const form = page.locator('form.selection');
    await page.waitForFunction(() => document.querySelector('.selection-status')?.textContent.includes('Vorschau —'));
    assert.equal(await form.locator('[name=origin_country]').inputValue(),'DE');
    assert((await form.locator('svg path').count())>230);
    await form.locator('[name=countries]').fill('DE, AT');
    await form.locator('[name=excluded_countries]').fill('DE');
    await form.locator('[name=radius_km]').fill('1000');
    await form.locator('[name=gpu_names]').fill('RTX A4000\nRTX 4060 Ti');
    await page.waitForFunction(() => document.querySelector('.selection-status')?.textContent.includes('Interruptible: AT. On-demand: AT.'));
    const germany = form.locator('svg path').filter({hasText:'Germany (DE)'});
    assert.equal(await germany.getAttribute('fill'),'#7b3e47');
    await Promise.all([page.waitForURL(/settings\?msg=/),form.getByRole('button',{name:'Auswahl speichern & live anwenden'}).click()]);
    const saved = await fs.readFile(config,'utf8');
    assert(saved.includes('# KEEP ME')); assert(saved.includes('daily_soft_eur = 3.0'));
    assert(saved.includes('LLAMA_CONTEXT_SIZE = "32768"')); assert(saved.includes('geolocation!=FR'));
    assert(saved.includes('RTX A4000')); assert.equal((await fs.stat(config)).mode & 0o077,0);
    // Real browser POST + cookie/origin auth, but only to two loopback mock sinks.
    await page.getByRole('button',{name:'Testnachricht senden',exact:true}).click();
    await page.waitForFunction(() => document.body?.textContent.includes('Webhook-Test mit Fehlern'));
    assert((await page.locator('body').innerText()).includes('1 von 2 Webhooks angenommen'));
    assert((await page.locator('body').innerText()).includes('HTTP 404'));
    assert.equal(deliveries.length,2);
    for (const delivery of deliveries) {
      assert(delivery.url.endsWith('?wait=true'));
      assert(delivery.body.content.includes('✅ PGPU-Testnachricht'));
      assert.deepEqual(delivery.body.allowed_mentions.parse,[]);
    }
    const auth = {Authorization:`Bearer ${token}`};
    const events = await (await fetch(`${base}/api/v1/events`,{headers:auth})).text();
    assert(!events.includes('SECRET-'));
    const state = await (await fetch(`${base}/api/v1/state`,{headers:auth})).json();
    assert.equal(state.slots.flatMap(slot => slot.instances).length,0);
    const budget = await (await fetch(`${base}/api/v1/budget`,{headers:auth})).json();
    assert.equal(budget.spent_today_usd,0); assert.equal(budget.soft_eur,3); assert.equal(budget.hard_eur,4);
    assert.deepEqual(budget.vast_usage.scope,['praxis-llm-s1-<token8>']);
    assert.equal((await fetch(`${base}/api/v1/alerts/test`,{method:'POST',headers:auth})).status,429);
    await page.waitForFunction(() => document.querySelector('.selection-status')?.textContent.includes('Interruptible: AT. On-demand: AT.'));
    assert.deepEqual(errors,[]);
    if (process.env.SCREENSHOT_DIR) {
      await fs.mkdir(process.env.SCREENSHOT_DIR,{recursive:true});
      await page.screenshot({path:path.join(process.env.SCREENSHOT_DIR,'settings.png'),fullPage:true});
    }
    console.log('PASS: browser login, country map/preview/exclusion, GPU alternatives, safe config save, Discord fanout/partial failure, billing scope, auth and cooldown; zero GPUs/rentals');
  } finally {
    if (browser) await browser.close();
    if (child.exitCode === null && child.signalCode === null) {
      child.kill('SIGTERM');
      await Promise.race([new Promise(r => child.once('exit',r)),delay(10000)]);
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
    }
    await new Promise(r => sink.close(r));
    await fs.rm(work,{recursive:true,force:true});
  }
})().catch(error => { console.error(error); process.exitCode=1; });
