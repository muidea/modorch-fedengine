# 开发验证与宿主交付边界

## 当前阶段的数据兼容规约

本项目处于开发阶段，没有历史数据兼容要求。只维护当前版本数据模型，不新增历史迁移链、旧格式回退、双格式读写、历史回填或自动修复逻辑。修改相关实现时，移除不再需要的历史兼容分支，直接使用当前结构；不受支持的存储版本明确报错，不自动转换或清空。

当前版本初始化、重复启动幂等、正常配置缺省值、来源事实未知状态、运行故障恢复、失败保留和稳定对象身份仍属于功能契约。部署前备份和匹配版本回退继续执行，不得借规约删除现有管理状态、凭据或业务数据。未来出现历史数据兼容需求时，单独确认范围和验收要求后再实施。本规约同时写入仓库根目录 [AGENTS.md](../AGENTS.md)，用于后续开发约束。

受支持 PostgreSQL/MySQL 源版本适配以及对外 PostgreSQL SQL/协议行为仍是产品要求，不属于历史存储兼容，不据此删减。

## 锁定依赖

- `rust-toolchain.toml` 固定 Rust/Cargo 1.94.0，并声明 rustfmt、Clippy；edition 2024。Workspace 的 `rust-version = "1.94"` 是当前支持基线，尚未承诺更低版本；内部包统一 `publish = false`。
- Modorch-RS Facade 0.0.3，通过 `https://github.com/muidea/modorch-rs` 固定源码 revision `336ba20196044e1b81fb2ca297a18b5e258a5800`，与 modorch-mqtt 使用同一 Git Source/Revision；匹配 Skill 1.0.7 / Contract 1.2.0。实际文件指纹由 managed skill preflight 核对。该版本为 GitHub Candidate 源码发布，不视为 crates.io 发布或完整生产认证。
- duckdb-rs 1.20000.1、核心 v2.0.0；pgwire 0.42.1；Cargo.lock 固定传递依赖。
- 使用 duckdb-rs 官方 download-lib 特性获取对应原生库，避免每次编译庞大的 bundled C++。构建首次需要网络。

本地和 CI 均由 Cargo 获取锁定的 Modorch Git 源码，不要求同级 Modorch checkout。不要替换为未经验证的同名 crate；本地联合开发如需 path override，必须统一覆盖相同 Git 源并重新核对解析后的 Package ID、版本与指纹。Linux 动态库位于 `target/duckdb-download/<triple>/v2.0.0/libduckdb.so`；宿主发布时需携带该库并设置加载路径。所有服务及镜像统一由 magicDataHub 编排交付，本库不提供服务 Dockerfile。

Modorch 0.0.3 的 Topic Name / Filter 采用 MQTT 3.1.1/5.0 语义，`:id` 是字面量，单层通配为 `+`。FedEngine 的 `query/execute/v1`、`catalog/control/v1`、`policy/authorize/v1` 和 `management/control/v1` 均使用精确订阅，不扩大匹配范围；Destination、Request 结果等待、受管任务、ServiceLease 和检查式排空仍按既有组件契约运行。

## 目录与测试归属

根目录是 `modorch-fedengine-workspace` 测试包和 workspace，没有服务 binary；正常依赖为空。`examples/fedengine-test-host.rs` 只负责开发/回归进程入口，通过 dev-dependency 使用 `tests/support/host` 的测试适配层，复用公开 Facade/Bundle，负责配置、显式选择、资源建议、唯一 Application 的启动与检查式退出。接入方式和当前边界见[嵌入说明](embedded-library.md)。其他组件库位于 `crates/`，无需额外的 `apps/` 层。

`config/application.toml` 是默认开发测试配置；`examples/config/federation.toml` 是包含 PostgreSQL/MySQL 数据源的完整示例，通过 `--config examples/config/federation.toml` 显式选择。程序一次只读取一份配置，不做文件合并。配置内的相对路径以进程工作目录为基准，文档中的启动命令从仓库根目录执行。

