# magicDataHub 接入边界

所有正式服务统一由 [magicDataHub](https://github.com/muidea/magicDataHub) 编排与部署，其开发基线已提供三个服务、联合预算、协议/远端验收及镜像运行入口。modorch-fedengine 只交付可嵌入库及 Bundle/Profile/Handle/资源建议，无可发布的独立服务或 Docker 镜像。本文件维护库与宿主的职责约定；具体宿主版本的联合结果、Git依赖和部署状态由 magicDataHub 独立验收。

## 三种应用的装配职责

| 宿主形态 | FedEngine 处理 | 宿主交付与验收 |
|---|---|---|
| 独立 MQTT App | 不注册/选择 FedEngine，不创建其 DB/端口 | 宿主 workspace、配置、MQTT 功能及排空验收 |
| 独立联邦数据库 App | 在唯一 Builder 注册 FedEngine 并显式选择 | 宿主可执行程序、独立配置、PG/HTTP、CRUD/DDL 与退出验证 |
| 联合 App | 在同一 Builder 注册真实 MQTT Bundle 和 FedEngine Bundle，合并 IDs 与预算 | 唯一 Application/Hub、双库流量、资源争用、补偿/代际和隔离验收 |

`modorch-fedengine-test-host` 位于 FedEngine 的测试支持目录，不能成为服务依赖；联合宿主直接注册公开 Bundle，不能调用测试适配层创建第二个 Application，也不复制 MQTT 协议、PGWire 或 DuckDB 执行层。组件 ID 冲突须在 build 前处理；FedEngine 当前不提供可自定义 namespace，一个 Application 只支持一个 FedEngine。

## 依赖、配置和入口

所有库和三个应用应从同一个 Git Source/revision 解析 Modorch-RS 0.0.3，使用 `cargo metadata --locked` 验证唯一 Package ID。0.0.3 的不同 Source 仍是不同 Rust 类型；FedEngine Facade 的实际消费 revision 待当前代码提交后确定，不能填此前远程 HEAD 作为占位。

每个独立运行应用分配独立管理文件、临时目录、端口、配置和凭据来源；即使进程不同也不共用可写 DuckDB 文件。相对路径规则和已有状态保留见[配置所有权](../configuration-ownership.md)。宿主对外服务需配置 TLS、认证和授权，管理 HTTP 的 Origin 检查不能替代管理员鉴权。

FedEngine 支持 PGWire/HTTP 两者、PG-only、HTTP-only，核心 managed storage 与网络入口分开。宿主可在 Ready 后绑定[受控 QueryClient/ManagementClient](../in-process-clients.md)，明确认证 Principal、调用预算和管理范围；DDL 通过 QueryClient。EmbeddedCore 仍未开放。联合 App 不直接暴露内部 Hub/执行服务；本库验收不代表 magicDataHub 联合应用完成。

## 联合预算与生命周期验收

FedEngine 建议只涵盖本库的同步 I/O 和每 Owner 任务配额。宿主须归并 MQTT 网络/会话/控制面需求，明确总线程、队列、等待者、消息额度、内存和磁盘预算，并在低配额和并发流量下验证。不能把两套建议相加后视为容量认证，也不能为通过测试在失败后静默提高额度。

| 验收方向 | 必须观察的结果 |
|---|---|
| 真实双库装配 | 同一 Builder、Application ID、Hub/Registry，返回的实际组件 ID 全部明确选择 |
| 注册零副作用 | 单纯注册不读 DB/密钥/端口；只启 MQTT 时不实例化 FedEngine |
| 流量并发 | MQTT 实际发布/订阅与 PG/HTTP、一次性 PG/MySQL CRUD/只读 JOIN 同时可用 |
| 部分启动失败 | 保留原主因和 cleanup；已激活入口被补偿，其他应用实例仍可用 |
| 低配额与慢操作 | 有界拒绝/背压和控制面可用性；远端慢 I/O 不抢占异步执行器 |
| 停止与代际 | StopIncomplete 保留真实资源；正常排空后端口/文件可重开；旧代际服务/任务不可复用 |
| 多 Application | 同进程和独立进程使用独立文件/目录/端口/凭据；停一方不破坏另一方 |

FedEngine 单库测试与测试注入 Restart 的数据库停止闭包为这些验收提供基线，不能替代真实 MQTT 联合结果。生产默认 FailFast，尚无公开任意热重启策略。规模、长时运行和所有外部故障组合继续专项验收，不能据一次功能 smoke 标为生产可用。

起点为[最小宿主](modorch-host.md)、[业务库架构](../architecture/embedded-library-design.md)和[契约测试映射](../../tests/contracts/mapping.json)。项目级进度由外部生态核对清单维护，本文件不存逐次运行日志。
