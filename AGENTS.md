# Modorch FedEngine 开发规约

## 当前阶段的数据模型

本项目处于开发阶段，当前没有历史数据兼容要求。开发和收口只维护当前版本的数据模型，不新增历史版本迁移链、旧 DTO/存储格式回退、双格式读写、历史回填或自动修复逻辑。修改相关代码时，删除不再需要的历史兼容分支，直接使用当前结构。

当前版本的初始化和重复启动必须幂等；发现不受支持的存储版本时明确报错，不静默升级、降级或清空数据。配置的正常缺省值、来源事实未知状态、运行故障恢复、失败保留、稳定对象身份和快照原子发布属于当前功能契约，应继续维护。

受支持 PostgreSQL/MySQL 源版本的适配及对外 PostgreSQL SQL/协议行为仍是产品要求，不属于历史存储兼容，不据此删减。

部署备份和匹配版本回退属于运行保障，不等于历史数据兼容。不得借此规约擅自删除现有实例的管理状态、凭据或业务数据。后续如需支持历史数据，先单独确认兼容范围及验收要求，再实现。

详细开发与验证入口见 [docs/development.md](docs/development.md)。

## 临时验证产物

临时验收报告、阶段回归记录、GUI 截图、执行日志和运行环境快照不提交、不归档，验证完成后清理。长期能力边界维护在正式能力/API 文档中；可复用测试代码、固定查询及预期夹具继续保留。文件分类与提交检查见 [docs/git-submission.md](docs/git-submission.md)。

## 业务库接口与宿主接入

公开接口和行为以 `crates/modorch-fedengine` Rustdoc 与 [业务库契约](contracts/README.md) 为准；外部接入使用项目托管的 `modorch-fedengine-integration` Skill，并运行其 preflight 核对实际 Facade/框架指纹。当前 EmbeddedCore 明确拒绝，受控 Query/Management Client 已开放，DDL 沿用 QueryClient；身份与授权见 [进程内 Client](docs/in-process-clients.md)；设计稿中的未来接口不得当作可调用实现。修改行为时同步契约、测试映射、正式说明与匹配的 Skill。只运行静态检查不能报告运行验收通过，发布边界见 [0.1.0 交付说明](docs/releases/0.1.0.md)。

## 库与服务编排边界

modorch-fedengine 只作为可嵌入库交付；后续所有服务、Application 编排及部署镜像统一在 magicDataHub 项目实现。本仓库不新增可发布服务 binary、服务 Dockerfile 或独立服务交付流程。`examples/fedengine-test-host.rs` 和 `tests/support/host` 仅用于开发回归，根包通过 dev-dependency 使用，不进入业务库依赖闭包。`docker/docker-compose.test.yml` 仅保留一次性 PG/MySQL 测试源。
