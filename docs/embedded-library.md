# 嵌入现有 Modorch 应用

`crates/modorch-fedengine` 提供对外 Facade `modorch_fedengine`，使用与宿主相同的 Modorch-RS 0.0.3 Git Source/revision（见[开发说明](development.md)）。当前交付包括纯工厂注册、Profile、只读 Handle；开发测试宿主复用同一 Bundle，内部组件统一为 `modorch-fedengine-*`，固定组件 ID 保持原值；尚未发布到 crates.io。

## 已实现的公共接口

| 接口 | 行为 |
|---|---|
| `FedEngineConfig` | 当前 `modorch_fedengine_core::Config` 的公开别名，支持原有配置加载/校验 |
| `FedEngineBundle::new(config)` | 接收已解析配置，不读取文件、密钥或进程环境 |
| `FedEngineBundle::profile(profile)` | 显式选择受支持的网络入口组合，不改变内部源管理模式 |
| `FedEngineBundle::register(&mut builder)` | 纯配置校验、计算资源建议、注册 Factory，返回 `Result<FedEngineRegisteredComponents, FedEngineError>` |
| `components()` | 从实际 Factory descriptor 取得的 `ComponentRef`，按注册顺序返回 |
| `initiator_ids()` / `module_ids()` | 宿主显式选择所需的真实 ID |
| `resource_requirements()` | `FedEngineResourceDemand`，仅返回同步 I/O 线程/任务容量与每 Owner 任务配额建议 |
| `handle()` / `observe(&ApplicationHandle)` | 获取可克隆的只读 Handle，并由宿主在启动/失败清理结果返回后关联真实 Application |
| `FedEngineHandle::status()` | 当前 Application 状态、组件选择覆盖、真实组件代际/历史失败，以及保守的 Ready/可用地址 |
| `pgwire_address()` / `management_address()` / `metrics()` | 只读地址和指标；未关联、未全选、非 Ready 时不暴露可用端口 |

Bundle 不持有 `ApplicationHandle`，不创建 Application/EventHub/Tokio Runtime、不安装日志或信号处理、不调用 build/start、不修改宿主组件选择、资源预算、消息限制或执行器。宿主解析配置路径、管理库/临时目录/证书路径及 SecretRef；相对路径仍沿用进程工作目录语义，不会自动相对配置文件重定位。组合部署建议由宿主传入绝对路径；各字段、密钥来源和状态保护见[配置与路径规则](configuration-ownership.md)。

注册和 build 不打开数据库、不创建目录、不加载扩展、不读取凭据、不绑定或监听端口、不连接数据源。业务资源在选中组件的 setup 中初始化；全体 ready 钩子完成后才激活网络入口，生命周期见[架构说明](architecture.md)。

## 宿主装配

本地开发依赖可写为：

```toml
[dependencies]
modorch = { git = "https://github.com/muidea/modorch-rs", rev = "336ba20196044e1b81fb2ca297a18b5e258a5800", version = "=0.0.3" }
modorch-fedengine = { path = "../modorch-fedengine/crates/modorch-fedengine" }
```

Git 引用必须固定到包含该 Facade 的实际提交；本文实现尚未提交或推送，不能用此前仓库 HEAD 声称已有此 API。完整可编译装配片段见[Facade Rustdoc](../crates/modorch-fedengine/src/lib.rs)。顺序如下：

1. 宿主加载 `FedEngineConfig`，确定所有路径、密钥引用和监听地址。
2. 在宿主 Builder 上调用 `FedEngineBundle::new(config).profile(profile).register(&mut builder)`；省略 profile 时沿用默认 PGWire 开启、HTTP 跟随配置的入口选择。
3. 将返回的所有 Initiator/Module ID 加入宿主的 `ComponentSelection`，保留其他业务库的选择。注册本身不启用组件，完全不选择 FedEngine 时，启动不会创建其数据库或网络入口。
4. 宿主归并资源建议，显式配置 Builder。建议采用既有单库基线公式，不包含其他库、全局任务/消息限制、数据库内存/磁盘开销，也不保证满载时无配额拒绝；联合宿主须单独设置和验收预算。
5. 宿主执行唯一的 build/start，检查 Ready，调用 `registered.observe(&app)` 关联健康观测，再接入请求。启动失败保留 `StartupError.source`、`cleanup` 和清理 handle，可尝试关联 `registered.observe(&error.handle)`；清理未完成时保留 Runtime 并继续检查式关闭。
6. 宿主负责运行期等待和停止。`shutdown_checked` 返回 `StopIncomplete` 时保留 Application/Runtime 及资源并重试；不能把 Forced 当作正常排空。

