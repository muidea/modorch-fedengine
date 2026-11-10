# 执行资源、取消与重建边界

同步远端调用返回前，取消请求或客户端错误不能释放它使用的资源。宿主持有 Application 和 Tokio Runtime，直到检查式退出实际完成；`StopIncomplete` 表示继续等待既有关闭事务，`Forced` 不表示正常排空。

## 所有权与退出顺序

`query-coordinator` 先取得实例读屏障，再按 DataSourceId 排序取得源锁，复核源版本后将这些 guard、执行/Registry 的 ServiceLease 交给 `duckdb-runtime`。Runtime 将它们与 Worker permit 一起捕获进 Owner 的 `spawn_blocking_io`；连接、Statement 和原生执行均位于该任务作用域。调用方不持有可提前释放这些资源的开关。

取消监控在客户端取消、执行截止时间或 Owner 停止时反复发送 DuckDB interrupt，持续到实际 Worker 退出。扩展正在等待远端响应时，中断不保证立即终止原生调用。结果等待 Future 或 ExecutionStream 被丢弃，也不等于阻塞任务已退出。DDL 已确认提交后的刷新使用 `wait_finished()`，在连接与执行屏障归还后才继续发布目录；不重新发送 DDL。

| 事件 | 必须保持的边界 |
|---|---|
| PortalSuspended | 继续保留同一执行流及实例/源屏障；有界通道背压不会释放 Worker 额度 |
| Close、完整消费、取消或截止时间 | 请求终止或关闭流；实际 Worker 退出后才归还连接、租约、锁与额度 |
| 客户端收到 `57014` | 原生 I/O 可能仍在运行；写入结果可能未知，不自动重放，不据此报告回滚 |
| 检查式退出超时 | 返回 StopIncomplete；保留 Application/Runtime 和资源，再次检查式退出等待同一事务 |
| 原生 I/O 最终退出 | 归还 Worker 捕获的资源；固定点排空后逆依赖拆卸，才返回 GracefullyStopped |
| 强制关闭 | Forced 单独保留，不能替代资源释放、远端副作用结束或安全重试的证据 |

## 入口与执行代际

PGWire 和管理 HTTP 都显式 `requires duckdb-runtime`；HTTP 还依赖 Registry。入口 Initiator 不依赖 Module，SQL/管理请求仍经过现有 Typed Request 路径。执行 Runtime 或其上游实例被重建时，两入口进入同一停止闭包，旧连接、Prepared 所在会话、悬停 Portal 和 HTTP 请求不能继续进入替代执行代际。

若显式配置框架 Restart 策略，先停止该依赖闭包并等待旧任务/租约归还，拆卸旧实例，再推进代际、初始化存储/源/目录和激活入口。旧 TaskScheduler 与服务句柄失效，应用级 Hub 仍是同一份。新会话重新认证、解析和绑定；持久源 incarnation、对象 OID 等当前模型身份按原恢复契约保留。重建不重放写入。

公开 Bundle 当前仍沿用 Factory 的 **FailFast** 策略，没有新增自动重启配置或承诺任意组件可独立热重启。真实数据库监督测试使用测试专用 Wrapper，仅将实例 Owner 策略设为 Restart，运行其余正式业务 Factory；该用例验证数据库闭包的代际安全，不等同于公开 Bundle 已开放自动恢复 API。生产宿主默认在故障后完成检查式排空，再按原配置重建 Bundle。

## 可复用验证

[ownership.rs](../tests/ownership.rs) 的两个 ignored 用例由 `scripts/test-federation.sh` 使用专用一次性数据库执行，CI 同样覆盖：

| 用例 | 关键断言 |
|---|---|
| `blocked_write_cancel_and_shutdown_retain_real_resources` | 正式 Bundle；真实 PG-backed 悬停 Portal 在 Close/Sync 前持有源/实例屏障，实际 Worker 结束后才归还；PG 触发器的 advisory lock 让 INSERT 真正阻塞在扩展 I/O 中，CancelRequest 返回 `57014` 后 Worker 仍占槽，后续查询返回 `53300`；两次短停机均为 StopIncomplete，源锁/实例屏障和跨进程 DuckDB 文件锁仍被持有；解锁后 GracefullyStopped、任务/会话/Worker/pending 归零，文件可重新打开 |
| `supervised_instance_rebuild_drains_io_and_invalidates_sessions_and_capabilities` | 全部使用正式业务 Factory，仅实例策略由测试 Wrapper 注入 Restart；同时存在真实源 Portal 与阻塞 INSERT，故障关闭旧会话后仍等待原生 I/O，旧代际和文件锁保持；解锁后才创建替代实例，两个入口代际推进，旧任务与服务能力不可复用，新连接可查询持久 OID/源身份 |

两个用例都通过远端非事务序列记录触发器进入次数，核对取消/关闭/重建前后 INSERT **只尝试一次**。最终远端行可能存在或不存在，两种结果都不能从取消响应推断；测试不将不确定结果当成可安全重试。

```sh
cargo test --workspace --locked
scripts/test-federation.sh
python3 scripts/test-embedded-host.py
```

默认工作区测试只编译并忽略远端用例。一次性脚本固定测试 Compose 项目并清理容器/卷；不得与另一轮相同项目并行，也不得使用业务 DSN。测试用独立子进程检查真实 DuckDB 文件锁，不依赖同进程连接缓存；临时管理文件及远端测试对象在退出后清理。

本专项在 PostgreSQL 17.7 的真实 libpq 阻塞路径执行；没有验证 MySQL 阻塞网络调用、任意网络黑洞、存储卡死、进程强杀或永久不返回的扩展。没有承诺原生 I/O 可在给定期限内中止；永久阻塞时宿主仍须保留资源，或按自身故障策略处理 Forced 与未知副作用。完整故障、规模/长时及 MQTT 联合资源预算继续单列，参见[开发说明](development.md)与[协议/执行矩阵](library-regression.md)。

管理文件的进程内占用与共享根连接一起存续，补充 DuckDB 原生跨进程锁。同文件的独立 Application 在实例 setup 被拒绝，路径规则和可复用用例见[配置所有权](configuration-ownership.md)。
