# Tempo 待办事项

- `[2026-09-08][decided] 动态预编译storage-change漏标继续暂缓` — 本地TIP20转账复现既有问题；升级方案明确排除，用户此前已暂缓。
  **Decision:** 曾草拟检测修复/回调测试，核对排除清单后全部撤回，未提交或推送。实验33项结果不计入最终候选；保留原32项测试与原始复现证据target/v114-validation/writer-smoke.json。

- `[2026-09-08][decided] PR #10 整合正式 v1.14/T11` — 普通merge origin/debank=24340a99；保留PR #10完整executor重放、native inspector、receipt/gas/canonical diff校验。
  **Decision:** 12处冲突以正式v1.14协议实现和PR #10导出链路为边界解决；删除旧Reth StateProviderTraitObjWrapper，直接用boxed provider。官方SDK引入属于后续Leafage独立PR。
- `[2026-09-08][done] 整合后本地首轮回归` — Rust/Cargo1.96.1，locked debank-rpc/tempo-node check成功。
  **Done:** debank-rpc32、consensus290、evm101、precompiles1006，共1429 passed/0 failed/1 upstream ignored；日志在target/v114-validation/。全workspace/build、CI、oracle及新pipeline组合仍待完成，不复用旧版本24h结果。

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

- `[2026-09-08][done] 本地 binary 与 T11 RPC fixture` — binary build 2m26s；RPC/EVM/precompile 单测1126 passed、0 failed、1 upstream ignored。
  **Done:** dev RPC 冒烟、真实 TIP-20 转账重放、T10/T11 strict ABI 与24 gas差值验证通过；两个本地节点已停止。证据在 target/tempo-v1140-smoke-*.json，并复制到 pipeline runs/tempo/v1.14.0/。
- `[2026-09-08][open] 镜像与主网验证` — draft PR #11，head 3aae029410，双架构 run 34174121220 进行中；consensus 单测另跑以验证 gossip/follow 变更。
  **Decision:** 测试资源仍待安排；独立6GiB/4CPU Compose 已通过 config 校验，快照 volume --plan 91.8h/60GiB 合格，未创建卷或容器。

- `[2026-09-08][done] Consensus 回归` — 290 passed、0 failed、0 ignored，65.06s。
  **Done:** 本地单测总计1416 passed；volume --plan 实测91.8h/60GiB合格。双架构镜像与远程测试仍待完成。

- `[2026-09-08][done] 双架构 CI 与 ECR manifest` — run 34174121220 三个 job 全 success，3aae029 manifest 包含正确的 linux/amd64 和 linux/arm64 digest。
  **Done:** digest、Compose 全文和1416项单测/本地 RPC证据已记入 docs/debank/merge-v1.14.0.md；后续提交仅报告，测试仍使用经核验的3aae029源码镜像。
- `[2026-09-08][open] 等待测试机预算例外确认` — 已展示独立6GiB/4CPU部署方案并向用户请求内存预算例外，尚未得到答复。
  **Decision:** 不建卷、不启动远程容器；不代合 PR 或发 release。

- `[2026-09-08][done] v1.14.0 测试部署与历史 RPC 对账` — 用户批准本轮测试机内存预算例外。
  **Done:** 独立节点 01:29:25Z 启动，60GiB 卷 vol-0582fcb2fe3cf922a，初始高度 37,909,435；历史20块 hash/根、5非空块完整 trace/state diff 匹配。
  **Decision:** pre_traceMany 的模拟交易 hash 来自 B256::random()，比较时仅在此方法去除此字段；近 head 检查仍待追块完成。

- `[2026-09-08][done] v1.14.0 主网验证完成` — 约14.5分钟追平快照后约59.8万块，0 restart/OOM。
  **Done:** 三段60个hash/各根、15个完整trace/state_diff、6组模拟RPC全部匹配；生产lag1、官方lag2–3，最终报告见 docs/debank/merge-v1.14.0.md。
  **Decision:** 提交报告并转为可review PR；用户亲自merge后再构建release，PR #10与真实T11边界验证保持独立后续。
