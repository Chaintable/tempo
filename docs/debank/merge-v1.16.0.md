# Tempo v1.16.0 upstream 合并验证

日期：2026-10-06。合并分支 `merge-v1.16.0`，审阅入口 [PR #16](https://github.com/Chaintable/tempo/pull/16)。同一镜像要同时服务通用节点和常规节点，两条路径的主网对账都已通过，等待用户 review/merge；release 尚未创建。

## 1. 升级内容与必要性

[上游 v1.16.0](https://github.com/tempoxyz/tempo/releases/tag/v1.16.0) 是 **T12 网络升级的必需版本**：主网激活时间 2026-10-13 14:00 UTC（`1791900000`），validator 和 RPC node 的升级优先级均为 **High**，旧版本在激活后可能掉出同步。生产两套 workload 都必须在激活前切换。中间的 [v1.15.1](https://github.com/tempoxyz/tempo/releases/tag/v1.15.1) 只更新 testnet 网络身份，对主网无影响。

T12 包含 TIP-1106（expiring 交易可用任意 uint64 nonce）、TIP-1095（受限收款方 TIP-20 可为 MPP 通道注资）、TIP-1116（native 合约接受末尾多余 calldata）、TIP-1088（DEX 报价与 swap 用同一套撮合取整，去掉多余 liquidity 写入）、TIP-1006（TIP-20 `burnAt`）。运维侧改动：finalization gossip 默认开启（#7950）、AA 2D 交易池刷新 base fee（#8065）、T12 前 expiring-nonce 错误分类修正（#8066）、snapshot replay 的 delivery batch 重置（#8091）。

T12 激活后两处行为会改变 DeBank 输出，属于对齐上游的预期变化：native 合约写 storage 前剩余 gas 必须大于 2300，否则 OOG（影响 `pre_traceMany` / `eth_multiCall` 模拟结果和 trace）；DEX tick liquidity 存储总量不再更新（影响 `state_diff`）。

## 2. Merge 冲突与影响面

从 Chaintable `main=dca132f388614a4fbfe58969b97cb0b178d8ca3b`（v1.15.0-ct.1）合入上游 `v1.16.0=74b69155ebaf6178bf7e3d59d18a2440164302d9`，merge commit `b266ebb34b6fc0e2e25384fb981a3827fd0aa537`。上游 v1.15.0 与 v1.16.0 在不同 release 分支上，v1.15.0 不是 v1.16.0 的祖先，因此以 v1.15.0 为显式 base 做三方合并，结果等于 v1.16.0 加上 fork patch（`v1.15.0..main`）。已逐文件核对：fork patch 涉及的文件与 main 上的 patch 一致，其余文件与 v1.16.0 一致。

冲突只在 GitHub workflow、根 `Cargo.toml`、`crates/evm/src/block.rs` 的 import，以及上游把 REVM 测试移到 `crates/revm/src/evm/tests.rs` 造成的位置冲突。没有需要人工裁量的决策点。

保留的 fork patch：

- `debank-rpc` crate 及 node 注册：`trace_debankBlock`（通用节点 ETL 拉取）、`pre_traceMany` 与 `eth_multiCall`（常规节点下游直调）。
- EVM storage action 访问入口、Tempo precompile 写入按账户归属（#15），供 trace 和 state diff 使用。
- T4 前 subblock 历史重放兼容（#14）：`TempoHaltReason`、subblock 交易 `tx` 字段与费用校验、T4 前不拒绝 `0x5b` nonce key。
- `handler.rs` 按交易构造 T1+ key authorization gas 参数，不用进程级缓存。
- Rust 1.96 构建设置；Chaintable 公共 ECR 的 `build.yml` / `release.yml`（上游 workflow 不保留，含 v1.16.0 新增的三个）。

为在 v1.16.0 上编译所做的适配：

- reth `75da5280` 要求 `StateProviderDatabase` 的参数实现 `EvmStateProvider`。`trace_debankBlock` 的两处调用改为 `into_evm_state_provider()` 包装，该包装只转发四个读接口，读取结果不变。
- 上游新增 `impl Clone for TempoTxResult`，补上 fork 的 `tx` 字段。
- 两个上游新测试补 fork 类型：TIP-20 测试 helper 返回 `ExecutionResult<TempoHaltReason>`，prewarming 测试补 `subblock_fee_recipients`。

`Cargo.lock` 以 v1.16.0 为基底，只新增 `debank-rpc`、`md-5` 两个包和 `tempo-node`、`tempo-revm` 各一条依赖（+52 行）；`alloy-rpc-types-trace` 随上游升到 2.5.0 依赖族。

## 3. 部署情况

生产 `production/blockchain-tempo` 只读实测（2026-10-06）：

| 路径 | workload | 镜像 | 下游 |
|---|---|---|---|
| 通用节点 | `nodex-node-f490914c`、`nodex-node-seed` | `public.ecr.aws/b2h7a5c4/chaintable/tempo-writer:v1.15.0-ct.1` | `etl-f490914c` 拉 `trace_debankBlock` → Kafka/S3 → leafage-evm |
| 常规节点 | `hybrid`、`hybrid-seed` | `294354037686.dkr.ecr.ap-northeast-1.amazonaws.com/blockchain/tempo:v1.14.0-debank` | jrpcx 直通，下游调 `pre_traceMany` / `eth_multiCall` |

两种 node 进程都不内嵌投递，测试只运行 node，不接 ETL、Kafka、S3、etcd。常规节点切换后改用本仓库 CI 产出的 `public.ecr.aws/b2h7a5c4/chaintable/tempo` 镜像（与 `tempo-writer` 同一 digest）。

测试部署在 lihe-dev：卷 `vol-0b6aaec1e82706923` 从 2026-10-02 生产 hybrid-seed-0 快照 `snap-0e5a924c7ed4821d5` 创建（gp3，初始化 300MiB/s），与旧测试卷 UUID 相同，挂载前已改为新 UUID；快照里的 `discovery-secret`、`known-peers.json` 及其备份已删除，首启生成新身份。按快照大小建的 60GiB 卷追平后只剩 1.1G，已在线扩到 100GiB。PR 镜像 `tempo-writer:b266ebb3`（[CI run 37417735492](https://github.com/Chaintable/tempo/actions/runs/37417735492)，amd64 + arm64，index digest `sha256:14238046c2673fe3e696bf43da46aab08a45f790b3f0f6502cfb8a3a8d42c511`，`tempo:b266ebb3` 同 digest）。

实际运行的 Compose（`/opt/app/tempo/writer_merge_v1.16.0/compose.yml`）：

```yaml
name: tempo-upstream-v1160
services:
  node:
    image: public.ecr.aws/b2h7a5c4/chaintable/tempo-writer:b266ebb3
    container_name: tempo-upstream-v1160-node
    entrypoint: ["/usr/local/bin/tempo"]
    command:
      - node
      - --chain=mainnet
      - --datadir=/var/data
      - --follow=auto
      - --log.stdout.filter=info
      - --http
      - --http.addr=0.0.0.0
      - --http.port=8545
      - --http.api=all
      - --ws
      - --ws.addr=0.0.0.0
      - --ws.port=8546
      - --ws.api=all
      - --port=30303
      - --discovery.port=30303
    user: "0:0"
    restart: on-failure:5
    stop_grace_period: 5m
    mem_limit: 6g
    cpus: 4
    logging:
      driver: json-file
      options:
        max-size: "100m"
        max-file: "5"
    volumes:
      - /opt/app/tempo/writer_merge_v1.16.0/data:/var/data
    ports:
      - "127.0.0.1:18645:8545"
      - "127.0.0.1:18646:8546"
      - "31403:30303/tcp"
      - "31403:30303/udp"
    networks: [tempo-upstream]
networks:
  tempo-upstream:
    ipam:
      config:
        - subnet: 10.42.18.0/24
```

## 4. 部署后测试情况

**本地**：`cargo check --locked`（`debank-rpc`、`tempo-node`、`tempo`）和全 workspace `--all-targets`（除 `tempo-xtask`，见第 5 节）通过；nightly rustfmt、`git diff --check` 通过；单测全部通过：`debank-rpc` 36、`tempo-evm` 103、`tempo-revm` 200、`tempo-primitives` 236、`tempo-precompiles` 1065（1 个上游 ignored）、`tempo-node` 46、`tempo-payload-builder` 26。clippy `-D warnings` 除第 5 节所列已有提示外通过。

**追块**：容器 2026-10-06 05:35:47 UTC 启动，版本日志 `Tempo 1.16.0 (b266ebb)`。快照高度 42,209,505，05:51:53 追到 42,835,103（625,598 块，约 16 分钟），`eth_syncing=false`；之后三次采样均落后官方 RPC 2 块。0 次重启、无 OOM，内存约 1.7GiB / 6GiB；30 分钟内无 ERROR 或 panic，3 条 WARN：启动时的 storage settings 沿用快照数据库设置（05:35:48）和 consensus marshal floor 未更新（05:36:02），以及追块结束、转入实时跟块时的 marshal set_floor（05:51:49）。

**块 hash**：快照末尾 42,209,486–505、追块起点 42,209,506–525、近链头 42,835,092–111 共 60 块，hash、parentHash、stateRoot、receiptsRoot、transactionsRoot 与官方 RPC 全部一致。

**通用节点路径（`trace_debankBlock`，对比生产 nodex-node）**：追块段 5 块、近链头 5 块、历史段（40,497,004–40,497,011，precompile 写入样本）5 块，以及追块段交易数最多的 6 块（20/14/13/13/13/12 笔），共 21 块。剔除 `process_start_timestamp`、对无序集合排序后，完整输出（含 `block_file` 和六字段 `state_diff`）全部一致。

**常规节点路径（`pre_traceMany` / `eth_multiCall`，对比生产 nodex-node 和 hybrid）**：

- 上述 21 块，每块把本块真实交易（普通交易与 0x76 AA 交易，带 `calls`、`nonceKey`、`feeToken`、`validBefore`）放在父块状态上执行，两种方法各 21 次；每组另加固定调用（TIP-20 view、state override 返回值、state override revert）各 4 次。78 次调用中，测试节点与 nodex-node 全部一致。
- 交易数最多的 6 块里，同一发送方的 expiring-nonce 交易在同一个 `pre_traceMany` 请求中顺序执行时，从第二笔起返回 `ExpiringNonceReplay`（生产两套节点同样如此，见第 5 节）。因此对这 85 笔交易各单独调用一次 `pre_traceMany`：85 笔都有 trace（共 237 条），三方一致。
- 先用同一组请求比较生产 hybrid（旧 debank 线）与 nodex-node，以确定两条线之间已有的差异：所有样本上两者均无差异。测试节点与 hybrid 也全部一致。

**覆盖范围**：主网在 10-13 之前只有 T12 激活前的块，T12 后的执行路径只由上游单测覆盖，没有主网对账。ETL、Kafka、S3 投递链路不在本次测试范围内（node 不投递，ETL 镜像不变）。

原始响应和汇总在 task_upstream_pipeline `runs/tempo/v1.16.0/mainnet-evidence/`（`summary-step5-20261006.json`、`summary-step5-busy-20261006.json`、`summary-step5-busy-single-20261006.json`）。

## 5. 其他问题

1. **生产常规节点磁盘将满（需 SRE 处理，紧急）**：2026-10-06 只读 `df`，`hybrid-0` 与 `hybrid-seed-0` 的 `/var/data` 均为 59G，已用 54G，**剩 2.4G（96%）**；通用节点 nodex-node 为 79G，剩 25G。测试卷数据目录 `du` 从 10-02 快照的 53G 到 10-06 追平后的 55G，约每天 0.5G，hybrid 可能在 T12 激活前写满。建议在本次镜像切换前或同时扩容。增长速率为粗估，未精确测量。
2. **常规节点换镜像仓库**：hybrid 从私有 ECR `blockchain/tempo:v1.14.0-debank` 切到公共 ECR `chaintable/tempo:v1.16.0-ct.N`，代码线从旧 `debank` 分支换到 `main`（包含 writer correctness、#14、#15 等改动）。本次对账中两条线在 `pre_traceMany` / `eth_multiCall` 输出上没有差异。
3. **`pre_traceMany` 顺序执行 expiring-nonce 交易**：同一请求中同一发送方的多笔 expiring-nonce 交易，从第二笔起返回 `ExpiringNonceReplay`。生产两套节点行为相同，不是本次引入的；下游若批量模拟这类交易，需要逐笔调用。
4. **clippy 1.99 的已有提示**：本机 clippy 1.99 在未改动的 `debank-rpc` 代码上报 5 处：jsonrpsee RPC 宏展开的 `double_must_use`（`lib.rs` 三个 trait），以及 `trace_block.rs:587/597` 的 `needless_borrows_for_generic_args`。jsonrpsee 版本两边都是 0.26.0，与本次合并无关。CI 不跑 clippy。
5. **`tempo-xtask` 测试无法编译**：测试代码 `include_str!` 读取 `.github/workflows/bench.yml`，fork 已删除上游 workflow；`main` 上同样如此，与本次合并无关。
