# 进程内 Query / Management Client

适用 0.1.0 开发树、业务契约 1.1.2。宿主仅依赖 `modorch_fedengine` Facade；Modorch 固定 0.0.3。正式服务、鉴权入口、Application 和部署镜像统一由 magicDataHub 编排。FedEngine 只交付库。

## 绑定与身份

宿主显式选择完整 Bundle，start 后 `registered.observe(&app)`。随后调用 `registered.query_client(&app, principal, budget)` 或 `registered.management_client(&app, principal, budget, grant)`。两者只接受本 Bundle 已见证且完整 Ready 的应用；每次调用/流读取检查 Ready 与组件代际。检查是快照，应用级 Hub 不是原子代际租约，生产 Bundle 仍为 FailFast，不开放自动热重启。停机或已观测代际变化返回 55000，重新绑定须重新验证宿主策略。

`FedEnginePrincipal::authenticated(name, origin)` 表示可信宿主已经完成认证，绑定再核对配置用户。它不校验口令；不能把请求传入的用户名直接当作认证结果。origin 用于消息来源，身份和来源均非空、最多 128 字节且不含控制字符。QueryClient 不隐式授予任何管理权限。

管理授权使用 `FedEngineManagementGrant::default()`，默认全部拒绝。宿主按策略调用 `.source("exact-id")`、`.database("exact-name")` 和 `.allow_writes()`；名称没有通配语义。来源读取、操作回执和写入均检查范围，新虚拟库映射还检查来源范围。授权固定于 Client 生命周期；宿主策略变更时丢弃并重新绑定。

## 查询与结果

`prepare(database, sql, parameter_types, &cancel)` 通过既有 Catalog/Policy/Query 路径检查 CONNECT、对象权限、类型、单源写与 DDL 能力。SQL 为单条、无可变会话；明确指定数据库。Prepared 句柄不透明，保存数据库 OID/版本和源版本，只能由同 Client 或其 Clone 执行。数据库同名重建、映射变化或源修订会按原版本门禁拒绝，宿主决定是否重新准备。

`execute(&prepared, parameters, &cancel)` 返回 `FedEngineResultStream`，`columns()`/`notices()` 读取描述，`next().await` 逐批取得 `FedEngineResultChunk::Rows` 或 `Complete`。检查 Completion.outcome，保留实际 affected_rows、DDL notice 和 CommitConfirmed；流结束但无终态会报错。DDL 使用同一 QueryClient，无独立 DDLClient；仍要求精确 DuckDB/扩展/远端版本与操作能力记录。无记录不放行，跨源写禁止。

`FedEngineClientBudget` 默认 timeout 10s、max_in_flight 4、SQL 64KiB、参数 128、结果 10,000 行/估算 8MiB、控制 256KiB。绑定校验硬上限，timeout 不超过配置查询/管理超时的较小者。Clone 共享 semaphore，超额立即 53300；流从接收到终态/失败/丢弃期间占用 permit。执行与流消费共用 deadline。输入参数嵌套深度限制 64；元数据描述、管理序列化与交付前累计输出受预算约束。

字节预算是值和结构的估算，不能视为进程内存硬上限。内部目录、已生成批次和同步扩展 I/O 仍受既有全局 worker/channel/DuckDB 与宿主 blocking-I/O 预算控制，接入方也须限制自身传入对象大小。

## 管理与未知结果

ManagementClient 提供 source、submit、operation、database、mutate_database，沿用原 CAS incarnation/revision、稳定 OID、幂等回执和异步发布规则。不提供全局列表、原生执行口、内部 DDL 通知或停机能力。源定义继续传 SecretRef，不在 API 中传明文凭据。

提交管理变更时由宿主持有稳定幂等 key。请求返回的操作可能仍在运行，使用 operation 读取终态；超时或 future 被丢弃后已运行工作仍可能继续。保留原 key，主动查证回执后再决定后续动作，不自动生成新 key 重放。

`FedEngineCancellation`、取消 query future、丢弃 stream 均请求协作取消。57014、outcome_unknown 或关闭接收器不证明远端事务回滚、原生线程退出、ServiceLease/worker 归还。库不自动重试写入；宿主必须检查真实终态，并最终执行 checked shutdown。StopIncomplete 时继续保留 Application/Runtime/资源并等待真实排空。

## 验证与当前范围

本地真实 Bundle 用例在 `tests/clients.rs`，仓库外 Facade 消费在 `examples/embedded-in-modorch/tests/host.rs`；`scripts/test-federation.sh` 仅用一次性 PostgreSQL/MySQL 验证 CRUD、跨源只读、DDL、权限、管理与未知写入排空。契约与测试对应见[Client 契约](../contracts/clients.md)和[测试映射](../tests/contracts/mapping.json)。

支持 PgServer 与 ManagedServer（含 HTTP-only）的进程内 Client。无网络 EmbeddedCore 仍明确拒绝，MQTT/FedEngine 联合宿主、magicDataHub 正式应用、规模和长时认证需独立验收，不由 Client 测试推断完成。
