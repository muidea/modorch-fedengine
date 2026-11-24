// Virtual database workspace; remote values are always rendered as text nodes.
export function createBrowser(ui) {
  const { el, button, replace, table, get, request, toast, refreshSource, sources } = ui;
  const workspaces = new Map();
  let current = null, root = null, epoch = 0, lastTab = 'objects', showSystem = false;
  const quote = name => `"${name.replaceAll('"', '""')}"`;
  const active = (w, e = epoch) => current === w && e === epoch && root?.isConnected;
  const note = text => el('div', { class: 'notice' }, text);
  const unknown = value => value === null || value === undefined ? '未知 / 未采集' : typeof value === 'object' ? JSON.stringify(value) : String(value);
  const code = value => el('pre', { class: 'metadata-code' }, unknown(value));
  function tab(label, selected, callback) { const node = button(label, callback, selected ? 'active' : ''); node.setAttribute('role', 'tab'); node.setAttribute('aria-selected', String(selected)); return node; }
  function params(w, extra = {}, pin = true) {
    return new URLSearchParams({ database: w.db.config.name, oid: w.db.oid, revision: w.db.revision,
      system: w.system, ...(pin && w.stamp ? { stamp: w.stamp } : {}), ...extra });
  }
  async function read(w, path, extra, pin = true) {
    const e = epoch;
    try {
      const page = await get(`browser/${path}?${params(w, extra, pin)}`);
      if (active(w, e) && !w.stamp) w.stamp = page.stamp;
      return page;
    }
    catch (error) {
      if (error.code === '40001' || error.code === '3D000') w.changed = true;
      throw error;
    }
  }
  function errorView(w, target, error, retry) {
    replace(target, el('div', { class: 'notice warning', role: 'alert' }, error.message,
      button(w.changed ? '重新加载当前目录' : '重试读取', w.changed ? () => reload(w) : retry)));
  }
  function pairList(values) {
    return el('dl', { class: 'metadata-facts' }, Object.entries(values).map(([label, value]) => [el('dt', {}, label), el('dd', {}, unknown(value))]));
  }
  function paged(headers, rows) {
    let page = 0; const host = el('div');
    const render = () => replace(host, table(headers, rows.slice(page * 50, (page + 1) * 50).map(row => typeof row === 'function' ? row() : row)),
      el('div', { class: 'browser-pager' }, `共 ${rows.length} 项 · 第 ${page + 1} 页`,
        button('上一页', () => { page--; render(); }, '', false, page === 0),
        button('下一页', () => { page++; render(); }, '', false, (page + 1) * 50 >= rows.length)));
    render(); return host;
  }
  async function reload(w) {
    const e = epoch;
    const latest = await get(`virtual-databases/${encodeURIComponent(w.db.config.name)}`);
    if (!active(w, e)) return;
    if (latest.oid !== w.db.oid) throw new Error('该数据库已删除或被同名新库替换，请返回数据库列表重新进入。');
    w.db = latest;
    w.stamp = null; w.changed = false; w.offset = 0; w.detail = null; w.sqlObject = null;
    if (w.tab !== 'objects') { render(w); return; }
    await loadSchemas(w); if (active(w)) renderObjectArea(w);
  }
  async function loadSchemas(w) {
    const e = epoch, serial = ++w.schemaSerial;
    if (root.querySelector('#object-area')) replace(root.querySelector('#object-area'), note('正在读取目录…'));
    try {
      const page = await read(w, 'schemas', { offset: 0 });
      if (!active(w, e) || serial !== w.schemaSerial) return;
      w.stamp = page.stamp;
      if (!w.schema && page.schemas.length) w.schema = page.schemas[0].name;
      if (!w.loaded) { w.loaded = true; renderObjectArea(w); }
    } catch (error) { if (active(w, e) && serial === w.schemaSerial && root.querySelector('#object-area')) errorView(w, root.querySelector('#object-area'), error, () => loadSchemas(w)); }
  }
  function renderObjectArea(w) {
    if (w.tab !== 'objects') return;
    const host = root.querySelector('#object-area');
    if (!host) return;
    if (w.detail) { renderDetail(w); return; }
    const search = el('input', { type: 'search', value: w.search, placeholder: '搜索对象名称', 'aria-label': '搜索对象名称' });
    let debounce;
    search.addEventListener('input', () => { w.search = search.value; w.offset = 0; clearTimeout(debounce); debounce = setTimeout(() => { if (active(w)) loadObjects(w); }, 250); });
    const kind = el('select', { 'aria-label': '对象类型' }, el('option', { value: '' }, '全部类型'), el('option', { value: 'table' }, '表'), el('option', { value: 'view' }, '视图'));
    kind.value = w.kind; kind.addEventListener('change', () => { w.kind = kind.value; w.offset = 0; loadObjects(w); });
    replace(host, el('div', { class: 'panel-heading' }, el('div', {}, el('h3', {}, w.schema || '选择 Schema')),
      el('div', { class: 'toolbar' }, search, kind)), el('div', { id: 'object-rows' }));
    if (w.schema) loadObjects(w);
  }
  async function loadObjects(w) {
    const e = epoch, serial = ++w.objectSerial, host = root.querySelector('#object-rows');
    if (!host) return;
    replace(host, note('正在读取对象…'));
    try {
      const page = await read(w, 'objects', { schema: w.schema, search: w.search, kind: w.kind, offset: w.offset });
      if (!active(w, e) || serial !== w.objectSerial || w.detail) return;
      replace(host, page.objects.length ? table(['对象名称', '类型', '列数', '访问'], page.objects.map(object => el('tr', {},
        el('td', {}, button(object.name, () => openObject(w, object.name), 'link')), el('td', {}, object.source_type?.toUpperCase().includes('VIEW') || ['v', 'm'].includes(object.kind) ? '视图' : '表'),
        el('td', {}, object.columns), el('td', {}, el('span', { class: 'badge' }, object.metadata_conflict ? '来源事实冲突' : `只读 · ${object.contributors ?? 1} 个贡献`))))) : note('此 Schema 中没有匹配的可见对象。'),
        el('div', { class: 'browser-pager' }, `第 ${Math.floor(w.offset / 50) + 1} 页 · 每页最多 50 项`,
          button('上一页', () => { w.offset -= 50; loadObjects(w); }, '', false, w.offset === 0),
          button('下一页', () => { w.offset += 50; loadObjects(w); }, '', false, !page.has_more)));
    } catch (error) { if (active(w, e) && serial === w.objectSerial) errorView(w, host, error, () => loadObjects(w)); }
  }
  async function openObject(w, name) {
    const e = epoch, serial = ++w.objectSerial, host = root.querySelector('#object-area');
    w.detail = null; replace(host, note('正在读取对象详情…'));
    try {
      const detail = await read(w, 'object', { schema: w.schema, object: name });
      if (!active(w, e) || serial !== w.objectSerial) return;
      w.detail = detail; w.detailTab = 'overview'; w.sourceView = false; renderDetail(w);
    } catch (error) { if (active(w, e) && serial === w.objectSerial) errorView(w, host, error, () => openObject(w, name)); }
  }
  function renderDetail(w) {
    const d = w.detail, o = d.object, m = o?.metadata ?? {}, host = root.querySelector('#object-area');
    if (!host) return;
    const sourceName = sources().find(s => s.id === d.source?.id)?.catalog_alias ?? d.source?.id;
    const names = d.external && !o ? [['overview', '概览'], ['columns', '列']] : [['overview', '概览'], ['columns', '列'], ['constraints', d.external ? '来源约束' : '约束'], ['indexes', d.external ? '来源索引' : '索引'], ['definition', '定义'], ['statistics', '统计'], ['source', '来源与状态']];
    const body = el('div', { class: 'panel-body' });
    const back = () => { if (w.sourceView) { w.sourceView = false; renderDetail(w); } else { w.detail = null; renderObjectArea(w); } };
    replace(host, el('div', { class: 'panel-heading' }, el('div', {}, button(w.sourceView ? '← 返回数据库对象' : '← 返回对象列表', back, 'link'),
      el('h3', {}, w.sourceView ? `${sourceName}.${d.source.schema}.${d.name}` : `${w.db.config.name}.${d.schema}.${d.name}`)),
      el('span', { class: 'badge' }, w.sourceView ? '来源事实' : d.external ? '外部映射对象' : '系统目录')),
      el('details', { class: 'metadata-help' }, el('summary', {}, '元数据说明'), el('p', {}, d.external
        ? '约束、索引和统计来自来源快照；实际约束以远端来源为准。'
        : '当前数据库的 PostgreSQL 系统目录投影。')),
      d.metadata_conflict ? el('div', { class: 'notice warning' }, '多个登记的来源事实不一致；当前按有权限的来源 ID 顺序选择执行路由和目录事实。请刷新相关来源核对。') : null,
      d.source?.stale ? el('div', { class: 'notice warning' }, '来源元数据已过期；当前仍展示最后一次发布的快照。') : null,
      el('div', { class: 'browser-tabs', role: 'tablist', 'aria-label': '元数据详情' }, names.map(([id, label]) => tab(label, w.detailTab === id, () => { w.detailTab = id; renderDetail(w); }))), body);
    const kinds = { f: '外部表', r: '表', p: '分区表', v: '视图', m: '物化视图' };
    switch (w.detailTab) {
      case 'overview': body.append(pairList({ '数据库': w.sourceView ? sourceName : w.db.config.name, 'Schema': w.sourceView ? d.source.schema : d.schema, '名称': d.name, 'OID': w.sourceView ? m.oid : d.oid, '类型': w.sourceView ? o.object_type : (kinds[d.kind] ?? d.kind), '备注': m.comment, '列数': o?.columns.length ?? d.columns?.rows.length })); break;
      case 'columns': {
        if (!o) { body.append(table(['列名', '类型 OID', '非空'], (d.columns?.rows ?? []).map(row => el('tr', {}, row.map(v => el('td', {}, unknown(v))))))); break; }
        body.append(paged(['列名', '来源类型', '映射类型', '可空', '默认 / 生成表达式', '备注', '更多'], o.columns.map(c => () => {
          const p = m.columns?.[c.name] ?? {};
          return el('tr', {}, [c.name, c.remote_type, c.mapped_type, c.nullable ? '是' : '否', p.default?.source_text ?? p.generated?.source_text, p.comment].map(v => el('td', {}, unknown(v))), el('td', {}, el('details', {}, el('summary', {}, '属性'), code(p))));
        }))); break;
      }
      case 'constraints': body.append(paged(['名称', '类型', '列', '来源定义 / 引用', '来源属性'], (m.constraints ?? []).map(c => () => el('tr', {},
        [c.name, c.kind, c.columns?.join(', '), c.definition?.source_text ?? (c.foreign_key ? `${c.foreign_key.schema}.${c.foreign_key.table} · ${c.foreign_key.reference_state}` : null)].map(v => el('td', {}, unknown(v))), el('td', {}, el('details', {}, el('summary', {}, '属性 / 引用详情'), code(c))))))); break;
      case 'indexes': body.append(paged(['名称', '方法', '唯一', '键 / 表达式', '来源定义 / 属性'], (m.indexes ?? []).map(i => () => el('tr', {}, [i.name, i.method, i.unique, i.keys?.map(k => k.column ?? k.expression?.source_text ?? '未知').join(', ')].map(v => el('td', {}, unknown(v))), el('td', {}, i.definition?.source_text ?? '未采集定义', el('details', {}, el('summary', {}, '排序 / 包含列 / 条件等属性'), code(i))))))); break;
      case 'definition': body.append(note('来源 SQL 与 PostgreSQL 译文分别展示，不自动执行。'), pairList({ '来源方言': m.view_definition?.source_dialect, '翻译状态': m.view_definition?.translation_status }), el('h4', {}, '来源定义'), code(m.view_definition?.source_text), el('h4', {}, 'PostgreSQL 译文'), code(m.view_definition?.pg_text)); break;
      case 'statistics':
        if (d.external && !w.sourceView) body.append(note('当前外部映射不拥有独占的本地物理存储。来源统计需按来源口径查看。'), pairList({ '本地表大小': null, '本地索引大小': null }), button('查看来源统计', () => { w.sourceView = true; renderDetail(w); }));
        else body.append(note('来源估算及采集时的物理统计；未知值保持未知，不等同于实时 COUNT。'), pairList(m.statistics ?? {}), el('h4', {}, '存储'), pairList(m.storage ?? {}), el('h4', {}, '活动'), pairList(m.activity ?? {}));
        break;
      case 'source': body.append(pairList({ '执行来源': sourceName, '全部贡献来源': d.contributors?.map(c => c.id).join(', '), '来源 Schema': d.source?.schema, '快照': d.source?.snapshot_id, '采集时间': d.source?.refreshed_at, '过期': d.source?.stale }),
        d.external ? button('查看来源对象', () => { w.sourceView = true; w.detailTab = 'overview'; renderDetail(w); }) : null,
        el('h4', {}, '分项采集状态'), code(m.status ?? m.statuses)); break;
    }
    if (d.external && ['constraints', 'indexes'].includes(w.detailTab)) body.prepend(note(`采集状态：${unknown(m.status?.[w.detailTab] ?? m.statuses?.[w.detailTab])}；空列表不代表来源不存在该能力。`));
  }
  function insertSql(w, text, statement = false) {
    const editor = root.querySelector('.sql-editor');
    if (!editor) return;
    const start = editor.selectionStart, end = editor.selectionEnd;
    const prefix = statement && start > 0 && editor.value[start - 1] !== '\n' ? '\n' : '';
    const suffix = statement && end < editor.value.length && editor.value[end] !== '\n' ? '\n' : '';
    editor.setRangeText(prefix + text + suffix, start, end, 'end');
    w.sql = editor.value; editor.focus();
  }
  function renderSqlSelection(w) {
    const host = root.querySelector('#sql-selection'); if (!host) return;
    const object = w.sqlObject;
    const parts = object ? [object.schema, object.name, ...(object.column ? [object.column] : [])] : w.schema ? [w.schema] : [];
    replace(host, el('span', { class: 'sql-selected-object', title: parts.join('.') }, parts.join('.') || '未选择对象'),
      button('插入名称', () => insertSql(w, object?.column ? quote(object.column) : parts.map(quote).join('.')), 'small', false, !parts.length),
      button('插入 SELECT', () => insertSql(w, `SELECT * FROM ${quote(object.schema)}.${quote(object.name)} LIMIT 100;`, true), 'small', false, !object));
  }
  function renderSql(w) {
    const host = root.querySelector('#sql-area');
    const editor = el('textarea', { class: 'sql-editor', spellcheck: 'false', 'aria-label': 'SQL 编辑器', placeholder: 'SELECT 1;' }); editor.value = w.sql;
    editor.addEventListener('input', () => { w.sql = editor.value; });
    editor.addEventListener('keydown', event => { if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') { event.preventDefault(); run(w, editor); } });
    const examples = el('select', { 'aria-label': '示例 SQL', disabled: Boolean(w.busy) }, el('option', { value: '' }, '插入示例 SQL'), el('option', { value: 'context' }, '当前连接'), el('option', { value: 'values' }, 'NULL 与空字符串'), w.detail ? el('option', { value: 'object' }, '当前对象采样') : null);
    examples.addEventListener('change', () => {
      const queries = { context: 'SELECT current_database(), current_schema();', values: "SELECT NULL::TEXT AS null_value, ''::TEXT AS empty_value, 9223372036854775807::BIGINT AS exact_value;", object: w.detail ? `SELECT * FROM ${quote(w.detail.schema)}.${quote(w.detail.name)} LIMIT 100;` : '' };
      if (queries[examples.value]) { w.sql = queries[examples.value]; editor.value = w.sql; editor.focus(); } examples.value = '';
    });
    replace(host, el('div', { id: 'sql-selection', class: 'sql-selection' }),
      editor, el('div', { class: 'sql-toolbar' }, button('执行 SQL', () => run(w, editor), 'primary', false, Boolean(w.busy)),
        button(w.job?.status === 'canceling' ? '正在取消…' : '停止', () => cancel(w), 'danger', false, !w.busy || w.job?.status === 'canceling'), examples,
        el('details', { class: 'sql-help' }, el('summary', {}, '使用说明'), el('p', {}, 'Ctrl / ⌘ + Enter 执行选中文本或整个编辑器。双击对象或列可插入名称。每个数据库独立保留草稿；切换数据库会停止当前查询。'),
          el('p', {}, `仅支持单条只读查询，最多 1000 行 / 2 MiB、60 秒；不支持写入、DDL、事务与 SET。执行账号：${w.db.config.owner}。`))), el('div', { id: 'sql-output', 'aria-live': 'polite' }));
    renderSqlSelection(w);
    renderResult(w); if (w.busy) poll(w);
  }
  async function run(w, editor) {
    if (w.busy) return;
    const sql = editor.value.slice(editor.selectionStart, editor.selectionEnd).trim() || editor.value.trim();
    if (!sql) return toast('请输入 SQL。', true);
    w.sql = editor.value; w.busy = true; w.cancelRequested = false; w.uncertain = false; w.pollError = null; w.resultPage = 0; w.resultTab = 'rows';
    w.submission = { id: crypto.randomUUID(), body: { database: w.db.config.name, oid: w.db.oid, revision: w.db.revision, sql, issued_at: Math.floor(Date.now() / 1000) } };
    w.job = { status: 'submitting' }; renderSql(w); await submit(w);
  }
  async function submit(w) {
    try {
      w.job = (await request(`browser/queries/${w.submission.id}`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(w.submission.body) })).body;
      w.uncertain = false;
      if (w.cancelRequested) { await cancel(w); return; }
      if (active(w) && w.tab === 'sql') renderSql(w); poll(w);
    } catch (error) {
      w.uncertain = error.uncertain; if (!error.uncertain) w.busy = false;
      w.job = { status: error.uncertain ? 'uncertain' : 'failed', error: { code: error.code, message: error.message } };
      if (active(w) && w.tab === 'sql') renderSql(w);
    }
  }
  async function cancel(w) {
    w.cancelRequested = true;
    if (!w.submission) return;
    if (w.job?.status === 'submitting') return;
    try {
      w.job = (await request(`browser/queries/${w.submission.id}/cancel`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{}' })).body;
      if (active(w) && w.tab === 'sql') renderResult(w); poll(w);
    } catch (error) { toast(error.message, true); }
  }
  function poll(w) {
    clearTimeout(w.timer);
    if (current !== w || !w.busy || w.uncertain || !w.submission || w.job?.status === 'submitting') return;
    const id = w.submission.id;
    w.timer = setTimeout(async () => {
      try {
        const job = await get(`browser/queries/${id}`);
        if (w.submission?.id !== id) return;
        w.job = job;
        w.busy = ['running', 'canceling'].includes(w.job.status);
        w.pollError = null;
        if (active(w) && w.tab === 'sql') { if (w.busy) renderResult(w); else renderSql(w); }
      } catch (error) {
        w.pollError = error.message;
        if (error.code === '42704') w.busy = false;
        if (active(w) && w.tab === 'sql') { if (w.busy) renderResult(w); else renderSql(w); }
      }
      if (w.busy && current === w) poll(w);
    }, 400);
  }
  function renderResult(w) {
    const host = root.querySelector('#sql-output'); if (!host) return;
    const stop = root.querySelector('.sql-toolbar .danger');
    if (stop) { stop.textContent = w.job?.status === 'canceling' ? '正在取消…' : '停止'; stop.disabled = !w.busy || w.job?.status === 'canceling'; }
    const job = w.job, result = job?.result;
    const statuses = { submitting: '提交中', uncertain: '提交结果待确认', running: '执行中', canceling: '取消中', canceled: '查询已取消 / 超时', succeeded: '执行成功', failed: '执行失败' };
    if (!job) { replace(host, el('div', { class: 'sql-empty' }, '暂无结果')); return; }
    const rowsTab = tab('结果', w.resultTab === 'rows', () => { w.resultTab = 'rows'; renderResult(w); });
    const messagesTab = tab('消息', w.resultTab === 'messages', () => { w.resultTab = 'messages'; renderResult(w); });
    const content = el('div', { class: 'sql-result' });
    replace(host, el('div', { class: 'sql-status' }, statuses[job.status] ?? job.status, ` · ${result?.elapsed_ms ?? job.elapsed_ms ?? '—'} ms · ${result?.rows.length ?? 0} 行`),
      w.uncertain ? note('请用原提交重试确认结果，不会自动重复执行。') : null,
      w.uncertain ? button('使用原提交重试', () => submit(w)) : null,
      w.pollError ? el('div', { class: 'notice warning' }, w.pollError) : null,
      result?.truncated ? el('div', { class: 'notice warning' }, '结果达到 1000 行或 2 MiB 展示上限，已停止读取；当前结果不完整。') : null,
      job.error ? el('div', { class: 'notice warning', role: 'alert' }, `SQLSTATE ${job.error.code ?? '—'} · ${job.error.message}`) : null,
      el('div', { class: 'browser-tabs', role: 'tablist', 'aria-label': 'SQL 输出' }, rowsTab, messagesTab), content);
    if (w.resultTab === 'messages') { content.append(code([job.error ? `SQLSTATE ${job.error.code ?? '—'}: ${job.error.message}` : statuses[job.status], ...(result?.notices ?? [])].join('\n'))); return; }
    if (!result) { if (w.busy) content.append(el('div', { class: 'sql-empty' }, '等待查询结果…')); return; }
    const start = w.resultPage * 50;
    content.append(table(result.columns.map(c => `${c.name} · ${c.type_name} (${c.type_oid})`), result.rows.slice(start, start + 50).map(row => el('tr', {}, row.map(cell => el('td', { class: cell === null ? 'sql-null' : '', title: cell === '' ? '空字符串' : null }, cell === null ? 'NULL' : cell === '' ? '""' : cell))))),
      el('div', { class: 'browser-pager' }, `第 ${w.resultPage + 1} 页`, button('上一页', () => { w.resultPage--; renderResult(w); }, '', false, start === 0), button('下一页', () => { w.resultPage++; renderResult(w); }, '', false, start + 50 >= result.rows.length)));
  }
  function render(w) {
    epoch++; clearTimeout(w.timer);
    const switchTab = tab => { w.tab = tab; lastTab = tab; render(w); };
    replace(root, el('div', { class: 'browser-tabs workspace-tabs', role: 'tablist', 'aria-label': '数据库工作区' },
      tab('对象浏览', w.tab === 'objects', () => switchTab('objects')), tab('SQL 调试', w.tab === 'sql', () => switchTab('sql'))),
      w.tab === 'objects' ? el('section', { id: 'object-area', class: 'panel' }) : el('section', { id: 'sql-area', class: 'panel panel-body' }));
    if (w.tab === 'objects') { if (w.schema) renderObjectArea(w); else { w.loaded = false; loadSchemas(w); } }
    else renderSql(w);
  }
  return {
    mount(host, database, tab) {
      root = host; current = workspaces.get(database.oid);
      if (!current || current.db.revision !== database.revision) {
        current = { db: database, tab: current?.tab ?? 'objects', sql: current?.sql ?? 'SELECT 1 AS connected;', schema: '', stamp: null,
          offset: 0, search: '', kind: '', system: showSystem, schemaSerial: 0, objectSerial: 0, resultPage: 0, resultTab: 'rows' };
      }
      workspaces.delete(database.oid);
      if (workspaces.size >= 8) workspaces.delete(workspaces.keys().next().value);
      workspaces.set(database.oid, current);
      if (tab) current.tab = tab;
      lastTab = current.tab;
      render(current);
    },
    mode() { return current?.tab ?? lastTab; },
    system(value) {
      showSystem = value;
      for (const w of workspaces.values()) {
        w.system = value; w.stamp = null; w.detail = null;
        if (!value && /^(pg_catalog$|information_schema$|pg_toast|pg_temp_)/.test(w.schema)) {
          w.schema = ''; w.sqlObject = null;
        }
      }
      if (current) render(current);
    },
    select({ schema, object, column, action, system }) {
      if (!current) return;
      const w = current;
      w.system = system;
      if (action && w.tab !== 'sql') { w.tab = lastTab = 'sql'; render(w); }
      if (schema) { w.schema = schema; w.loaded = true; w.offset = 0; w.search = ''; w.detail = null; }
      w.sqlObject = object ? { schema, name: object, column } : null;
      if (w.tab === 'sql') {
        renderSqlSelection(w);
        if (action === 'insert') insertSql(w, column ? quote(column) : [schema, object].filter(Boolean).map(quote).join('.'));
      } else if (object) openObject(w, object);
      else if (schema) renderObjectArea(w);
    },
    leave() { epoch++; clearTimeout(current?.timer); if (current?.busy && current.submission) cancel(current); current = null; root = null; },
    reload() { if (current) return reload(current); },
    refresh() {
      const ids = [...new Set(current.db.config.schemas.map(s => s.source))];
      refreshSource(ids);
    },
  };
}
