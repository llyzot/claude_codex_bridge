# CCB 核心迁移 Rust：当前可行性

检查基准：2026-10-11，`HEAD 7e6b6bb8`，检查前工作区干净。本轮只读源码并创建本文；未启动 Agent、安装或调用模型。本轮新验证仅配置测试 11 项通过。历史通过记录与本轮复测分开看。

**结论：可行，已有可运行的原生核心和真实 Pi Lead → Codex Worker 回投证据；尚不能作为现有 CCB 的完整生产替代。** 下一步应补齐接口、恢复与 Herdr 边界，再切默认入口，不必重新从零重写。

## 已实现与边界

| 范围 | 当前源码事实 |
|---|---|
| 两条运行路径 | 默认仍是 Python；`CCB_CORE_RUNTIME=rust` 是 Python 调用私有 API-1 核心。另有无需 Python 核心的 `ccb-native` API-2 自主 daemon。见 `lib/native_kernel.py:27`、`:92`，`rust/crates/ccb-kernel/src/cli.rs:149`。 |
| 队列、持久化、回投 | Rust 持有 writer lock、redo journal、追加偏移恢复；daemon 执行终态、重试、Reply 与 continuation 入队，并自主 tick。见 `rust/crates/ccb-kernel/src/store.rs:61`、`:332`、`:377`，`rust/crates/ccb-kernel/src/daemon.rs:645`、`:720`，`rust/crates/ccb-kernel/src/control_plane.rs:181`。 |
| 三家协议 | Pi 用请求锚点、settled 和同会话 idle 确认；Codex 绑定 thread/turn，再读终态；Claude 绑定 session、用户 UUID，要求 `is_error=false`。见 `rust/crates/ccb-kernel/src/pi_rpc.rs:323`、`:336`，`codex_rpc.rs:356`，`claude_stream.rs:217`、`:259`。文件均在同一 Rust `src` 目录。 |
| 配置与入口 | 可读 compact/TOML/hybrid；编译仅支持 `inplace` 和 Pi/Codex/Claude，须提供 executable manifest。原生 CLI 命令仍有限；Windows 原生路由须显式开关，仅支持 ccb/ask。见 `rust/crates/ccb-kernel/src/config.rs:256`，`cli.rs:92`，`platforms/windows/launcher/src/main.rs:30`、`:97`。 |
| 安装、进程生命周期 | Windows 安装器仍要求 Python、创建托管运行时；Rust 构建产物并不等于无 Python 安装。Windows Job Object 管理自有子进程；非 Windows 对应函数仍为空实现。见 `platforms/windows/installer/install.ps1:883`、`:943`，`rust/crates/ccb-kernel/src/security.rs:100`、`:324`。 |

历史 checkpoint 记录真实 Worker 返回及 Pi 同进程/会话自动恢复评审，测试还明确要求初始 Lead 回合不等待、不轮询（`test/test_native_real_models.py:65`）。这说明自动回投机制已经落地，**原因是 daemon、协议关联和 continuation 调度，而不是换语言本身**。历史真实测试选用私有 Pi 配置里的已有 Git Bash；不等于所有 provider 工具已原生 pwsh 化。

Claude Lead 回投目前主要是协议 fixture；真实 CLI 只验了未登录失败。成功 Claude 推理、原生 Herdr 生命周期/接入仍未验。来源：`docs/analysis/native-real-model-checkpoint-20261011.md`，`test/test_native_claude_stream.py:44`、`:68`；本轮未复测模型。

## 收益、风险与验收

可靠收益是把跨文件状态变化集中到常驻单写核心，减少 Python/Rust 双方各自推断状态；协议关联与事务恢复更容易统一测试。部署去 Python 需完成 CLI、Keeper、配置和安装迁移才兑现。Rust ownership/type checking 能发现许多并发错误，但不保证业务 exactly-once，也不能证明更快（[Rust 官方并发说明](https://doc.rust-lang.org/book/ch16-00-concurrency.html)）。本机 Windows 原生/现有 Python 的完整性能对照尚缺，不预测倍数或工期。

关键风险是重启后旧 running 任务无法确定实际执行结果：当前重建调度但不自动重放；Codex/Claude 的取消、deadline、审批控制未齐，Pi 已有 abort。见 `rust/crates/ccb-kernel/src/daemon.rs:154`、`:384`。JSON 动态字段和系统 API 仍需契约/故障测试，语言不会替代它们。

建议按以下顺序验收后迁移：

1. 冻结公开命令、配置、状态契约；补齐 worktree、Keeper、Herdr 及 provider 控制，使用相同输入比较旧/新结果。
2. 在独立 namespace 验 Claude 成功回投、Herdr Pi/Codex、排队与重复/迟到信号、崩溃恢复；旧 running 必须明确对账，不能猜测重放。
3. 对比启动、空闲 CPU/RSS、收执及回投 p50/p95；验证状态兼容和版本回滚后，再切默认 Rust 入口与安装器，最后移除 Python 核心依赖。

阶段边界亦见 `docs/analysis/rust-core-refactor-progress.json`：`complete=false`，生产安装及既有 Workspace 未变。
