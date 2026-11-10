# 进程内 Client 契约

## FE-CLIENT-001

触发：宿主通过 registered.query_client / management_client 绑定能力。
保证：仅绑定同 Bundle 已见证、完整 Ready 的 Application；调用时及流读取时检查 Ready 与组件代际，停机或已观测代际变化返回 55000。Principal 是可信宿主对已认证配置用户的断言，不是密码校验器。查询继续执行 CONNECT、对象 Policy 与单源写/DDL 检查；管理默认全拒绝，宿主显式授予精确源/虚拟库范围和写权限。SQL 身份不自动获得管理员权限，操作回执不能跨授权源读取。
边界：每次检查是状态快照，应用级 Hub 不提供原子代际租约；生产 Bundle 保持 FailFast，不开放热重启。宿主负责认证、授权策略与变更后丢弃/重绑 Client。EmbeddedCore 仍明确拒绝。

## FE-CLIENT-002

触发：准备、参数执行、消费结果流或取消/丢弃能力。
保证：不暴露内部 Hub/原生执行端口；SQL 和参数数量、控制描述、累计行数及估算结构/值字节受限。Clone 共享有界准入，超额拒绝 53300。流持有调用 permit，执行和消费共用 deadline；超时、取消、丢弃请求协作停止，不宣称原生工作退出。参数嵌套深度上限 64；交付前检查累计输出预算。Completion 保留真实终态，缺失终态报错。
边界：估算字节不是进程内存硬上限；已生成批次、内部目录与原生内存继续受全局配置/宿主预算控制。Client permit 结束不代表 worker 已归还；退出仍须 checked shutdown。

## FE-CLIENT-003

触发：执行已准备 SQL、管理 CAS、DDL 或等待超时后的未知写结果。
保证：Prepared 是绑定同一 Client/Clone 的不透明能力，固定数据库 OID/版本及源版本，执行继续校验；同名库换身份不能复用。DDL 通过 QueryClient 的已有精确能力门禁，无独立 DDLClient。管理继续使用原 CAS、幂等 key 与异步状态，调用超时/丢弃后工作可能继续；未确认的写结果保留 unknown，绝不自动重放。
边界：无可变 SQL 会话/多语句接口、无隐式重试、无全局管理列表/内部通知接口。宿主保留幂等 key，主动查询回执后决策；取消不证明回滚。接入说明见[进程内 Client](../docs/in-process-clients.md)。
