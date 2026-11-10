# 在线数据源管理 API

默认配置使用 `./data/fedengine.duckdb`，管理入口为 `http://127.0.0.1:15433/api/v1`。业务 SQL 继续使用 PGWire。管理入口属于可信部署边界，不增加账号、角色或数据授权接口；跨机器部署时由部署侧控制入口访问。既有 SQL 授权和 DDL capability 校验继续生效，可在启动配置中为将来登记的 source id 预置既有 grants。

`[management]` 控制启用、监听地址、凭据目录、操作队列上限、并发数、排空等待和 HTTP 等待时限。启用时禁止 `:memory:`；关闭时仍可使用内存实例。队列满返回 `503`。排空超时不会强制卸载，操作保持 `WaitingForDrain`，关闭或完成挂起 Portal 后继续执行。

## 内置 Web 管理端

启用 `[management]` 后，浏览器访问同一监听端口的 `/manage/`（默认 `http://127.0.0.1:15433/manage/`）；根路径也提供该页面。HTML、CSS、JavaScript 编译进二进制，无 CDN、前端构建、Node 运行时或额外部署服务。

页面提供数据源列表及详情、登记、连接探测、Schema 发现、纳管范围与默认 Schema 配置、凭据引用更新、激活/应用、目录刷新与分页、停用和删除；业务虚拟库页支持创建、完整编辑映射、允许连接配置及删除。源 ID、类型和 catalog_alias 沿用现有接口的不可在线修改约束。账号、授权及 DDL capability 仍由服务配置管理，页面不会隐式授权。

使用流程：

1. 在凭据目录或服务进程环境中保存结构化凭据 JSON（`username`、`password`、可选 `options`），页面仅填写引用。
2. 点击“登记数据源”，填写连接登记 ID、产品、实例地址、可选源库和虚拟库名。源库留空时枚举实例；PG 多库输入名称前缀，下一步可逐项修改。
3. 核对逐库状态与 Schema 范围；系统库/不可用/无权限目标不可选择。已存在原生目标展示合并提示并锁定已有虚拟库名；MySQL 选定的源库共同映射到一个虚拟库。
4. 点击“登记并激活所选数据库”，逐目标展示结果。登记和激活均通过现有持久化操作，部分失败保留成功项；发现本身不会登记或激活任何目标。
5. 来源详情中的“发现数据库”重新枚举实例；“发现 Schema”和“刷新元数据”仍只作用于当前目标。范围与凭据引用修改仍需显式应用。
6. 自动虚拟库通过来源贡献维护。手工业务库用于显式账号同名入口和业务范围；owner/CONNECT 不授予 SELECT，SQL 调试与 PG 客户端共用查询与授权链路。

页面自动跟踪源当前操作及本浏览器会话最近 40 项操作；可输入操作编号找回服务端记录。这不是完整审计列表。刷新页面保留这些编号和待确认请求；不保存目录快照或明文连接串。所有变更使用 API 的幂等键和版本检查；表单提交时使用打开表单时的版本，冲突后需刷新并重新核对。网络中断、结果不明或发布屏障繁忙时保留原请求，以原幂等键重试，不自动重放新意图。停用和删除需要页面内确认，且不执行远端 DROP。

Web 与 API 共用可信管理访问边界，不新增登录认证或跨域访问。默认仍绑定 loopback。浏览器写请求必须同源；无 Origin 的现有命令行调用继续支持。经反向代理使用时须保留与浏览器 Origin 对应的 Host，或采用同源转发。页面使用本地静态资源、内容安全策略及禁用缓存，不把新增页面当成可直接公开到公网的认证边界。

## 首次导入与存储

