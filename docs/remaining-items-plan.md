# 剩余项分阶段收口方案

本文依据[协议兼容边界](protocol-compatibility.md)与[管理 API 契约](management-api.md)制定实施与验收方案，并同步整合从参考实现（`federated-query-engine`）评估接纳的关系型扩展与查询准确度能力（已明确排除 UNS、vxBase、iSYS 及独立工具扩展 `pg_adapter`）。目标继续保持对外尽量符合 PostgreSQL SQL/协议行为，以对外声明的 PG 版本锁定语义和目录验收基线，不同远端版本通过适配层处理差异。已确认的账号同名虚拟库、默认隐藏系统对象、权限范围、认证成功报文时机及普通停用恢复身份策略保持原契约。

开发阶段约束：按当前版本直接设计存储、接口和 DTO，不新增历史迁移、旧格式回退、历史回填或兼容分支，也不保留带 `_v1`..`_v5`、`_scoped_v1`..`_scoped_v4` 等历史阶段版本号的冗余扩展函数名。配置缺省值、来源能力未知值、当前版本的幂等初始化、运行中失败保留及稳定对象身份不属于历史数据兼容；继续保留。已有运行实例的升级备份与匹配回退仍须执行，不能借本约束丢弃当前管理状态或凭据。

架构与安全红线：
- **纯可嵌入库边界**：严禁引入子进程采集（如 `--metadata-scanner-worker`）或覆写宿主全局进程配额；所有阻塞 I/O、资源预算与启停受 Modorch-RS 0.0.3 `TaskScheduler` / `blocking_io` / `FedEngineBundle` 控制，不新增独立 `.duckdb` 账本或审计文件（统一落在唯一 `_fedengine` 库）；`FedEngineBundle::new`/`register` 保持零 I/O 副作用。
- **扩展加载边界**：核心仅接纳 5 个定制 `*_scanner.duckdb_extension` 连接器扩展（`postgres_scanner`、`mysql_scanner`、`oracle_scanner`、`sqlserver_scanner`、`odbc_catalog_scanner`）。当前开发阶段显式允许 `allow_unsigned_extensions = true` 以加载自编定制扩展，同时继续关闭 `autoload`/`autoinstall` 并强制 `manifest.json` 路径约束与 SHA-256 校验；生产发布前须切换为扩展验签或静态编入定制 `libduckdb`。所有内部扩展函数均在 `modorch-fedengine-ddl` 白名单外显式封堵。

## 1. 参考采用与实施顺序

参考实现：`federated-query-engine-9.00.00.00`（排除 UNS/vxBase/iSYS、独立宿主服务、supOS 鉴权许可及 `pg_adapter`）。

| 参考文件 | 可采用的机制 | 不能据此推断的能力 / 禁止照搬项 |
| --- | --- | --- |
| pkg/utils/query_parameters.go | 在原始 AST 上确定参数数量、声明类型和显式 cast，冲突/未解析类型明确报错 | 该文件也有类型支持白名单，不是完整 PG 参数推断器 |
| pkg/utils/numeric_parameter.go | 文本/二进制 numeric 解码、精度/scale 检查、精确 SQLSTATE | 明确限制为 DECIMAL(38)，不能作为任意精度 numeric 或 PG 除法已经实现的证据 |
| pkg/utils/query_result_encoding.go | PG 文本/二进制编码独立于 HTTP；数组保留元素类型与 NULL | 二进制数组实现有一维限制，不能据此承诺任意多维数组 |
| internal/modules/kernel/pgserver/biz/fed/{pg_compat_rewrite.go,pg_settings.go} | AST 改写、GUC/SHOW 响应与目录上下文分流 | 部分大小/扫描函数使用常量占位、regclass 模板猜测不得照搬；不照搬 BEGIN/COMMIT No-Op 假事务 |
| internal/initiators/duckdb/biz/{util.go,mysql_cast.go,mysql_pushdown*,greenplum_cross_pushdown.go,pushdown_projection.go} | 跨源类型映射、非法 CAST 拦截、方言/标识符纠偏与受控读下推 | 必须受 `modorch-fedengine-policy` / `ddl` 绑定与授权门禁约束，绝不绕过虚拟库权限 |
| internal/initiators/duckdb/biz/{metadata_pressure.go,remote_metadata_spool.go,remote_metadata_repair.go} | 库内内存压力控制、有界磁盘 Spool、按需修复队列 | 不照搬 `--metadata-scanner-worker` 子进程与修改宿主全局参数 |
| internal/mutation/ + internal/ddl/ | 各源写入精度规则与能力校验原则 | Rust 侧直接基于 `_fedengine` 快照与 AST 绑定组装参数调用去版本化扩展接口，不经 `pg_bind_*` |

