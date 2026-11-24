# DDL 能力矩阵

生产启用必须同时满足 grants、源可写（非只读且非 StarRocks）、ddl_enabled、namespace allowlist、全局 DDL 开关、精确能力记录。记录匹配 DuckDB 核心、已加载扩展和远端版本；启动仅查询版本，不对业务源执行破坏性探测。

| 操作 | 内部路径 | 门禁 / 边界 |
|---|---|---|
| CREATE/DROP SCHEMA | `*_guarded_ddl` (v5, 支持 `conditional`) | PG/GP/KingbaseES、MySQL/MariaDB、SQL Server、Dameng、Db2、HANA 支持；Oracle 返回 0A000；DROP 须开启 `allow_drop_schema`，无 CASCADE |
| CREATE/DROP TABLE | `*_guarded_ddl` (v2/v5/v6, 支持 `conditional`) | 覆盖全部 10 个事务源；v6 支持列级 `NOT NULL`/`DEFAULT`/`PRIMARY KEY`/`UNIQUE` 与表级 `PRIMARY KEY`/`UNIQUE`/`FOREIGN KEY`/`CHECK`；无临时表及任意选项 |
| CREATE TABLE AS SELECT | DuckDB 原生 Catalog CTAS / `*_guarded_ddl` (v6 `create_table_as`) | PG/MySQL 家族走 DuckDB 原生 Catalog CTAS；Oracle/SQL Server/Dameng/Db2/HANA 走 v6 `create_table_as`（仅允许同源标量查询，不支持 CTE 与参数占位符）；须开启 `CREATE_TABLE_AS` |
| ALTER TABLE | `*_guarded_ddl` (v3/v4/v5/v6, 支持列/约束 `conditional`) | 逐动作支持 `ADD COLUMN`（含 v6 列约束/默认值）、`DROP COLUMN`、`RENAME COLUMN`、`RENAME TABLE`、`COLUMN TYPE`、`DEFAULT`、`NOT NULL`、`CONSTRAINT`；MySQL/MariaDB `SET/DROP NOT NULL` 与类型修改均在 `LOCK TABLES` 下完整保留列属性；MySQL 8.0.16+ 与 MariaDB 原生执行 `CHECK` 约束，MySQL 5.7 因不强制检查而明确拒绝 |
| CREATE / DROP / RENAME INDEX | `*_guarded_ddl` (v2/v3, 支持 `IF [NOT] EXISTS`) | 覆盖全部 10 个事务源；PG/MySQL/SQL Server 验证所属表 `owner`（`DROP INDEX IF EXISTS` 对不存在索引放行）；Oracle/Dameng/Db2/HANA 使用 schema 级索引名；受保护/唯一约束索引禁止删除或改名 |
| REINDEX INDEX | `*_guarded_ddl` (v4) | 通过 `parse_reindex_index` 严格语法验证；PG/GP/KingbaseES、Oracle、SQL Server、Dameng 支持；MySQL/MariaDB/Db2/HANA 返回 0A000 |
| SQL Server < 2005 兼容 | `sqlserver_ddl_capabilities` | `MssqlMajorVersion < 9` 返回 `executor_available: false`，执行器在预检阶段以 `0A000` 明确拒绝（不触发内部语法错误） |
| StarRocks DDL | 策略门禁 | 固定只读源，任何 DDL 返回 0A000 |

ALTER 细分能力为 `ALTER_TABLE_ADD_COLUMN`、`ALTER_TABLE_DROP_COLUMN`、`ALTER_TABLE_RENAME_COLUMN`、`ALTER_TABLE_COLUMN_TYPE`、`ALTER_TABLE_COLUMN_DEFAULT`、`ALTER_TABLE_COLUMN_NULLABILITY`、`ALTER_TABLE_ADD_CONSTRAINT`、`ALTER_TABLE_DROP_CONSTRAINT`。普通索引要求 `CREATE_INDEX` 与 `CREATE_INDEX_BTREE`，唯一索引要求 `CREATE_INDEX` 与 `CREATE_INDEX_UNIQUE_BTREE`；`RENAME_INDEX`、`DROP_INDEX` 与 `REINDEX` 需要对应能力与索引归属校验（`index_adapter=true`）。

| 数据源家族 | `ddl_capabilities` / `guarded_ddl` 函数 | 扩展关系元数据 (`column_properties`/`constraints`/`indexes`) |
|---|---|---|
| Postgres / Greenplum / KingbaseES | `{postgres,greenplum,kingbasees}_*` | 原生系统目录查询 |
| MySQL / MariaDB | `mysql_*` | `information_schema` 查询 |
| StarRocks | 只读禁止 | 仅基础目录 |
| Oracle | `oracle_*` | `oracle_relational_metadata` |
| SQL Server | `sqlserver_*` | `sqlserver_relational_metadata` |
| Dameng / Db2 / SAP HANA | `{dameng,db2,hana}_*` | `odbc_relational_metadata` |

DDL 成功后增加源代际并调用对应清晰缓存函数（`pg_clear_cache`/`postgres_clear_cache`、`mysql_clear_cache`、`*_clear_cache`）。所有 Guarded DDL 执行过程写入 `_fedengine.ddl_operations` 审计账本；结果未知不会自动重试。

具体版本组合需在目标环境运行 `scripts/test-federation.sh`（以及各源联调容器）验证；未单独验收的变种保持能力门禁关闭。
