# modorch-fedengine

FedEngine 0.1.0 开发中的公开 Facade，在宿主持有的 Modorch-RS Application 内注册联邦 SQL 组件。仓库：<https://github.com/muidea/modorch-fedengine>。当前 publish=false，未发布至 crates.io；Git 消费须固定包含本接口的实际提交。

`FedEngineBundle::new(config).register(&mut builder)` 仅校验配置、注册 Factory，返回实际组件 IDs、资源建议与只读 Handle。宿主显式选择组件、归并预算并 build/start 自己的唯一 Application，随后 observe；宿主保留配置、路径、密钥来源、Tokio、日志、信号和 checked shutdown 所有权。

框架要求 `modorch = "=0.0.3"`，Git Source `https://github.com/muidea/modorch-rs`，revision `336ba20196044e1b81fb2ca297a18b5e258a5800`，必须与宿主解析为同一 Package ID。`FedEngineProfile` 支持 PgServer 和 ManagedServer（PG+HTTP、PG-only、HTTP-only）；EmbeddedCore 明确拒绝，受控 Query/Management Client 已提供，DDL 通过 QueryClient 沿用已有门禁。宿主负责认证 Principal 和显式管理授权；取消不证明原生工作退出。

```rust,no_run
use modorch::{ApplicationBuilder, ComponentSelection, ExecutionResources};
use modorch_fedengine::{FedEngineBundle, FedEngineConfig};

# fn compose() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
let config = FedEngineConfig::load("application.toml")?;
let mut builder = ApplicationBuilder::new("host");
let registered = FedEngineBundle::new(config).register(&mut builder)?;
let mut selection = ComponentSelection::new();
for id in registered.initiator_ids() { selection.enable_initiator(id); }
for id in registered.module_ids() { selection.enable_module(id); }
let demand = registered.resource_requirements();
let app = builder.selection(selection).execution_resources(ExecutionResources {
    blocking_io_threads: demand.blocking_io_threads,
    blocking_io_capacity: demand.blocking_io_capacity,
    max_tasks_per_owner: demand.max_tasks_per_owner,
    ..Default::default()
}).build()?;
// The host starts app, calls registered.observe, and always checks shutdown.
# Ok(())
# }
```

一个 Application 只支持一个固定 ID 的 FedEngine。不同实例独立使用管理文件、临时目录、端口和凭据来源；更名不自动重命名既有数据或 SecretRef。相对路径沿用进程工作目录语义，组合宿主应明确准备绝对路径。

注册错误后丢弃 Builder。启动失败保留 StartupError 主因和 cleanup；StopIncomplete 时保留 Application、Runtime 与实际资源并重试。Forced 不代表正常排空，取消不保证远端写回滚，未知结果不自动重放。完整可编译启动/信号/退出样例在源码仓库的 `examples/embedded-in-modorch/`；公开签名和约束见本包 Rustdoc。
