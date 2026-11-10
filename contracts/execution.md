# 查询与管理契约

## FE-QUERY-001

触发：经正式 Bundle 的 PGWire 执行 Simple/Extended、参数、Portal 或 CancelRequest。
保证：保留现有受支持 PostgreSQL 协议/类型/会话子集、认证与权限校验，结果/输出有界。Portal 续读沿用同一执行，Close/Sync 排空对应工作；取消返回 57014 不代表远端事务已回滚或资源已归还。
边界：不宣称完整 PostgreSQL、跨源事务、任意精度 numeric 或任意 SQL 兼容，准确范围见[协议说明](../docs/protocol-compatibility.md)。

## FE-WRITE-001

触发：DML、DDL 或含写入的多语句请求。
保证：写入和 DDL 完整绑定单一 DataSourceId，跨源只读 JOIN 可用；业务 SQL 不能自行 ATTACH/LOAD。含写入的多语句在执行前拒绝。DDL 需要精确 DuckDB/扩展/远端版本与已验证操作能力记录，无记录/版本不匹配不得开放。统计行数、取消、重建或超时均不自动重放写入；未知结果保留未知语义。
边界：受支持组合和 DDL 变体见[能力矩阵](../docs/ddl-capability-matrix.md)。

## FE-MANAGE-001

触发：在线源修订、刷新、虚拟库变更、恢复或重复幂等请求。
保证：现有 CAS、incarnation、revision、稳定 OID、幂等回执和快照原子发布保持。失败采集不能覆盖已应用目录；远端 DDL 成功后刷新失败仍保留成功/Notice，并标 MetadataStale 阻断不可信查询，不能伪报远端 DDL 失败诱导重试。未完成操作在重启后保留可观测结果，配置源只首次导入，不覆盖在线修订。
边界：管理 HTTP 默认 loopback；Origin 校验不等于管理员鉴权，生产访问由宿主建立 TLS/身份验证与授权。宿主进程内接入使用[受控 Client](clients.md)，不直接调用内部 typed 消息。API 见[管理说明](../docs/management-api.md)。
