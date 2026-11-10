# FedEngine 嵌入式业务库架构

本文件描述 0.1.0 开发树已经实现的业务库边界。公开接口来自 `crates/modorch-fedengine`，框架固定 Modorch-RS 0.0.3；行为规范见[业务库契约](../../contracts/README.md)。协议、SQL 和管理能力沿用现有实现，不因库化重新实现。受控 [Query/Management Client](../in-process-clients.md) 已开放；EmbeddedCore 和任意热重启策略尚未开放。

## 分层与所有权

| 层 | 当前职责 | 不承担的宿主职责 |
|---|---|---|
| `modorch-fedengine` | Facade、纯注册 Bundle、Profile、资源建议、只读观测 | 配置文件选择、Application/Runtime 创建、全局预算、日志与信号 |
| `tests/support/host` | 仅供测试的 Bundle 适配，根包通过 dev-dependency 使用 | 不参与业务库依赖闭包或服务发布 |
| `examples/fedengine-test-host.rs` | 开发/回归进程，配置、信号和排空示例 | 不交付独立服务二进制或镜像 |
| 业务组件 `modorch-fedengine-*` | 源目录、策略、SQL 规划/执行、受控 DDL、管理和网络入口 | 不接管其他业务库生命周期 |
| magicDataHub 服务宿主 | 显式路径和引用、注册/选择、联合预算、build/start/observe、失败和退出 | 不通过 Handle 绕过权限或执行边界 |

注册返回真实 Factory descriptor 的 ComponentRef。核心 Initiator 为 `duckdb-instance`、`datasource-registry`、`duckdb-runtime`；Module 为 `catalog-manager`、`policy-manager`、`query-coordinator`、`source-manager`。`pgwire-listener`、`management-api` 随 Profile 注册。固定 ID 意味着每个 Application 只支持一个 FedEngine；多个 Application 使用独立文件、目录、端口和凭据来源。

## 从注册到服务

构造和 register 仅处理纯值与 Factory，宿主 build 也不启动资源。宿主用返回 ID 显式选择，缺失依赖由框架拒绝；register 若发生晚期重复 ID，丢弃 Builder 后重新装配。资源建议只是单库装配的有界公式，宿主须结合其他库的线程、任务/消息、查询、内存和磁盘负载另定预算。

实例 setup 使用受管 blocking I/O 打开管理文件，建立当前存储与源状态；文件唯一占用与实际根连接一起存续。两入口 setup 只预绑定，全部 ready 后才 listen。PGWire/HTTP 明确依赖执行 Runtime，使实例重建/停止涵盖旧会话与请求。业务行为和依赖图见[架构与生命周期](../architecture.md)。

Profile 只决定入口，不暗改 managed storage。默认 PgServer 开 PGWire，HTTP 跟随配置；ManagedServer 可只开 HTTP。关闭入口仅跳过该入口专属配置，主体 principal/源/虚拟库/共享超时仍校验。具体矩阵及公式字段见[嵌入说明](../embedded-library.md)。

## 观测、失败和排空

框架 0.0.3 的组件钩子不能证明整个 Application 已 Ready。宿主在 start 返回后调用 observe，关联真实 Application ID；Handle 保守地汇总所选组件与应用健康，保留真实代际、历史失败、StopIncomplete/Forced，并隐藏非 Ready 的地址。地址/状态/指标是独立快照，不保证下一次连接一定成功。

启动失败保留 source、cleanup 和清理 handle。运行结束无论主因如何都进行 checked shutdown；StopIncomplete 保留资源和 Runtime 并重试，Forced 保留异常回执。取消和结果可读均不等于真实工作退出。同步扩展调用无法保证即时中断，也不能因超时自动重放写入。实际资源证据见[资源契约](../resource-lifecycle.md)。

生产 Bundle 默认 FailFast。测试 Wrapper 注入实例 Restart 只用于证明旧入口/执行停止闭包与代际边界，不能作为公开自动热重启 API。公开 QueryClient/ManagementClient 通过可信宿主身份与显式管理授权接入，DDL 沿用 QueryClient。EmbeddedCore 继续作为独立待办。

## 数据与后续组合

TOML、物理管理文件、WAL 和 SecretRef 保持既有规则；名称变化不新建空库，当前存储版本不匹配明确拒绝，遵循 AGENTS.md 的当前模型约束。详见[迁移对照](../migration/fedengine-to-modorch-fedengine.md)与[配置所有权](../configuration-ownership.md)。

[最小宿主](../integration/modorch-host.md)已经有仓库外消费者验证；[magicDataHub 接入设计](../integration/magicdatahub.md)区分已可复用能力与尚未交付的联合宿主、资源预算和真实双库验收。单库回归不能替代联合交付。
