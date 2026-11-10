# FedEngine 业务库契约

适用 `modorch-fedengine` 0.1.0 开发树；契约 revision **1.1.1**。Modorch-RS 固定 0.0.3 / `336ba20196044e1b81fb2ca297a18b5e258a5800`，框架契约 1.2.0。此目录定义当前可观察行为；公开签名以 [Facade Rustdoc 源码](../crates/modorch-fedengine/src/lib.rs)为准，不将设计草案视为已实现 API。

| 契约 | 范围 |
|---|---|
| [注册与观测](bundle.md) | 纯注册、显式选择、Profile、只读 Handle、宿主预算 |
| [生命周期与资源](lifecycle.md) | 入口屏障、补偿、真实资源排空和失败/代际 |
| [查询与管理](execution.md) | PostgreSQL 协议子集、权限、单源写/DDL、管理发布和不重放 |
| [进程内 Client](clients.md) | 宿主身份、管理授权、参数/结果预算、版本与未知写入 |
| [配置与状态](configuration.md) | 路径、凭据、管理文件唯一占用及现有状态 |

[测试映射](../tests/contracts/mapping.json)以测试名、来源和执行套件对应规则。集成用例区分正式 Bundle、网络边界夹具、测试注入 Restart 和仓库外消费者，不能将一种证据扩大到其他范围。

```sh
# 无 Cargo、Docker、网络或业务数据 I/O，只检查合同/测试映射、链接和元数据。
python3 scripts/check-library-contracts.py
# 真正运行映射的本地 Cargo 用例，并逐条核对测试成功行。
python3 scripts/check-library-contracts.py --run local
# 运行独立消费者，其中还包括 fmt/Clippy/doc/真实二进制冒烟。
python3 scripts/check-library-contracts.py --run external
```

脚本静态通过只输出 `execution_status: NotRun`。`--run` 失败、缺少已映射测试的成功行或输出被截断均不能判定 Passed。真实远端证据由 `scripts/test-federation.sh` 创建并清理专用 PG/MySQL 容器；其结果单列，不用本地测试数量推断远端已通过。它会创建/删除一次性库和表，不能改成业务 DSN。

契约 ID 不得静默复用为相反语义。改变公开行为时同时更新实现、测试、映射、正式文档和 Skill 兼容指纹；历史工作记录留在外部核对清单，临时输出不归档。发布边界见[0.1.0 交付说明](../docs/releases/0.1.0.md)。
