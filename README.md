# modorch-fedengine

基于 [详细设计 v1.1](docs/FedEngine_详细设计文档_v1.1.md) 的 可嵌入式 Rust 联邦 SQL 业务库。仓库为 [`muidea/modorch-fedengine`](https://github.com/muidea/modorch-fedengine)，Git 地址为 `git@github.com:muidea/modorch-fedengine.git`；对外 Facade 为 `modorch-fedengine`，内部组件统一使用 `modorch-fedengine-*` 命名。使用 Modorch-RS 管理组件、PGWire 提供 PostgreSQL 协议子集，DuckDB 负责本地计算及 PostgreSQL/MySQL 远端访问。

外部 Modorch 0.0.3 宿主可通过 `FedEngineBundle` 纯注册组件，用 Profile 选择 PGWire/管理 HTTP，再显式配置预算并启动自己的 Application。只读 Handle 经宿主关联后观察真实健康、代际、指标及可用端口。接入步骤和当前接口边界见[嵌入现有 Modorch 应用](docs/embedded-library.md)。后续所有服务由 `magicDataHub` 项目统一编排，负责 Application、配置、预算、日志、信号和部署。本库不交付独立服务二进制或 Docker 镜像。

[独立宿主样例](examples/embedded-in-modorch/README.md)附带自己的 manifest/锁文件；`python3 scripts/test-embedded-host.py` 从仓库外临时目录编译运行，只通过公开 Facade 验证 PG/HTTP、组件选择和失败排空。

## 本地库验证

需要 Rust 1.94.0（由 `rust-toolchain.toml` 固定）和 C/C++ 运行库。Modorch-RS Facade 0.0.3 通过固定 Git revision 获取，无需同级源码目录；与 modorch-mqtt 使用同一框架来源。准确依赖和指纹见 [开发说明](docs/development.md)。

```sh
cargo check --workspace --locked
cargo run --locked --example fedengine-test-host -- --check-config
export FEDENGINE_PASSWORD='replace-with-a-local-password'
cargo run --locked --example fedengine-test-host -- --config config/application.toml
```

上述 `fedengine-test-host` 仅为开发/回归示例，配套适配层位于 `tests/support/host`，只通过 dev-dependency 使用。它不是服务交付入口。开发配置的 PGWire 监听 `127.0.0.1:15432`，管理 API 监听 `127.0.0.1:15433`，源定义持久化到 `./data/fedengine.duckdb`。用户为 `fedengine`，首次启动未接入远端源，可执行 `SELECT 1` 并通过管理 API 在线接入。退出使用 SIGINT/SIGTERM。生产配置须提供 TLS 证书、私钥并设置 `tls_required=true`。

## 接入数据源

```sh
python3 scripts/fetch-extensions.py --dest data/extensions
```

以 `examples/config/federation.toml` 为模板，将脚本输出的绝对路径配置为 `[duckdb].extension_directory`，通过环境变量设置 `FEDENGINE_POSTGRES_DSN`、`FEDENGINE_MYSQL_DSN`。源连接串从环境变量或凭据目录内的 `file:` 引用读取，不能提交到版本库。远端账号应限制实际对象权限。

`config/application.toml` 是开发测试宿主的默认配置，`examples/config/federation.toml` 是独立的联邦配置示例。每次启动加载一份完整配置，不会合并这两个文件。准备好扩展和上述环境变量后，从仓库根目录运行：

```sh
cargo run --locked --example fedengine-test-host -- --config examples/config/federation.toml
```

配置中的相对路径以进程工作目录为基准。`[[datasources]]` 仅作首次导入；后续启动以管理库为准，不覆盖在线修改。启用管理服务后，在浏览器打开 `http://127.0.0.1:15433/manage/` 使用内置轻量 Web 管理端，无需独立前端服务或构建步骤。点击数据库名称可进入对象浏览与只读 SQL 调试，查看 Schema、表/视图及来源元数据，执行查询并查看结果、SQLSTATE 与取消状态。登记、发现、Schema 选择、激活、刷新及停用示例见[管理 API](docs/management-api.md)。

显式配置源、namespace allowlist、对象 grants 后才允许访问。DDL 还要求精确版本与验收证据对应的 `[[capabilities]]`；示例默认没有能力记录，因此不会提前开放 DDL。

```sql
SELECT a.id, b.amount
FROM pg_crm.public.customers a
JOIN mysql_erp.erp.orders b ON a.id=b.customer_id;
```

跨源 SELECT/JOIN 可用；写入和 DDL 必须完整绑定到一个已注册的 DataSourceId。显式事务、COPY、业务 SQL 中的 ATTACH/LOAD/CALL、任意文件和外部函数默认拒绝。

## DBeaver 浏览

使用 PostgreSQL 驱动连接 PGWire，database 可填写已启用且授权的 `catalog_alias`，也可填写显式配置的业务虚拟库。Startup 未提供或传空 database 时使用登录用户名；同名库须通过配置或[管理 API](docs/management-api.md#在线业务虚拟库)显式创建。账号虚拟库通过多个本地 schema 映射业务源，使用标准 `schema.table` 完成跨源查询；跨 database 三段引用默认返回 `0A000`，仅在显式开启 `pgwire.allow_cross_database` 时作为联邦扩展提供。原数据源 database 仍保留远端 schema。DBeaver 可开启“显示所有数据库”，并保持“显示系统对象”关闭；内部管理库不公开。虚拟库配置与业务范围见[账号虚拟库方案](docs/virtual-database-pg-alignment-plan.md)，客户端支持范围与限制见[协议兼容说明](docs/protocol-compatibility.md)。

## 验证与文档

```sh
cargo test --workspace --locked
scripts/test-federation.sh
```

第二条命令仅使用专用一次性 PostgreSQL/MySQL 容器，结束后删除这些容器及卷。需要 Docker、Compose、curl 与网络。不要把测试 DSN 指向业务数据库。

- [业务库分层](docs/architecture/embedded-library-design.md)、[宿主接入](docs/integration/modorch-host.md)
- [迁移与现有数据保护](docs/migration/fedengine-to-modorch-fedengine.md)
- [业务库契约](contracts/README.md)、[0.1.0 交付与发布边界](docs/releases/0.1.0.md)
- [magicDataHub 接入方案与待验收边界](docs/integration/magicdatahub.md)
- [架构与生命周期](docs/architecture.md)
- [数据源与 Schema 在线管理方案](docs/datasource-management.md)、[管理 API 与调用示例](docs/management-api.md)
- [SQL 能力](docs/sql-capability.md)、[DDL 能力门禁](docs/ddl-capability-matrix.md)
- [协议与类型兼容](docs/protocol-compatibility.md)
- [库化协议与执行回归矩阵](docs/library-regression.md)
- [资源所有权、取消和重建边界](docs/resource-lifecycle.md)
- [配置、路径与实例状态保护](docs/configuration-ownership.md)
- [开发验证与 skill-hub](docs/development.md)
- [已确认的剩余项方案](docs/remaining-items-plan.md)
- [Git 提交与本地文件分类](docs/git-submission.md)

当前为开发实现。JDBC 元数据接口及指定版本 DBeaver 流程已有实测；完整 PostgreSQL/psql 兼容、生产长时负载和全面故障注入仍未验收。具体边界以能力文档和实际证据为准。

宿主进程内调用使用[受控 Query / Management Client](docs/in-process-clients.md)：可信身份、显式管理范围、参数化查询与有界结果流；DDL 沿用查询门禁，取消不等于原生退出。
