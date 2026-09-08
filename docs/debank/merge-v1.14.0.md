# Tempo v1.14.0 upstream 合并验证

日期：2026-09-08。状态：合并完成，编译和回归进行中；测试机验证待资源条件满足。本报告未宣称可交付 release。

## 1. 升级内容与必要性

[上游 v1.14.0](https://github.com/tempoxyz/tempo/releases/tag/v1.14.0) 将 RPC node/validator 均列为 High，要求在 T11 激活前升级。Mainnet 时间为 **2026-09-10 14:00 UTC（新加坡 22:00）**，testnet 提前一天。现行 v1.13.0 的 mainnet genesis 不含 t11Time，目标版本为 1789048800。

T11 延长 expiring nonce 至五分钟、提高预编译输入 gas、启用严格 ABI 解码并加强重复输入校验。此外升级 Reth 状态 overlay/RPC 路径、tempo/1 gossip、snapshot download 默认参数与 txpool 过滤能力。正式 fork 尚未包含 v1.13.1/v1.13.2，因此本次累计包含 ABI decoder 内存限制和 snapshot/consensus 恢复修复。T12 runtime hook 已预埋，当前 mainnet/testnet 没有 T12 激活时间。

## 2. Fork 保留项与影响分析

从 `origin/debank=eb52df6878989a78fce4b628e259ad329c78a898` 合入目标 `1ec5653e39`；保留 DeBank 三个自定义 RPC 的 crate、注册、Rust 1.96 构建环境及 DeBank build/release workflow。

Reth `10aa6a512→5f02aa393` 删除 StateProviderTraitObjWrapper，并将 Trace::inspect 参数限定为 StateCacheDb。DeBank 的 StateDiffTraceDB 需要继续捕获读写：两个 parent-hash provider 直接交给 StateProviderDatabase，调用原 inspect 相同的 `evm_with_env_and_inspector → transact → from_evm_err`，随后维持 `diff_db.commit(state)`。该适配只影响 pull RPC，未改 node 的共识执行器。

Cargo.lock 以 upstream v1.14.0 为基底。结构化对账确认没有移除或升级任何 upstream package；只新增 debank-rpc、md-5 0.10.6，并给 tempo-node 加入 debank-rpc 依赖。

正向影响：新状态 provider、precompile gas/ABI、nonce 规则会参与 RPC 重放/模拟；须检查历史和近 head 的 hash、trace/state_diff，以及 T11 前后 fixture。反向影响：自定义 replay 保留独立内存 DB，不写 node 的 canonical DB；原 inspector 与 commit 顺序保持不变。Git 冲突解决不能替代运行验证。

## 3. 部署情况

生产只读实测：`production/blockchain-tempo` 两个 hybrid pod 运行 v1.13.0-debank，node/jrpcx Ready、restart=0；hybrid-0 syncing=false、16 peers，head 正常增长。测试采用单独常规节点，只开放 localhost RPC/metrics，不启 ETL/Kafka/S3/etcd 投递；首次启动前重建 P2P 身份。

测试部署尚未执行。lihe-dev 有 93.19GiB 物理内存，已有限额容器合计 93GiB，另有不设限的 morph-archive-trace-test，违反总限额加 8GiB 的部署条件。现有 Tempo 节点供另一套 T11 pipeline 使用，未替换或停止。候选生产周期快照为 snap-00c445d5bb1442d9c（2026-09-04 05:02 UTC，60GiB），后续新测试卷使用初始化速率 300MiB/s，不启用生产快照 FSR。

Compose 将在镜像确定后补入独立部署附件；未对生产或其他测试任务做变更。

## 4. 测试结果

- merge 冲突已解决；git diff --check 通过。
- 首次 check 定位 Trace::inspect API 不兼容，已按第 2 节适配。
- `cargo check --workspace --locked` 通过（2m06s，含 tempo、node、EVM、e2e）；适用单测、binary build、双架构 CI 进行中。
- 测试机同步、两段各 20 块 hash、自定义 RPC parity：未执行。
- T11 主网尚未激活，不预报激活后真实链验证结果。

## 5. 待跟进事项

[PR #10](https://github.com/Chaintable/tempo/pull/10) 的完整 executor 生命周期、AA trace/event/state-diff 修复保持独立 review，未并入本分支。此次保持生产分支既有 RPC 行为，不把其已知缺陷误算为 v1.14.0 新增问题；也不宣称 writer-to-Leafage 全链路通过。PR #10 所述 Leafage storage-wipe consumer 依赖需另行验证。

需要先解决测试资源条件，再完成运行验证与本报告；用户 review/merge PR 后才进入 release，生产切换由 SRE 执行。
