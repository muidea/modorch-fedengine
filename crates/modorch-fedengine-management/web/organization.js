// Instance discovery is read-only. Each reviewed target uses the durable source workflow.
export const DIALECTS = [
  { value: 'postgres', label: 'PostgreSQL', icon: 'PG', port: '5432', family: 'pg', bootstrap: 'postgres', requireDb: false },
  { value: 'greenplum', label: 'Greenplum', icon: 'GP', port: '5434', family: 'pg', bootstrap: 'postgres', requireDb: false },
  { value: 'kingbasees', label: 'KingbaseES', icon: 'KB', port: '54321', family: 'pg', bootstrap: 'test', requireDb: false },
  { value: 'mysql', label: 'MySQL', icon: 'MY', port: '3306', family: 'mysql', requireDb: false },
  { value: 'mariadb', label: 'MariaDB', icon: 'MA', port: '3306', family: 'mysql', requireDb: false },
  { value: 'starrocks', label: 'StarRocks', icon: 'SR', port: '9030', family: 'mysql', requireDb: false },
  { value: 'oracle', label: 'Oracle', icon: 'OR', port: '1521', family: 'odbc', requireDb: true },
  { value: 'sqlserver', label: 'SQL Server', icon: 'SS', port: '1433', family: 'sqlserver', bootstrap: 'master', requireDb: false },
  { value: 'dameng', label: 'Dameng (DM8)', icon: 'DM', port: '5236', family: 'odbc', requireDb: true },
  { value: 'db2', label: 'IBM DB2', icon: 'DB', port: '50000', family: 'odbc', requireDb: true },
  { value: 'hana', label: 'SAP HANA', icon: 'HA', port: '39017', family: 'odbc', requireDb: true },
];
export function dialectMeta(kind) {
  return DIALECTS.find(item => item.value === kind) ?? { value: kind, label: kind, icon: String(kind ?? 'DB').slice(0, 2).toUpperCase(), port: '5432', family: 'pg', requireDb: false };
}
export function createRegistration({ el, button, field, check, table, openDialog, request, get, write, reload, lines, toast, sources }) {
  const value = (body, name) => body.querySelector(`[name="${name}"]`).value.trim();
  const pause = () => new Promise(resolve => setTimeout(resolve, 300));
  async function waitOperation(operation) {
    while (!['Succeeded', 'Failed', 'Interrupted'].includes(operation.status)) {
      await pause(); operation = await get(`operations/${encodeURIComponent(operation.id)}`);
    }
    if (operation.status !== 'Succeeded') throw new Error(operation.error ?? operation.status);
    return operation;
  }
  async function hash(text) {
    return [...new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text)))].map(b => b.toString(16).padStart(2, '0')).join('');
  }
  return function register(source = null) {
    const target = source?.desired.definition.organization;
    const definition = source?.desired.definition;
    const initialKind = source?.kind ?? 'postgres';
    const initialMeta = dialectMeta(initialKind);
    const kind = el('select', { name: 'kind' }, ...DIALECTS.map(d => el('option', { value: d.value }, d.label)));
    const body = el('div', {}, el('details', { class: 'small-note' }, el('summary', {}, '填写说明'), el('p', {}, '凭据引用指向含 username、password 及可选 options 的 JSON。Oracle / Dameng / DB2 / HANA 需填写源数据库 / 服务名；填写连接后发现数据库，核对范围和数据库名，再激活。')),
      el('div', { class: 'form-grid' },
        field('连接登记 ID', 'connection_id', target?.connection_id ?? '', { required: true, placeholder: '例如 crm_pg' }),
        el('label', { class: 'field' }, '产品 / 协议', kind),
        field('实例主机', 'host', target?.instance.host ?? '', { required: true, placeholder: '192.168.235.50' }),
        field('端口', 'port', target?.instance.port ?? initialMeta.port, { required: true, type: 'number', min: 1, max: 65535 }),
        field('源数据库（可选 / Oracle·ODBC必填）', 'database', '', { hint: '留空枚举该实例；Oracle / Dameng / DB2 / HANA 需填写库名或服务名。' }),
        field('引导数据库（PG / SQLServer）', 'bootstrap_database', initialMeta.bootstrap ?? 'postgres', { hint: '仅用于 PG 家族或 SQL Server 实例枚举；不改变对外登录规则。' }),
        field('凭据引用', 'secret_ref', definition?.secret_ref ?? '', { required: true, placeholder: 'file:crm.json 或环境变量名' }),
        field('数据库名 / 多库名前缀', 'virtual_name', target?.connection_id ?? '', { required: true }),
        field('Owner 账号', 'owner', target?.owner ?? 'fedengine', { required: true }),
        field('其他 CONNECT 账号', 'connect', target?.connect.join(', ') ?? '', { hint: '逗号分隔；仍需单独配置 SELECT 授权。' }),
        field('显式实例身份（可选）', 'instance_identity', target?.instance.identity ?? '', { hint: '仅在确认代理 / 别名属于同一实例时填写。' })),
      check('来源只读', 'read_only', definition?.read_only ?? true));
    kind.value = initialKind;
    kind.addEventListener('change', () => {
      const meta = dialectMeta(kind.value);
      body.querySelector('[name=port]').value = meta.port;
      if (meta.bootstrap) body.querySelector('[name=bootstrap_database]').value = meta.bootstrap;
    });
    let job = null, submitted = null, busy = false;
    openDialog('添加数据源 · 连接实例', body, async () => {
      if (busy) return; busy = true;
      try {
        const meta = dialectMeta(kind.value);
        const connection = { id: value(body, 'connection_id'), kind: kind.value, instance: { host: value(body, 'host'), port: Number(value(body, 'port')), identity: value(body, 'instance_identity') || null }, secret_ref: value(body, 'secret_ref'), database: value(body, 'database') || null, bootstrap_database: (meta.family === 'pg' || meta.family === 'sqlserver') ? value(body, 'bootstrap_database') || null : null };
        if (meta.requireDb && !connection.database) throw new Error(`${meta.label} 需要填写源数据库 / 服务名。`);
        if (!/^(?:file:[A-Za-z0-9_.\/-]+|[A-Za-z_][A-Za-z0-9_]*)$/u.test(connection.secret_ref) || connection.secret_ref.includes('..')) throw new Error('请填写凭据引用，不能填写连接串或密码。');
        const input = { connection, issued_at: submitted?.issued_at ?? Math.floor(Date.now() / 1000) };
        if (submitted && JSON.stringify(submitted.connection) !== JSON.stringify(connection)) { job = null; input.issued_at = Math.floor(Date.now() / 1000); }
        submitted = input; job ??= crypto.randomUUID();
        let result = (await request(`discovery/${job}`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(input) })).body;
        const progress = el('div', { class: 'notice' }, '正在发现数据库…'); body.append(progress);
        const cancel = button('取消发现', () => request(`discovery/${job}/cancel`, { method: 'POST' }), 'small'); progress.append(cancel);
        while (result.status === 'running') { await pause(); result = await get(`discovery/${job}`); }
        progress.remove();
        if (result.status !== 'succeeded') throw new Error(result.error ?? result.status);
        if (!body.closest('dialog')?.open) return;
        review(connection, result.candidates, { name: value(body, 'virtual_name'), owner: value(body, 'owner'), connect: lines(value(body, 'connect')), readOnly: body.querySelector('[name=read_only]').checked });
      } finally { busy = false; }
    }, '探测并发现数据库');
  };
  function review(connection, candidates, settings) {
    const isMysqlFamily = dialectMeta(connection.kind).family === 'mysql';
    const entries = candidates.map(candidate => {
      const existing = sources().find(source => {
        const target = source.desired.definition.organization;
        return target && candidate.merged && target.virtual_database === candidate.virtual_database;
      })?.desired.definition.organization;
      const enabled = candidate.status === 'available';
      const selected = el('input', { type: 'checkbox', checked: enabled, disabled: !enabled, 'aria-label': `纳管 ${candidate.database}` });
      let suggested = connection.database ? settings.name : `${settings.name}_${candidate.database}`.slice(0, 63);
      if (candidate.merged) suggested = candidate.virtual_database;
      const name = el('input', { value: suggested, required: enabled, disabled: !enabled || candidate.merged, 'aria-label': `${candidate.database} 数据库名` });
      const schemas = el('input', { value: candidate.schemas.join(', '), disabled: !enabled || isMysqlFamily, 'aria-label': `${candidate.database} Schema 范围` });
      const status = el('span', {}, candidate.merged ? '合并到已有数据库' : ({available:'新目标', forbidden:'无权限', unavailable:'不可用', excluded:'系统库已排除'}[candidate.status] ?? candidate.status));
      const result = el('div', { class: 'subtext' }, candidate.error ?? '');
      return { candidate, existing, selected, name, schemas, status, result };
    });
    const body = el('div', {}, el('details', { class: 'small-note' }, el('summary', {}, '映射规则'), el('p', {}, '每个原生数据库独立映射为一个数据库；同一实例同一源库的登记合并。PostgreSQL / 其他库保留原 Schema；MySQL 家族保留源 database 名作为 Schema。不同端口或不同源库不合并。已排除或不可用的库不会登记。')),
      table(['纳管', '源数据库', '数据库', 'Schema 范围', '状态'], entries.map(e => el('tr', {}, el('td', {}, e.selected), el('td', {}, e.candidate.database), el('td', {}, e.name), el('td', {}, e.schemas), el('td', {}, e.status, e.result)))));
    let busy = false;
    openDialog('添加数据源 · 核对映射并激活', body, async () => {
      if (busy) return;
      const selected = entries.filter(e => e.selected.checked && e.candidate.status === 'available');
      if (!selected.length) throw new Error('请选择至少一个可管理的源数据库。');
      if (new Set(selected.map(e => e.name.value.trim().toLowerCase())).size !== selected.length) throw new Error('不同原生数据库需要独立的数据库名；同库来源复用已有名称。');
      busy = true;
      try {
        for (const e of selected) {
          try {
            e.result.textContent = '正在登记 / 激活…';
            const digest = await hash(JSON.stringify([connection.id, connection.kind, connection.instance.identity ?? [connection.instance.host.replace(/\.$/u, '').toLowerCase(), connection.instance.port], e.candidate.database]));
            const id = `${connection.id}_${digest.slice(0, 16)}`;
            const schemas = isMysqlFamily ? [e.candidate.database] : lines(e.schemas.value);
            const definition = { id, catalog_alias: `remote_${digest.slice(0, 24)}`, kind: connection.kind, secret_ref: connection.secret_ref, organization: { connection_id: connection.id, instance: connection.instance, database: e.candidate.database, virtual_database: e.name.value.trim(), owner: e.existing?.owner ?? settings.owner, connect: e.existing?.connect ?? settings.connect }, default_schema: schemas[0] ?? '', allowed_schemas: schemas, read_only: settings.readOnly, ddl_enabled: false };
            await reload();
            let source = sources().find(s => s.id === id);
            if (!source) { await waitOperation(await write('POST', 'sources', definition, `登记 ${e.candidate.database}`, true)); source = await get(`sources/${encodeURIComponent(id)}`); }
            if (source.current_operation) await waitOperation(await get(`operations/${source.current_operation}`));
            source = await get(`sources/${encodeURIComponent(id)}`);
            if (Object.keys(definition.organization).some(key => (key === 'instance' ? Object.keys(definition.organization.instance).some(part => source.desired.definition.organization?.instance?.[part] !== definition.organization.instance[part]) : JSON.stringify(source.desired.definition.organization?.[key]) !== JSON.stringify(definition.organization[key]))) || JSON.stringify(source.desired.definition.allowed_schemas) !== JSON.stringify(schemas) || source.desired.definition.secret_ref !== definition.secret_ref || source.desired.definition.read_only !== definition.read_only) throw new Error('该连接登记已存在不同目标配置；请从来源详情调整，或使用另一个连接登记 ID。');
            if (source.status !== 'Active' || source.applied_revision !== source.desired_revision) await waitOperation(await write('POST', `sources/${encodeURIComponent(id)}/apply`, {target:{incarnation:source.incarnation,expected_revision:source.desired_revision}}, `激活 ${e.candidate.database}`, true));
            e.result.textContent = '已激活'; e.selected.checked = false; e.selected.disabled = true;
          } catch (error) {
            e.result.textContent = error.message;
            if (error.uncertain) throw error;
          }
        }
        await reload(); toast('所选数据库处理完成，请逐项核对结果。');
      } finally { busy = false; }
    }, '登记并激活所选数据库');
  }
}
