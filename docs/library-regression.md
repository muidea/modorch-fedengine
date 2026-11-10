# 库化后的协议与执行回归

本矩阵维护可复用验收入口与能力边界，不保存单次运行日志。测试专用适配层 `modorch_fedengine_test_host::FedEngine` 通过公开 Facade 注册正式 `FedEngineBundle`，使用宿主持有的唯一 Application 启动和检查式退出；以下协议及远端业务回归走这条装配路径。外部宿主的依赖、选择和生命周期验收另见[嵌入说明](embedded-library.md)与[独立样例](../examples/embedded-in-modorch/README.md)。

## 验收矩阵

| 范围 | 可复用测试入口 | 关键断言 |
|---|---|---|
| 受控进程内 Client | [clients.rs](../tests/clients.rs)、仓库外 host.rs | 参数化/描述/结果预算、Clone 准入、宿主身份/管理范围、CAS/OID、不重放；ignored 真实双库 CRUD/JOIN/DDL 与取消后 drain |
| PGWire 基础与安全 | [protocol.rs](../tests/protocol.rs) | 真实 TLS/SCRAM、Simple/Extended、类型与参数文本/二进制、NULL、错误后 Sync 恢复；显式空虚拟库使用正式持久化管理组件，不注入合成的 Active 源 |
| Portal 与取消 | [protocol.rs](../tests/protocol.rs) | 命名 Portal 的 maxRows/Flush 续读同一执行；Close 释放配额；真实 CancelRequest 返回 `57014`，同连接继续可用；悬停且输出通道已满时关闭仍核对真实排空 |
| PG 行为对照 | [pg_alignment.rs](../tests/pg_alignment.rs) | 本地类型、会话和错误码断言；ignored 用例直接对照原生 PG 的 decode、参数、regclass 与元数据结果，不以查询成功替代值/OID/错误码比对 |
| PG/MySQL CRUD | [federation.rs](../tests/federation.rs) | 两种真实源上的参数化 INSERT、UPDATE/DELETE 影响行数、无主键表重复行与 NULL；不为计算行数重放写入 |
| 跨源读与协议 | [federation.rs](../tests/federation.rs) | PG/MySQL 只读 JOIN；涉及两源的长查询通过真实 CancelRequest 中断，随后同连接可继续 JOIN；原始 Extended 报文暂停/续读 JOIN Portal，并在剩余行未读时 Close/Sync |
| 写入边界 | [federation.rs](../tests/federation.rs) | 跨源 INSERT/CTAS/FK 拒绝；含写入的多语句 Simple Query 在第一条执行前返回 `0A000`，原表行数不变 |
| DDL 能力门禁 | [federation.rs](../tests/federation.rs) | 无能力记录时两源 CREATE TABLE 均返回 `0A000`，独立原生查询确认无远端对象；分别篡改 PG/MySQL 能力记录的远端版本，启动在 Setup 拒绝并保留 StartupError 与成功清理回执 |
| 已验证 DDL | [federation.rs](../tests/federation.rs) | 精确版本组合下的 Schema/Table、同源 CTAS、ALTER ADD/DROP、受控普通/唯一索引；直接检查远端索引存在/删除、唯一约束及 MySQL 受保护索引拒删；结构变化使旧 Prepared 返回 `0A000` |
| 刷新失败与恢复 | [management.rs](../tests/management.rs) 的 `zero_source_online_activation_catalog_refresh_rollback_portal_drain_and_recovery` | 真实远端 DDL 已提交但刷新失败时保留成功与 Notice、标记 MetadataStale 并阻断业务查询；失败操作保留既有状态，修复后刷新恢复 Active |
| 在线排空与重启 | 同一 management 用例、`mysql_strict_identity_rotates_on_resume_and_startup` | 真实 PG Portal 阻止停源完成，WaitingForDrain 不阻塞无关查询，Close/Sync 后完成；保留/严格身份策略、删除重建 incarnation、重复重启和持久化中断恢复，故障源不阻塞健康源 |
| 延迟 I/O 与所有权 | [ownership.rs](../tests/ownership.rs) | 正式 Bundle 的真实 PG 阻塞写入、取消后的 Worker/源锁/实例保留、StopIncomplete 重试及跨进程文件锁；测试注入 Restart 的正式数据库闭包验证旧会话和能力失效、不重放写入，详见[资源专项](resource-lifecycle.md) |
| 配置与实例隔离 | [instances.rs](../tests/instances.rs) | 同管理文件及路径别名拒绝、并发首次打开、启动补偿、现有状态/版本保护；ignored 真 PG 验证独立凭据目录、同名引用及更名重启后的 OID/incarnation，见[配置规则](configuration-ownership.md) |
| 管理端与虚拟库 | [browser.rs](../tests/browser.rs)、[organization.rs](../tests/organization.rs)、[虚拟库测试](../tests/virtual_database.rs)及 `virtual_database_*` | 只读调试配额、取消/超时、映射与授权、来源发现、目录原子发布、CONNECT/owner/OID 与重启；远端用例由一次性脚本执行 |

`tests/common/protocol_engine.rs` 只保留给管理队列/任务配额故障注入等专用夹具。其 `management_quota_queue_backpressure_and_shutdown_preserve_unfinished_intent` 不计为正式 Bundle 端到端验收。`engine_control.rs` 的 ignored 用例直接验证原生控制与挂载，也不计为 Bundle 协议证据。

## 复现

从仓库根目录执行，依赖版本和原生库准备见[开发说明](development.md)：

```sh
python3 .agents/skills/modorch-app-development/scripts/preflight.py --manifest-path Cargo.toml
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
scripts/test-federation.sh
```

默认 workspace 测试不执行 ignored 远端用例。一次性脚本创建专用 PostgreSQL 17.7 / MySQL 5.7 容器，运行受控 Client、PG 对照、原生控制、浏览、来源组织、联邦、资源所有权、实例隔离和管理回归，结束删除该测试项目的容器、网络与卷。可通过 `FEDENGINE_TEST_POSTGRES_PORT` / `FEDENGINE_TEST_MYSQL_PORT` 改变测试端口；不要把 DSN 指向业务数据库，也不要与另一轮相同 Compose 项目的测试并行。规模用例 `metadata_scale_refresh` 单独执行，不在功能回归脚本内。

测试从一次性源读取实际 DuckDB/扩展/远端版本元组，再为本轮允许的 DDL 建立能力记录；这不授权生产环境自动探测 DDL，也不表示其他版本组合已获验证。升级任一版本后需重新执行，门禁及操作范围见[DDL 矩阵](ddl-capability-matrix.md)。

## 验收边界

- JOIN 取消用例覆盖远端表执行及恢复；独立[资源专项](resource-lifecycle.md)另用真实 PG advisory lock 阻塞扩展写入，核对取消后的租约/实例所有权、StopIncomplete 重试和测试注入 Restart 的代际闭包。`57014` 不等于 worker 已退出或写入已回滚；不推广到任意网络/扩展故障或公开自动重建 API。
- 不提供跨源事务或写入、完整 PostgreSQL SQL/协议、任意精度 numeric 或未经验证的 DDL 变体。准确范围见[协议说明](protocol-compatibility.md)和[SQL 能力](sql-capability.md)。
- JDBC/DBeaver 的既有固定版本验证单独维护，Rust/原始报文测试不能替代驱动或 GUI 回归。本矩阵不证明 MQTT/FedEngine 联合宿主、其他平台、规模、24 小时负载或完整网络/存储故障矩阵已通过。
