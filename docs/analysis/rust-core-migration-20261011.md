# CCB 核心迁移到 Rust：本地源码评估

日期：2026-10-11。检查基线：本地提交 `1c1c4d08`，含 Windows Pi 放行、后台 Python 环境修复和原生插件/技能直接引用。本文只评估；没有迁移实现、启动或停止 Agent、修改用户 Workspace。

## 结论

**可行，但适合分阶段迁移常驻调度核心，不适合立即重写整个 CCB。** 优先考虑协议服务、Mailbox/Job 状态机、存储索引和完成事件处理；保留已有 Python Provider 适配层作为迁移过渡。收益首先在部署、常驻资源和结构清晰度；不能保证 Agent 启动或模型任务显著加速。自动回投和唤醒是否可靠，主要取决于持久化、关联、幂等和恢复协议，换语言本身不会补齐这些条件。

这里区分三类依据：**源码事实**指当前文件可验证的行为；**历史测量**指仓库现存性能记录，不能外推到当前 Windows；**建议/推断**指尚未实现或测量的设计。

## 1. “核心”实际包含什么

| 边界 | 当前源码事实 | 迁移重点 |
|---|---|---|
| 常驻控制平面 | `CcbdApp` 组合服务图，socket 服务有请求线程和维护线程 | 保持 CLI/RPC 接口、启动围栏、后台所有权 |
| 调度与路由 | `JobDispatcher` 对提交/完成和 callback 使用互斥保护，记录 Job/事件并维护活动槽位 | 每 Agent 串行、不同 Agent 可并行、取消和重试 |
| Mailbox/Message Bureau | 请求与回复共享持久化 FIFO；头部 claim、投递租约、消息/尝试/回复分离 | 顺序、租约版本、重试幂等、不能提前释放 |
| 完成判定 | Provider 产生 observation/decision，Tracker 结合 Detector 和 Selector | 精确任务/会话/轮次关联，区分中间输出和完成 |
| 存储与恢复 | JSON/JSONL 记录、完成快照、执行状态、callback edge；恢复按 Provider 能力决定 | 格式兼容、崩溃中间态、恢复/重排队 |
| Provider 接入 | Backend 同时暴露执行适配器、会话绑定、启动器；大量 CLI/终端专有逻辑 | Rust/Python 边界协议和后续逐个迁移 |