当前所有受支持 Profile 均保留核心注册集合：

| 类型 | ID |
|---|---|
| 核心 Initiator | `duckdb-instance`、`datasource-registry`、`duckdb-runtime` |
| Module | `catalog-manager`、`policy-manager`、`query-coordinator`、`source-manager` |
| 入口 Initiator | `pgwire-listener`、`management-api`（由 Profile 决定） |

## Profile 与配置边界

| Profile | PGWire | 管理 HTTP | 内部管理模式 |
|---|---|---|---|
| 默认（未调用 profile） | 开 | 跟随 `management.enabled` | 保持配置 |
| `PgServer { management_api: false }` | 开 | 关 | 保持配置，可保留 managed storage / 虚拟库 |
| `PgServer { management_api: true }` | 开 | 开 | 要求 `management.enabled = true` |
| `ManagedServer { pgwire: true }` | 开 | 开 | 要求 `management.enabled = true` |
| `ManagedServer { pgwire: false }` | 关 | 开 | 要求 `management.enabled = true` |
| `EmbeddedCore` | — | — | 注册前返回 `FedEngineError::UnsupportedProfile` |

`management.enabled` 继续决定内部源管理/恢复模式，Profile 只控制网络入口。因此关闭 HTTP 应使用 `PgServer { management_api: false }`，无需关闭内部 managed storage 或删除虚拟库。尚未开放无网络的 EmbeddedCore；[受控进程内 Client](in-process-clients.md) 已开放，但不能通过任意删减组件选择宣称支持该 Profile。依赖缺少显式选择时由 Modorch build 拒绝；只选部分可启动组件时，Handle 也不会报告完整 Profile Ready。

`Config::validate_for_ingress(pgwire, management_http)` 分开校验入口；Bundle 根据 Profile 调用它。HTTP-only 不校验或读取 PG 监听地址、TLS、连接/Portal 上限和口令 SecretRef；仍保留业务 principal、源权限、受管理存储、虚拟库、源操作并发/队列/凭据目录以及共享查询超时的校验。当前共享查询超时仍取自 `pgwire.query_timeout_ms`（对象浏览查询也使用它），即使 PG 入口关闭也必须非零；跨库查询策略亦保持原有语义。HTTP 关闭时仅跳过其监听地址校验，内部管理配置仍须有效。

`Config::load()` / `validate()` 保持默认 Profile 的入口校验语义；需要接受未启用入口的无效字段时，由宿主用自己的解析器反序列化现有 Config，再交给 Bundle 作 Profile 校验。该低层校验 API 本身不代表开放 EmbeddedCore。PG 关闭后的资源建议不计 PG 连接配额，核心源管理资源建议仍保留。

配置仍使用当前同一套 DTO，未增加旧格式回退：HTTP-only 的 `users` 仍声明业务 principal，既有 `password_secret_ref` 字段可为空且不会被读取；未启用入口的配置字段仍按现有结构解析，Profile 决定哪些值需要校验。

## 只读观测与代际

0.0.3 的组件 Context 不提供最终 Application 健康视图，因此 Handle 不从 `ready` 或 `activate_ingress` 钩子推断整体 Ready。宿主在启动返回后调用一次 `registered.observe(&app)`，启动前取得的 Handle 克隆也会共享这次关联；同一 Application 重复关联幂等。Bundle 的 `duckdb-instance` 仅增加透明生命周期转发和实际 Application ID 见证，不新增组件、Hub、任务或 Runtime。

