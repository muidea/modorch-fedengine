# 在 Modorch 宿主中接入 FedEngine

实际可运行的消费者位于[独立 workspace](../../examples/embedded-in-modorch/README.md)，只依赖公开 Facade、同源 Modorch 和宿主 Tokio。它已经涵盖 PG+HTTP、PG-only、HTTP-only；单个 Application 内还注册宿主 Probe。这里的步骤适用于当前 0.1.0 开发树，接口与[业务库契约](../../contracts/README.md)一致。

## 依赖与配置

本地消费者可使用：

```toml
[dependencies]
modorch = { git = "https://github.com/muidea/modorch-rs", rev = "336ba20196044e1b81fb2ca297a18b5e258a5800", version = "=0.0.3" }
modorch-fedengine = { path = "../modorch-fedengine/crates/modorch-fedengine" }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal", "time"] }
```

Git 消费尚待包含当前接口的实际提交，crates.io 未发布。独立宿主使用自己的 Cargo.lock。主程序拥有配置文件选择、CWD、日志、Runtime 和信号，不在 Bundle 中安装全局资源。

先加载当前 Config 并明确路径/端口/SecretRef。`Config::load` 不相对配置文件重定位；可参考[样例路径准备](../../examples/embedded-in-modorch/src/config.rs)固定为宿主基目录下的绝对路径。PGWire 用户口令引用环境变量；源可引用各自凭据目录的 file。HTTP-only 跳过 PGWire 认证/TLS 字段，但保留业务 principal 与共享配置校验。

## 项目 Skill

本仓库的 `.agents/skills/modorch-fedengine-integration/` 提供匹配当前 Facade 的接入规则及自包含预检。预检读取目标 manifest 的 locked/offline 实际依赖图，同时验证唯一框架来源与 Facade 源码指纹；外部消费者脚本已在独立 manifest 上执行它。

在配置了相应本地技能仓库的环境中，先 `skill-hub list` 确认该 ID，再 `skill-hub use modorch-fedengine-integration --repo skills-repo --non-interactive`、`skill-hub apply --pattern modorch-fedengine-integration`；只需在目标项目启用，不自动修改全局目录。当前 Skill 仅归档到本机技能仓库，不能声称其他机器的远程目录已经包含它。无需 skill-hub 时可在当前源码检出中直接阅读该目录；源码依赖与 Skill 安装是两个步骤。

## 装配与运行

1. 在唯一 ApplicationBuilder 上注册其他业务库和 `FedEngineBundle::new(config).profile(profile)`。注册失败丢弃 Builder，重新装配。
2. 通过返回 `initiator_ids()` / `module_ids()` 显式启用，合并其他业务库选择。不要依赖手写/猜测 ID，不通过删选核心组件冒充 EmbeddedCore。
3. 读取 `resource_requirements()` 并归并宿主预算。只含 FedEngine 时可直接采用建议；联合预算还需任务/消息、同步线程、内存、磁盘及控制面容量验证。
4. 宿主 build/start。启动成功后 `registered.observe(&app)`，通过 Handle 检查实际 Ready 和地址。启动失败保留 StartupError 的 source、cleanup、handle；失败早于本 Bundle setup 时 observe 可能因缺少身份见证被拒绝，原错误仍有效。
5. 运行期等待信号或 `app.wait()`；任一路径结束都排空，不因查询/观察错误提前退出 Runtime。

[主程序](../../examples/embedded-in-modorch/src/main.rs)使用以下退出策略：

```rust,ignore
loop {
    match app.shutdown_checked(ShutdownOptions { timeout }).await {
        Err(FrameworkError::StopIncomplete { .. }) => {
            // Keep app, Runtime and retained work alive; retry under host policy.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        result => break result,
    }
}
```

完整 import 和错误处理由上述可编译样例维护，片段本身不是第二套实现。GracefullyStopped 才证明正常排空；Forced 保留原回执及异常语义。永久阻塞的扩展调用可能没有有界退出时间，宿主不能靠丢弃 Runtime 或重放写入伪装释放。

Handle 仅用于观测，没有查询/管理/停机客户端。状态、地址与指标是不同采样，不能当作连接租约。当前支持 PgServer、ManagedServer；EmbeddedCore 明确拒绝。Profile 表、固定 IDs 与关联限制见[嵌入 API 说明](../embedded-library.md)。

## 验证

```sh
python3 scripts/check-library-contracts.py
python3 scripts/check-library-contracts.py --run local
python3 scripts/check-library-contracts.py --run external
```

后两条只运行各自范围。远端 CRUD/DDL、PG/MySQL JOIN、来源恢复与同步阻塞由 `scripts/test-federation.sh` 在专用一次性容器中验证；可复用矩阵见[协议与执行回归](../library-regression.md)。根工作区测试不包含外部消费者，独立样例脚本也不代表 MQTT 联合运行已通过。

## 进程内调用

Ready 后通过 registered.query_client / management_client 绑定能力，宿主先完成认证并显式授予管理范围；[完整约束与预算](../in-process-clients.md)适用。Opaque Prepared 保留数据库身份与版本，取消/超时不证明写回滚，未知结果不自动重放。EmbeddedCore 仍未开放。

宿主可开启 Hub 消息/全局/目标字节预算；库的 PGWire、管理 HTTP、Query Client 及下游目录/权限/管理请求均声明逻辑输入成本。超额明确拒绝，不自动放宽。成本含可变输入，不是 RSS/结果流上限；结果与元数据仍需独立预算。
