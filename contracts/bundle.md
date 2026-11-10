# 注册与观测契约

## FE-BUNDLE-001

触发：构造 Bundle、register 和宿主 build。
保证：只校验纯配置、计算建议和注册实际 Factory，不创建 Application/Runtime/Hub，不启用组件、不打开数据库/扩展/密钥/端口、不改变现有文件或宿主预算。未选组件不实例化。

## FE-BUNDLE-002

触发：宿主选择返回的组件 ID、重复注册或缺少依赖。
保证：实际 descriptor 返回稳定 ComponentRef，必需依赖须显式启用。一个 Application 只支持一个固定 ID 的 FedEngine。无效配置/资源溢出在首个注册前拒绝；重复 ID 保留真实 FrameworkError，可能发生在部分注册之后，宿主须丢弃 Builder 后重建。

## FE-PROFILE-001

触发：选择 PgServer 或 ManagedServer。
保证：仅切换网络入口，不隐式切换内部 managed storage。未启 PGWire 时不校验/读取其 TLS、监听、认证字段，但仍校验 principal、管理、虚拟库、源和共享查询超时。EmbeddedCore 明确返回 UnsupportedProfile；受控 QueryClient/ManagementClient 见[Client 契约](clients.md)，DDL 沿用 QueryClient。

## FE-HANDLE-001

触发：宿主在 start 返回后 observe，随后读取 status、地址或 metrics。
保证：关联前 Unobserved，关联错误不假定 Ready；部分选择不报告完整 Profile Ready。地址仅在整个 Application 与完整 Profile Ready 时可用。观测绑定真实 Application ID，不能换绑其他实例；保留真实失败、各组件代际、StopIncomplete 和 Forced。Handle 没有查询、消息、停止或 Runtime 控制能力；多个字段是分别采样的快照，不是租约。

资源需求只是沿用单库基线公式的建议，不保证联合满载无配额拒绝，亦不包括数据库内存/磁盘；宿主负责预算归并。完整配置边界见[嵌入说明](../docs/embedded-library.md)。

## FE-MESSAGE-001

触发：宿主启用 EventHub 字节预算后调用 PGWire、管理 HTTP 或受控 Client。
保证：库的查询、目录、权限和管理下游请求均声明有界逻辑 payload 成本，计入可变 SQL/参数/标识符等输入；超额由框架拒绝，不通过关闭宿主限制或重放请求绕过。成本不等于深度 RSS，也不覆盖查询结果流，结果/元数据另有库预算。序列化成本只计数不保存第二份凭据缓冲。
