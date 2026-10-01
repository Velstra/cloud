// Production React UI against the existing REST contract fixture. No cloud account required.
import { createHash } from 'node:crypto';
import { spawn } from 'node:child_process';
import { createServer, request } from 'node:http';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { browser } from '../../velstra-cloud-console/tests/console/harness.mjs';
const root = fileURLToPath(new URL('../', import.meta.url));
const fake = spawn(process.execPath, [fileURLToPath(new URL('../../velstra-cloud-console/tests/console/fake-api.mjs', import.meta.url))], { env: { ...process.env, FAKE_PORT: '0' }, stdio: ['ignore', 'pipe', 'inherit'] });
const fakePort = await new Promise((resolve, reject) => { let text = ''; fake.stdout.on('data', (data) => { text += data; const m = /listening (\d+)/.exec(text); if (m) resolve(Number(m[1])); }); fake.on('exit', () => reject(new Error('Fixture exited'))); });
let viewer = false, projectAdmin = false, projectAdminDowngraded = false, customOperator = false, volumeOnly = false, migrationError = false, unauthorizedLists = [], refuseSession = false, refuseDetail = false, refuseAudit = false, creates = 0, consoleMessages = 0, projectLists = 0;
const streams = new Set();
const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://localhost').pathname;
  if (path.startsWith('/api/')) {
    if (req.method === 'GET' && path === '/api/v1/projects') {
      projectLists++;
      if (projectLists === 1) await new Promise((r) => setTimeout(r, 200));
    }
    if (req.method === 'GET' && path === '/api/v1/images') { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({items:[{meta:{name:'images/catalogue-boot'},spec:{family:'TestOS',version:'1',format:'Raw'}}]})); return; }
    if (req.method === 'GET' && path === '/api/v1/flavors') { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({items:[{meta:{name:'flavors/test-size'},spec:{vcpus:2,memoryMib:4096,rootDiskGib:10}}]})); return; }
    if (req.method === 'GET' && path === '/api/v1/flavors/test-size') { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({meta:{name:'flavors/test-size'},spec:{vcpus:2,memoryMib:4096,rootDiskGib:10}})); return; }
    if (migrationError && req.method === 'GET' && path === '/api/v1/projects/p1/migrations') { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({items:[{meta:{name:'projects/p1/migrations/blocked-transfer',generation:1,createdAt:Date.now()},spec:{instance:'projects/p1/instances/web-1',fromNode:'node-a',toNode:'node-b',mode:'Live'},status:{observedGeneration:0,conditions:[{kind:'Moved',status:'Unknown',reason:'DestinationError',message:'The destination cannot prepare a receiver',observedGeneration:1}]}}]})); return; }
    if (req.method === 'POST' && path.endsWith(':console')) { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({ session: 'test-session', ticket: 'one-time-fixture', readOnly: viewer, encrypted: true })); return; }
    if (refuseSession && req.method === 'GET' && path === '/api/v1/sessions/current') { res.writeHead(503, { 'content-type': 'application/json' }); res.end(JSON.stringify({ error: { message: 'Session backend unavailable' } })); return; }
    if (viewer && path === '/api/v1/sessions/current') { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({ subject: 'viewer', cellAdmin: false, projects: { p1: 'viewer' } })); return; }
    if (projectAdmin && path === '/api/v1/sessions/current') { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({ subject: 'project-admin', cellAdmin: false, projects: { p1: projectAdminDowngraded ? 'viewer' : 'admin' }, capabilities: { p1: { projects: projectAdminDowngraded ? ['read'] : ['read', 'operate', 'write', 'administer'] } } })); return; }
    if (customOperator && path === '/api/v1/sessions/current') { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({ subject: 'custom-operator', cellAdmin: false, projects: { p1: 'roles/volume-writer' }, capabilities: { p1: volumeOnly ? { volumes: ['read', 'write'] } : { instances: ['read', 'operate'], volumes: ['read', 'write'] } } })); return; }
    if (req.method === 'POST' && path.endsWith('/networks')) creates++;
    if (volumeOnly && req.method === 'GET' && (/^\/api\/v1\/projects\/p1\/(instances|networks|snapshots)$/.test(path) || path === '/api/v1/projects/p1:explainQuota')) unauthorizedLists.push(req.url);
    if (volumeOnly && req.method === 'GET' && path === '/api/v1/users') { res.writeHead(403, { 'content-type': 'application/json' }); res.end(JSON.stringify({ error: { message: 'Operator access required' } })); return; }
    if (projectAdmin && req.method === 'PATCH' && path === '/api/v1/projects/p1') projectAdminDowngraded = true;
    if (refuseDetail && req.method === 'GET' && path.endsWith('/networks/browser-network')) { res.writeHead(503, { 'content-type': 'application/json' }); res.end(JSON.stringify({ error: { message: 'Temporarily unavailable' } })); return; }
    if (refuseAudit && req.method === 'GET' && path === '/api/v1/audit') { res.writeHead(503, { 'content-type': 'application/json' }); res.end(JSON.stringify({ error: { message: 'Audit temporarily unavailable' } })); return; }
    const upstream = request({ hostname: '127.0.0.1', port: fakePort, path: req.url, method: req.method, headers: req.headers }, (answer) => { res.writeHead(answer.statusCode, answer.headers); answer.pipe(res); });
    upstream.on('error', () => { res.writeHead(502); res.end(); }); req.pipe(upstream); res.on('close', () => upstream.destroy()); return;
  }
  try { const name = path.startsWith('/assets/') ? path : '/index.html'; const data = await readFile(root + 'dist' + name); res.setHeader('content-type', name.endsWith('.js') ? 'text/javascript' : name.endsWith('.css') ? 'text/css' : name.endsWith('.woff2') ? 'font/woff2' : 'text/html'); res.end(data); } catch { res.writeHead(404); res.end(); }
});
server.on('upgrade', (req, socket) => {
  const accept = createHash('sha1').update(req.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
  streams.add(socket); socket.on('close', () => streams.delete(socket)); socket.on('error', () => {});
  const text = Buffer.from('Binary console ready\r\n');
  socket.write(Buffer.concat([Buffer.from([0x82, text.length]), text]));
  socket.on('data', (data) => { if ((data[0] & 15) === 1) consoleMessages++; if ((data[0] & 15) === 8) socket.end(Buffer.from([0x88, 0])); });
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const url = `http://127.0.0.1:${server.address().port}/`;
const pages = [];
const wait = async (b, expression) => { for (let i = 0; i < 100; i++) { if (await b.evaluate(expression)) return; await new Promise(r => setTimeout(r, 100)); } throw new Error(`Timed out: ${expression}\n${await b.evaluate("document.body.innerText")}`); };
const click = (b, text) => b.evaluate(`[...document.querySelectorAll('button')].find(x=>x.innerText===${JSON.stringify(text)}).click()`);
const fill = (b, selector, value) => b.evaluate(`(()=>{const e=document.querySelector(${JSON.stringify(selector)});const proto=e.tagName==='SELECT'?HTMLSelectElement.prototype:HTMLInputElement.prototype;Object.getOwnPropertyDescriptor(proto,'value').set.call(e,${JSON.stringify(value)});e.dispatchEvent(new Event(e.tagName==='SELECT'?'change':'input',{bubbles:true}));})()`);
const login = async (b) => { await b.goto(url); await fill(b, '#u', 'operator'); await fill(b, '#p', 'a test operator passphrase'); await click(b, 'Sign in'); await wait(b, '!!document.querySelector("#rail")'); };
try {
  const b = await browser({width: 1440, height: 1000}); pages.push(b); await login(b);
  await wait(b, 'document.body.innerText.includes("Cloud overview")');
  await wait(b, '!document.body.innerText.includes("Checking resources")');
  assert.ok(projectLists <= 3, `concurrent first inventory reads must share their project lookup (got ${projectLists})`);
  assert.ok(await b.evaluate(`(()=>{const networks=document.querySelector('#rail a[href="#/c/networks"] svg');const subnets=document.querySelector('#rail a[href="#/c/subnets"] svg');return !!networks && !!subnets && networks.innerHTML!==subnets.innerHTML})()`), 'collections in one section use distinct icons');
  await b.goto(url + '#/c/volumes/new');
  await wait(b, '!!document.querySelector("form")');
  assert.equal(await b.evaluate(`document.querySelector('form').innerText.includes('Pool')`), true, 'cell admin may choose a volume pool');
  migrationError = true;
  await b.goto(url + '#/c/migrations');
  await wait(b, 'document.body.innerText.includes("blocked-transfer")');
  assert.match(await b.evaluate('document.body.innerText'), /Retrying/, 'a receiver error must not remain Creating before its first report');
  migrationError = false;
  await fill(b, '[aria-label="Filter navigation"]', 'ceph');
  await b.evaluate('document.querySelector("[aria-label=\\"Clear navigation filter\\"]").click()');
  assert.equal(await b.evaluate('document.activeElement.getAttribute("aria-label")'), 'Filter navigation');
  await b.goto(url + '#/c/networks/new');
  await wait(b, '!!document.querySelector("form select option[value=p1]")');
  await fill(b, 'form select', 'p1'); await fill(b, 'input[placeholder="network-1"]', 'browser-network');
  await b.evaluate(`(()=>{const button=[...document.querySelectorAll('form button')].find(x=>x.innerText==='Create'); button.click();button.click();})()`);
  await wait(b, '!!document.querySelector("h2")?.textContent.includes("browser-network") && !document.querySelector("form")');
  assert.equal(creates, 1, 'double activation must create once');
  assert.match(await b.evaluate('decodeURIComponent(location.hash)'), /p1\/browser-network/, 'all-project creation retains project');
  await wait(b, `[...document.querySelectorAll("button")].some(x=>x.innerText==="Edit")`);
  refuseDetail = true;
  await b.evaluate(`[...document.querySelectorAll('button')].filter(x=>x.innerText==='Refresh').at(-1).click()`);
  await wait(b, 'document.body.innerText.includes("Displaying the last known state")');
  refuseDetail = false;
  await b.evaluate(`[...document.querySelectorAll('button')].filter(x=>x.innerText==='Refresh').at(-1).click()`);
  await wait(b, '!document.body.innerText.includes("Displaying the last known state")');
  await click(b, 'Edit'); await wait(b, '!!document.querySelector("form")');
  const reported = await fetch(`http://127.0.0.1:${fakePort}/__test/converge?name=projects/p1/networks/browser-network`, {method:'POST'});
  assert.ok(reported.ok);
  await click(b, 'Save'); await wait(b, '!location.hash.endsWith("/edit")');
  await click(b, 'Edit'); await wait(b, '!!document.querySelector("form")');
  const changed = await fetch(`http://127.0.0.1:${fakePort}/api/v1/projects/p1/networks/browser-network`, {method:'PATCH', headers:{authorization:'Bearer testtoken','content-type':'application/json'}, body:JSON.stringify({spec:{mtu:1400}})});
  assert.ok(changed.ok);
  await click(b, 'Save'); await wait(b, 'document.body.innerText.includes("changed while you were editing")');
  await b.goto(url + '#/c/networks'); await wait(b, '!!document.querySelector("tbody tr")');
  await b.evaluate(`[...document.querySelectorAll('button')].find(x=>x.innerText==='Refresh').focus()`);
  await b.key('Enter');
  await new Promise(r => setTimeout(r, 200));
  assert.equal(await b.evaluate('location.hash'), '#/c/networks', 'Enter on a focused button must not open the highlighted row');
  await b.goto(url + '#/c/subnets/new'); await wait(b, '!!document.querySelector("form")');
  await fill(b, 'form select', 'p1');
  await click(b, 'Create');
  await wait(b, '!!document.querySelector("form [role=alert]")');
  assert.equal(await b.evaluate('document.activeElement.getAttribute("placeholder")'), 'subnet-1', 'focus missing name');
  await b.goto(url + '#/c/volumes/new'); await wait(b, '!!document.querySelector("form")');
  await fill(b, 'form select', 'p1');
  await wait(b, `!document.querySelector('[aria-label="From image"]')?.innerText.includes("Loading")`);
  await b.evaluate(`document.querySelector('[aria-label="From image"]').click()`);
  await wait(b, 'document.body.innerText.includes("TestOS · 1 · catalogue")');
  await b.key('Escape');
  await b.goto(url + '#/c/instances/new'); await wait(b, '!!document.querySelector("form")');
  await b.evaluate(`document.querySelector('[aria-label="Flavor"]').click()`);
  await wait(b, `[...document.querySelectorAll('[role=option]')].some(e=>e.innerText==='test-size')`);
  await b.evaluate(`[...document.querySelectorAll('[role=option]')].find(e=>e.innerText==='test-size').click()`);
  await b.evaluate(`[...document.querySelectorAll('button')].find(e=>e.innerText.includes('advanced settings')).click()`);
  await wait(b, `document.querySelectorAll('form input[type=number]')[2]?.value === '10'`);
  await fill(b, 'form input[type=number]', '1');
  await wait(b, `!document.querySelector('[aria-label="Flavor"]').innerText.includes('test-size')`);
  assert.equal(await b.evaluate(`document.querySelector('form input[type=number]').value`), '1', 'custom size survives clearing the flavor');
  await b.goto(url + '#/c/projects/p1/edit');
  await wait(b, '!!document.querySelector("form")');
  await click(b, 'Save');
  await wait(b, '!location.hash.endsWith("/edit")');
  await b.goto(url + '#/c/instances/p1%2Fweb-1/edit');
  await wait(b, '!!document.querySelector("form")');
  await click(b, 'Save'); await wait(b, '!location.hash.endsWith("/edit")');
  await b.goto(url + '#/c/instances/p1%2Fweb-1');
  await wait(b, `[...document.querySelectorAll('button')].some(x=>x.innerText==='Attach')`);
  await click(b, 'Attach');
  await wait(b, `document.querySelector('.xterm-rows')?.innerText.includes('Binary console ready')`);
  await b.type('x'); await new Promise(r=>setTimeout(r,150));
  assert.equal(consoleMessages, 1, 'console input reaches the stream once');
  await click(b, 'Detach'); await wait(b, `document.body.innerText.includes('Attach again')`);
  await click(b, 'Attach again'); await wait(b, `document.body.innerText.includes('● attached')`);
  await b.type('y'); await new Promise(r=>setTimeout(r,150));
  assert.equal(consoleMessages, 2, 'reattaching must dispose the previous input subscription');
  await b.goto(url + '#/c/instances/p1%2Fdb-1');
  await wait(b, 'document.body.innerText.includes("db-1") && document.body.innerText.includes("Access")');
  assert.equal(await b.evaluate(`[...document.querySelectorAll('button')].some(x=>x.innerText==='Attach')`), false, 'an unplaced instance must not offer a live console session');
  assert.ok(await b.evaluate(`document.body.innerText.includes('Console available when running')`), 'console availability must be explained in user terms');
  await b.goto(url + '#/c/projects/p1');
  await b.emulate({'prefers-reduced-motion':'reduce'});
  assert.ok(Number.parseFloat(await b.evaluate('getComputedStyle(document.querySelector(".arrive-up") || document.querySelector(".arrive-right")).animationDuration')) < 0.01);
  refuseAudit = true;
  await b.goto(url + '#/c/instances/p1%2Fweb-1');
  await wait(b, `!![...document.querySelectorAll('summary')].find(x=>x.textContent.trim()==='Activity history')`);
  await b.evaluate(`[...document.querySelectorAll('summary')].find(x=>x.textContent.trim()==='Activity history').click()`);
  await wait(b, `document.body.innerText.includes('Some activity is unavailable')`);
  assert.ok(await b.evaluate(`document.body.innerText.includes('Created')`), 'an audit outage must not hide available operations');
  assert.ok(await b.evaluate(`document.body.innerText.includes('the node refused the change')`), 'a failed operation must show its stored error in activity history');
  assert.match(await b.evaluate('document.body.innerText'), /Updated\s+· failed/, 'a completed operation with an error must be marked failed');
  refuseAudit = false;
  await click(b, 'Refresh activity');
  await wait(b, `!document.body.innerText.includes('Some activity is unavailable') && document.body.innerText.includes('Deletion requested · refused')`);
  assert.deepEqual(b.thrown, [], 'no browser exceptions');
  viewer = true;
  const mobile = await browser({width: 390, height: 844}); pages.push(mobile); await login(mobile);
  await wait(mobile, 'document.body.innerText.includes("Your workspace")');
  assert.equal(await mobile.evaluate('!!document.querySelector("a[href=\\"#/c/instances/new\\"]")'), false, 'viewer has no create affordance');
  assert.ok(await mobile.evaluate('document.documentElement.scrollWidth <= innerWidth'), 'overview fits phone');
  await mobile.goto(url + '#/c/instances'); await wait(mobile, '!!document.querySelector("tbody tr")');
  assert.equal(await mobile.evaluate(`document.querySelectorAll('[aria-label^="Select "]').length`), 0, 'viewer must not be offered bulk selection');
  await mobile.evaluate(`[...document.querySelectorAll('tbody tr')].find(x=>x.innerText.includes('web-1')).click()`);
  await wait(mobile, '!!document.querySelector("[data-detail=true]")');
  assert.equal(await mobile.evaluate(`[...document.querySelectorAll('summary')].some(x=>x.textContent.trim()==='More actions')`), false, 'viewer must not be offered mutating actions');
  assert.equal(await mobile.evaluate('getComputedStyle(document.querySelector(".resource-workspace [data-slot=resizable-panel]")).display'), 'none', 'phone detail owns available width');
  await wait(mobile, `[...document.querySelectorAll('button')].some(x=>x.innerText==='Attach')`);
  await click(mobile, 'Attach');
  await wait(mobile, `document.body.innerText.includes('Read-only session') && document.querySelector('.xterm-rows')?.innerText.includes('Binary console ready')`);
  await mobile.type('z'); await new Promise(r=>setTimeout(r,150));
  assert.equal(consoleMessages, 2, 'viewer console must not transmit input');
  await mobile.goto(url + '#/c/instances/new');
  await wait(mobile, `document.body.innerText.includes('do not have permission to create')`);
  assert.equal(await mobile.evaluate('!!document.querySelector("form")'), false, 'direct create URL must not expose a viewer form');
  await mobile.goto(url + '#/c/instances/p1%2Fweb-1/edit');
  await wait(mobile, `document.body.innerText.includes('do not have permission to edit')`);
  assert.equal(await mobile.evaluate('!!document.querySelector("form")'), false, 'direct edit URL must not expose a viewer form');
  assert.deepEqual(mobile.thrown, [], 'no mobile browser exceptions');
  viewer = false; projectAdmin = true;
  const memberAdmin = await browser({width: 1440, height: 900}); pages.push(memberAdmin); await login(memberAdmin);
  await wait(memberAdmin, `document.body.innerText.includes('Manage members')`);
  await memberAdmin.evaluate(`[...document.querySelectorAll('a')].find(x=>x.textContent.includes('Manage members')).click()`);
  await wait(memberAdmin, `document.body.innerText.includes('Save members')`);
  assert.equal(await memberAdmin.evaluate(`[...document.querySelectorAll('button')].some(x=>x.innerText==='New project')`), false, 'project admin cannot create global projects');
  await fill(memberAdmin, 'input[placeholder^="user id or subject"]', 'another-user');
  await click(memberAdmin, 'Add');
  await click(memberAdmin, 'Save members');
  await wait(memberAdmin, `document.body.innerText.includes('Changing who may is a project admin')`);
  assert.equal(await memberAdmin.evaluate(`[...document.querySelectorAll('button')].some(x=>x.innerText==='Save members')`), false, 'member editor updates permissions after save');
  assert.deepEqual(memberAdmin.thrown, [], 'no project-admin browser exceptions');
  projectAdmin = false; customOperator = true;
  const custom = await browser({width: 1440, height: 900}); pages.push(custom); await login(custom);
  await wait(custom, 'document.body.innerText.includes("Your workspace")');
  assert.equal(await custom.evaluate('!!document.querySelector("a[href=\\"#/c/instances/new\\"]")'), false, 'custom operator cannot create instances');
  await custom.goto(url + '#/c/volumes'); await wait(custom, '!!document.querySelector("#rail")');
  await wait(custom, `[...document.querySelectorAll('button')].some(x=>x.innerText==='New volume')`);
  await custom.goto(url + '#/c/instances'); await wait(custom, '!!document.querySelector("tbody tr")');
  assert.equal(await custom.evaluate(`[...document.querySelectorAll('button')].some(x=>x.innerText==='New instance')`), false, 'operate does not imply write');
  await custom.goto(url + '#/c/instances/new'); await wait(custom, `document.body.innerText.includes('do not have permission to create')`);
  assert.deepEqual(custom.thrown, [], 'no custom-role browser exceptions');
  for (const prior of [b, mobile, memberAdmin, custom]) prior.close();
  // Closing a tab can leave a request that was already forwarded to the
  // fixture in flight. Let that role's requests drain before attributing
  // subsequent inventory reads to the volume-only session.
  await new Promise((resolve) => setTimeout(resolve, 500));
  volumeOnly = true;
  const volumeWriter = await browser({width: 1440, height: 900}); pages.push(volumeWriter); await login(volumeWriter);
  await wait(volumeWriter, `document.body.innerText.includes('Your workspace')`);
  assert.equal(await volumeWriter.evaluate('!!document.querySelector("#rail a[href=\\"#/c/instances\\"]")'), false, 'unreadable collection is absent from navigation');
  assert.equal(await volumeWriter.evaluate('!!document.querySelector("#rail a[href=\\"#/c/volumes\\"]")'), true, 'readable collection stays in navigation');
  assert.equal(await volumeWriter.evaluate(`[...document.querySelectorAll('section.overview-panel h2')].some(x=>x.innerText==='Instances')`), false, 'overview does not advertise unreadable instances');
  await volumeWriter.goto(url + '#/c/volumes/new');
  await wait(volumeWriter, '!!document.querySelector("form")');
  assert.equal(await volumeWriter.evaluate(`document.body.innerText.includes('Could not load options')`), false, 'tenant volume form does not fetch operator-only pools');
  assert.equal(await volumeWriter.evaluate(`document.querySelector('form').innerText.includes('Pool')`), false, 'tenant volume form uses automatic pool placement');
  await volumeWriter.goto(url + '#/c/volumes');
  await wait(volumeWriter, `document.body.innerText.includes('data-1')`);
  await volumeWriter.goto(url + '#/c/users');
  await wait(volumeWriter, `document.body.innerText.includes('Users')`);
  assert.equal(await volumeWriter.evaluate(`document.body.innerText.includes('data-1')`), false, 'previous volume rows never appear on a denied users page');
  assert.deepEqual(unauthorizedLists, [], 'inventory does not request unreadable project collections');
  assert.deepEqual(volumeWriter.thrown, [], 'no volume-only browser exceptions');
  volumeWriter.close(); customOperator = false; volumeOnly = false; refuseSession = true;
  const retrySession = await browser({width: 1280, height: 800}); pages.push(retrySession);
  await retrySession.goto(url); await fill(retrySession, '#u', 'operator'); await fill(retrySession, '#p', 'a test operator passphrase'); await click(retrySession, 'Sign in');
  await wait(retrySession, `document.body.innerText.includes('Session unavailable')`);
  assert.ok(await retrySession.evaluate(`!!localStorage.getItem('velstra-react-token')`), 'temporary session outage retains the token');
  assert.equal(await retrySession.evaluate('!!document.querySelector("#rail")'), false, 'unknown identity cannot enter the console');
  refuseSession = false; await click(retrySession, 'Retry'); await wait(retrySession, '!!document.querySelector("#rail")');
  assert.deepEqual(retrySession.thrown, [], 'session retry has no browser exceptions');
  console.log('PASS: admin CRUD, duplicate prevention, project routing, failed refresh recovery, validation focus, reduced motion, viewer, project-admin and additive custom permissions, binary console output, reattachment, read-only input and mobile detail');
} finally {
  for (const b of pages) b.close();
  for (const socket of streams) socket.destroy();
  server.closeAllConnections(); server.close(); fake.kill();
}