当前开发阶段只支持当前存储结构，不提供历史数据迁移或旧格式回退；不匹配的版本明确拒绝。规约见 [开发说明](development.md#当前阶段的数据兼容规约)。

`[[datasources]]` 只在管理库第一次初始化时导入；空列表也会写入完成标记。之后以管理表为准，编辑 TOML 不会覆盖在线配置或复活已删除的源。联邦配置示例使用独立的 `./data/federation.duckdb`，避免与已初始化的默认零源数据库混淆。

部署时将数据库文件及其 WAL 放在同一持久化卷中，同时保留凭据目录。不要同时启动两个进程使用同一管理库。下载扩展、临时文件、管理数据库和凭据不纳入 Git。

凭据引用兼容环境变量名，也支持 `file:crm.dsn`。文件路径相对于 `management.credentials_directory`，拒绝绝对路径、上级目录和逃出目录的符号链接。文件保存完整连接串，每次探测或挂载重新解析，不写入管理表或操作结果。只编辑文件不会切换当前连接；凭据变更应显式 PATCH 同一引用再 apply，或对仍在运行的源重新 apply 已生效版本，创建新 revision 并重新解析。未变更定义的 Disabled 源恢复复用原 revision 和连接身份，不作为凭据轮换操作；若停用期间修改凭据，恢复前也须显式 PATCH。恢复上一版本需要其引用仍可解析；恢复失败时源进入 `Degraded` 并关闭准入。

## 实例发现与自动虚拟库

`POST /discovery/{uuid}` 接收 `{issued_at, connection}`，返回 `202` 的发现任务；`GET /discovery/{uuid}` 读取结果，`POST /discovery/{uuid}/cancel` 请求取消。UUID 是重放身份，相同参数重试返回原任务，不同参数返回 `409/40001`。`issued_at` 为 Unix 秒，提交有效期 300 秒，完成结果保留 300 秒；重启后重新发现。此入口不要求持久化写操作的 Idempotency-Key，不产生登记副作用。

```json
{
  "issued_at": 1793880000,
  "connection": {
    "id": "crm_instance", "kind": "postgres",
    "instance": {"host": "db.example", "port": 5432, "identity": null},
    "secret_ref": "file:crm.json", "database": null,
    "bootstrap_database": "postgres"
  }
}
```

将示例 `issued_at` 替换为当前时间。PG 的 `bootstrap_database` 默认 postgres；MySQL 通过 information_schema 连接枚举可见库。可选 `instance.identity` 显式关联代理/别名，缺省按规范化主机和端口区分；不查询 DNS 推断同实例。

任务包含 `status=running/succeeded/failed/canceled`、`candidates` 和 `error`。逐库结果包含 `database`、`schemas`、`status=available/forbidden/unavailable/excluded`、`virtual_database`、`merged`、`error`。实例发现成功仍需逐库检查状态；只有 available 目标可选择。已合并目标返回已有名称，新目标返回建议名。最多 128 个库/1024 个 Schema，超限明确失败。

自动组织的凭据引用内容示例（密码由部署侧填入，不能放入 API）

```json
{"username":"source_reader","password":"replace-in-credential-file","options":{"sslmode":"disable","connect_timeout":"10"}}
```

PG options 支持 sslmode/sslcert/sslkey/sslrootcert/connect_timeout/application_name；MySQL 支持 ssl_mode/ssl_ca/ssl_cert/ssl_key/connect_timeout。连接端点、账号和目标库禁止通过 options 覆盖。两种产品使用各自 DSN 转义规则，密码中的空格、引号、反斜杠、@ 不按 URI 或 Go DSN 解码。

核对完成后，对每个选定目标调用 `POST /sources`，增加 `organization`：

```json
{
  "id":"crm_public", "kind":"postgres", "catalog_alias":"remote_crm_public",
  "secret_ref":"file:crm.json", "read_only":true, "ddl_enabled":false,
  "allowed_schemas":["public"], "default_schema":"public",
  "organization":{
    "connection_id":"crm_instance", "instance":{"host":"db.example","port":5432},
    "database":"crm", "virtual_database":"business_crm",
    "owner":"fedengine", "connect":[]
  }
}
```

`catalog_alias` 为内部执行别名，不能与虚拟库名相同，且不对 PG 客户端公开。登记成功后读取 source target，按现有 `apply` 流程激活。源库、实例与对外名建立后不可在线改写；同原生目标必须复用已有虚拟名，不同目标占用同名返回冲突。一个 MySQL 连接登记只使用一个虚拟名；每个执行目标的 Schema 范围只允许其源 database。

自动库返回 `managed=true`，其 `config.schemas` 包含每个来源贡献，重复本地 Schema 名表示不同贡献，PG 与 Web 目录只展示一个节点。不能用手工库 PUT/DELETE 覆盖自动库；通过停用/删除来源调整。停用最后一个贡献保留空库身份；删除最后一个登记移除自动库。来源移除不执行远端 DROP。空库可以登录，账号未指定数据库仍只尝试账号同名库。

合并后的目录与查询按具体表选择首个已授权且采集到该表的来源。浏览器对象返回 `contributors`、`metadata_conflict` 和当前 `source`。事实冲突时展示确定性来源，不交叉拼接不同快照。授权可引用具体 source id 或 `connection_id`；后者仅覆盖该登记的目标，不扩展为其他贡献凭据的权限。没有 `organization` 的高级执行目标采用显式连接串和手工组织，不参与原生身份合并；Web 默认不走此入口。

## 可复制调用

以下命令从仓库根目录执行，示例依赖 `curl`、`jq`。预先将完整 PostgreSQL 连接串保存到 `secrets/crm.dsn`，保护文件读取权限，并按 `docs/development.md` 准备匹配版本的扩展。自动安装继续由 `duckdb.install_extensions` 控制，在线登记不会自行开启。

```sh
base=http://127.0.0.1:15433/api/v1
curl -sS -X POST "$base/sources" \
  -H 'Content-Type: application/json' -H 'Idempotency-Key: register-crm-1' \
  -d '{"id":"crm","kind":"postgres","catalog_alias":"pg_crm","secret_ref":"file:crm.dsn","default_schema":"public","allowed_schemas":["public"],"read_only":false,"ddl_enabled":false}'
# 返回操作记录，其中 id 是操作编号；登记完成后状态为 Registered。
curl -sS "$base/operations/替换为返回的操作编号"
curl -sS "$base/sources/crm"

# 后续写请求同时比较 incarnation 与 desired_revision。
target=$(curl -sS "$base/sources/crm" | jq '{incarnation,expected_revision:.desired_revision}')
body=$(jq -n --argjson target "$target" '{target:$target}')
curl -sS -X POST "$base/sources/crm/discover" \
  -H 'Content-Type: application/json' -H 'Idempotency-Key: discover-crm-1' -d "$body"
# 等待发现操作完成；result_schemas 是发现结果，不会自动纳入。

# 完整提交绑定。只纳入已经存在的 schema；null 表示显式清除默认绑定。
body=$(jq -n --argjson target "$target" '{target:$target,bindings:[{remote_schema:"public",enabled:true}],default_schema:"public"}')
curl -sS -X PUT "$base/sources/crm/schemas" \
  -H 'Content-Type: application/json' -H 'Idempotency-Key: schemas-crm-1' -d "$body"
# 等待该操作成功后读取新的 revision，再激活。
target=$(curl -sS "$base/sources/crm" | jq '{incarnation,expected_revision:.desired_revision}')
body=$(jq -n --argjson target "$target" '{target:$target}')
curl -sS -X POST "$base/sources/crm/apply" \
  -H 'Content-Type: application/json' -H 'Idempotency-Key: apply-crm-1' -d "$body"
curl -sS "$base/sources/crm/catalog?limit=100&offset=0"
```

MySQL 登记使用 `kind:"mysql"`，连接串必须包含明确的 `database`，`default_schema` 与所选 schema 必须是该 database 名称。发现仅返回当前 database；不能通过该数据源纳入同服务器上的其他 database。SQL 使用 `catalog_alias.database.table`。

## 请求与结果

所有写接口要求 `Idempotency-Key`。同键同参数返回原操作，同键不同参数返回 `409`，已完成的失败操作也不会被同键重放。重试新意图时使用新键并重新读取目标版本。除登记外，请求体都包含 `target:{incarnation,expected_revision}`。源 id 和别名复用后，旧 incarnation 仍然冲突。

| 请求 | 请求体中除 target 外的字段 | 结果 |
| --- | --- | --- |
| `POST /sources` | 完整源定义，见上例；enabled 不会自动激活新登记 | 登记操作 |
| `GET /sources`、`GET /sources/{id}` | 无 | 配置、版本、实际状态、当前操作、最近错误、目录时间 |
| `PATCH /sources/{id}` | `secret_ref` | 新期望版本，不立即切换 |
| `POST /sources/{id}/probe` | 无 | 连接探测操作 |
| `POST /sources/{id}/discover` | 无 | 发现操作，结果在 `result_schemas` |
| `PUT /sources/{id}/schemas` | `bindings:[{remote_schema,enabled}]`、`default_schema`（字符串或 null） | 新期望版本，显式 apply 后生效 |
| `POST /sources/{id}/apply` | 无 | 激活或应用期望版本；运行中重应用当前版本创建新 revision，未变更定义的 Disabled 恢复复用原 revision |
| `POST /sources/{id}/disable` | 无 | 保存停用意图并等待真实排空 |
| `POST /sources/{id}/refresh` | 无 | 清理扩展缓存，保存并发布完整目录 |
| `GET /sources/{id}/catalog` | 查询参数 `schema`、`object`、`object_type`、`include`、`snapshot_version`/`snapshot_id`、`page_token`、`offset`、`limit`（1–1000） | 分页对象、完整 schema 范围、版本、generation 与 stale 标记 |
| `DELETE /sources/{id}` | 无 | 删除已停用的登记，保留历史，不执行远端 DROP |
| `GET /operations?offset=0&limit=10&search=...` | 无 | 全部已保存操作的分页记录，含总数 |
| `GET /operations/{id}` | 无 | 操作状态、阶段、结果及脱敏错误 |

写请求返回 `202` 前，操作和期望状态已经提交；必须读取操作至 `Succeeded` / `Failed` / `Interrupted` 才能确认结果。读请求返回 `200`；输入错误为 `400`，不存在为 `404`，版本/幂等/状态冲突为 `409`。接口超时或 HTTP 连接断开不代表远端工作已取消，可用原幂等键找回操作。操作完成前，同一源不能接受其他变更或探测。

目录包括表/视图、列序号、远端类型、映射类型、可空性及索引定义；无法映射的类型和特殊索引通过 `supported:false` 明确展示。刷新仅扫描有效绑定范围。结构变化会使旧 Prepared Statement 失效，需要重新 Parse/Bind。服务确认远端 DDL 成功后立即标记旧绑定失效，释放执行 worker 的源锁与实例租约，再等待同一刷新流程发布结果。正常情况下 CommandComplete 后可以立即查询新结构；等待受 management.request_timeout_ms 和语句剩余期限限制。刷新失败或超时仍返回已提交的 DDL 成功，并发送明确 Notice；源保持 stale 或继续后台刷新，可通过管理状态/refresh 恢复，不能重放 DDL。

停用 schema 通过提交完整 bindings 并 apply 完成；移出时从 bindings 删除。停用当前默认 schema 时必须指定另一个启用的默认值或传 null，未指定默认绑定的短名称不会回退到本地 DuckDB。停用/移除仅改变登记和挂载，远端数据保持原样。

### 管理列表分页

Web 数据源、虚拟数据库、操作记录均默认每页 10 项，可选择 20、50、100 项，支持上一页、下一页和页码跳转。筛选或修改每页条数时回到第一页；刷新、进入详情后返回保留列表条件与页码；条目减少时自动收缩到最后一个有效页。数据库树仅显示在对象浏览和 SQL 调试工作区。

数据源和虚拟数据库按名称及稳定 ID 排序，在已有受限完整清单上进行浏览器分页；完整清单仍供数据库树、映射选择和统计使用。数据源可按名称、ID、类型、虚拟库名筛选；虚拟数据库可按名称、Owner、映射 Schema 筛选。

`GET /api/v1/operations` 查询服务端全部已保存操作，与浏览器用于进度通知的最近 40 项缓存独立。返回 `{items,total,offset,limit}`，默认 `offset=0,limit=10`，`limit` 必须为 1–100，`offset` 必须为非负整数。`search` 最多 256 UTF-8 字节，对操作编号、源 ID、动作（如 `register`）、状态（如 `Failed`）、阶段进行不区分大小写的子串匹配；Web 也支持完整中文动作和状态名称。未知参数、重复参数和非法值返回 400。

记录按 `created_at` 降序、操作 ID 降序排列，先筛选再分页。`offset` 向下对齐当前页大小；超过末页则返回最后有效页，空集的 offset 为 0。分页读取当前记录，新增操作可能使页内位置变化；它不是固定历史快照。查看单项的最新状态仍使用 `GET /operations/{id}`。

## 关系元数据与一致分页

基础对象字段继续保留 `schema/name/object_type/columns/indexes`。`metadata` 包含本地稳定 OID、远端 source_id、列 attnum/default/identity/generated/comment、PK/UNIQUE/FK/CHECK、索引键项/INCLUDE/排序/谓词、视图定义及分类状态。`remote_type` 保留来源类型，`mapped_type` 是锁定扩展实际 DESCRIBE 的结果，二者不混用。

```sh
curl -sS "$base/sources/crm/catalog?schema=public&object=orders&include=core,column_properties,constraints,indexes,comments,view_definitions,statistics,storage,activity,column_statistics,status&limit=1"
```

`schema` 与 `object` 是独立参数，必须按 URL 规则编码。`include` 可选择上述类别，省略时返回完整对象；基础对象和列列表始终返回，未选择的扩展字段从 JSON 中移除。选择 `column_properties` 返回列属性整体，其中也可能含列注释。对象详情、约束、索引和状态范围读取使用相同端点与 object/include 组合，不另建一套事实服务。

返回 `snapshot_id`、同值的 `snapshot_version`、refreshed_at、revision、generation、stale、total、offset、objects 及 `next_page_token`。最后一页 token 为 null。续页须带回原 schema/object/object_type/include 过滤条件及 token，可以调整 limit；token 绑定 source、过滤条件、snapshot 与下一 offset。它不承载凭据，也不是授权令牌。

```sh
curl -sS --get "$base/sources/crm/catalog" \
  --data-urlencode 'schema=public' \
  --data-urlencode 'include=core,status' \
  --data-urlencode 'limit=100' \
  --data-urlencode "page_token=$next_page_token"
```

跨页变更过滤条件、请求旧 snapshot_version 或发布了新结构均返回 HTTP `409` / SQLSTATE `40001`，调用者从第一页重新读取。非法 token/include 返回 `400`。无变化刷新复用 snapshot_id，只更新 freshness；不能用时间变化推断结构版本改变。旧快照、失败扫描和已停用源的 HTTP 管理状态仍可观察，PG 可连接范围另按有效源和会话权限限制。

每类状态包含 `support`（supported/partial/unsupported/unknown）、`scan_state`、complete、stale、尝试/成功时间、脱敏 error_code 和 unsupported_fields。未扫描或能力不足不会伪装成功空集；失败/不完整扫描保留旧事实并标记状态。comments 与 view_definitions 从组合 details/列属性采集派生。

表达式保留 `source_text/source_dialect`，只有安全、可展示的 PG 文本才填 `pg_text`。MySQL 生成式、表达式索引等保留原始事实，未实现 PG 翻译的部分标 partial；标准 PG 目录返回 NULL。默认值/定义涉及不可安全公开的用户函数或对象时也可能为 NULL，不代表远端没有定义。MySQL 5.7/8 的文本（含空字符串）、整数及普通十进制字面量默认值按类型转换；函数默认值、二进制/时间默认值、生成式仍保守保留原文，未证明的 PG 文本为 NULL。

`metadata.statistics` 采集远端目录的 `estimated_rows/pages/all_visible_pages`，PG 目录投影至 `pg_class.reltuples/relpages/relallvisible`。PG 的 -1 行数估计保留其“未统计”含义；MySQL 的 table_rows 是存储引擎估计，页数未知并标记 partial/page_counts。

`include=storage` 返回 `metadata.storage`：PostgreSQL 的 `main_bytes/fsm_bytes/vm_bytes/init_bytes/table_bytes/indexes_bytes/total_bytes` 来自对应的源端大小函数；MySQL 的 `data_length/index_length/data_free` 来自 information_schema.tables，保留存储引擎自身含义，不能加总为独占空间或转换为 PG relation fork 大小，PG 大小字段保持 NULL，状态为 partial/postgresql_storage_sizes。

`include=activity` 返回 `metadata.activity`：PostgreSQL 的 `seq_scan/seq_tup_read/idx_scan/idx_tup_fetch/n_tup_ins/n_tup_upd/n_tup_del/n_tup_hot_upd/n_live_tup/n_dead_tup` 来自 pg_stat_all_tables，计数随源端重启或统计重置可能减少；MySQL 此分类为 unsupported/not_scanned，字段保持 NULL。没有统计行的对象也保留 NULL。

以上均为 refresh 时的源端快照，不执行 COUNT、业务扫描或 ANALYZE，不代表 fedengine 本地 DuckDB 占用或实时查询次数。未采集的事实保留 NULL；各分类独立发布，采集失败保留旧事实、last_success_at 并标 stale；成功采集到 NULL 可清除旧值。统计值变化也会产生新 snapshot_id，即使业务结构未变。

`include=indexes` 同时返回各索引的 `storage`（与表相同的七项 PG 大小）和 `activity`（idx_scan/idx_tup_read/idx_tup_fetch）。源数据库连接中的 `pg_relation_size(regclass[,text_fork])/pg_table_size/pg_indexes_size/pg_total_relation_size` 和表/索引 `pg_stat_get_numscans(oid)` 读取已发布、经 SELECT 权限过滤的统计。fork 支持 main/fsm/vm/init 及 text 参数、表达式；可见已知关系的非法 fork 返回 22023，NULL/未知 OID 返回 NULL。虚拟库映射是本地 foreign table，不把源端物理统计伪装成本地统计；数据库总大小仍未知。

`include=column_statistics` 返回 `metadata.column_statistics`，按列名保存 PostgreSQL pg_stats 的 null_frac、avg_width、n_distinct。未授权、未统计或未采集的指标保留未知；MySQL 为 unsupported/non_value_column_statistics。标准 `pg_stat_all_tables/pg_stat_user_tables/pg_stat_all_indexes/pg_stat_user_indexes` 填入上述已采集的远端计数，其余字段 NULL；`pg_stats` 只公开非值指标，most_common_vals/histogram_bounds 等包含业务值的字段不采集、不公开。源端列统计不存在时不伪造统计行。

当前结构持久化连接身份 epoch。默认 `management.mysql_identity_policy="preserve"`：无变化刷新、重启、仅修改 schema/default 绑定以及未变更定义的 disable→apply 保留存活身份。删除列保留编号墓碑，PG 可观察的对象删除重建分配新身份；运行中重 apply 或显式 PATCH 凭据后 apply 切换 epoch。MySQL 缺少可靠源身份，分类状态明确标记 partial/offline_identity_continuity_unproven。

可选 `management.mysql_identity_policy="strict"` 在 MySQL 重启、停用恢复和 MetadataStale 后恢复刷新时切换 revision/epoch，使旧 Prepared 与旧对象身份失效；严格切换失败关闭准入，不恢复旧身份继续提供业务查询。该模式整体切换身份，不能精确识别哪些表在离线期间同名重建。默认策略不变，两种策略均有真实数据库回归。

采集在前后两次基础结构枚举发现 DDL 漂移时最多重试三次；持续漂移返回冲突并保留旧发布。多次远端调用不构成全局一致事务。当前默认仍为手动 refresh 和服务内 DDL 后刷新，不启用周期扫描。

账号业务虚拟库及 schema 映射可通过 TOML virtual_databases 或下述在线管理 API 显式维护。业务源 HTTP 目录继续描述远端事实，不将本地映射对象当成新数据源。见[账号虚拟库方案](virtual-database-pg-alignment-plan.md)。

## 在线业务虚拟库

与 sources 共用既有可信管理 HTTP 边界。虚拟库变更是本地同步发布事务，成功返回 `200` 和持久化回执，不创建远端异步 source operation，不执行远端 SQL。

| 接口 | 请求/返回 |
| --- | --- |
| `GET /virtual-databases` | 所有有效虚拟库定义、OID、revision、schema 映射与快照指针 |
| `GET /virtual-databases/{name}` | 单个有效虚拟库；不存在返回 `404` |
| `POST /virtual-databases` | 完整 `VirtualDatabaseConfig`；创建独立业务库 |
| `PUT /virtual-databases/{name}` | `{ "target": { "oid": 200000000, "expected_revision": 1 }, "definition": { ... } }`；完整替换，不能改名 |
| `DELETE /virtual-databases/{name}` | `{ "target": { "oid": 200000000, "expected_revision": 1 } }`；删除本地虚拟库和投影 |

所有变更必须携带 `Idempotency-Key`（1～128 个可打印 ASCII 非空白字符）。回执包含 `idempotency_key/name/oid/revision/deleted`。同键同请求返回历史提交回执，重放删除或创建不会恢复旧对象；同键不同请求、OID 或 revision 不匹配返回 `409/40001`。幂等键在虚拟库接口内独立保存；请求超时或断开不能推断回滚，应保留原键重试，并 GET 当前状态核对。完全相同的 definition 替换保留 revision，其余定义变化递增 revision；删除回执使用下一 revision，同名重建分配新 OID、revision 从 1 开始。

```sh
curl -sS -X POST "$base/virtual-databases" \
  -H 'Content-Type: application/json' -H 'Idempotency-Key: create-account-v1' \
  -d '{"name":"account","owner":"fedengine","connect":[],"allow_connections":true,"schemas":[{"name":"business","source":"crm","remote_schema":"public"}]}'
```

owner/connect 必须是已配置的 PG 账号；本地库与 schema 名称不超过 63 字节，系统命名空间与重复映射拒绝。源必须已登记且映射在其 desired 启用 schema 绑定内；远端源未激活时可以保留映射，但只有有效快照才提供对象目录；pg_catalog 描述已采集对象的存在，information_schema 按 owner/授权过滤，数据读取另须 SELECT。CONNECT 和 owner 不授予源 SELECT，虚拟库映射仍只读。每库最多 256 个映射、最多 256 个有效虚拟库；幂等回执最多 10,000 条、覆盖/墓碑最多 4,096 个名称，达到限额返回 `503/54000`，不静默清除回执或复用身份。

发布遵循实例写屏障 → 存储 writer → 本地事务的顺序。活跃 worker 或悬停 portal 持有实例读租约时，返回 `409/55P03`，不等待客户端，也不写入定义或回执；排空/关闭 portal 后使用原键重试。该屏障覆盖整个实例，其他数据库上的活跃执行也可能导致冲突。定义、身份、覆盖/墓碑、目录投影及回执原子提交，阻塞 I/O 任务持有屏障和 ServiceLease 至实际退出；HTTP 等待者消失不会提前释放屏障。

PG 登录绑定当时数据库 OID。定义变化后，旧 Prepared 的 Bind、Describe、Execute 返回 `0A000`，新 SQL 使用新映射；源 generation 不变也会失效。CONNECT 撤销和 allow_connections=false 仅限制新登录，已接入连接保留 CONNECT 准入，源对象权限仍逐次检查。删除虚拟库后旧连接查询返回 `3D000`；同名重建也不能重新绑定，必须重新连接。这是管理 API 的删除策略，不提供 PostgreSQL `DROP DATABASE`/FORCE 的完整会话终止语义。

当前结构保存虚拟库覆盖与回执，revision 为必需字段，不读取缺失 revision 的历史格式。API 接管的同名定义优先于 TOML，删除墓碑阻止 TOML 在重启时复活该库；未被 API 接管的定义继续按 TOML 重启对齐。API 覆盖不提供在线清除接口，需要继续变更时使用 API。部署前正常停服务并备份管理库和配置；程序回退必须使用匹配的管理库备份。

## 目录刷新与资源预算

普通刷新在独立的 DuckDB 采集实例内完成远端枚举、类型映射和扩展事实采集，不修改正式实例的挂载或缓存。旧的有效目录在采集和本地准备期间仍可读取；已标记 stale 的源继续服从原有业务阻断规则。Apply/Disable 仍关闭目标源准入并排空，正式 ATTACH/DETACH 和缓存清理保留短实例独占窗口。

PostgreSQL 普通刷新在来源仍为 Active、旧快照有效且完整、结构摘要不变、details/column_properties/constraints/indexes 均成功完整采集时保留正式扫描器缓存，避免无结构变化的刷新清空同产品全部来源的缓存。结构摘要覆盖列、默认/生成式、约束、索引定义和视图定义/依赖，排除动态统计及扫描时刻；定义变化使旧 Prepared 失效。事实不完整、stale 恢复和 MySQL 刷新继续清缓存；MySQL 缺少可靠源身份，不能仅凭同名同结构判定缓存可保留。锁定扩展仍只提供产品级清理，结构变化时对其他同产品来源的影响尚需独立评估，不引入扩展改造或历史格式回退。

远端事实使用压缩暂存供本地重试，每次准备只解码一份新目录，不额外保留完整事实图。目录准备使用目标源当前快照和批量身份索引，在独立本地事务中生成不可见投影；普通状态更新只读写管理记录与目录头。对象事实按对象压缩保存，分页与计数使用独立的快照/schema/名称/种类列筛选后才解码命中事实，禁止对全量 JSON 做目录筛选。当前存储版本为 7，仅接受新库或同版本初始化，不迁移旧库。准备结束后以非排队方式尝试实例屏障，复核源代际后提交，并在释放屏障前更新运行注册表。提交任务持有屏障、内存额度和服务租约直到实际退出；之后由另一个受管任务执行非强制 checkpoint，并保留 writer、内存额度和准备阶段服务租约到其实际退出。文件库自动 checkpoint 阈值为 1GB；主动 checkpoint 避开实例屏障，冲突时保留已提交 WAL，后续发布重试或由自动阈值兜底，不回滚已确认提交。准备失败或取消回滚事务；代际变化拒绝过期结果，保留旧快照并明确标记失败。准备事务持有 writer 时只允许最多 100ms 的非排队尝试；其间持实例屏障的其他写入必须立即尝试 writer，禁止排队等待。超过窗口先回滚再退让，最多进行三次本地准备，不重复远端采集。持续冲突返回操作失败 `55P03`，重试需要新管理操作键。

虚拟库变更仍遵循实例屏障 → writer → 本地事务；writer 忙时立即返回 `409/55P03`，可用原 Idempotency-Key 重试，不持屏障排队等待目录准备。查询绑定的关系名称按不可变快照共享缓存，最多保留 16 份/总计 100k 个名称；目录头与新快照名称索引在准备阶段生成，成功提交后在屏障内切换内存视图，普通状态/虚拟库写入同样更新视图；查询逐次使用最新提交目录头，复核权限、库修订与源代际，不缓存授权结论。查询及挂起 Portal 的真实租约不被强制释放。等待实例空闲的管理操作通过非排队尝试进行退让，不在读写锁内排队阻挡后续查询。

`management.metadata_memory_budget_mb` 默认 512，`metadata_operation_memory_mb` 默认 256；操作先预留完整额度，并受 `max_concurrent_operations` 共同约束。独立采集实例的 DuckDB memory_limit 为操作额度的一半且最高为 128MiB，核心与分类采集以及累计序列化事实的字节限额为操作额度的八分之一，超限以 `54000` 失败，不发布截断目录。字节限额不等于进程 RSS 硬上限；Rust 容器、正式实例、分配器与原生库内存仍需通过独立 RSS 测量和部署资源限制管理。

### 进程内存与部署额度

`metadata_memory_budget_mb` 是采集及准备任务的共享准入额度，`metadata_operation_memory_mb` 是每项任务预留量；二者不是进程 RSS 硬限制。DuckDB `memory_limit` 也不包含全部 Rust、驱动、线程及原生库分配。配置使用的 MB 字符串与验收使用的 MiB/GiB 必须明确区分；RSS 验收的 1GiB 为 1024MiB。

准备阶段逐对象读取旧事实，身份保留、比较及淘汰不载入完整旧目录；源目录投影直接写入有类型的表，移除 VARCHAR 暂存及转换副本。批次在 4096 行或估算 2MiB 后刷新；这是软阈值，单个值、列或完整约束组可超出，不是堆内存硬限制。约束组完成目标对象可见性标注后才刷新，保留授权过滤。虚拟映射通过受控 SQL 从已准备的源投影生成，不复制整份对象图；本地 OID、schema、只读语义及未知物理统计独立保留。在线虚拟库变更和启动恢复只读目录头。所有变化仍在不可见事务内完成，提交和运行注册表切换保持原子性。

开启 `modorch_fedengine_duckdb=debug` 可读取 `metadata phase started/finished` 的 phase、elapsed_ms、rss_kib、peak_rss_kib 和 rss_delta_kib，覆盖采集、身份处理、本地准备、源/虚拟投影、提交及 checkpoint。RSS 为阶段边界观测，VmHWM 是进程截至该时刻的历史峰值，不能当作该阶段独占分配；实际进程回归另以 20ms 采样并读取内核高水位。非 Linux 平台缺少 `/proc` 时省略内存字段，不填零冒充已测量。

部署应根据正式查询引擎、并发采集、目录准备、结果缓冲和原生开销的混合负载峰值加余量定容。`examples/config/resource-budget.toml` 提供 2GiB 分配的候选起点，仍须验证；外部 cgroup/systemd 限制会计入部分文件缓存，与进程 RSS 不是同一指标。进程硬限额不能仅由 DuckDB 额度推断。1GiB 继续作为固定元数据工作负载的效率验收线，不用扩大部署额度替代该验收。

## 数据库浏览与只读 SQL 调试

从业务虚拟库列表点击库名进入工作区后，左侧显示数据库树，供“对象浏览”和“SQL 调试”共用。数据源页面（含详情）、业务虚拟库列表和操作记录不显示数据库树或其展开按钮，也不预留侧栏空间。主导航仍为数据源、业务虚拟库、操作记录。

数据库树按数据库 → Schema → 表/视图 → 列逐级展开。点击库名切换当前库，保留对象浏览/SQL 调试模式；只展开目录不切换查询库。Schema 和对象按页读取，列在展开对象时读取，每次显示 50 列；树最多保留 8 个数据库的已加载目录。库名支持筛选，窄屏在工作区内通过顶部“数据库树”按钮展开侧栏；离开工作区收起，重新进入时可再次展开。

工作区分为“对象浏览”和“SQL 调试”；不增加独立浏览器入口。目录筛选、分页和 SQL 草稿保存在当前页面内存中，切换详情或页签保留，最多保留最近访问的 8 个数据库工作区，刷新整个网页后重置。离开工作区或切换数据库请求取消仍在执行的调试任务。页面保留操作状态、错误和过期警告，通用帮助折叠展示，避免重复常驻说明。

管理 HTTP 沿用可信管理边界，没有额外的数据库用户登录。工作区使用**服务端当前虚拟库定义中的 owner** 作为查询身份，“使用说明”显示该账号；客户端不能指定 principal。owner 的 CONNECT 和现有来源 SELECT 授权继续生效，管理端访问权限本身并不补授来源 SELECT 权限。此入口适用于可信管理员，不能直接作为面向不可信用户的 SQL 服务。

### 目录接口

以下 GET 接口位于 `/api/v1/browser/`，参数 `database`、`oid`、`revision` 必填，身份来自业务虚拟库 API。目录只读取本地已发布的 PG 目录投影，不连接来源执行 COUNT、ANALYZE 或扫描数据。不存在、被同名新库替换、定义变化和禁止连接分别明确返回错误。

| 接口 | 其他参数 | 返回内容 |
|---|---|---|
| `schemas` | `search`、`offset`、`stamp`、`system` | 当前身份可见 Schema，每页最多 100 项，含空 public |
| `objects` | `schema`、`search`、`kind`、`offset`、`stamp`、`system` | 轻量对象摘要：OID、名称、relkind、来源类型、列数，每页最多 50 项 |
| `object` | `schema`、`object`、`stamp`、`system` | 对象详情、映射身份、来源身份与固定快照事实 |

`kind` 可为 `table`、`view` 或空；名称搜索按不区分大小写的字面子串匹配，`%`、`_` 不作为通配符。参数使用 URL 编码。页响应包含 `stamp`、`offset`、`has_more`；偏移量最大 1,000,000。后续分页必须携带 `stamp`，前端从首次 Schema 读取开始固定该标记。标记涵盖虚拟库 OID/版本、映射、可见来源快照、授权和过期状态。请求前后复核，变化返回 `40001` / HTTP 409，用户重新加载后从头读取，不拼接两个快照。重新加载允许读取同 OID 的最新定义；同名库换 OID 必须返回库列表重新进入。

默认隐藏 `pg_catalog`、`information_schema`、`pg_toast*`、`pg_temp_*`，勾选“显示系统 Schema”可浏览标准目录。内部 `_fedengine` 始终隐藏。浏览范围与 owner 的 PG 目录投影一致，未绑定或停用的来源不冒充有效 Schema；已发布对象可被 owner 浏览，但管理权限和所有权不补授源 SELECT。没有 SELECT 时对象详情仅返回本地结构，来源定义和统计仍受授权限制。

对象列表的来源表/视图区分使用已持久化的索引字段，不解压列载荷；再通过当前 PG 投影取映射 OID 与列数。来源视图映射到虚拟库后 relkind 仍为 f，视图筛选依据来源类型，不将映射伪装为本地视图。

对象详情展示概览、列、来源约束、来源索引、视图定义、统计、来源与分项采集状态。来源表达式、PG 译文及翻译状态分别保留，未知统计不显示成零。来源约束/索引不表示虚拟对象在本地强制执行；来源视图与虚拟外部对象的身份分别显示。单对象来源读取定点定位到固定来源快照，解压前固定最多读取 8 MiB 的解压数据，响应也最多 8 MiB，超限报 `54000`，不截断后冒充完整详情。系统目录详情使用 PG 属性投影，其来源物理事实显示未知。

“重新加载目录”仅重新读取本地快照；“刷新来源元数据”先选择一个映射来源，再复用原有带幂等键和 CAS target 的异步 `sources/{id}/refresh`，不隐式批量扫描。操作记录继续显示其结果，完成后手工重新加载目录。

### SQL 任务接口

| 方法与路径 | 含义 |
|---|---|
| `POST /api/v1/browser/queries/{uuid}` | 新建或按相同 UUID 确认原提交 |
| `GET /api/v1/browser/queries/{uuid}` | 读取状态与结果 |
| `POST /api/v1/browser/queries/{uuid}/cancel` | 请求取消，已终结任务保持原结果 |

提交体为 `{ "database": "fedengine", "oid": 200000000, "revision": 1, "sql": "SELECT 1", "issued_at": 1793840000 }`；时间为当前 Unix 秒，允许向前偏差 30 秒，超过 5 分钟的提交拒绝。UUID 由客户端生成，同 UUID 与完全相同参数返回原任务，参数不同返回 `40001`。不能在网络结果未知时生成新 UUID 自动重跑；当前页可用原提交重试确认。任务状态和结果只存于进程内存，重启或过期后不可找回，不承诺持久化查询历史。

仅支持单条、无绑定参数的只读 Query（SELECT / WITH / VALUES）。AST 递归检查排除写 CTE、SELECT INTO、加锁查询、DML、DDL、事务和 SET，随后仍通过 query-coordinator 的 Prepare/Execute、函数白名单、来源授权、版本核验、准入预算和执行取消路径。不会直连 DuckDB 或绕过既有来源权限，源 SQL 能力与扩展限制也保持一致。

同一服务最多 4 个活动 SQL 调试任务，保留槽最多 16 个（包括终结结果），单结果最多 1000 行 / 2 MiB，保留约 5 分钟；达到槽上限返回 `53300`，等待过期后可再次提交。结果包含列名、类型 OID、类型修饰、PG 文本单元格；NULL 为 JSON null，其他值为字符串，避免 int8/numeric 经 JS number 丢精度。字段在转换前保守估算文本与 JSON 转义预算，可能在 2 MiB 之前停止读取。达到行数或字节预算时返回 `truncated=true` 并取消剩余读取，不声明完整结果。

执行最长为 `min(pgwire.query_timeout_ms, 60000)`；单对象读取和目录查询也受该执行期限约束。取消、超时返回 SQLSTATE `57014`，其他查询错误在任务 `error.code/message` 中展示。任务停止等待结果不等于底层 worker 已归还；原查询管线继续持有实际执行租约直到 worker 退出，checked shutdown 仍等待真实排空。调试任务归 management-api 的 TaskScheduler 管理，HTTP 入站不占 source-manager 串行操作通道。

编辑器支持 Ctrl/Cmd+Enter、优先执行选中 SQL。树节点单击选择，双击表/视图或列插入名称；“插入名称”和“插入 SELECT”在编辑器光标或选区插入，标识符使用 PostgreSQL 双引号转义，不执行查询。表名包含 Schema，列名只插入列标识符；SELECT 模板默认 LIMIT 100。切换数据库保留各库草稿，新提交绑定当前数据库，已提交任务的目标不变。

结果与消息分栏显示，50 行一页，区分 NULL 与空字符串，展示耗时、行数和截断；未执行时只显示“暂无结果”。执行账号、只读范围、结果额度及超时说明收至默认折叠的“使用说明”。执行任务与页面轮询相互独立，HTTP 断开不自动启动另一次查询；页面明确停止或离开工作区时发起取消。浏览器写入仍受同源检查保护，没有开放跨域执行。