建议收口顺序为：**批次 0（5 扩展去版本化与 DuckDB v2.0 接入底座）→ 批次 1（查询准确度、参数契约与 PG 协议）→ 批次 2（多数据源接入与受控读下推）→ 批次 3（Rust 原生 Guarded CRUD/DDL 闭环）→ 批次 4（高级元数据、库内压力治理与规模/耐久/生产遗留）**。任何未通过原型及实库对照验收的变体保持 `0A000` 关闭。

## 2. 批次 0：5 扩展去版本化与 DuckDB v2.0 接入底座

1. **5 扩展 `2.0.0` 分支接口去版本化**：在 `duckdb-postgres`、`duckdb-mysql`、`oracle_scanner`、`sqlserver_scanner`、`odbc_catalog_scanner` 的 `2.0.0` 分支移除全部 `_v1`..`_v5`、`_scoped_v1`..`_scoped_v4` 冗余命名及历史分叉：
   - CRUD：`<product>_crud_metadata`、`<product>_guarded_mutation`、`<product>_guarded_insert_returning`；
   - DDL：`postgres_ddl_capabilities`/`mysql_ddl_capabilities`、`postgres_ddl_describe`/`mysql_ddl_describe`、`postgres_guarded_ddl`/`mysql_guarded_ddl`；
   - 缓存与关系元数据：使用 DuckDB `TableFunctionSet` 重载统一 **`pg_clear_cache`**、**`mysql_clear_cache`**（0 参/1 参）、**`oracle_relational_metadata`**、**`odbc_relational_metadata`**（3 参/4 参）。
2. **开发期加载与 N 对 1 扩展映射**：在 `modorch-fedengine-duckdb` 启用开发期 `allow_unsigned_extensions = true`（主实例与隔离采集实例一致），保持 `autoload`/`autoinstall = false` 与 `manifest.json` SHA-256 校验；支持多方言映射到 5 个 `*_scanner.duckdb_extension`，禁止定制扩展触发官方在线 `INSTALL` 覆盖，并按扩展对齐版本探测查询。（注：`oracle_scanner`、`sqlserver_scanner`、`odbc_catalog_scanner` 运行时依赖宿主环境的 `libodbc.so.2` 与对应厂商 ODBC 驱动。）
3. **内部函数安全封堵**：`modorch-fedengine-ddl` 显式拒绝业务 SQL 调用上述 5 个扩展的任何内部/管理函数。

## 3. 批次 1（原批次 A 与 DECIMAL(38)）：准确度、参数契约与 regclass

### 3.1 原始参数描述与执行参数映射分离

1. 在任何兼容改写前，从原始 SQL AST 建立参数表，记录序号、客户端声明 OID、显式 cast、函数签名和解析结果。不能再从被改成 NULL 的 SQL 推导客户端参数数量。
2. 分开维护客户端参数契约和执行参数映射。客户端 ParameterDescription/Bind 使用原始序号和类型；DuckDB 使用执行 SQL 实际引用的参数。若执行 SQL 不再引用某些参数，保留客户端槽并在完成数量、格式和值校验后，不将其传给 DuckDB。
3. 统一 AST 参数重编号及原始序号到执行序号的映射，不能用字符串替换 `$1`。参数重复引用、序号间隙、全部参数被消去等情况都有明确行为。
4. Parse、Describe、Bind、Execute 共用同一份已解析描述；结果为 typed NULL 也不改变参数契约。保留明确声明类型，再按 PG 规则处理 cast、隐式转换和冲突；未能解析的类型明确返回相应错误。
5. 扫描所有直接替换为 NULL/常量的函数，一次修复同类问题，而非仅为四个已发现函数添加例外。