- `modorch-fedengine-ddl/src/`：`analysis` 负责语句分析，`binding` 负责对象及约束绑定，`model` 定义领域类型，`index` 生成受控模板，`parameters` 处理参数 AST。
- `modorch-fedengine-duckdb/src/`：`instance` 打开唯一实例，`storage` 当前结构初始化与持久化，`control` 受控挂载及恢复，`credentials` 解析引用，`catalog` 扫描结构，`discovery` 枚举原生数据库，`relational` 采集扩展关系事实，`identity` 维护稳定身份，`projection` 发布 PG 目录，`virtuals` 持久化逻辑数据库并同步外部表映射，`init` 装配运行时，`runtime` 管理执行准入，`worker` 执行和输出行批次，`codec` 转换数据，`metadata` 查询元数据，`extensions` 校验扩展，`error` 转换错误。
- `modorch-fedengine-pgwire/src/`：监听生命周期、认证、取消分别独立；`session/` 保存连接状态，并拆分游标、解析、Simple 和 Extended 协议实现。

上述适配 crate 的 `lib.rs` 声明内部模块并导出组件接口。内部模块保持私有，模块拆分不改变组件边界或 Modorch 生命周期。

`modorch-fedengine-management` 的 `module` 接收 Typed Request，`reducer` 校验意图与版本，`coordinator` 编排受管长任务，`api` 提供生命周期受管 HTTP 入口，`pagination` 定义操作记录分页参数并绑定来源目录快照、过滤条件与续页参数；`browser` 提供虚拟库目录与有界调试任务，`browser_query` 通过现有协调器消费只读结果流；`discovery` 使用 source-manager 的受管任务、共享并发和内存许可执行实例枚举，Web 的 `organization.js` 核对逐目标结果后逐项提交持久化登记/激活。管理配置、凭据引用、首次导入及调用示例见 [管理 API](management-api.md)。

SQL 分析、配置和授权测试分别位于 ddl、core、policy 的 `tests/`。通过 `include_str!` 读取仓库配置示例，避免依赖测试进程工作目录。根目录 `tests/` 保留协议、引擎控制和在线管理系统测试；临时目录由作用域守卫自动清理。

```sh
cargo test -p modorch-fedengine-ddl --locked
cargo test -p modorch-fedengine-core --locked
cargo test -p modorch-fedengine-policy --locked
```

根目录 `cargo test` 默认只测试根包；完整检查使用 `cargo test --workspace`。首次配置工具链及下载 DuckDB 原生库需要网络。

库化后的协议、执行、DDL 门禁及在线恢复断言对应[回归矩阵](library-regression.md)。`tests/protocol.rs` 通过正式 Bundle/独立薄适配运行；管理故障注入夹具和原生控制测试单列，不替代 Bundle 端到端证据。

`tests/ownership.rs` 的两个 ignored 用例由一次性联邦脚本执行，覆盖真实 PG 阻塞 I/O、取消后的资源保留、StopIncomplete/文件锁和显式测试 Restart 的数据库闭包。入口依赖和生产 FailFast 边界见[资源与退出说明](resource-lifecycle.md)。

`examples/embedded-in-modorch` 是显式排除的独立 workspace，保留单独 Cargo.lock，不由根工作区测试隐式覆盖。运行 `python3 scripts/test-embedded-host.py` 将它复制到仓库外进行独立依赖/框架指纹、fmt/Clippy/test/doc 和真实二进制验收；CI 使用同一命令。脚本复用根 target 的编译/原生库缓存，临时宿主及业务状态会清理，详见[样例说明](../examples/embedded-in-modorch/README.md)。

## skill-hub

所有使用的 skill 通过 skill-hub 托管，项目目录为 `.agents/skills/`。应用、组件、框架兼容检查分别使用 modorch-app-development、modorch-module-development、modorch-framework-maintenance；没有修改框架核心，不虚构框架发布或回归验收。

```sh
skill-hub status --json
skill-hub repo sync --json
skill-hub list
skill-hub use --pattern 'modorch-*' --repo skills-repo --non-interactive
skill-hub apply
python3 .agents/skills/modorch-app-development/scripts/preflight.py --manifest-path Cargo.toml
skill-hub validate --pattern 'modorch-*' --links
```