- 关联前为 `FedEngineObservation::Unobserved`，Ready 为 false、可用地址为 None；不把未知状态猜成未启用或失败。
- 关联后从 ApplicationHandle 按需读取实际健康。未选择为 NotSelected，部分选择为 PartialSelection；仅完整选择且整个 Application 与全部 Profile 组件 Ready 时报告 Ready。
- `components` 返回真实 `ComponentHealth`，代际分别读取 `generation`，不构造虚假的统一 Bundle 代际。成功恢复后可 Ready 且保留历史 `last_failure`，当前应用 `failure` 与历史失败分开。
- 关闭、恢复或故障使 Ready 失效时立即隐藏地址；`StopIncomplete`、Stopped、Forced 保留原 Application 状态，不把 Forced 当作正常释放证据。状态、地址、指标分别采样，不是连接租约，仍可能与下一次请求之间的停机竞争。
- 同 ID 的其他 Application 会因 ID 见证不匹配而拒绝关联，已关联 Handle 也拒绝切换 Application。若宿主在本 Bundle 开始 setup 之前就失败，身份尚未见证，关联会被拒绝；Handle 保持 Unobserved，真实失败仍由宿主持有的 StartupError 报告。

Handle 私下保留关联的 Application 引用以读取实时健康，但不暴露任何消息、Runtime 或 Shutdown 能力，不自动停机。宿主必须完成检查式退出并管理 Runtime 的真实存续；读取 Handle 不驱动初始化、查询或资源清理。`FedEngineMetrics` 为既有原子计数快照，不是 SQL 查询，也不承诺全局一致采样。

## 注册失败与实例边界

配置校验和资源建议的溢出/边界检查在首个 Factory 注册前完成。无效配置不会留下已注册组件。Bundle 不提供外部资源的预探测，实际文件、凭据、端口和远端错误仍在 setup/激活中报告并补偿。

组件 ID 固定，因此一个 Application 仅支持一个 FedEngine。第二个 Bundle 或宿主的同名同角色组件会返回 `FedEngineError::Framework(FrameworkError::DuplicateComponent(...))`，保留实际冲突 `ComponentRef`。此前未发布接口的 FrameworkResult 现由 FedEngineError 包装，新增类型化 UnsupportedProfile；不添加双格式或旧接口回退。

Modorch 0.0.3 的 Builder 没有已注册 ID 查询和注册回滚接口，重复 ID 可能在前序 Factory 已注册后才被发现。**register 返回错误后，宿主必须丢弃该 Builder 并重建整个组合，不得继续 build/start 部分注册的结果。** 跨 Application 实例应使用各自的状态文件、临时目录和端口，不能通过更换组件名共用同一状态库。实例 setup 会拒绝同进程中管理文件的重复占用；占用与实际连接一起保留，正常排空或完成启动补偿后才释放。

## 开发测试宿主

`tests/support/host` 中的 `modorch_fedengine_test_host::FedEngine::start(config)` 仅供开发/测试，根包通过 dev-dependency 使用；正式服务统一由 magicDataHub 注册 Bundle 并编排，无库级服务二进制或镜像。测试适配层使用默认 Bundle Profile：始终开启 PGWire，管理 HTTP 跟随 `management.enabled`，内部源管理模式和配置路径语义保持原样。适配层不再直接注册业务 Factory 或维护组件 ID/资源公式；它从 Bundle 返回结果显式选择全部组件，将资源建议用于测试宿主 Builder，并保留原有 64/64 消息队列容量和其他默认预算。

适配层只 build/start 一个 Application，成功后自动关联同一 Bundle 的只读 Handle。`metrics()` 返回相同计数快照；`address()` / `management_address()` 现在采用 Handle 的可用地址语义：失去 Ready、开始停止或停止未完成时返回 None，不能再用于查询已绑定但不可接流量的地址。现有公开 `handle` 仍由测试宿主用于 wait 和检查式退出。