主要落点为 pg-compat/planner、query 参数处理、contract 中的描述符/执行请求及 pgwire extended 路径；复用既有模块，不新增无必要的组件。

验收以真实 PG 为基线：参数数量和 OID 正确；显式声明、无声明、NULL、文本/二进制、错误长度、重复参数、参数间隙及混合表达式均对照。消除合法请求的错误 08P01，同时保留真正 Bind 数量不符的协议错误。

### 3.2 `DECIMAL(38)` 精确数值、Wire 类型与非法 CAST 拦截

1. 结合 5 个定制扩展自带的 `*_exact_numeric` / `mysql_exact_decimal` 标量与排序规则，完成文本及 base-10000 二进制 `DECIMAL(38)` 解码与输出编码（保留 scale 尾部零与 `-0`），消除无界 `numeric` 退化 `DOUBLE` 导致的精度/排序失真；联动核对 `__fed_pg_divide` 护栏（超出能力维持 `0A000`）。
2. 补齐 `TINYINT`/`UTINYINT`/`USMALLINT`/`UINTEGER`/`UBIGINT`、`UUID`、`INTERVAL`、`POINT(STRUCT(x,y))` 及标准 1D 数组 Wire 编码闭环；在绑定阶段拦截非法类型 `CAST` 返回 `0A000`/`42704`。

### 3.3 名称解析、`SHOW` 与错误规范化

- regclass 参数解码区分二进制 OID、文本数值 OID和待解析名称。名称在有当前用户、数据库、search_path、权限及快照上下文的位置解析，不在无上下文的标量 codec 中猜测。
- 常量 cast、to_regclass 和参数绑定共享名称解析器，覆盖 schema 限定、带引号名称、大小写、同名遮蔽、可见性和当前库三段名称。
- 文本输出按 PG 规则选择可见的简写名或限定名并正确引用；无法解析的数值 OID 按 PG 规则处理。二进制仍输出 OID，不改变为名称字符串。
- regtype/regproc 等单独制定查找与显示规则；函数重载语义不能复用表名称解析冒充完成。
- 补齐基于 AST 的 `SHOW`/`current_setting` 完整响应集合（未知 GUC 返回 `42704`）与标准化 SQLSTATE 错误映射及凭据/原始 SQL 脱敏。
- Bind 后的值解释与实际执行纳入快照/代际校验，跨刷新或删除重建不能命中另一对象；既有 Prepared 失效和租约约束继续生效。

验收包括目前复现的 22P02 名称绑定、regclass 文本显示，以及跨 search_path、权限隐藏、删除重建、刷新、不同结果格式的 PG 对照。

## 4. 批次 2：多数据源扩展与受控读下推

1. 扩充 `DataSourceKind`、凭据 JSON / Secret 挂载与发现流程，通过 5 个定制扩展覆盖：`postgres` / `greenplum` / `kingbasees`、`mysql` / `mariadb` / `starrocks`、`oracle`、`sqlserver`、`dameng` / `db2` / `hana`，并纳入源端类型映射与统一按作用域清理缓存（`CALL pg_clear_cache('alias')` / `CALL mysql_clear_cache('alias')` / `sqlserver_clear_cache()` / `odbc_catalog_clear_cache()`）。
2. 在 `modorch-fedengine-ddl` AST 绑定、`modorch-fedengine-policy` 授权及 `DDLReadFence` 之后实施受控单源/特定跨源读下推与方言纠偏（MySQL/StarRocks 标识符与方言改写、Greenplum 跨源下推、大小写折叠重名投影列去重）。

## 5. 批次 3：Rust 原生 Guarded CRUD / DDL 闭环（已实现并本地自验）

