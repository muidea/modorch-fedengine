#!/usr/bin/env node
// Real Chromium + isolated FedEngine. Node >= 22, no npm dependencies.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdir, mkdtemp, readFile, writeFile, rm, readdir, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, resolve, join } from 'node:path';
import { parseArgs } from 'node:util';

const { values } = parseArgs({ options: {
  binary: { type: 'string', default: 'target/debug/examples/fedengine-test-host' },
  chrome: { type: 'string', default: '/opt/google/chrome/chrome' },
  screenshot: { type: 'string' },
} });
const temporary = await mkdtemp(join(tmpdir(), 'fedengine-web-'));
const processes = [];
const sleep = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));
async function until(fn, message, timeout = 15000, diagnose = null) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const value = await fn();
    if (value) return value;
    await sleep(80);
  }
  const detail = diagnose ? await Promise.resolve(diagnose()).catch(error => `diagnose failed: ${error.message}`) : '';
  throw new Error(detail ? `Timed out: ${message}\n${detail}` : `Timed out: ${message}`);
}
function launch(command, args, options) {
  const child = spawn(command, args, { stdio: ['ignore', 'pipe', 'pipe'], ...options });
  child.output = ''; child.failure = null;
  child.on('error', error => { child.failure = error; });
  for (const stream of [child.stdout, child.stderr]) stream.on('data', data => { child.output = (child.output + data).slice(-100000); });
  processes.push(child); return child;
}
function running(child) {
  if (child.failure) throw child.failure;
  if (child.exitCode !== null || child.signalCode !== null) {
    throw new Error(`Process exited (code=${child.exitCode}, signal=${child.signalCode}): ${child.output}`);
  }
}
/** Format child process status and recent output for timeout diagnostics. */
function childSummary(child) {
  return `pid=${child.pid} exitCode=${child.exitCode} signalCode=${child.signalCode}\n${child.output.slice(-4000)}`;
}
/** Read DevTools WebSocket endpoint from Chrome's DevToolsActivePort file if ready. */
async function readDevToolsPort(profileDir) {
  try {
    const [port, wsPath] = (await readFile(join(profileDir, 'DevToolsActivePort'), 'utf8')).trim().split(/\r?\n/u);
    return /^\d+$/u.test(port ?? '') && wsPath?.startsWith('/devtools/') ? `ws://127.0.0.1:${port}${wsPath}` : null;
  } catch {
    return null;
  }
}
class DevTools {
  constructor(socket) {
    this.socket = socket; this.nextId = 0; this.pending = new Map(); this.listeners = new Map();
    socket.addEventListener('message', event => {
      const message = JSON.parse(event.data);
      if (message.id) {
        const task = this.pending.get(message.id);
        if (task) { clearTimeout(task.timer); this.pending.delete(message.id); message.error ? task.reject(new Error(JSON.stringify(message.error))) : task.resolve(message.result); }
      } else for (const fn of this.listeners.get(message.method) ?? []) fn(message.params);
    });
  }
  send(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = ++this.nextId;
      const timer = setTimeout(() => { this.pending.delete(id); reject(new Error(`CDP timeout: ${method}`)); }, 15000);
      this.pending.set(id, { resolve, reject, timer }); this.socket.send(JSON.stringify({ id, method, params }));
    });
  }
  on(method, callback) { this.listeners.set(method, [...this.listeners.get(method) ?? [], callback]); }
  async evaluate(expression) {
    const result = await this.send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true, userGesture: true });
    if (result.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text);
    return result.result.value;
  }
}

