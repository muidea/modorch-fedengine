# 独立 Modorch 宿主样例

本目录是独立 Cargo workspace，带自己的 `Cargo.lock` 和 Rust 1.94.0 工具链。业务依赖只有公开的 `modorch-fedengine` Facade；Modorch 固定为 0.0.3 的同一 Git revision，不依赖 `modorch-fedengine-test-host`、根测试包或根包的测试依赖。

宿主加载配置，以明确的绝对基目录固定文件路径，注册 Bundle，从返回的实际 ID 显式选择，应用资源建议，build/start 唯一 Application，关联只读 Handle，再等待信号或应用故障并检查式退出。示例没有安装日志 Subscriber；需要日志时由宿主自行安装。启动错误保留原始 `StartupError`；StopIncomplete 时保留 Runtime 并重试，Forced 回执以失败退出。

## 仓库外验收

从项目根目录执行：

```sh
python3 scripts/test-embedded-host.py
```

脚本复制此样例到系统临时目录，将 Facade 路径改为当前源码绝对路径。它断言独立 workspace、公开依赖边界和唯一 Modorch Package ID，执行 Skill 指纹预检、fmt、Clippy、6 项消费者测试和 2 项路径准备测试、Rustdoc、构建及实际二进制冒烟。冒烟从与配置文件不同的宿主工作目录启动，并验证配置不变且没有在配置目录新建状态。在同一临时状态库上分别验证 SIGINT、SIGTERM 排空和启动端口占用补偿；临时宿主、配置及状态均在结束时清理。

默认 Cargo 使用 `--locked --offline`，编译缓存及 DuckDB v2.0.0 原生库复用根 `target`；`--target-dir` 可指定其他缓存目录。这不会把宿主加入根 workspace。复制后刷新 Rust 文件时间，并在测试中断言编译时 manifest 目录等于实际运行目录，避免复用旧位置的消费者二进制。

首次环境先准备根项目依赖及 DuckDB 原生库；`--online` 允许 Cargo 获取锁定依赖，也可用 `DUCKDB_LIB_DIR` 指定已经准备的原生库。Cargo 的 `--offline` 不控制原生构建脚本的下载。测试使用已有 `USER` 环境变量作为一次性测试口令，不修改进程环境；二进制冒烟仅向自己的子进程传入专用测试口令。当前实际验收平台为 Linux x86_64 / Rust 1.94.0。

## 手动运行

在本目录执行（已缓存依赖时可加 `--offline`）：

```sh
export FEDENGINE_EMBEDDED_PASSWORD=disposable-demo-password
CARGO_TARGET_DIR=../../target cargo run --locked -- application.toml
```

Ready 行输出实际 PGWire/HTTP 临时端口，可用 PostgreSQL 客户端连接 `hostdb`，用户 `fedengine`。Ctrl-C 或 SIGTERM 会触发检查式排空。从本目录启动时，数据位于本目录 `data/`。[路径准备函数](src/config.rs)不做 I/O，保留 `:memory:`、绝对路径和原有 SecretRef；主程序选择进程工作目录作为基目录。如需同时运行其他实例，分别配置绝对状态/临时/凭据路径和端口。示例配置不访问远端数据源。

## 消费者契约覆盖

- 无 Tokio Runtime 时纯注册/build：占用端口、缺失密钥/TLS/扩展文件均不被读取；已有文件字节保持。
- 完全不选 FedEngine：只运行宿主 Probe，保留宿主预算，不创建数据库或关闭宿主持有的端口。
- 重复 ID、未显式选择的必需依赖：在 start 前返回真实框架错误，注册错误后丢弃 Builder。
- PG+HTTP、PG-only、HTTP-only：真实 SCRAM/参数化查询及 HTTP；宿主 Context Hub、FedEngine 观测和 Application 具有同一 Application ID。
- 宿主 Ready 失败：保留主因/cleanup/handle，补偿预绑定端口，并可用同一状态文件重建宿主。
- 慢 HTTP 请求：StopIncomplete 保留任务并隐藏地址，客户端退出后再次检查式关闭才完成释放。

完整规则见[配置所有权](../../docs/configuration-ownership.md)。正式 API 和能力限制见[嵌入说明](../../docs/embedded-library.md)。此样例只验 FedEngine 与宿主 Probe，MQTT 联合运行、远端 CRUD/DDL 的联合验收及数据库组件监督重建另行验收。当前 Facade 尚未提交/推送或发布，Git 消费者需等待包含接口的实际提交后再固定 revision。