证据：[服务图](../../lib/ccbd/app_runtime/service_graph.py#L74)、[线程服务](../../lib/ccbd/socket_server_runtime/loop.py#L49)、[调度器](../../lib/ccbd/services/dispatcher.py#L63)、[共享 FIFO](../../lib/ccbd/services/dispatcher_runtime/fifo_order.py#L8)、[Mailbox claim](../../lib/mailbox_kernel/service_runtime/transitions_runtime/claiming.py#L12)、[Backend 接口](../../lib/provider_core/registry.py#L16)、[执行协议](../../lib/provider_execution/base.py#L55)、[Tracker](../../lib/completion/tracker.py#L43)。

本地物理行统计：`lib` 下 1,856 个 Python 文件、274,102 行；其中 `ccbd` 402 个/50,532 行，`provider_backends` 630 个/63,036 行，`provider_execution` 56 个/5,456 行，Mailbox 17 个/1,785 行，Completion 29 个/1,797 行，Storage 12 个/2,032 行。**包含空行、注释和大量 facade，不是需要逐行翻译的规模，也不是工期估算。** 这些数字说明“全部改成 Rust”与“迁移调度内核”是明显不同的任务。

## 2. 必须保留的自动回投/唤醒约束

1. **请求与结果共同遵守同一条 Agent FIFO。** 回复事件在需要时物化为 delivery Job，保持原始入队位置；不能另开高优先级回复通道绕过旧任务。重启后按持久化 Mailbox 入队顺序恢复，而非按 delivery Job 后来写入 JobStore 的顺序。[FIFO 实现](../../lib/ccbd/services/dispatcher_runtime/fifo_order.py#L8)、[重建排序](../../lib/ccbd/services/dispatcher_runtime/state.py#L77)
2. **一次只 claim 队首且不占用他人的租约。** claim 校验已有租约、队首身份，更新事件和 lease_version。Rust 的 channel 本身不提供持久化或这些校验。[claim/lease](../../lib/mailbox_kernel/service_runtime/transitions_runtime/claiming.py#L12)
3. **投递成功不等于 Lead 处理完成。** 现有实现明确禁止以“发送成功”或经过一段时间代替 Provider 完成事件；否则新任务会挤入仍在运行的 Lead。[发送后的完成语义](../../lib/ccbd/services/dispatcher_runtime/reply_delivery_runtime/start_completion.py#L6)
4. **`ask --chain` 是持久化依赖关系，不是普通回信。** 当前要求存在活动父 Job、单目标、每父 Job 一条未完成依赖；普通嵌套 `ask` 需要改为 `--chain` 或独立任务 `--silence`。[校验](../../lib/ccbd/services/dispatcher_runtime/callbacks.py#L33)、[callback edge 建立](../../lib/ccbd/services/dispatcher_runtime/callbacks.py#L47)、[单边约束](../../lib/ccbd/services/dispatcher_runtime/callbacks.py#L99)
5. **Worker 完成后提交 continuation，并允许崩溃后修复。** 检查已有 continuation 避免重复，保存 CHILD_COMPLETED/CONTINUATION_SUBMITTED；repair 从持久记录补齐未完成的转换。[continuation](../../lib/ccbd/services/dispatcher_runtime/callbacks.py#L155)、[repair](../../lib/ccbd/services/dispatcher_runtime/callbacks.py#L229)
6. **完成证据必须来自正确实例和任务。** Pi 使用 `agent_settled`、runtime_instance_id 和 request_anchor；Claude hook 对 req_id 验证。仅有“终端不再输出”不能覆盖这些关联规则。[Pi 权威事件](../../lib/provider_backends/pi/pane_execution.py#L67)、[Pi 实例/任务校验](../../lib/provider_backends/pi/pane_execution.py#L308)、[Claude hook 校验](../../lib/provider_backends/claude/execution_runtime/hook_results_runtime.py#L129)
7. **重启恢复存在 Provider 差异。** 当前普通执行根据保存的执行状态恢复；回复投递 Job 重启后被标记为需重新排队，不能直接当作完成；不可恢复任务有明确失败或重新提交原因。[恢复分支](../../lib/ccbd/services/dispatcher_runtime/restore_runtime/execution.py#L97)

迁移应追求“至少一次投递 + 状态转换/continuation 去重”，不能声称端到端严格 exactly-once：终端注入、外部 CLI 执行和本地记录无法天然形成一个原子事务。现有多文件写入也不是跨文件事务。[完成写入顺序](../../lib/ccbd/services/dispatcher_runtime/finalization_runtime/persistence.py#L15)、[JSONL append](../../lib/storage/jsonl_store.py#L11)

## 3. 已有 Rust 能复用多少

| 组件 | 当前落地 | 复用判断 |
|---|---|---|
| Windows launcher | 选择入口、寻找 Python、传环境并执行 Python | 可复用 exe 入口和安装逻辑；不是已存在的 Rust 核心 |
| `ccb-rs-helper` | 当前声明仅 echo、native.output.observe、storage.scan.inventory、storage.scan.summary；stdin 读到 EOF 后一次响应 | 输出解析与空间统计可抽出 crate；不能当成持久调度服务 |
| `ccb-runtime-accelerator` | Codex observation 和监测；库的非 Unix serve 明确报错，main 无条件导入 UnixStream | 部分解析模型/fixtures 可复用；Windows IPC 和服务入口必须重做 |
| `ccb-agent-sidebar` | 独立终端 UI 客户端 | 继续做客户端即可，不能替代 Job/Mailbox 权威服务 |

证据：[launcher 转交 Python](../../platforms/windows/launcher/src/main.rs#L29)、[helper capabilities](../../tools/ccb-rs-helper/src/main.rs#L121)、[一次 stdin 请求](../../tools/ccb-rs-helper/src/main.rs#L146)、[helper capability dispatch](../../tools/ccb-rs-helper/src/main.rs#L159)、[accelerator 非 Unix 限制](../../rust/crates/ccb-runtime-accelerator/src/lib.rs#L214)、[Unix-only main](../../rust/crates/ccb-runtime-accelerator/src/main.rs#L1)、[UI Cargo](../../tools/ccb-agent-sidebar/Cargo.toml#L1)。

**文档与源码差异：** 历史 phase8/phase9 描述 JSONL/jobs helper 能力，但当前 helper 的 capability dispatch 不包含这些项。不能把历史描述当作当前可直接复用模块。[历史 phase9](../plantree/plans/python-rust-hybrid-performance/history/phase9-adaptive-job-query-2026-06-15.md#L5)、[当前 dispatch](../../tools/ccb-rs-helper/src/main.rs#L159)

## 4. 可能收益与不能保证的收益

**部署收益（有条件）：** 真正的 Rust daemon/CLI 可减少 Python 解释器启动、venv 查找和 Python 依赖安装。但如果 Provider 和启动/配置仍由 Python 承担，Python 依赖仍需保留；把入口包装成 exe 不会消除它。[现有 launcher 选 Python](../../platforms/windows/launcher/src/main.rs#L29)、[Provider 协议](../../lib/provider_execution/base.py#L55)

**运行收益（待测）：** 常驻实例中批量 JSONL 解析、索引、尾部读取、事件路由和减少 Python 对象分配，可能降低 CPU/RSS。若保留“一次调用启动一次 helper”，进程启动和序列化成本会抵消小任务收益。仓库历史 Linux 测量已经有反例：phase8 Python p50 16.762ms、Rust helper 31.545ms；phase9 adaptive 查询 Python p50 2.042ms、Rust 2.987ms。记录明确没有默认启用；优化读取形状比语言变化更有效。这些是旧环境测量，不能作为本机 Windows 基准。[phase8 实测 JSON](../../dev_tools/perf_results/python_rust_phase8_job_summary_projection_helper.json#L11)、[phase9 实测](../plantree/plans/python-rust-hybrid-performance/history/phase9-adaptive-job-query-2026-06-15.md#L25)

**可靠性收益（有条件）：** Rust 类型和所有权可以使非法状态、共享可变状态更难写出来；但 FIFO、掉电持久性、Agent 完成含义和重放去重仍需显式设计和测试。[Rust 并发说明](https://doc.rust-lang.org/book/ch16-00-concurrency.html)、[当前 callback repair](../../lib/ccbd/services/dispatcher_runtime/callbacks.py#L229)

**对你当前启动慢的问题：** 现有本地隔离三 Agent 测量已经是 cold 16.10s、warm 14.12s，Pi 配置引用 6ms/5ms。cold supervisor 10.60s，其中 build_start_cmd 4.91s、namespace_ensure 2.54s、终端 respawn 1.27s；不能仅凭这些阶段耗时判断 Python CPU 占比。模型请求、CLI 启动、终端/Herdr IPC 和原生插件加载不会因 CCB 换 Rust 自动消失。这里没有重测，更没有 Rust 版本对照。

测量原件：`C:/Users/user/Desktop/work/ccb-pi-windows-test-20261010/native-assets-result.json`（只读读取；数值单位见 JSON，阶段项为 ms）。

## 5. Windows 迁移的具体边界

- **先保留现有 loopback TCP 控制平面。** endpoint 包含地址、generation、token_ref，已有 token 握手及 Windows ACL 保护；Linux/macOS 保留 Unix socket。不要在迁移语言的同一阶段切换 IPC 协议。Named pipe 可以后续评估，Tokio 有 Windows 支持，但不是当前契约。[TCP endpoint](../../lib/ccbd/control_plane_transport/endpoint.py#L91)、[token 创建](../../lib/ccbd/control_plane_transport/token_auth.py#L38)、[token 握手](../../lib/ccbd/control_plane_transport/token_auth.py#L147)、[transport factory](../../lib/ccbd/control_plane_transport/factory.py#L16)、[Tokio named pipe](https://docs.rs/tokio/latest/tokio/net/windows/named_pipe/index.html)
- **不要让外部同步调用堵住事件循环。** Herdr 交互、进程创建、磁盘读取、配置和恢复都有阻塞环节；Tokio 有助于并发 I/O，但大量普通文件操作不会自动变快。需要清晰的任务并发限制、阻塞池和超时。[现有独立请求/维护线程](../../lib/ccbd/socket_server_runtime/loop.py#L49)、[Tokio 使用边界](https://tokio.rs/tokio/tutorial)
- **保持原目录引用策略。** Windows 无管理员 symlink 权限时使用 junction；清理目标必须不遍历源目录，保留 marker 所有权检查。迁移不能重新引入启动时整目录快照。[junction](../../lib/provider_core/projected_assets.py#L26)、[live routing](../../lib/provider_core/projected_assets.py#L69)
- **文件替换、ACL、长路径和进程所有权仍是 Windows 专门工作。** 当前原子写入有平台判断，JSONL append 没有 fsync；不要把“Rust write 成功”当作断电可靠性保证。[JSON save](../../lib/storage/json_store.py#L26)、[atomic 平台边界](../../lib/storage/atomic.py#L18)、[JSONL 写入](../../lib/storage/jsonl_store.py#L11)
- **桌面 App 的文件虚拟化需要单独处理。** 本会话曾出现 Codex MSIX 环境内安装与普通终端可见文件不同，改用普通 Windows Terminal 安装解决。Rust exe 不会自动改变父进程所属隔离上下文；不能以换语言承诺解决。此项为本会话操作观察，没有据此重现或修改环境。

## 6. 建议的最小迁移路线

### 阶段 0：固定行为和基准

保存当前 CLI 帮助、RPC request/response、JSON/JSONL schema 和 fixture；当前 API_VERSION/SCHEMA_VERSION 都是 2。将非确定性时间/ID归一化后建立状态转换轨迹对照。隔离环境测 1/3/10 Agent 的启动、空闲 CPU/RSS、ask receipt p50/p95、完成事件到 Lead 收到 continuation 的延迟、长历史查询，以及各阶段外部进程数量。Worker 用假 Provider 控制延迟，避免把模型网络波动计入调度性能。[版本常量](../../lib/ccbd/api_models_runtime/common.py#L6)

本次提交前针对 live assets、旧缓存工具、Herdr client、后台 Python 环境和 Windows 入口的测试共 **219 项通过**。这不是全仓库测试结果。此前 Pi 完成相关测试在修改版本和未修改基线均为 43 通过、9 失败，失败案例相同；迁移基准应先固定或修复这些既有失败，不能把它们计为新实现通过。

必须覆盖：请求/回复交错 FIFO、Worker 中间输出、旧/外来 req_id 和实例、Lead 忙碌、用户取消、permission prompt、完成先于日志刷新、callback 连续提交、在 callback 各持久化步骤中间崩溃、daemon 重启、终端断线重绑、lease 到期。现有测试可作为合同素材：[Mailbox 测试](../../test/test_v2_mailbox_kernel_service.py#L1)、[dispatcher](../../test/test_v2_ccbd_dispatcher.py#L1)、[统一 FIFO 重启](../../test/test_unified_message_fifo.py#L350)、[回复启动完成](../../test/test_reply_delivery_start_completion.py#L1)、[租约到期](../../test/test_v2_mailbox_lease_expiry.py#L1)、[Pi 完成](../../test/test_pi_pane_execution.py#L1)、[Windows transport](../../test/test_ccbd_windows_tcp_loopback_transport.py#L1)、[热重载事务](../../test/test_ccbd_reload_transaction.py#L1)。不要用全量 pass 掩盖既有失败。

### 阶段 1：独立 Rust 只读路径

提取 helper/accelerator 中可复用解析代码为库；提供**一个长期运行的受限 sidecar**或后续原生模块边界，批量处理同类型 observation 和只读历史查询。原 Python 仍为唯一写入权威。以 fixture 和真实录制日志做差分；先测收益，不默认开启未达目标路径。

### 阶段 2：Mailbox/Job/Callback 内核

Rust 持有单一事件状态机和持久化写入权；Python 适配器只提交标准化 observation、执行 start/poll/restore/cancel 操作。固定 `ProviderSubmission`/`CompletionItem`/`CompletionDecision` 的线协议与版本，将不可序列化的 Python `runtime_state` 运行对象保留在适配器进程中，仅跨边界传 opaque handle。否则现有 Python Protocol 不能直接作为 IPC 契约。[执行模型](../../lib/provider_execution/base.py#L16)、[适配器 registry](../../lib/provider_execution/registry.py#L15)

首期保持 JSON/JSONL 可读和旧格式兼容，Rust/Python 禁止同时写同一状态目录。不在同一改动中引入 SQLite/WAL 和语言替换；若将来选择事务数据库，应作为独立 schema 迁移、回滚和 durability 评估。先可回放旧记录，再在隔离项目切换 daemon。

### 阶段 3：常驻服务与 CLI

迁移 `ccbd` 控制平面、生命周期围栏和调度服务；保持 `ccb`/`ask`/`back` 用户界面、退出码、RPC JSON、启动参数和 Windows TCP token 契约。当前请求 worker 串行且有 bootstrap gate 与 authority lock 顺序；可以并发读取连接，但不能直接对每个状态变更请求 Tokio spawn 而取消状态转换串行化。纯 Rust CLI 带来部署好处，但 Provider 仍保留 Python 时应明确展示依赖。采用明确 daemon 版本/状态 schema 检查；旧进程继续运行时不能直接热替换其状态权威。[现有请求锁定合同](../../lib/ccbd/socket_server_runtime/protocol.py#L22)

### 阶段 4：先 Pi/Codex，再 Claude/其他 Provider

以你实际使用的 Pi + Codex Windows Herdr 路径作为首批 Provider；保留 Pi 原生扩展完成事件、Codex 精确会话证据、现有 auth/config 边界。Claude 的 hook、队列激活和 session 选择随后迁移。其余 Provider 逐个迁移，最后才能真正取消 Python 运行时。全面重写完整 Provider 矩阵的风险高于先迁移常驻内核。

## 7. Go/No-go 验收

| 项目 | 建议门槛（设计目标，未实现） |
|---|---|
| 行为 | 固定 fixture 的关键状态/排序与 Python 一致；不能新增丢回投、提前释放、外来完成 |
| 崩溃 | 在提交、完成、continuation 和 lease 转换间每个注入点重启；无静默丢失、无重复 continuation 权威记录 |
| 非阻塞 | Lead 已结束提交轮次、Worker 工作期间仍可接收其他请求；Worker 结束后自动生成可执行续轮 |
| 用户可见恢复 | 分别记录 continuation 入队、实际投递、Lead 新轮次开始三个时刻；入队成功不能代替真正恢复执行 |
| 性能 | 同环境多次测量 p50/p95、CPU/RSS；建议热点路径 p95 降低 ≥20%，且其他路径无明显回归，否则保留 Python 默认 |
| Windows | 标准用户权限、pwsh、Windows Terminal、Herdr 隔离 namespace；原始 Workspace/Agent 没有控制操作 |
| 回滚 | 旧版本能够读取保持兼容的状态；任何不兼容 schema 都需独立备份/迁移流程 |

**明确建议：** 当前先接受已经修复的 Python 版本作为可用基线。若决定投资 Rust，先做只读差分和内核边界原型，再根据本机实测决定是否迁移常驻服务。对于“Lead 不阻塞、Worker 自动回投并唤醒”的目标，现有状态机的完整移植比把所有 Python 文件改成 `.rs` 更重要。