仅在 status 显示过期时通过 apply 刷新；不要手工复制或修改 managed skill。当前项目还托管了 skill-hub-project-usage、skill-capability-index、skill-hub-skill-authoring；本库专用 `modorch-fedengine-integration` 通过 skill-hub 创建/校验并归档到本地技能仓库，面向外部宿主，检查实际 Facade 与框架指纹。维护已托管技能时比较源/副本，不覆盖此前改动；新技能用 create，修改后 validate/feedback，远端 push 独立授权。

## 检查

```sh
python3 scripts/check-library-contracts.py
python3 -B -m unittest discover -s tests/contracts -p 'test_*.py' -v
python3 .agents/skills/modorch-fedengine-integration/scripts/preflight.py --manifest-path Cargo.toml
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo doc --workspace --no-deps --locked
cargo build --example fedengine-test-host --locked
python3 scripts/smoke.py
scripts/test-federation.sh
```

验证产生的执行日志、临时验收报告、截图和运行环境快照不提交、不归档，完成验证后清理；运行期间使用被忽略的 `docs/validation/` 或仓库外临时目录。长期维护的客户端查询/预期结果位于 `tests/fixtures/metadata/`，能力边界维护在[协议兼容说明](protocol-compatibility.md)和[管理 API](management-api.md)中。

扩展通过 HTTPS 下载官方 `postgres_scanner`、`mysql_scanner`，保存核心版本、平台、来源 URL 和 SHA256；运行时在首次加载时以固定缓冲流式核对清单与文件，并保留 DuckDB 签名验证；已加载到同一实例的扩展不重复读盘校验。预置清单中的受支持扩展在开放会话前加载。业务 SQL 无法 INSTALL/LOAD，生产推荐预置离线扩展，`install_extensions=false`。升级核心/扩展/源版本后须重验对应能力矩阵。

## 规模与故障验证边界

