// Directory shared by object browsing and SQL debugging; each database pins its own catalog stamp.
export function createNavigator({ el, button, replace, get, toast, host, open }) {
  const records = new Map();
  let filter = '', showSystem = false, selection = null;
  const slot = () => ({ rows: [], offset: 0, more: false, loaded: false, loading: false, open: false });
  const fresh = database => ({ database, schemas: slot(), objects: new Map(), columns: new Map(), open: false });
  const alive = record => records.get(record.database.oid) === record;
  function choose(record, schema, object, column, action) {
    selection = { oid: record.database.oid, schema, object, column };
    open({ database: record.database, schema, object, column, action, system: showSystem });
    // Keep the clicked node mounted so a second native click can produce dblclick.
    syncSelection();
  }
  function syncSelection() {
    const key = JSON.stringify([selection?.oid, selection?.schema, selection?.object, selection?.column]);
    for (const row of host.querySelectorAll('[data-selection]')) row.classList.toggle('selected', row.dataset.selection === key);
    for (const database of host.querySelectorAll('[data-oid]')) {
      const selected = database.dataset.oid === String(selection?.oid);
      const row = database.querySelector('.database-row'), name = row.querySelector('.directory-name');
      row.classList.toggle('current', selected);
      if (selected) name.setAttribute('aria-current', 'true'); else name.removeAttribute('aria-current');
    }
  }
  function reset(record) {
    const next = fresh(record.database); next.open = record.open;
    records.set(record.database.oid, next);
    if (next.open) load(next, next.schemas, 'schemas');
    render();
  }
  async function load(record, target, path, extra = {}, more = false, step = 100) {
    if (target.loading) return;
    target.loading = true; target.error = null; render();
    try {
      const parameters = new URLSearchParams({ database: record.database.config.name, oid: record.database.oid,
        revision: record.database.revision, system: showSystem, ...(record.stamp ? { stamp: record.stamp } : {}),
        ...extra, offset: more ? target.offset + step : 0 });
      const page = await get(`browser/${path}?${parameters}`);
      if (!alive(record)) return;
      record.stamp = page.stamp;
      let rows;
      if (path === 'object') rows = page.object?.columns ?? (page.columns?.rows ?? []).map(row => ({ name: row[0], remote_type: row[1] }));
      else rows = page[path];
      target.rows = more ? [...target.rows, ...rows] : rows;
      target.offset = page.offset ?? 0; target.more = Boolean(page.has_more); target.loaded = true;
    } catch (error) {
      if (alive(record)) { target.error = error.message; record.changed ||= ['40001', '3D000'].includes(error.code); }
    } finally { target.loading = false; if (alive(record)) render(); }
  }
  function retry(record, target, callback) {
    return el('div', { class: 'directory-error', role: 'alert' }, target.error,
      button(record.changed ? '重新加载目录' : '重试', record.changed ? () => reset(record) : callback, 'link'));
  }
  function toggle(record, target, path, extra, step) {
    target.open = !target.open;
    if (target.open && (!target.loaded || target.error)) load(record, target, path, extra, false, step);
    else render();
  }
  function caret(label, expanded, callback) {
    const node = button(expanded ? '▾' : '▸', callback, 'tree-toggle');
    node.setAttribute('aria-label', label); node.setAttribute('aria-expanded', String(expanded));
    return node;
  }
  function nameButton(text, label, callback) {
    const node = button(text, callback, 'link directory-name'); node.title = text;
    node.setAttribute('aria-label', label); return node;
  }
  function objectNode(record, schema, object) {
    const key = JSON.stringify([schema, object.name]);
    const columns = record.columns.get(key) ?? slot(); record.columns.set(key, columns);
    const selected = selection?.oid === record.database.oid && selection.schema === schema && selection.object === object.name;
    const label = nameButton(object.name, `选择对象 ${schema}.${object.name}`, () => choose(record, schema, object.name));
    label.addEventListener('dblclick', () => choose(record, schema, object.name, null, 'insert'));
    const view = object.source_type?.toUpperCase().includes('VIEW') || ['v', 'm'].includes(object.kind);
    return el('div', { class: 'directory-object' },
      el('div', { class: `directory-row ${selected && !selection.column ? 'selected' : ''}`, 'data-selection': JSON.stringify([record.database.oid, schema, object.name, null]) },
        caret(`展开或收起列 ${schema}.${object.name}`, columns.open, () => toggle(record, columns, 'object', { schema, object: object.name })),
        el('span', { class: 'directory-icon', 'aria-hidden': 'true' }, view ? '▧' : '▤'), label),
      columns.open ? el('div', { class: 'directory-columns' },
        columns.error ? retry(record, columns, () => load(record, columns, 'object', { schema, object: object.name })) :
          columns.loading ? el('small', {}, '读取中…') : columns.rows.length ? columns.rows.slice(0, columns.shown ?? 50).map(column => {
            const node = nameButton(column.name, `选择列 ${schema}.${object.name}.${column.name}`, () => choose(record, schema, object.name, column.name));
            node.addEventListener('dblclick', () => choose(record, schema, object.name, column.name, 'insert'));
            return el('div', { class: `directory-row ${selected && selection.column === column.name ? 'selected' : ''}`, 'data-selection': JSON.stringify([record.database.oid, schema, object.name, column.name]) },
              el('span', { class: 'directory-icon', 'aria-hidden': 'true' }, '·'), node,
              el('small', { class: 'column-type', title: column.remote_type ?? '' }, column.remote_type ?? ''));
          }) : el('small', {}, '暂无列'),
        columns.rows.length > (columns.shown ?? 50) ? button('显示更多列', () => { columns.shown = (columns.shown ?? 50) + 50; render(); }, 'link') : null) : null);
  }
  function schemaNode(record, schema) {
    const objects = record.objects.get(schema.name) ?? slot(); record.objects.set(schema.name, objects);
    const selected = selection?.oid === record.database.oid && selection.schema === schema.name && !selection.object;
    return el('div', { class: 'directory-schema' }, el('div', { class: `directory-row ${selected ? 'selected' : ''}`, 'data-selection': JSON.stringify([record.database.oid, schema.name, null, null]) },
      caret(`展开或收起 Schema ${schema.name}`, objects.open, () => toggle(record, objects, 'objects', { schema: schema.name }, 50)),
      el('span', { class: 'directory-icon', 'aria-hidden': 'true' }, '▦'),
      nameButton(schema.name, `选择 Schema ${schema.name}`, () => choose(record, schema.name)),
      schema.stale ? el('small', { class: 'directory-stale' }, '过期') : null),
      objects.open ? el('div', { class: 'directory-objects' },
        objects.error ? retry(record, objects, () => load(record, objects, 'objects', { schema: schema.name }, false, 50)) :
          objects.rows.length ? objects.rows.map(object => objectNode(record, schema.name, object)) : el('small', {}, objects.loading ? '读取中…' : '空 Schema'),
        objects.more ? button('加载更多对象', () => load(record, objects, 'objects', { schema: schema.name }, true, 50), 'link', false, objects.loading) : null) : null);
  }
  function expandDatabase(record) {
    record.open = !record.open;
    // Bound retained directory payloads while allowing the complete database list.
    const retained = [...records.values()].filter(r => r !== record && (r.open || r.schemas.loaded || r.schemas.loading));
    while (record.open && retained.length >= 8) {
      const old = retained.shift(); records.set(old.database.oid, fresh(old.database));
    }
    if (record.open && (!record.schemas.loaded || record.schemas.error)) load(record, record.schemas, 'schemas');
    else render();
  }
  function render() {
    const visible = [...records.values()].filter(r => r.database.config.name.toLocaleLowerCase().includes(filter.toLocaleLowerCase()))
      .sort((a, b) => a.database.config.name.localeCompare(b.database.config.name));
    replace(host, visible.map(record => {
      const database = record.database, selected = selection?.oid === database.oid;
      const name = nameButton(database.config.name, `打开数据库 ${database.config.name}`, () => {
        if (!database.config.allow_connections) return toast('该数据库当前禁止连接。', true);
        if (!record.open) expandDatabase(record);
        choose(record);
      });
      name.disabled = !database.config.allow_connections;
      if (selected) name.setAttribute('aria-current', 'true');
      return el('div', { class: 'directory-database', 'data-database': database.config.name, 'data-oid': database.oid },
        el('div', { class: `directory-row database-row ${selected ? 'current' : ''}` },
          caret(`展开或收起数据库 ${database.config.name}`, record.open, () => expandDatabase(record)),
          el('span', { class: 'directory-icon', 'aria-hidden': 'true' }, '▱'), name),
        record.open ? el('div', { class: 'directory-schemas' },
          record.schemas.error ? retry(record, record.schemas, () => load(record, record.schemas, 'schemas')) :
            record.schemas.rows.length ? record.schemas.rows.map(schema => schemaNode(record, schema)) : el('small', {}, record.schemas.loading ? '读取中…' : '暂无 Schema'),
          record.schemas.more ? button('加载更多 Schema', () => load(record, record.schemas, 'schemas', {}, true), 'link', false, record.schemas.loading) : null) : null);
    }), visible.length ? null : el('small', {}, records.size ? '没有匹配的数据库' : '暂无数据库'));
  }
  return {
    update(databases) {
      const ids = new Set(databases.map(d => d.oid));
      for (const id of records.keys()) if (!ids.has(id)) records.delete(id);
      for (const database of databases) {
        const old = records.get(database.oid);
        if (!old || old.database.revision !== database.revision) {
          const next = fresh(database); next.open = Boolean(old?.open);
          records.set(database.oid, next);
          if (next.open) load(next, next.schemas, 'schemas');
        } else old.database = database;
      }
      render();
    },
    select(database) {
      if (selection?.oid !== database.oid) selection = { oid: database.oid };
      const record = records.get(database.oid);
      if (record && !record.open) expandDatabase(record);
      render();
    },
    search(value) { filter = value; render(); },
    system(value) { showSystem = value; for (const record of [...records.values()]) reset(record); },
    reload() { for (const record of [...records.values()]) reset(record); },
  };
}
