# 配置、路径与实例状态

宿主选择配置文件、工作目录、密钥引用和监听地址，再把已解析的 `FedEngineConfig` 传给 Bundle。库不改变工作目录、环境变量或配置文件，不根据 Application、仓库或包名生成新的数据文件名。配置仍是当前 TOML/DTO，`Config::load/validate` 保持原有行为；没有自动迁移或旧格式回退。

## 路径规则

`Config::load(path)` 只读取指定 TOML 并校验，不把其中的相对路径改为相对配置文件。配置与开发测试宿主的相对路径以**进程工作目录**为基准。嵌入宿主应在启动前将文件路径固定为绝对路径，避免工作目录变化影响文件位置。

| 配置 | 宿主负责的值 |
|---|---|
| `duckdb.database_path` | 每实例独立的管理文件；保留已部署路径，例如 `data/fedengine.duckdb`。`:memory:` 是独立内存实例，不作路径转换 |
| `duckdb.temp_directory` | 每实例独立的可写临时目录 |
| `duckdb.extension_directory` | 明确的扩展目录；只读的相同版本扩展可共享，安装/更新由宿主协调 |
| `management.credentials_directory` | 每实例明确的受保护凭据目录 |
| `pgwire.tls_cert` / `tls_key` | 宿主选择的证书/私钥路径 |
| 两个 `listen` | 各实例独立端口，测试可使用 `127.0.0.1:0` 让 OS 分配 |

[独立宿主样例的准备函数](../examples/embedded-in-modorch/src/config.rs)接收配置和宿主明确指定的绝对基目录，只转换上述相对文件路径，不创建目录、读取密钥或重写引用。样例主程序选择当前工作目录，以保留原路径语义；宿主也可在自己的代码中明确选择其他基目录。该函数属于样例，不是新的库 API。Bundle 注册仍无文件、端口或密钥 I/O。

## 密钥引用

现有 `FEDENGINE_*` 环境变量名称继续有效；更名不会替换它们。源 `secret_ref` 支持环境变量名及 `file:relative/path`，后者只在配置的凭据目录内解析，每次探测/挂载重新读取，并拒绝空值、越界及通过符号链接逃逸。组织化连接仍使用当前凭据 JSON，普通源使用 DSN，不做格式猜测或 DSN 自动改写。

PGWire 用户的 `password_secret_ref` 仍只接受环境变量名，在选中的 PGWire 组件 setup 时读取。宿主提供当前引用对应的环境或凭据文件；无需把明文密码写回配置。缺失、空值或读取失败明确报错，不退回空密码、新默认值或另一个实例的凭据。关闭 PGWire 的 Profile 不读取其口令。

同一进程的环境变量由整个进程共享；要隔离同类引用的内容，宿主应给环境变量分配不同名称，或对源使用各自凭据目录中的同名 `file:` 引用。Application 名称不会创建密钥命名空间。

## 管理文件的唯一所有者

不同 Application 即使名称相同，也必须使用独立管理文件、临时目录和端口。一个 Application 内仍只支持一套固定组件 ID 的 FedEngine Bundle。

DuckDB 的原生跨进程文件锁之外，实例 setup 增加进程内占用检查，阻止两个 FedEngine 同时打开同一管理文件，包括并发创建。检查已有文件及父目录的规范路径，Linux/Unix 还比对文件设备号和 inode，防止符号链接、父目录别名和硬链接绕过。冲突以 `management database file is already owned by another FedEngine instance` 作为 StartupError 主因，走正常启动补偿；已有实例继续可用。

占用与实际共享根连接一起存续，不以观察 Handle、取消结果或 teardown 入口代表释放。启动补偿和正常排空关闭连接后才允许重新打开；`StopIncomplete` 时实际工作、连接与占用继续保留，遵循[资源生命周期](resource-lifecycle.md)。`:memory:` 不占用文件，多个内存实例彼此独立；它仍只适用于现有 static 模式，不能绕过 managed storage 要求文件的校验。

这是 FedEngine 间的协调机制。宿主仍须保护活动文件，不能在运行中删除、替换、移动管理库及 WAL，也不能用绕过 FedEngine 的原生连接共享其可写文件。非 Unix 平台的硬链接身份检查及不同文件系统的锁行为需单独验收。

## 重启与部署保护

更改宿主名称或仓库/对外包名时，继续传入原管理文件、临时路径和 SecretRef。已登记源以持久化状态为准，启动配置的源只在首次导入，不能借更名覆盖在线修订、incarnation 或稳定 OID。在线虚拟库的持久化覆盖和幂等回执亦保留。

部署前备份管理文件及 WAL、配置和凭据引用，记录程序/存储版本及回退方案。备份须在已排空或具备一致性保证的条件下完成。当前实现对不支持的存储版本明确拒绝，不自动升级、降级、清空或重建。不要为适配新包名先建一个空库再切换路径；实际存储变更须另行确认验收范围。

## 可复用验收

```sh
cargo test --test instances --locked
python3 scripts/test-embedded-host.py
scripts/test-federation.sh
```

[实例回归](../tests/instances.rs)验证同文件/路径别名拒绝、并发首次打开、失败补偿、版本拒绝保留原数据、独立端口与状态/幂等键、名称变化后的持久身份。其 ignored 用例由一次性联邦脚本执行：两个真实 PostgreSQL 数据库使用相同源 ID、别名和 `file:source.dsn`，各实例查询到自己的标记；更名重启保留 OID/源 incarnation，删除一个凭据文件只使该实例明确降级，恢复文件后重启可恢复身份。该测试创建并删除一次性远端库/表，不能指向业务数据库。

外部消费者验收还从与配置文件不同的工作目录启动真实二进制，验证文件位于宿主工作目录、原配置字节不变，SIGINT/SIGTERM 和启动失败均完成检查式排空。该范围不代表 MQTT/FedEngine 联合宿主或多租户安全隔离已验收。