远端目录采集在独立 DuckDB 实例进行，本地批量身份分配与目录投影在不可见事务内准备，最终屏障只覆盖版本复核、提交及运行注册表切换。正式挂载与缓存清理仍有独立的排空窗口；长事务/悬停 Portal 可推迟管理操作完成，但等待操作不应阻挡新查询。锁顺序、资源预算与失败语义见[管理 API](management-api.md#目录刷新与资源预算)。

规模验收必须同时记录采集、类型探测、身份处理、投影、准备、正式挂载和最终屏障耗时，以及并发查询 p99、最大停顿与进程 RSS。DuckDB memory_limit 和采集字节预算不能替代进程内存测量。连续刷新应观察到内存稳定平台；不要求分配器每轮立即归还全部 RSS。

完整资源、网络、存储和进程强杀故障矩阵，以及 24 小时耐久验证仍需专项执行。单次目录发布成功或本地投影耗时不能代替全链路容量验证。后续工作见[剩余项方案](remaining-items-plan.md)。

## CI 执行策略

Actions 在所有分支 push、PR 和手动触发时执行，同一 PR 或分支的新运行会取消旧运行。运行环境固定为Ubuntu24.04、Rust1.94.0、Python3.12，Web验证使用Node.js24与runner预置Chrome；外部Action均固定实际发布commit。

`library-contracts` 对所有变更执行无需 Cargo/Docker 的契约/链接/元数据静态检查与证据检查器负向测试。`changes` 判断改动范围，`validate` 在一个 job 内共享构建目录，最终 `check` 要求契约检查成功并汇总构建结果。分支保护继续使用 `check`：只有改动判断成功，且代码验证成功或纯文档变更按规则跳过时才通过；失败、取消和意外跳过都不能伪报通过。

| 改动 / 触发方式 | 执行内容 |
|---|---|
| 仅 `README.md`、`LICENSE`、`docs/**/*.md` | 执行契约/链接/元数据静态检查，跳过 Rust 构建和数据库集成，再完成最终 `check` |
| 其他路径，包括代码、配置、示例、依赖、脚本、skill、CI，以及未知新路径 | fmt、JavaScript语法、依赖准备、框架/Facade指纹、Clippy、workspace测试、仓库外宿主、真实进程/Web smoke、Rustdoc及一次性数据库联邦集成；Rustdoc警告视为错误 |
| 手动触发 | 无论改动范围如何都执行完整验证 |

CI 由 Clippy 完成编译检查。网络/Web smoke 前显式执行 `cargo build --example fedengine-test-host --locked`；根集成测试不再隐式产生可发布的服务程序。本地仍可使用上文列出的完整检查命令。

Cargo依赖及编译产物通过Rust cache复用，key包含固定的Modorch revision；DuckDB原生库和扩展通过单独缓存复用，key包含runner平台、架构、Cargo.lock和扩展下载脚本。扩展脚本在命中缓存后仍校验manifest与文件SHA256。只有仓库默认分支运行保存缓存，PR与其他分支使用缓存并在缺失时正常下载和构建，不依赖默认分支名为master或main。

恢复缓存后先执行 `cargo fetch --locked`，再执行离线指纹预检，保证冷缓存也能工作。格式检查放在缓存恢复和依赖下载之前，以便尽早失败。未增加构建矩阵或将 Rust 检查拆成多个重复编译的 job；实际节省时间以 Actions 分步耗时为准。

## Web 管理端验证

静态页面位于 `crates/modorch-fedengine-management/web/`，由 `modorch-fedengine-management/src/web.rs` 编译嵌入，通过原有受管 HTTP 监听提供。修改静态资源后需要重新构建二进制，不依赖 npm 或外部 CDN。

`tests/management.rs` 验证页面与静态资源可达、响应策略、未知路径及浏览器同源写边界。浏览器回归使用 Node.js 22 及以上版本的标准库和本机 Chromium，不安装 npm 依赖：

```sh
cargo build --example fedengine-test-host --locked
node --check crates/modorch-fedengine-management/web/app.js
node --check crates/modorch-fedengine-management/web/browser.js
node scripts/web-smoke.mjs --chrome /opt/google/chrome/chrome
```

`tests/browser.rs` 覆盖真实 HTTP 目录、精度保真的只读结果、只读 AST 拒绝、幂等提交、截断、取消、超时和退出；其 ignored 来源用例由 `scripts/test-federation.sh` 在一次性 PG/MySQL 上执行，核对映射、分页、来源详情、授权与快照变化。

脚本启动独立临时状态库、随机端口和专用无头浏览器，验证数据库树仅在对象浏览/SQL 工作区显示（含退出后的桌面/窄屏隐藏）、库切换和查询取消、各库独立草稿、对象/列按需加载、原生双击与光标/选区插入、标识符转义、目录冲突重载、系统 Schema 默认隐藏、空 public、SQL 选中执行、NULL/int8、错误、截断分页和窄屏，以及登记、异步状态、失败探测、凭据引用与 Schema 配置、过期表单冲突、响应丢失后的原键重试、业务虚拟库完整映射编辑和删除、源停用/删除、操作记录。列表回归创建一次性数据源、虚拟库与超过 40 项的持久化操作记录，检查分页、条数切换、跳页、筛选重置、刷新/详情返回保留、删除后页码收缩、新浏览器完整历史及窄屏布局。JavaScript 语法检查及默认浏览器回归已接入 CI，使用 Node.js 24 和 runner 预置 Chrome。目录分页、快照冲突和不可信名称渲染使用浏览器层合成 HTTP 响应，不能替代真实远端目录采集验收。可通过 `FEDENGINE_WEB_POSTGRES_DSN` 和 `FEDENGINE_WEB_POSTGRES_SCHEMA` 显式指定含表的 PostgreSQL schema，额外验证只读探测、发现、激活、刷新及实际目录展示；需事先准备匹配扩展，脚本不执行远端业务 DDL。

可选 `--binary` 指定构建程序，`--screenshot /tmp/页面.png` 生成临时检查截图。服务日志、状态库和浏览器配置退出时清理，截图由调用者验证后删除，不归档。

## 服务与测试容器边界

所有正式服务、Application 编排、运行配置和镜像由 magicDataHub 项目负责。本库没有服务 Dockerfile 或独立服务部署命令。宿主使用公开 `modorch-fedengine` Bundle，统一管理 Runtime、日志、信号、联合预算和检查式退出。

`docker/docker-compose.test.yml` 只启动一次性 PostgreSQL/MySQL 测试源，不构建或运行 FedEngine 服务。固定测试口令不是生产凭证；脚本结束使用专用 project 的 `down -v` 清理。

## JDBC 与目录规模复现

真实 JDBC 程序及准备说明见 [tests/jdbc](../tests/jdbc/README.md)。目录 SQL 回放会验证规划、Describe、执行和结果转换；回放通过不能替代 GUI 实测。

```sh
FEDENGINE_BENCH_OBJECTS=1000 cargo test -p modorch-fedengine-duckdb catalog_scale_baseline -- --ignored --nocapture
FEDENGINE_BENCH_OBJECTS=10000 cargo test -p modorch-fedengine-duckdb catalog_scale_baseline -- --ignored --nocapture
```

该基线只测本地 synthetic 投影；每表一列，不包含远端扫描、远端请求、生产写锁竞争或业务负载影响。实际扫描还需要每对象一次扩展 DESCRIBE，不能据此推断远端请求数恒定。

隔离数据库验证默认使用 25432/23306；端口已占用时可设置 FEDENGINE_TEST_POSTGRES_PORT/FEDENGINE_TEST_MYSQL_PORT，例如 35432/33306。脚本只管理 fedengine-test 项目的临时容器，并在启动失败或测试退出时清理。

### 真实远端规模回归

规模验收使用 release 构建，常规功能回归仍使用默认测试构建。在专用一次性 PostgreSQL 容器上设置 `FEDENGINE_POSTGRES_DSN`，先准备锁定扩展，再单独执行以下用例。它创建独立测试 schema 和临时管理库，检查首次激活、连续刷新、映射目录、对象身份、并发无关及同源查询、RSS 平台及受管任务排空；不要指向业务数据库。测试失败时也应销毁一次性容器及卷。

```sh
python3 scripts/fetch-extensions.py --dest data/extensions
FEDENGINE_SCALE_OBJECTS=10000 FEDENGINE_SCALE_COLUMNS=1 FEDENGINE_SCALE_ROUNDS=10 \
  cargo test --release --locked --test management metadata_scale_refresh -- --ignored --nocapture --test-threads=1
FEDENGINE_SCALE_OBJECTS=10000 FEDENGINE_SCALE_COLUMNS=10 FEDENGINE_SCALE_ROUNDS=10 \
  cargo test --release --locked --test management metadata_scale_refresh -- --ignored --nocapture --test-threads=1
```

输入限制为最多 10k 表/100k 列；正式实例使用 256MB DuckDB memory_limit，一列表场景操作额度为 256MiB，多列表场景在运行前固定为 512MiB（共享额度 512MiB，因此仅一项采集可运行）；独立采集实例的 DuckDB 内存上限为 128MiB。默认 RSS 上限为 1024MiB，可通过 `FEDENGINE_SCALE_RSS_MB` 显式调整，不能在失败后悄悄改变验收预算。用例断言查询无错误、最大停顿不超过 1 秒、p99 相比空载基线增加不超过 100ms、最后三轮 RSS 波动不超过 64MiB，并检查进程峰值预算。它使用闭环查询加 20ms 间隔，不代表开放环吞吐量或生产容量认证。正式屏障及各阶段耗时从 debug 日志单独核对；Apply 的正式挂载窗口与普通刷新分别评估。临时日志验证后清理，不归档。

### 实际 release 进程规模回归

`metadata-scale.py` 启动真实 release 二进制，使用独立端口、状态库和临时日志；只允许显式确认的一次性 PostgreSQL。需要 Python 的 psycopg2，源容器的 `max_locks_per_transaction=512`，源 DSN 从环境变量读取，日志及输出不打印凭据。夹具 schema、状态及日志在退出时删除，输出只写 stdout，不归档。

```sh
cargo build --release --example fedengine-test-host --locked
export FEDENGINE_POSTGRES_DSN='一次性 PostgreSQL 的 DSN'
python3 scripts/metadata-scale.py --disposable-source \
  --binary target/release/examples/fedengine-test-host \
  --library-dir target/duckdb-download/x86_64-unknown-linux-gnu/v2.0.0 \
  --objects 10000 --columns 10 --rounds 10 --repetitions 3
```

默认使用 256MB 正式 DuckDB、512MiB 元数据共享额度/单操作额度、128MB 独立采集实例、1024MiB 进程 RSS 上限；一列场景指定 `--columns 1 --operation-mib 256`。每次重复创建独立进程，包含首次 Apply 和十轮 Refresh。核对阶段耗时、内核 VmHWM、查询 p99/最大停顿、末三轮 RSS 平台、对象身份及投影列数，检查查询和操作无错误及正常退出。轮末平台采样等待后台 checkpoint 收尾，峰值保留全程采样；状态成功不等同于资源已归还。预算不因失败上调；工程优化目标为 RSS 800MiB、最大停顿 500ms，外部验收仍为 RSS 1024MiB、停顿 1s、p99 基线+100ms、末三轮波动64MiB。

多源场景可设置 `--sources 2 --objects 5000 --columns 10 --operation-mib 256`，保持远端总量为100k列并并发刷新；`--mappings 2` 验证同一来源的多份逻辑映射。若映射倍增或混合业务需独立容量预算，必须在运行前显式设置并单独报告，不能与默认单映射验收混为一组。查询为闭环并在每轮间隔20ms，不代表开放环吞吐量或24小时耐久。实际部署额度的候选配置与边界见管理 API。


## 数据源组织回归

在专用 PostgreSQL/MySQL 测试实例中运行（不要指向业务库）：

```sh
cargo test --locked --test organization
cargo test --locked --test organization -- --ignored --nocapture --test-threads=1
```

第二条需要 `FEDENGINE_POSTGRES_DSN` 和 `FEDENGINE_MYSQL_DSN`，使用 `docker/docker-compose.test.yml` 的一次性实例；测试创建并移除独立源库、角色和表。覆盖实例枚举/系统排除/逐库失败、名称预留、同库合并、重叠 Schema/关系去重、PG public、MySQL 多库和大小写、来源权限、逐表路由、事实冲突、停用/删除贡献和重启 OID。`scripts/test-federation.sh` 包含该测试入口；扩展相关用例仍按既有能力边界处理。

`node scripts/web-smoke.mjs` 检查真实 Chromium 的管理与浏览交互；可额外提供 `FEDENGINE_WEB_POSTGRES_CREDENTIALS`（结构化凭据 JSON）及 `FEDENGINE_WEB_POSTGRES_HOST/PORT/DATABASE` 验证新接入向导、核对后激活、原生 public 和 SQL 上下文。这些参数仅进入一次性服务环境，不写入仓库。默认主机 127.0.0.1、端口 36432、库 crm。

当前管理存储版本 7。新组织模型使用独立状态路径验证，不转换或清空既有版本的开发实例；部署需单独备份并选用匹配存储版本。

## 配置与独立实例回归

路径、密钥引用及部署状态规则见[配置所有权](configuration-ownership.md)。`cargo test --test instances --locked` 验证同文件拒绝、启动补偿和多实例状态隔离；其中真实凭据目录隔离用例由 `scripts/test-federation.sh` 在一次性 PostgreSQL 中执行。外部宿主脚本另验配置文件与宿主工作目录不同的启动方式。

## 业务库契约与发布准备

[业务契约](../contracts/README.md)以 13 个稳定 ID 对应测试映射，`check-library-contracts.py` 默认只做静态检查，明确输出 execution_status=NotRun；`--run local`、`--run external` 才逐条核对实际测试成功行，不依据通过总数认定覆盖。原生 PG/MySQL 用例和测试注入 Restart 单独标明范围。发布与包元数据见[0.1.0 交付边界](releases/0.1.0.md)，所有包保留 publish=false。
