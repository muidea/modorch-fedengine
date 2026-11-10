# 从独立 FedEngine 接入 modorch-fedengine

本次迁移改变装配与公共入口，保留当前协议/SQL/管理实现及数据模型。仓库已更名为 `git@github.com:muidea/modorch-fedengine.git`；本地 Facade 为 `modorch-fedengine` 0.1.0。发布和 Git 消费条件见[交付说明](../releases/0.1.0.md)。

## 名称与资源对照

| 对象 | 当前名称/行为 | 接入时处理 |
|---|---|---|
| Git 仓库 | `muidea/modorch-fedengine` | 更新 origin/源码位置；不据仓库名推导磁盘路径 |
| 公共 Cargo 包 / Rust 路径 | `modorch-fedengine` / `modorch_fedengine` | 外部宿主仅依赖该 Facade 和同源 Modorch |
| 内部组件包 | `modorch-fedengine-*` | Cargo 包、目录和 Rust import 同步更名；不修改组件 ID/Topic |
| 根包 | `modorch-fedengine-workspace` | 仅承载集成测试与开发示例，无服务二进制 |
| 服务编排 | `magicDataHub` → `FedEngineBundle::register` | 所有服务统一在 magicDataHub 编排和部署；本库无服务镜像 |
| 开发测试入口 | `examples/fedengine-test-host.rs` → `tests/support/host` → 同一 Bundle | 仅开发/回归，不作为可发布服务或宿主依赖 |
| 外部宿主 | `FedEngineBundle::register`，宿主显式选择与启动 | 不调用独立适配层再嵌套一个 Application |
| 固定组件 ID | `duckdb-instance` 等 | 保持不变；同 App 重复注册拒绝 |
| 默认管理文件 | `./data/fedengine.duckdb` 及 WAL | 继续使用已部署位置；不要改成新包名后创建空库 |
| 相对路径 | 相对进程 CWD，不相对 TOML 文件 | 嵌入前由宿主明确基目录并准备绝对路径 |
| `FEDENGINE_*` / `file:` | 保持当前引用和凭据格式 | 提供相同来源或明确规划新引用；缺失不回退 |
| 源状态与身份 | 管理库中的修订、incarnation、OID 和回执 | 不用启动配置覆盖在线更改 |
| 存储版本 | 当前受支持模型 | 不匹配明确拒绝；无自动迁移/旧格式回退/清空修复 |

## Cargo 与宿主代码

Modorch 统一使用 `version = "=0.0.3"` 和 Git rev `336ba20196044e1b81fb2ca297a18b5e258a5800`，不能把本地 path 和 Git 来源混用后仅凭版本字符串判作同一类型。宿主 `cargo metadata --locked` 应只有一个 modorch Package ID；同名版本不代表相同 Source。不要通过重命名依赖隐藏双版本。

当前源码可用 Facade 的本地 path；Git 消费须先由维护者提交并推送包含该 Facade 的实际版本，再固定那个 revision。此前远程 HEAD 不能代表当前未提交工作树。所有包保留 `publish = false`，未宣称 crates.io 可用。

原根 CLI 与服务 Dockerfile 已移除；`FedEngine::start(config)` 仅保留在测试支持包。magicDataHub 通过 Bundle 的纯注册接入，取得返回 ID/资源建议，归并自身组件后只 build/start 一个 Application。成功后 observe；失败检查 StartupError；始终执行检查式退出。完整可运行代码在[独立样例](../../examples/embedded-in-modorch/src/main.rs)，步骤见[宿主接入](../integration/modorch-host.md)。

## 操作顺序

1. 记录现有程序/存储版本、管理文件及 WAL、配置、SecretRef 和扩展位置。在排空后或具备一致性保证的条件下备份，保留匹配程序的回退方案。
2. 保持原配置格式与数据路径，验证口令/源引用可读取、目录可写、扩展版本匹配。日志不输出明文凭据。
3. 将宿主控制项留在 main。每个 Application 独立分配管理文件、临时目录和端口；不要让新旧进程同时使用同一管理文件。
4. 运行静态契约检查和仓库外消费者，确认编译/配置、真实 PG/HTTP、失败补偿及正常排空。远端功能用一次性源回归，不向业务库运行测试夹具。
5. 在受控部署中停止旧进程并确认排空，再以原路径启动新装配，检查源状态、incarnation/OID、目录与幂等回执。出现不支持的存储版本时停止切换，保留原文件并使用匹配版本恢复。

本项目当前开发规约只维护当前模型。备份/回退不是历史兼容实现；需要改变存储结构或支持历史数据时，应另行确定范围和验收，不能在这次装配迁移中暗增迁移链。详细路径与唯一占用边界见[配置规则](../configuration-ownership.md)。
