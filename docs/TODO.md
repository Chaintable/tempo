# Tempo 待办事项

## 已完成

- ~~ECR repo 创建~~ → `blockchain/tempo` 已创建，CI 已更新
- ~~Endpoint 注册~~ → data/pre/trace/archive.tempo.blockchain 全部可用
- ~~生产部署~~ → 2 台机器（172.22.141.54: data+pre, 172.22.179.114: trace+archive），镜像 v1.4.3

## 待办

### 1. 监控告警
- Prometheus metrics（节点已暴露 9001 端口）
- Grafana dashboard
- 飞书告警配置

### 2. pre_traceMany fee log 改造
- 当前 pre_traceMany 缺少 Tempo handler 层产生的 TIP-20 fee log（普通 tx 少 1 条，AA tx 少 2~3 条）
- 改造方案：EVM 执行后根据 gasUsed + basefee 计算 fee，读链上 fee token 偏好，追加 Transfer log
- 前置依赖：DeBankCore `engine.py` 中 Tempo 需加入 gasPrice 配置列表（当前硬编码为 0x0）
- 详见 `docs/test-report.md` 中"pre_traceMany 缺少 TIP20 fee 相关 log"章节

### 3. trace_debankBlock 上线

- [ ] background-tracer dry-run 验证（上线阻塞项，需 binary 部署到 dev 机器）
- [ ] 合并 PR #4 (`feature/debank_rpc` → `debank`)
- [ ] 生产部署（2 台机器更新镜像）
- [ ] 部署 background-tracer sidecar（Kafka/S3 + chain_id=4217）

### 4. 多 call AA tx 适配 (上线阻塞)

- Tempo 0x76 tx 支持 `calls: Vec<Call>` 多调用原子执行
- 当前 `DebankTransaction.to/input/value` 只展示第一个 call，其余 call 信息仅在 traces 中
- **链上已存在多 call AA tx**（如 block 0x9eeb98: approve + swap 双 call）
- 实际表现：`txs` 中 `to=null, input=第一个call`，第二个 call 目标/数据仅在 traces `trace_address=[1]` 可见
- **影响范围**: leafage-evm（tx 索引/state 关联）和 DeBankCore（tx 解析）都消费 txs 字段，to/input 不完整会导致数据缺失
- 需在上线前修复或确认消费方可容忍

### 5. dev 机器清理
- blockchain-misc-x3 上的 tempo 容器已停止，EBS 已 detach（snapshot: snap-08859f84cfb8b1611）
- 确认不再需要后可删除 volume vol-0e14b18861b31f2cc 和 snapshot

## 参考信息

- 代码分支: `feature/debank_rpc` / `debank`
- ECR: `294354037686.dkr.ecr.ap-northeast-1.amazonaws.com/blockchain/tempo:v1.4.3`
- Release: https://github.com/DeBankDeFi/tempo/releases/tag/v1.4.3
- 线上节点: data/pre/trace/archive.tempo.blockchain

## 2026-09-08 upstream v1.14.0

- `[2026-09-08][decided] 独立合并 v1.14.0` — 用户已确认 pipeline Step1 GO，从 origin/debank eb52df687 创建 merge-v1.14.0；PR #10 保留独立 review。
  **Decision:** 保留 DeBank RPC crate、注册及 build/release workflow；目标 upstream Cargo.lock 为基底，补 fork 依赖，保留现行 Rust 1.96 toolchain。CI 冲突保持现行 upstream workflow 删除策略；node.rs 合并双方 import。
- `[2026-09-08][open] 编译与语义验证` — Reth 状态读取路径变更需验证 DeBank 三个 RPC；T11 gas、ABI、nonce 与历史 fork replay 为回归重点。
  **Decision:** 先跑完整 locked check/build 和适用测试，再提交 PR 镜像、部署隔离常规节点；生产操作交 SRE。
- `[2026-09-08][decided] Reth state provider API 适配` — 上游删除 StateProviderTraitObjWrapper，state_at_block_id 仍返回 StateProviderBox。
  **Decision:** 仿照目标 Reth 的 spawn_with_state_at_block，将 boxed provider 直接交给 StateProviderDatabase；保留两个独立的同一 parent-hash provider 和现有 replay 逻辑。
  **Decision:** 首次 check 复现 E0308：Trace::inspect 新签名只收 StateCacheDb，不能接受 StateDiffTraceDB。对照旧/新 Reth 实现确认均为 evm_with_env_and_inspector → transact → from_evm_err，因此在 DeBank wrapper 路径直接使用同一 EVM factory 调用，保留 diff_db.commit；不移除 state-diff 捕获。
- `[2026-09-08][open] lihe-dev 部署资源不足` — 物理 93.19GiB；已有限额容器合计 93GiB，另有无内存/CPU 限额的 morph-archive-trace-test（实用 8.218GiB），不满足总限额加 8GiB 的上机规则。
  **Decision:** 不新增测试容器、不停止其他任务；先完成本地验证和 PR 镜像，并准备可审核的部署方案。现有 Tempo 节点 amd64-0a36d6f 同时被 T11 pipeline 使用，不能直接换镜像。

- `[2026-09-08][done] 完整 locked check` — cargo check --workspace --locked exit=0，2m06s。
  **Done:** Cargo.lock 保留全部 upstream package，只增加 debank-rpc/md-5 与 tempo-node dependency edge；改动 Rust 文件按 nightly rustfmt 校验。
