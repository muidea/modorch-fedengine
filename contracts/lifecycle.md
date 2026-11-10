# 生命周期与资源契约

## FE-LIFE-001

触发：实例 setup、全体 ready、入口激活或启动失败。
保证：setup 最多预绑定，所有 ready 成功前不可 TCP 连接，activate_ingress 才 listen。启动失败保留 StartupError 主因、cleanup 与清理 handle，并补偿已预绑定/已激活资源；失败等待被取消不代表补偿已经完成。

## FE-LIFE-002

触发：取消、输出阻塞、关闭超时或真实同步扩展 I/O 未退出。
保证：ServiceLease、任务、worker 配额、源/实例屏障、连接和管理文件占用保留至实际退出。StopIncomplete 时宿主保留 Application/Runtime/资源并重试，端口不再被 Handle 报作可用。Forced 保留原回执，不作为正常排空证据。GracefullyStopped 后资源才可重用。

## FE-LIFE-003

触发：宿主排空后重建 Bundle，或测试显式对实例 Factory 注入 Restart。
保证：旧入口/会话与执行 Runtime 属于同一停止闭包；真实工作退出后才允许替代实例和新代际，旧任务/服务能力失效。同一 Application 内的监督重建保留应用级 Hub；宿主重建整个 Application 则取得新的应用身份与 Hub。持久身份不因重建重置，不重放结果未知的写入。
边界：公开 Bundle 默认 FailFast，没有自动热重启策略 API。测试注入 Restart 的结果不能记成公开自动恢复能力，永久阻塞调用亦不保证能在固定时限结束。

实际租约、文件锁和远端故障边界见[资源所有权](../docs/resource-lifecycle.md)。
