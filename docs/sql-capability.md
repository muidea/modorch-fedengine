# SQL 能力与边界

使用 sqlparser 的 DuckDB AST 解析整条 SQL，明确绑定注册源的 catalog/schema/object。未识别语法或能力一律拒绝；不会按前缀或字符串分号判断安全性。

| 范围 | 行为 |
|---|---|
| SELECT、聚合、CTE、子查询、JOIN | 支持 PostgreSQL/Greenplum/KingbaseES、MySQL/MariaDB/StarRocks、Oracle/SQL Server/Dameng/Db2/SAP HANA；函数为显式安全白名单，含 StarRocks 单源/类型改写 |
| INSERT/UPDATE/DELETE | 完整对象和依赖必须属于一个受支持事务源；经各扩展 Guarded CRUD 通道执行（StarRocks 固定只读，0A000 拒绝写入） |
| INSERT ... RETURNING | 支持全部 10 个事务源单标识列 RETURNING（PG 家族原生 RETURNING、MySQL `LAST_INSERT_ID()`/MariaDB 原生 RETURNING、SQL Server `OUTPUT INSERTED`、DB2 `FINAL TABLE`、Oracle/Dameng 单行 `RETURNING ... INTO ?`、SAP HANA 同连接 `CURRENT_IDENTITY_VALUE()`）；Oracle/Dameng 多行 INSERT RETURNING 返回 0A000 |
| UPDATE / DELETE ... RETURNING | UPDATE RETURNING 支持 PostgreSQL/Greenplum/KingbaseES（原生 RETURNING）、SQL Server（`OUTPUT INSERTED`）、DB2（`FINAL TABLE`）、Oracle/Dameng（单行 `RETURNING ... INTO ?`），MySQL/MariaDB/HANA 返回 0A000；DELETE RETURNING 支持 PG 家族、MariaDB（原生 RETURNING）、SQL Server（`OUTPUT DELETED`）、DB2（`OLD TABLE`）、Oracle/Dameng（单行 `RETURNING ... INTO ?`），MySQL/HANA 返回 0A000 |
| Oracle 多行写入与日期/二进制参数 | 多行 INSERT VALUES 自动改写为 `INSERT ALL ... SELECT 1 FROM DUAL`；DATE/TIMESTAMP/TIMESTAMPTZ 占位符分别包装 `TO_DATE`/`TO_TIMESTAMP`/`TO_TIMESTAMP_TZ`，BYTEA 按 BLOB 绑定 |
| 参数复用与乱序绑定 | 经 `param_map` 在 DuckDB `?` 占位符执行前完成槽位映射与重放，全部 `?` 占位符方言（MySQL 家族与 5 个 ODBC 源）均支持 `$1 ... $1` 与 `$2 ... $1` |
| 无主键表 | 不自行逐行拼装写入，Guarded CRUD 在表锁与结构指纹验证下执行 |
| 多语句 Simple Query | 允许受支持的只读查询及 SET/RESET；带写入、DDL 或事务时整条消息执行前拒绝 |
| 参数 | 显式 PG OID 或 SQL cast；无法推断返回 42P18；不猜测未知类型 |
| 显式事务 BEGIN/COMMIT/ROLLBACK | 0A000；ReadyForQuery 保持 Idle |
| COPY、临时对象、视图 DDL、任意管理 SQL | 拒绝 |
| 文件函数、远端执行函数、INSTALL/LOAD/ATTACH | 业务 SQL 拒绝；仅内部受控初始化与元数据通道可用 |
| pg_catalog / information_schema | 不开放任意系统目录访问；扩展元数据统一由 relational capture 与 capability contract 收口 |

跨源写入在远端执行前拒绝；单源 CTAS 在 PG/MySQL 家族走 DuckDB 原生 Catalog CTAS，在 Oracle/SQL Server/Dameng/Db2/SAP HANA 走 Guarded DDL v6 `create_table_as`（经 `query_sql` 同源校验后下推）。CREATE TABLE / ADD COLUMN 通过 Guarded DDL v6 契约支持内置标量类型白名单（含 MySQL/MariaDB `boolean`/`time`/`bytea`/`uuid`、Oracle `bytea`/`timestamptz`/`uuid`、SQL Server `timestamptz`、Dameng `timestamptz`/`uuid`、Db2/HANA `uuid`）、列级选项与表级主键/唯一/外键/检查约束，ALTER 逐动作验收。非唯一对象名、未知源、未授权 namespace 都会拒绝。

MySQL/MariaDB 与 PostgreSQL 家族使用独立 Guarded CRUD 契约；Oracle、SQL Server、Dameng、Db2、SAP HANA 在独立 ODBC 连接/事务内通过 `*_crud_metadata` + `*_guarded_mutation` / `*_guarded_insert_returning` 执行，并统一返回准确受影响行数与 RETURNING 列；SQL Server 2005 以前版本在能力探测阶段返回 `executor_available: false` 并以 `0A000` 拒绝。StarRocks 为 OLAP 只读分析数据源，即使配置中未标记只读，策略层与计划层也会拒绝所有写与 DDL（SQLSTATE `0A000`）。

单语句写入不等于全局事务。不同源没有全局一致快照；不承诺跨源原子提交。MySQL/MariaDB 表须使用支持事务的 InnoDB 引擎。触发器、外部策略等超出契约的特性按各源元数据验证门禁拒绝。