1. **Guarded DML**：`modorch-fedengine-ddl` 支持单标识列 `INSERT ... RETURNING <col>` 与多方言 Guarded Mutation AST 重写；`modorch-fedengine-duckdb::guarded_crud` 对接 `<product>_crud_metadata` / `<product>_guarded_mutation` / `<product>_guarded_insert_returning` 及 `source_diagnostics`，严格区分 `COMMITTED` / `CONFIRMED_NOT_EXECUTED` / `CONFIRMED_ROLLED_BACK` / `COMMIT_STATUS_UNKNOWN`（`.unknown()`）；`pgwire/simple.rs` 与 `worker.rs` 支持 returning 结果集返回。
2. **Guarded DDL**：`modorch-fedengine-ddl` 支持 `RENAME TABLE` / `RENAME INDEX` / `ADD|DROP CONSTRAINT` / `TRUNCATE` 等 AST 与 v2..v5 类型意图抽取；`modorch-fedengine-duckdb::guarded_ddl` 实现 `<product>_ddl_capabilities` 校验、`<product>_guarded_ddl` 调用与唯一 `_fedengine.ddl_operations` 账本落库，保持 DDL 后异步刷新与 `MetadataStale` 状态流转（详细动作门禁见 [DDL 能力矩阵](ddl-capability-matrix.md)）。
   - 说明：本地单元测试与 `analyze`/`struct_pack`/returning 解码/账本测试均已通过；真库 live PG/MySQL/Oracle/SQLServer/Dameng/DB2 Guarded 端到端待连接对应远端环境与扩展 `make` 构建验证。

## 6. 批次 4（原批次 B、C 长期项与批次 D）：高级元数据、压力治理与规模/耐久验收

### 6.1 表达式、约束与统计完善（库内实现已就绪）

- 11 种数据源元数据能力矩阵（`MetadataSupportMatrix`）覆盖核心结构、列属性、约束、索引（含单体大小/扫描计数 `pg_stat_all_indexes`）、关系与非值列统计（`null_frac`/`avg_width`/`n_distinct`）及存储/活跃计数，未认证部分保持显式 `unsupported`，不造假定义。
- `modorch-fedengine-catalog` 增加 `NamesCache` + `SingleFlight` 并发合并，避免相同快照并发查重击穿透存储；`modorch-fedengine-management::demand_repair` 增加有界单飞优先级按需修复队列。

### 6.2 高阶复杂类型与生产发布专项目标（独立长期项，不阻塞当前开发收口）

- **超过 38 位任意精度 numeric、JSONB（OID 3802）及多维/非 1 下界业务数组**：必须经独立原型验证通过源读取、值表示、算术/运算符与 PG Wire 全链路后按单独里程碑验收；未达标前继续返回 `0A000`。
- **生产发布扩展验签**：生产打包阶段将开发期 `allow_unsigned_extensions = true` 收紧为自有公钥验签或静态编入定制 `libduckdb`。
- **与 `magicDataHub` 联合宿主验收**：完成 MQTT + FedEngine 同 Application 联合资源预算与双库组合验收。

### 6.3 规模、库内压力治理与故障验收（库内机制就绪，10k 本地规模回归已实测）

- `modorch-fedengine-duckdb::memory` 提供预算内 RSS 压力分级采样（`Normal`/`Elevated`/`Critical`）、`temp_directory` 有界磁盘 Spool（`BoundedTempSpool`）及残留清理；`runtime::isolated_io` 结合中断监控确保取消后真实释放租约与工作连接；可复用的本地 10k 表投影规模基线与发布验证命令见[开发文档](development.md#规模与故障验证边界)。
- 外部多源实库 24 小时耐久与强杀故障矩阵需在真实环境独立执行。

## 7. 每批交付与完成定义

每批必须交付实现差异、支持矩阵更新、真实 PG/源端对照、正常/异常/权限/取消/恢复证据、适用的 fmt/test/clippy/doc/build/smoke 和机器报告。未运行的项目标 `NotRun`，外部不可用条件标环境缺口，不记为通过。已实现并通过验证的条目方可从清单移出。