let cdp;
try {
  const binary = resolve(values.binary);
  let config = await readFile('config/application.toml', 'utf8');
  config = config.replaceAll('listen = "127.0.0.1:15432"', 'listen = "127.0.0.1:0"')
    .replaceAll('listen = "127.0.0.1:15433"', 'listen = "127.0.0.1:0"')
    .replace('database_path = "./data/fedengine.duckdb"', `database_path = "${temporary}/management.duckdb"`)
    .replace('temp_directory = "./data/tmp"', `temp_directory = "${temporary}/tmp"`)
    .replace('credentials_directory = "./secrets"', `credentials_directory = "${temporary}"`);
  if (process.env.FEDENGINE_WEB_POSTGRES_DSN || process.env.FEDENGINE_WEB_POSTGRES_CREDENTIALS) {
    config = config.replace('[duckdb]', `[duckdb]\nextension_directory = "${resolve('data/extensions')}"`);
    config += '\n[[grants]]\nprincipal="fedengine"\nsource="web_live"\nschema="*"\nobject="*"\nactions=["SELECT"]\n';
    config += '\n[[grants]]\nprincipal="fedengine"\nsource="web_organization"\nschema="*"\nobject="*"\nactions=["SELECT"]\n';
  }
  await writeFile(join(temporary, 'application.toml'), config);
  const env = { ...process.env, FEDENGINE_PASSWORD: 'disposable-web-smoke', RUST_LOG: 'fedengine_test_host=info,modorch_fedengine=info', NO_COLOR: '1' };
  const targetRoot = resolve(dirname(binary), '../..');
  let libDir = null;
  try {
    for (const triple of await readdir(join(targetRoot, 'duckdb-download'))) {
      const candidate = join(targetRoot, 'duckdb-download', triple, 'v2.0.0');
      try {
        if ((await stat(join(candidate, 'libduckdb.so'))).isFile()) {
          libDir = candidate;
          break;
        }
      } catch {}
    }
  } catch {}
  if (!libDir) {
    let newest = -1;
    for (const profile of ['debug', 'release']) {
      const buildDir = join(targetRoot, profile, 'build');
      try {
        for (const entry of await readdir(buildDir)) {
          if (!entry.startsWith('libduckdb-sys-')) continue;
          const candidate = join(buildDir, entry, 'out');
          try {
            const info = await stat(join(candidate, 'libduckdb.so'));
            if (info.isFile() && info.mtimeMs > newest) {
              newest = info.mtimeMs;
              libDir = candidate;
            }
          } catch {}
        }
      } catch {}
    }
  }
  if (libDir) {
    env.LD_LIBRARY_PATH = `${libDir}:${process.env.LD_LIBRARY_PATH ?? ''}`;
  }
  const engine = launch(binary, ['--config', join(temporary, 'application.toml')], { env });
  const address = await until(
    () => {
      running(engine);
      return /management_address=Some\((127\.0\.0\.1:\d+)\)/u.exec(engine.output)?.[1];
    },
    'FedEngine ready',
    30000,
    () => `binary=${binary} ${childSummary(engine)}`,
  );
  const origin = `http://${address}`;
  async function api(path, method = 'GET', body = null) {
    const response = await fetch(`${origin}/api/v1/${path}`, {
      method, headers: { 'Content-Type': 'application/json', 'Idempotency-Key': `smoke-${crypto.randomUUID()}` },
      ...(body ? { body: JSON.stringify(body) } : {}),
    });
    const value = await response.json(); assert(response.ok, JSON.stringify(value)); return value;
  }
  async function finished(operation) {
    return until(async () => {
      const current = await api(`operations/${operation.id}`);
      return ['Succeeded', 'Failed', 'Interrupted'].includes(current.status) && current;
    }, 'operation completion');
  }
  const chromeProfile = join(temporary, 'chrome');
  await mkdir(chromeProfile, { recursive: true });
  const browserEnv = { ...process.env, DBUS_SESSION_BUS_ADDRESS: '/dev/null', NO_AT_BRIDGE: '1' };
  delete browserEnv.DISPLAY;
  delete browserEnv.WAYLAND_DISPLAY;
  const browser = launch(values.chrome, [
    '--headless=new',
    '--no-sandbox',
    '--disable-setuid-sandbox',
    '--disable-dev-shm-usage',
    '--disable-gpu',
    '--no-first-run',
    '--no-default-browser-check',
    '--password-store=basic',
    '--use-mock-keychain',
    '--disable-background-networking',
    '--disable-component-update',
    '--disable-default-apps',
    '--disable-extensions',
    '--disable-sync',
    '--metrics-recording-only',
    '--mute-audio',
    '--no-pings',
    '--disable-crash-reporter',
    '--remote-debugging-address=127.0.0.1',
    '--remote-debugging-port=0',
    `--user-data-dir=${chromeProfile}`,
    'about:blank',
  ], { env: browserEnv });
  const endpoint = await until(
    async () => {
      running(browser);
      return /DevTools listening on (ws:\/\/[^\s]+)/u.exec(browser.output)?.[1] ?? await readDevToolsPort(chromeProfile);
    },
    'Chromium ready',
    60000,
    () => readFile(join(chromeProfile, 'DevToolsActivePort'), 'utf8').catch(() => '<missing>').then(
      portFile => `chrome=${values.chrome} portFile=${JSON.stringify(portFile)} ${childSummary(browser)}`,
    ),
  );
  const browserOrigin = new URL(endpoint).origin.replace(/^ws:/u, 'http:');
  const pageTarget = await until(
    async () => {
      running(browser);
      try {
        const response = await fetch(`${browserOrigin}/json/list`);
        if (!response.ok) return null;
        const targets = await response.json();
        return targets.find(target => target.type === 'page' && target.webSocketDebuggerUrl) ?? null;
      } catch {
        return null;
      }
    },
    'Chromium page target',
    30000,
    () => `endpoint=${endpoint} ${childSummary(browser)}`,
  );
  const socket = new WebSocket(pageTarget.webSocketDebuggerUrl);
  await once(socket, 'open'); cdp = new DevTools(socket);
  const exceptions = [], writes = [], blocked = [];
  cdp.on('Runtime.exceptionThrown', event => exceptions.push(event.exceptionDetails.exception?.description ?? event.exceptionDetails.text));
  cdp.on('Network.requestWillBeSent', event => {
    if (event.request.url.startsWith(origin) && event.request.method !== 'GET') writes.push(event.request);
  });
  cdp.on('Log.entryAdded', event => { if (event.entry.text.includes('Content Security Policy')) blocked.push(event.entry.text); });
  await cdp.send('Page.enable'); await cdp.send('Runtime.enable'); await cdp.send('Network.enable'); await cdp.send('Log.enable');
  await cdp.send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false });
  await cdp.send('Page.navigate', { url: `${origin}/manage/` });
  const evaluate = expression => cdp.evaluate(expression);
  async function waitText(text, timeout = 15000) { try { await until(() => evaluate(`Boolean(document.body?.innerText?.includes(${JSON.stringify(text)}))`), text, timeout); } catch (error) { const domText = await evaluate('document.querySelector("#content")?.innerText ?? document.body?.innerText ?? location.href').catch(e => String(e)); throw new Error(`${error.message}: ${domText}; exceptions=${JSON.stringify(exceptions)}; engine=${childSummary(engine)}`); } }
  async function click(text, scope = 'document') {
    await until(() => evaluate(`[...${scope}.querySelectorAll('button')].some(node => node.textContent === ${JSON.stringify(text)} && !node.disabled && node.getClientRects().length)`), `button enabled: ${text}`);
    await evaluate(`(() => { const node = [...${scope}.querySelectorAll('button')].find(node => node.textContent === ${JSON.stringify(text)} && !node.disabled && node.getClientRects().length); if (!node) throw new Error('Button unavailable: ' + ${JSON.stringify(text)}); node.click(); })()`);
  }
  async function fill(name, value) {
    await evaluate(`(() => { const node = document.querySelector(${JSON.stringify(name)}); if (!node) throw new Error('Missing input'); node.value = ${JSON.stringify(value)}; node.dispatchEvent(new Event('input', {bubbles:true})); node.dispatchEvent(new Event('change', {bubbles:true})); })()`);
  }
  async function submit() { await evaluate(`document.querySelector('#editor').requestSubmit()`); }
  async function closed() { try { await until(() => evaluate(`!document.querySelector('#dialog').open`), 'dialog closed'); } catch (error) { throw new Error(`${error.message}: ${await evaluate('document.querySelector("#dialog-error").textContent')}`); } }
  async function sourceState(predicate) { return until(async () => { const source = await api('sources/web_pg'); return predicate(source) && source; }, 'source state'); }
  async function settleSource() {
    await until(() => evaluate(`document.querySelector('#pending').hidden`), 'request accepted');
    await sourceState(source => !source.current_operation);
    await click('↻ 刷新状态'); await sleep(200);
  }
  await waitText('管理服务已连接', 30000);
  await waitText('尚未登记数据源');
  await click('＋ 登记数据源');
  await fill('[name=connection_id]', 'web_pg'); await fill('[name=host]', '127.0.0.1'); await fill('[name=virtual_name]', 'web_pg');
  await fill('[name=secret_ref]', 'postgresql://invalid:placeholder@localhost/example'); await submit();
  await waitText('不能填写连接串或密码');
  assert.equal(writes.length, 0);
  assert.equal(await evaluate('sessionStorage.getItem("fedengine.pending")'), null);
  await click('取消', 'document.querySelector("#dialog")');
  // Exercise the lower-level target API for unavailable-source lifecycle checks.
  await finished(await api('sources', 'POST', { id:'web_pg',kind:'postgres',catalog_alias:'web_pg',secret_ref:'file:missing.dsn',default_schema:'public',allowed_schemas:['public'],read_only:true,ddl_enabled:false }));
  await click('↻ 刷新状态'); await waitText('web_pg');
  await sourceState(source => source.status === 'Registered' && !source.current_operation);
  await click('管理 →'); await waitText('凭据引用');
  await click('探测连接'); await settleSource();
  const probe = await api('sources/web_pg');
  assert.equal(probe.applied_revision, null);
  const probeId = await evaluate('JSON.parse(sessionStorage.getItem("fedengine.operations"))[0]');
  assert.equal((await api(`operations/${probeId}`)).status, 'Failed');
  console.log('Passed: registration, asynchronous tracking and failed credential probe');

  // Lose the PATCH response after the server received it, then reload and retry.
  let dropPatch = true;
  let mockCatalog = false;
  let catalogMode = 'first';
  let mockDirectory = false, directoryConflict = false;
  let holdQueryResponse = false, heldQueryResponse = null;
  const directoryRequests = [];
  const treeTable = 'odd"table <img src=x onerror=alert(1)>';
  const treeColumn = 'col"umn <img src=x>';
  const catalogRequests = [];
  const interceptErrors = [];
  const interceptions = new Set();
  let stoppingInterception = false;
  async function stopIntercepting() {
    stoppingInterception = true;
    try {
      await cdp.send('Fetch.disable');
      // Disabling Fetch releases paused requests; a concurrent continue may then be rejected.
      while (interceptions.size) await Promise.all([...interceptions]);
    } finally { stoppingInterception = false; }
  }
  cdp.on('Fetch.requestPaused', event => {
    const task = (async () => {
      if (holdQueryResponse && event.request.method === 'POST' && event.responseStatusCode && /\/browser\/queries\/[^/]+$/u.test(new URL(event.request.url).pathname)) {
        heldQueryResponse = event.requestId; return;
      }
      if (mockDirectory && event.request.url.includes('/api/v1/browser/')) {
        const url = new URL(event.request.url); directoryRequests.push(url);
        const detail = url.pathname.endsWith('/object');
        const conflict = !detail && directoryConflict;
        const page = detail ? { stamp:url.searchParams.get('stamp'), object:{columns:[{name:treeColumn,remote_type:'text'}]} } : {
          stamp:url.searchParams.get('stamp'), offset:0,has_more:true,objects:[{name:treeTable,kind:'r',source_type:'BASE TABLE',columns:1}] };
        await cdp.send('Fetch.fulfillRequest',{requestId:event.requestId,responseCode:conflict?409:200,
          responseHeaders:[{name:'Content-Type',value:'application/json'}],body:Buffer.from(JSON.stringify(conflict?{code:'40001',error:'directory snapshot changed'}:page)).toString('base64')});return;
      }
      if (event.request.method === 'PATCH' && dropPatch && event.responseStatusCode) {
        dropPatch = false; await cdp.send('Fetch.failRequest', { requestId: event.requestId, errorReason: 'ConnectionReset' }); return;
      }
      if (mockCatalog && event.request.url.includes('/catalog')) {
        catalogRequests.push(new URL(event.request.url));
        const page = {
          snapshot_id: 'test-snapshot', snapshot_version: 'test-snapshot', revision: 1, generation: 1,
          refreshed_at: '2026-01-01T00:00:00Z', stale: false, total: 51, offset: catalogMode === 'first' ? 0 : 50,
          objects: [{ schema: 'public', name: '<img src=x onerror=alert(1)>', object_type: 'table', columns: [{ name: 'id', remote_type: 'integer' }], metadata: {} }],
          next_page_token: catalogMode === 'first' ? 'opaque-test-token' : null,
        };
        const conflict = catalogMode === 'conflict';
        await cdp.send('Fetch.fulfillRequest', { requestId: event.requestId, responseCode: conflict ? 409 : 200,
          responseHeaders: [{ name: 'Content-Type', value: 'application/json' }],
          body: Buffer.from(JSON.stringify(conflict ? { code: '40001', error: 'snapshot changed' } : page)).toString('base64') }); return;
      }
      await cdp.send('Fetch.continueRequest', { requestId: event.requestId });
    })().catch(error => {
      if (!stoppingInterception || !error.message.includes('Fetch domain is not enabled')) interceptErrors.push(error.message);
    }).finally(() => interceptions.delete(task));
    interceptions.add(task);
  });
  await cdp.send('Fetch.enable', { patterns: [
    { urlPattern: `${origin}/api/v1/sources/web_pg`, requestStage: 'Response' },
    { urlPattern: `${origin}/api/v1/sources/web_pg/catalog*`, requestStage: 'Request' },
  ] });
  await click('凭据引用'); await fill('[name=secret_ref]', 'file:rotated.dsn'); await submit();
  await waitText('有一项请求需要确认结果');
  await sourceState(source => source.desired_revision === 2 && !source.current_operation);
  await cdp.send('Page.reload'); await waitText('有一项请求需要确认结果'); await waitText('管理服务已连接');
  await click('使用原幂等键重试');
  await until(() => evaluate(`document.querySelector('#pending').hidden`), 'pending resolved');
  const patched = await sourceState(source => !source.current_operation);
  assert.equal(patched.desired_revision, 2);
  const patches = writes.filter(request => request.method === 'PATCH');
  assert.equal(patches.length, 2);
  const key = request => Object.entries(request.headers).find(([name]) => name.toLowerCase() === 'idempotency-key')?.[1];
  assert.equal(key(patches[0]), key(patches[1])); assert.equal(patches[0].postData, patches[1].postData);
  console.log('Passed: lost response, reload recovery and identical idempotent retry');

  await click('管理 →'); await click('纳管范围'); await fill('[name=default_schema]', '');
  await submit(); await closed(); await settleSource();
  const schemas = await api('sources/web_pg');
  assert.equal(schemas.desired.definition.default_schema, '');
  assert.equal(schemas.desired_revision, 3);
  // An external change invalidates the open form; the UI must not silently rebase it.
  await click('凭据引用');
  const external = await api('sources/web_pg', 'PATCH', {
    target: { incarnation: schemas.incarnation, expected_revision: schemas.desired_revision }, secret_ref: 'file:external.dsn',
  });
  assert.equal((await finished(external)).status, 'Succeeded');
  await fill('[name=secret_ref]', 'file:stale-form.dsn'); await submit(); await waitText('页面版本已过期');
  assert.equal((await api('sources/web_pg')).desired.definition.secret_ref, 'file:external.dsn');
  await click('取消', 'document.querySelector("#dialog")');
  console.log('Passed: schema configuration, explicit default clearing and stale-form conflict');

  await click('↻ 刷新状态'); await sleep(200);
  await click('查询目录'); await waitText('目录读取失败');
  mockCatalog = true; await click('查询目录'); await waitText('共 51 项');
  assert.equal(await evaluate('document.querySelectorAll("#catalog img").length'), 0);
  await click('查看事实'); await waitText('remote_type'); await click('取消', 'document.querySelector("#dialog")');
  catalogMode = 'second'; await click('下一页'); await waitText('第 2 页');
  assert.equal(catalogRequests.at(-1).searchParams.get('page_token'), 'opaque-test-token');
  catalogMode = 'conflict'; await click('上一页'); await waitText('目录快照已变化');
  catalogMode = 'first'; await click('从第一页重新读取'); await waitText('第 1 页');
  assert.equal(catalogRequests.at(-1).searchParams.has('page_token'), false);
  mockCatalog = false; await stopIntercepting();
  console.log('Passed: catalog error, pagination, snapshot conflict and untrusted text rendering (synthetic HTTP facts)');

  await evaluate('document.querySelector("[data-page=databases]").click()');
  await click('＋ 创建数据库'); await fill('[name=name]', 'web_account'); await fill('[name=owner]', 'fedengine');
  await click('＋ 添加映射'); await fill('.mapping-name', 'business'); await fill('.mapping-source', 'web_pg'); await fill('.mapping-remote', 'public');
  await submit(); await closed(); await waitText('web_account');
  await click('编辑定义与映射'); await click('＋ 添加映射');
  await evaluate(`(() => { const row = document.querySelectorAll('.mapping-row')[1]; row.querySelector('.mapping-name').value='second'; row.querySelector('.mapping-source').value='web_pg'; row.querySelector('.mapping-remote').value='public'; })()`);
  await submit(); await closed();
  const database = await api('virtual-databases/web_account');
  assert.deepEqual(database.config.schemas.map(schema => schema.name), ['business', 'second']);
  assert.equal(database.revision, 2);
  const otherDatabase = await api('virtual-databases','POST',{name:'web_other',owner:'fedengine',connect:[],allow_connections:true,schemas:[]});
  await click('↻ 刷新状态');
  for (const page of ['sources','operations','databases']) {
    await evaluate(`document.querySelector('[data-page=${page}]').click()`);
    assert.equal(await evaluate(`getComputedStyle(document.querySelector('#database-navigation')).display === 'none' && document.querySelector('#toggle-database-tree').hidden && !document.querySelector('.workspace-body').classList.contains('database-workspace')`),true,`no database tree on ${page}`);
    assert.equal(await evaluate(`document.querySelector('#main').getBoundingClientRect().left === document.querySelector('.workspace-body').getBoundingClientRect().left`),true,`no reserved tree space on ${page}`);
  }
  await click('web_account','document.querySelector("#content")'); await waitText('对象浏览'); await waitText('此 Schema 中没有匹配的可见对象');
  assert.equal(await evaluate(`getComputedStyle(document.querySelector('#database-navigation')).display !== 'none'`),true,'tree in object browser');
  assert.equal(await evaluate('document.querySelector("#stats").hidden'), true);
  assert.equal(await evaluate('document.querySelectorAll("nav button").length'), 3);
  await evaluate(`(() => { const checkbox=document.querySelector('#database-system'); checkbox.checked=true; checkbox.dispatchEvent(new Event('change',{bubbles:true})); })()`);
  await waitText('pg_catalog');
  await evaluate(`[...document.querySelectorAll('button')].find(n=>n.getAttribute('aria-label')==='选择 Schema pg_catalog').click()`);
  await until(() => evaluate(`document.querySelector('#object-area h3')?.textContent==='pg_catalog' && document.querySelector('#object-rows tbody')`),'browse system schema');
  assert.equal(await evaluate('document.querySelector("#database-tree").textContent.includes("_fedengine")'), false);
  await evaluate(`(() => { const checkbox=document.querySelector('#database-system'); checkbox.checked=false; checkbox.dispatchEvent(new Event('change',{bubbles:true})); })()`);
  await waitText('此 Schema 中没有匹配的可见对象');
  assert.equal(await evaluate(`document.querySelector('#object-area h3').textContent`),'public');
  await until(() => evaluate('document.querySelector("#database-tree").textContent.includes("public") && !document.querySelector("#database-tree").textContent.includes("pg_catalog")'), 'system schemas hidden again');

  await click('SQL 调试'); await fill('[aria-label="SQL 编辑器"]', "SELECT 9223372036854775807::BIGINT AS exact_value, NULL::TEXT AS missing, '' AS empty");
  await click('执行 SQL'); await waitText('执行成功'); await waitText('9223372036854775807');
  assert.equal(await evaluate('document.querySelector(".sql-null").textContent'), 'NULL');
  assert.equal(await evaluate('[...document.querySelector("#sql-output").childNodes].some(n => n.nodeType === Node.TEXT_NODE && n.textContent.includes("null"))'), false);
  await fill('[aria-label="SQL 编辑器"]', 'SELECT 9 AS selected_value; DELETE FROM nowhere;');
  await evaluate(`(() => { const e = document.querySelector('.sql-editor'); e.setSelectionRange(0,26); e.dispatchEvent(new KeyboardEvent('keydown', {key:'Enter', ctrlKey:true, bubbles:true})); })()`);
  await waitText('selected_value'); await waitText('执行成功');
  await fill('[aria-label="SQL 编辑器"]', 'DELETE FROM nowhere'); await click('执行 SQL'); await waitText('SQLSTATE 0A000');
  await click('消息'); await waitText('SQL debugger accepts one read-only query');
  await fill('[aria-label="SQL 编辑器"]', 'SELECT i::INTEGER AS row_value FROM range(1001) t(i)'); await click('执行 SQL'); await waitText('执行成功'); await waitText('当前结果不完整');
  await click('下一页'); await waitText('第 2 页');
  await fill('[aria-label="SQL 编辑器"]', 'SELECT sum(a.i*b.j) FROM range(1000000000) a(i), range(1000000000) b(j)');
  await click('执行 SQL'); await waitText('执行中'); await click('停止'); await waitText('SQLSTATE 57014');
  await fill('[aria-label="SQL 编辑器"]', 'SELECT 42 AS preserved_editor'); await sleep(2800);
  assert.equal(await evaluate('document.querySelector(".sql-editor").value'), 'SELECT 42 AS preserved_editor');
  await click('对象浏览'); await waitText('此 Schema 中没有匹配的可见对象'); await click('SQL 调试');
  assert.equal(await evaluate('document.querySelector(".sql-editor").value'), 'SELECT 42 AS preserved_editor');
  async function clickAria(label) {
    await until(() => evaluate(`[...document.querySelectorAll('button')].some(n=>n.getAttribute('aria-label')===${JSON.stringify(label)})`),label);
    await evaluate(`[...document.querySelectorAll('button')].find(n=>n.getAttribute('aria-label')===${JSON.stringify(label)}).click()`);
  }
  await clickAria('打开数据库 web_other'); await until(() => evaluate(`document.querySelector('#page-title').textContent==='web_other' && !!document.querySelector('.sql-editor')`),'SQL database switch');
  await fill('[aria-label="SQL 编辑器"]','SELECT current_database() AS selected_database'); await click('执行 SQL'); await waitText('selected_database'); await waitText('执行成功');
  assert.equal(await evaluate(`document.querySelector('#sql-output tbody').textContent`),'web_other');
  holdQueryResponse = true;
  await cdp.send('Fetch.enable',{patterns:[{urlPattern:`${origin}/api/v1/browser/queries/*`,requestStage:'Response'}]});
  await fill('[aria-label="SQL 编辑器"]','SELECT sum(a.i*b.j) FROM range(1000000000) a(i), range(1000000000) b(j)'); await click('执行 SQL');
  await until(()=>heldQueryResponse,'query accepted but response delayed');
  const runningQuery = writes.filter(r=>r.method==='POST' && /\/browser\/queries\/[^/]+$/u.test(new URL(r.url).pathname)).at(-1);
  assert.equal(JSON.parse(runningQuery.postData).database,'web_other');
  await clickAria('打开数据库 web_account'); await until(() => evaluate(`document.querySelector('#page-title').textContent==='web_account'`),'return to SQL database');
  assert.equal(await evaluate('document.querySelector(".sql-editor").value'),'SELECT 42 AS preserved_editor');
  holdQueryResponse = false; await cdp.send('Fetch.continueRequest',{requestId:heldQueryResponse});
  await until(async () => (await api(`browser/queries/${new URL(runningQuery.url).pathname.split('/').at(-1)}`)).status==='canceled','switch cancels previous query');
  await stopIntercepting();
  mockDirectory = true;
  await cdp.send('Fetch.enable',{patterns:[{urlPattern:`${origin}/api/v1/browser/objects?*`,requestStage:'Request'},{urlPattern:`${origin}/api/v1/browser/object?*`,requestStage:'Request'}]});
  await clickAria('展开或收起 Schema public'); await clickAria(`选择对象 public.${treeTable}`);
  assert.equal(directoryRequests.some(u=>u.pathname.endsWith('/object')),false);
  assert.equal(await evaluate(`document.querySelectorAll('#database-tree img').length`),0);
  const queryCount = () => writes.filter(r=>r.method==='POST' && /\/browser\/queries\/[^/]+$/u.test(new URL(r.url).pathname)).length;
  const beforeInsert = queryCount();
  await fill('[aria-label="SQL 编辑器"]','SELECT '); await evaluate(`document.querySelector('.sql-editor').setSelectionRange(7,7)`);
  await click('插入名称');
  assert.equal(await evaluate('document.querySelector(".sql-editor").value'),`SELECT "public"."${treeTable.replaceAll('"','""')}"`);
  await clickAria(`展开或收起列 public.${treeTable}`); await clickAria(`选择列 public.${treeTable}.${treeColumn}`);
  await fill('[aria-label="SQL 编辑器"]','SELECT placeholder'); await evaluate(`document.querySelector('.sql-editor').setSelectionRange(7,18)`); await click('插入名称');
  assert.equal(await evaluate('document.querySelector(".sql-editor").value'),`SELECT "${treeColumn.replaceAll('"','""')}"`);
  await click('插入 SELECT'); assert.equal(await evaluate('document.querySelector(".sql-editor").value.includes("LIMIT 100;")'),true);
  await fill('[aria-label="SQL 编辑器"]','SELECT '); await evaluate(`document.querySelector('.sql-editor').setSelectionRange(7,7)`);
  const columnPoint = await evaluate(`(() => { const n=[...document.querySelectorAll('button')].find(n=>n.getAttribute('aria-label')===${JSON.stringify(`选择列 public.${treeTable}.${treeColumn}`)}); const r=n.getBoundingClientRect(); return {x:r.x+r.width/2,y:r.y+r.height/2}; })()`);
  for (const clickCount of [1,2]) {
    await cdp.send('Input.dispatchMouseEvent',{type:'mousePressed',button:'left',clickCount,...columnPoint});
    await cdp.send('Input.dispatchMouseEvent',{type:'mouseReleased',button:'left',clickCount,...columnPoint});
  }
  assert.equal(await evaluate('document.querySelector(".sql-editor").value'),`SELECT "${treeColumn.replaceAll('"','""')}"`,'native double click inserts the selected column');
  assert.equal(await evaluate(`document.querySelectorAll('#database-tree .directory-row.selected').length`),1);
  assert.equal(queryCount(),beforeInsert,'tree insertion must not execute SQL');
  if (values.screenshot) {
    const screenshot = await cdp.send('Page.captureScreenshot', { format: 'png' });
    await writeFile(values.screenshot, Buffer.from(screenshot.data, 'base64'));
  }
  directoryConflict = true; await click('加载更多对象'); await waitText('directory snapshot changed');
  directoryConflict = false; await click('重新加载目录'); await clickAria('展开或收起 Schema public'); await clickAria(`选择对象 public.${treeTable}`);
  mockDirectory = false; await stopIntercepting(); await clickAria('刷新数据库树');
  await fill('[aria-label="SQL 编辑器"]','SELECT 42 AS preserved_editor');
  assert.equal(await evaluate(`document.querySelector('#sql-area').innerText.includes('服务端固定 owner') || document.querySelector('#sql-area').innerText.includes('执行只读 SQL 后在此查看结果和消息')`),false);
  console.log('Passed: database tree limited to object/SQL workspaces, SQL context switching/cancel, per-database drafts, lazy objects/columns, quoted insertion, snapshot retry and untrusted directory names');
  for (const width of [1024,375,320]) {
    await cdp.send('Emulation.setDeviceMetricsOverride', {width,height:844,deviceScaleFactor:1,mobile:width<500});
    assert.equal(await evaluate('document.documentElement.scrollWidth <= window.innerWidth'), true, `SQL viewport ${width}`);
  }
  await click('对象浏览'); await click('☰ 数据库树'); await waitText('public'); await waitText('此 Schema 中没有匹配的可见对象');
  assert.equal(await evaluate('document.documentElement.scrollWidth <= window.innerWidth'), true);
  assert.equal(await evaluate(`getComputedStyle(document.querySelector('#database-navigation')).display !== 'none'`),true,'expanded mobile workspace tree');
  for (const page of ['sources','operations','databases']) {
    await evaluate(`document.querySelector('[data-page=${page}]').click()`);
    assert.equal(await evaluate(`getComputedStyle(document.querySelector('#database-navigation')).display === 'none' && getComputedStyle(document.querySelector('#toggle-database-tree')).display === 'none' && !document.querySelector('.workspace-body').classList.contains('directory-open')`),true,`no mobile database tree on ${page}`);
  }
  await click('web_account','document.querySelector("#content")');
  assert.equal(await evaluate(`!document.querySelector('#database-navigation').hidden && getComputedStyle(document.querySelector('#toggle-database-tree')).display !== 'none' && document.querySelector('#toggle-database-tree').getAttribute('aria-expanded') === 'false'`),true,'mobile workspace tree can reopen');
  await cdp.send('Emulation.setDeviceMetricsOverride', {width:1440,height:1000,deviceScaleFactor:1,mobile:false});
  assert.equal(await evaluate(`getComputedStyle(document.querySelector('#database-navigation')).display !== 'none'`),true,'desktop workspace tree restored');
  await click('← 数据库');
  const otherCurrent = await api('virtual-databases/web_other');
  assert.equal(otherDatabase.name,'web_other');
  await api('virtual-databases/web_other','DELETE',{target:{oid:otherCurrent.oid,expected_revision:otherCurrent.revision}}); await click('↻ 刷新状态');
  console.log('Passed: database entry, empty public schema, read-only SQL, precision/NULL, selection, errors, truncation/paging, cancel, editor persistence and narrow layouts');
  await click('删除'); await click('确认删除数据库', 'document.querySelector("#dialog")'); await closed(); await waitText('尚未创建数据库');
  console.log('Passed: virtual database creation, full mapping preservation and deletion');

  await evaluate('document.querySelector("[data-page=sources]").click()'); await click('管理 →');
  await click('停用'); await click('确认停用', 'document.querySelector("#dialog")'); await closed(); await settleSource();
  await sourceState(source => source.status === 'Disabled');
  await click('删除登记'); await click('确认删除登记', 'document.querySelector("#dialog")'); await closed();
  await until(async () => (await api('sources')).length === 0, 'source deleted');
  await evaluate('document.querySelector("[data-page=operations]").click()'); await waitText('操作记录');
  await until(() => evaluate('document.querySelectorAll("#operation-rows tbody tr").length > 0'),'operation history loaded');
  await fill('[aria-label="搜索操作编号、源 ID、动作或状态"]', 'partial-operation-id'); await sleep(2800);
  assert.equal(await evaluate(`document.querySelector('[aria-label="搜索操作编号、源 ID、动作或状态"]').value`), 'partial-operation-id');
  await fill('[aria-label="搜索操作编号、源 ID、动作或状态"]', '');
  assert.deepEqual(exceptions, []); assert.deepEqual(blocked, []); assert.deepEqual(interceptErrors, []);
  await cdp.send('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 1, mobile: true });
  assert.equal(await evaluate('document.documentElement.scrollWidth <= window.innerWidth'), true);
  console.log('Passed: source disable/delete, operation history, responsive layout and browser runtime/CSP');

  // Optional read-only lifecycle against an explicitly supplied PostgreSQL source.
  if (process.env.FEDENGINE_WEB_POSTGRES_DSN) {
    const schema = process.env.FEDENGINE_WEB_POSTGRES_SCHEMA ?? 'public';
    await cdp.send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false });
    await evaluate('document.querySelector("[data-page=sources]").click()');
    await finished(await api('sources', 'POST', {id:'web_live',kind:'postgres',catalog_alias:'web_live',secret_ref:'FEDENGINE_WEB_POSTGRES_DSN',default_schema:schema,allowed_schemas:[schema],read_only:true,ddl_enabled:false}));
    await click('↻ 刷新状态'); await waitText('web_live'); await click('管理 →');
    async function completeLive(action) {
      await click(action);
      await until(() => evaluate('document.querySelector("#pending").hidden'), 'live source accepted');
      const source = await until(async () => { const source = await api('sources/web_live'); return !source.current_operation && source; }, 'live source completion');
      const id = await evaluate('JSON.parse(sessionStorage.getItem("fedengine.operations"))[0]');
      const operation = await api(`operations/${id}`);
      assert.equal(operation.status, 'Succeeded', operation.error ?? 'live operation');
      await click('↻ 刷新状态'); return source;
    }
    await completeLive('探测连接'); await completeLive('发现 Schema');
    assert.equal((await completeLive('激活 / 应用')).status, 'Active');
    await completeLive('刷新元数据');
    await click('查询目录'); await waitText('共 ');
    const catalog = await api('sources/web_live/catalog?limit=50');
    assert(catalog.total > 0, 'optional source schema must contain at least one table or view');
    await waitText(catalog.objects[0].name);
    await api('virtual-databases', 'POST', {name:'web_live_db',owner:'fedengine',connect:[],allow_connections:true,schemas:[{name:'mapped',source:'web_live',remote_schema:schema}]});
    await evaluate('document.querySelector("[data-page=databases]").click()'); await click('↻ 刷新状态'); await click('web_live_db');
    await waitText('mapped'); await waitText(catalog.objects[0].name); await click(catalog.objects[0].name);
    await waitText('外部映射对象'); await click('列'); await waitText(catalog.objects[0].columns[0].name);
    await click('来源约束'); await click('来源索引'); await click('定义'); await click('统计'); await click('来源与状态');
    await waitText(catalog.snapshot_id); await click('查看来源对象'); await waitText('返回数据库对象'); await click('← 返回数据库对象');
    await waitText('web_live_db.mapped.');
    await click('刷新来源元数据'); await fill('[aria-label="刷新来源"]','web_live'); await submit(); await closed();
    await until(async () => !(await api('sources/web_live')).current_operation, 'browser source refresh');
    await click('重新加载目录'); await waitText(catalog.objects[0].name);
    await click('SQL 调试'); await fill('[aria-label="SQL 编辑器"]', `SELECT * FROM mapped."${catalog.objects[0].name.replaceAll('"','""')}" LIMIT 1`); await click('执行 SQL'); await waitText('执行成功');
    if (values.screenshot) {
      const screenshot = await cdp.send('Page.captureScreenshot', { format: 'png' });
      await writeFile(values.screenshot, Buffer.from(screenshot.data, 'base64'));
    }
    await click('← 数据库'); await click('删除'); await click('确认删除数据库', 'document.querySelector("#dialog")'); await closed();
    await evaluate('document.querySelector("[data-page=sources]").click()'); await click('管理 →');
    await click('停用'); await click('确认停用', 'document.querySelector("#dialog")'); await closed();
    await until(async () => (await api('sources/web_live')).status === 'Disabled', 'live source disabled');
    await click('↻ 刷新状态'); await click('删除登记'); await click('确认删除登记', 'document.querySelector("#dialog")'); await closed();
    await until(async () => (await api('sources')).length === 0, 'live source deleted');
    console.log('Passed: real PostgreSQL probe, discover, activation, refresh, metadata and disable/delete (read-only)');
  }
  if (process.env.FEDENGINE_WEB_POSTGRES_CREDENTIALS) {
    await cdp.send('Emulation.setDeviceMetricsOverride', {width:1440,height:1000,deviceScaleFactor:1,mobile:false});
    await evaluate('document.querySelector("[data-page=sources]").click()'); await click('＋ 登记数据源');
    await fill('[name=connection_id]', 'web_organization');
    await fill('[name=host]', process.env.FEDENGINE_WEB_POSTGRES_HOST ?? '127.0.0.1');
    await fill('[name=port]', process.env.FEDENGINE_WEB_POSTGRES_PORT ?? '36432');
    await fill('[name=database]', process.env.FEDENGINE_WEB_POSTGRES_DATABASE ?? 'crm');
    await fill('[name=virtual_name]', 'web_native');
    await fill('[name=secret_ref]', 'FEDENGINE_WEB_POSTGRES_CREDENTIALS');
    await submit(); await waitText('核对映射并激活'); await waitText('新目标');
    assert.equal((await api('sources')).length,0,'discovery must not register targets');
    await submit();
    await until(() => evaluate('document.querySelector("#dialog-body").textContent.includes("已激活")'), 'native UI activation', 45000);
    const managed=await api('virtual-databases/web_native');assert.equal(managed.managed,true);
    await click('取消','document.querySelector("#dialog")');
    await evaluate('document.querySelector("[data-page=databases]").click()'); await click('web_native');
    await waitText('public'); await click('SQL 调试');
    await fill('[aria-label="SQL 编辑器"]','SELECT current_database() AS native_database');await click('执行 SQL');await waitText('执行成功');await waitText('web_native');
    await click('← 数据库');
    assert.equal(await evaluate('[...document.querySelectorAll("#content button")].some(b => b.textContent === "编辑定义与映射")'),false);
    const source=(await api('sources'))[0];
    await finished(await api(`sources/${source.id}/disable`,'POST',{target:{incarnation:source.incarnation,expected_revision:source.desired_revision}}));
    const disabled=await api(`sources/${source.id}`);
    await finished(await api(`sources/${source.id}`,'DELETE',{target:{incarnation:disabled.incarnation,expected_revision:disabled.desired_revision}}));
    console.log('Passed: real instance wizard discovery, scope review, durable activation, native public mapping, SQL context and source-owned database removal');
    assert.deepEqual(exceptions,[]);assert.deepEqual(blocked,[]);
  }
  // Real, isolated inventories and persisted history, independent of browser progress cache.
  await cdp.send('Emulation.setDeviceMetricsOverride', {width:1440,height:1000,deviceScaleFactor:1,mobile:false});
  for (let i=0;i<21;i++) {
    const name=`paged_${String(i).padStart(2,'0')}`;
    assert.equal((await finished(await api('sources','POST',{id:name,kind:'postgres',catalog_alias:name,secret_ref:'MISSING_PAGED_DSN',default_schema:'public',allowed_schemas:['public'],read_only:true}))).status,'Succeeded');
    const source=await api(`sources/${name}`);
    assert.equal((await finished(await api(`sources/${name}`,'PATCH',{target:{incarnation:source.incarnation,expected_revision:source.desired_revision},secret_ref:'MISSING_PAGED_DSN'}))).status,'Succeeded');
    await api('virtual-databases','POST',{name:`paged_db_${String(i).padStart(2,'0')}`,owner:'fedengine',connect:[],allow_connections:true,schemas:[]});
  }
  const pager='document.querySelector("#content .list-pagination")';
  async function pageText(text) {
    await until(() => evaluate(`${pager}?.innerText.includes(${JSON.stringify(text)})`), `pager ${text}`);
  }
  async function goto(page) { await fill('[aria-label="跳转页码"]',String(page)); await click('跳转',pager); }
  for (const page of ['sources','databases']) {
    await evaluate(`document.querySelector('[data-page=${page}]').click()`); await click('↻ 刷新状态');
    await pageText('共 21 项');
    const items=page==='sources'?'#source-rows tbody tr':'#database-rows .database-entry';
    assert.equal(await evaluate(`document.querySelectorAll('${items}').length`),10);
    await click('下一页',pager); await pageText('第 2 / 3 页');
    await click('↻ 刷新状态'); await sleep(300); await pageText('第 2 / 3 页');
    if (page==='sources') { await click('管理 →'); await click('← 返回数据源列表'); }
    else { await click('paged_db_10','document.querySelector("#content")'); await click('← 数据库'); }
    await pageText('第 2 / 3 页');
    await goto(3); await pageText('第 3 / 3 页');
    assert.equal(await evaluate(`document.querySelectorAll('${items}').length`),1);
    const search=page==='sources'?'搜索数据源':'搜索数据库';
    await fill(`[aria-label="${search}"]`,page==='sources'?'paged_20':'paged_db_20'); await pageText('共 1 项 · 第 1 / 1 页');
    await evaluate(`document.querySelector('[aria-label="${search}"]').focus()`); await sleep(2800);
    assert.equal(await evaluate(`document.activeElement.getAttribute('aria-label')`),search);
    await fill(`[aria-label="${search}"]`,'no-such-page'); await pageText('共 0 项 · 第 1 / 1 页');
    await fill(`[aria-label="${search}"]`,'');
    await fill('[aria-label="每页条数"]','20'); await pageText('第 1 / 2 页');
    assert.equal(await evaluate(`document.querySelectorAll('${items}').length`),20);
    await click('下一页',pager); await pageText('第 2 / 2 页');
    if (page==='databases') {
      await click('删除'); await click('确认删除数据库','document.querySelector("#dialog")'); await closed();
      await pageText('共 20 项 · 第 1 / 1 页');
    }
    await fill('[aria-label="每页条数"]','10');
  }
  await evaluate('sessionStorage.removeItem("fedengine.operations")');
  await cdp.send('Page.reload'); await waitText('数据源列表');
  await evaluate('document.querySelector("[data-page=operations]").click()');
  const history=await api('operations?limit=100'); assert(history.total>40);
  await pageText(`共 ${history.total} 项`);
  const seen=new Set();
  const pages=Math.ceil(history.total/10);
  for(let i=1;i<=pages;i++) {
    await pageText(`第 ${i} / ${pages} 页`);
    for(const id of await evaluate('[...document.querySelectorAll("#operation-rows .operation-id")].map(n=>n.textContent)')) {
      assert(!seen.has(id),'operation pages must not duplicate');seen.add(id);
    }
    if(i<pages) await click('下一页',pager);
  }
  assert.equal(seen.size,history.total,'fresh browser must see all persisted history');
  await fill('[aria-label="搜索操作编号、源 ID、动作或状态"]','paged_20'); await pageText('共 2 项 · 第 1 / 1 页');
  await click('查看结果'); await waitText('操作详情'); await click('取消','document.querySelector("#dialog")');
  await fill('[aria-label="搜索操作编号、源 ID、动作或状态"]','absent'); await pageText('共 0 项 · 第 1 / 1 页');
  await fill('[aria-label="搜索操作编号、源 ID、动作或状态"]',''); await pageText(`共 ${history.total} 项`);
  await fill('[aria-label="每页条数"]','20'); await pageText(`第 1 / ${Math.ceil(history.total/20)} 页`);
  await goto(2); await pageText(`第 2 / ${Math.ceil(history.total/20)} 页`);
  await click('↻ 刷新状态'); await sleep(300); await pageText(`第 2 / ${Math.ceil(history.total/20)} 页`);
  for (const page of ['sources','databases','operations']) {
    await evaluate(`document.querySelector('[data-page=${page}]').click()`);
    await cdp.send('Emulation.setDeviceMetricsOverride', {width:320,height:844,deviceScaleFactor:1,mobile:true});
    await until(() => evaluate('!!document.querySelector(".list-pagination")'),'mobile pager');
    assert.equal(await evaluate('document.documentElement.scrollWidth <= window.innerWidth'),true,`${page} mobile pagination`);
    assert.equal(await evaluate('document.querySelector("#database-navigation").hidden'),true);
  }
  assert.deepEqual(exceptions,[]); assert.deepEqual(blocked,[]);
  console.log('Passed: source/database/history pagination, sizes/jump, filters, focus/refresh/detail retention, deletion clamp, fresh-browser complete history and mobile layout');
  engine.kill('SIGTERM'); await once(engine, 'exit');
  assert.equal(engine.exitCode, 0); assert.match(engine.output, /GracefullyStopped/u);
  console.log('Passed: isolated process checked drain');
} finally {
  try { await cdp?.send('Browser.close'); } catch { /* Browser may already have exited. */ }
  cdp?.socket.close();
  for (const child of processes.reverse()) {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill('SIGTERM');
      await Promise.race([once(child, 'exit'), sleep(5000)]);
      if (child.exitCode === null && child.signalCode === null) { child.kill('SIGKILL'); await once(child, 'exit'); }
    }
  }
  await rm(temporary, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
}