`examples/fedengine-test-host.rs` 通过 Facade 配置类型加载原 TOML，继续拥有命令行、日志 Subscriber、Tokio Runtime 和 SIGINT/SIGTERM；它等待信号或 Application 结束，再调用 `shutdown_until_drained()`。启动失败原样返回可 downcast 的 `StartupError`，保留主因、首次 cleanup 和清理 handle；首次清理失败时继续排空并记录重试结果。启动后的观测关联/就绪检查失败也先执行检查式清理。`shutdown()` 返回真实回执，`shutdown_until_drained()` 仅对 StopIncomplete 保留资源并重试，Forced 回执不会转换成 GracefullyStopped。

[测试宿主回归](../tests/standalone.rs)验证默认 HTTP 开/关与 Bundle 组件集合一致、真实 PG/HTTP 服务、停止时隐藏地址、PG/HTTP 端口占用失败的完整 StartupError、同一状态文件补偿后重启，以及慢 HTTP 请求使检查式退出保留任务、客户端退出后继续排空并释放端口。管理与虚拟库既有集成用例继续通过该薄适配验证状态/身份与重复启动行为。

## 可复用验证与当前边界

[独立宿主样例](../examples/embedded-in-modorch/README.md)提供单独 workspace/锁文件的可运行消费者。根目录执行 `python3 scripts/test-embedded-host.py` 会复制样例到仓库外临时目录，只通过公开 Facade 接入，检查唯一 Modorch 0.0.3 与宿主所有权，并运行 6 项外部消费者契约、2 项宿主路径准备测试及真实二进制信号/失败/不同工作目录冒烟。它单独执行 fmt/Clippy/test/doc，不包含在根 `cargo test --workspace` 内；CI 已添加专门步骤。

```sh
cargo test --test bundle --locked
cargo test --test ingress --locked
cargo test --test standalone --locked
cargo test -p modorch-fedengine --doc --locked
cargo test -p modorch-fedengine --lib --locked
```

[Bundle 回归](../tests/bundle.rs)验证纯注册/现有状态保护、配置错误/重复 ID/缺依赖、未选择不实例化和宿主预算/执行器保持，以及正式组件的 PG/HTTP 运行。Profile/Handle 用例覆盖 HTTP-only 无 PG 配置/口令、PG-only 保留 managed storage 和虚拟库查询、错误/未启用/部分选择、跨 Application 拒绝与隔离、慢 HTTP 请求的 StopIncomplete 和真实退出。低层[观测测试](../crates/modorch-fedengine/src/handle/tests.rs)使用真实框架监督和无数据库 Probe，验证恢复中不 Ready、恢复后实际代际推进与旧能力失效，以及 Forced 状态；它不代表整个数据库业务的监督重启已验收。

[资源专项](resource-lifecycle.md)补充真实 PG 扩展阻塞写入、取消后资源保留、短停机重试、跨进程文件锁和持久身份验证。数据库监督用例通过测试 Wrapper 对正式实例 Factory 注入 Restart，证明旧闭包排空后才推进代际并关闭旧会话；公开 Bundle 仍为 FailFast，没有增加自动重建策略接口。

仓库外样例已覆盖纯注册、未选择不实例化、缺依赖/重复 ID、三种入口组合的真实服务、宿主 Ready 失败补偿和 StopIncomplete 重试。MQTT 真并发及联合宿主的远端 CRUD/DDL 组合回归仍需后续收口。本项不改变现有数据模型、对象身份、凭据或迁移规则。

## 契约与交付入口

当前分层见[业务库架构](architecture/embedded-library-design.md)，完整宿主步骤见[modorch-host](integration/modorch-host.md)，名称/API/路径对照见[迁移说明](migration/fedengine-to-modorch-fedengine.md)。[业务契约](../contracts/README.md)及其测试映射区分静态映射和真实执行；[发布说明](releases/0.1.0.md)明确源码分发与未发布状态。magicDataHub 的未交付组合项见[接入边界](integration/magicdatahub.md)。
