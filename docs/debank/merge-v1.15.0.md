# Tempo v1.15.0 upstream 合并验证

日期：2026-09-25，2026-09-26 更新。合并分支 `merge-v1.15.0`，审阅入口 [PR #14](https://github.com/Chaintable/tempo/pull/14)。主网快照追块、区块哈希和三个自定义 RPC 对账通过，等待用户 review/merge；release 尚未创建。

## 1. 升级内容与必要性

[上游 v1.15.0](https://github.com/tempoxyz/tempo/releases/tag/v1.15.0) 将 validator 和 RPC node 的升级优先级均列为 **Medium**，属于改善节点可靠性、同步稳定性和性能的维护版本；未宣布新的主网激活期限。生产 `production/blockchain-tempo` 当前运行 v1.14.0-debank，因此建议按常规维护窗口升级，无需按硬分叉截止时间紧急切换。

主要更新包括 Commonware v2026.9.0、validator 从快照启动时的网络身份与本地 DKG 状态检查、签名及授权编码和预编译执行的分配优化、减少重复哈希和无效 txpool 检查。新增的 finalized-block P2P 传播可降低 RPC node 对单一上游连接的依赖，但 `--consensus.devp2p.finalizations` 默认关闭；本轮保持默认值。

## 2. Merge 冲突与影响面

从 Chaintable `main=c7230be79608d9d0965c85c18723f0a5f3b79481` 合入上游 `v1.15.0=464e51994b541b37cb875d47747e38bb67e3692a`。当前 main 已包含 [PR #10](https://github.com/Chaintable/tempo/pull/10)、[#12](https://github.com/Chaintable/tempo/pull/12)、[#13](https://github.com/Chaintable/tempo/pull/13) 的 writer/RPC 改动。冲突涉及 20 个 GitHub workflow、根 Cargo.toml 和 Cargo.lock；没有需要人工选择业务语义的冲突。

保留的 fork patch 是 `debank-rpc` crate、node 注册和 EVM storage action 访问入口，供 `trace_debankBlock`、`eth_multiCall`、`pre_traceMany` 及 writer 下游消费。`handler.rs` 保留逐交易构造 T1+ key authorization gas 参数的修复，避免跨 fork 的进程级缓存影响历史执行。另保留 Rust 1.96 构建设置及 Chaintable 的公共 ECR 构建和发布 workflow。Cargo.lock 以上游 v1.15.0 为基底，补入 fork 和 2026-09-26 历史兼容修复所需依赖。自动合并的 EVM、node、handler 路径已由编译、单测和主网 RPC 对账覆盖。

## 3. 部署情况

生产只读实测：`production/blockchain-tempo` 的 hybrid-0 与 hybrid-seed-0 均运行 v1.14.0-debank，Ready 且无重启；hybrid-0 的 `eth_syncing=false`，chainId=4217。本轮只在 lihe-dev 部署独立普通 node，不运行 ETL sidecar，不配置 S3、Kafka 或 etcd，也不接入生产投递。生产对账 RPC 仅只读调用。

测试卷 `vol-06e9836f251e55950` 从 2026-09-21 完成的生产快照 `snap-0b3ddc3bcb9905f97` 创建，为本 run 独立的 80GiB gp3 卷，挂载 `/opt/apps/tempo-merge-v1150/writer`。新卷 UUID 为 `fb6c1a82-533f-44a3-bfb7-7ac826158be9`；快照中的 P2P 身份文件移入本 run 的 `identity-backup`，首次启动生成新身份。测试容器于 2026-09-25 03:16:44 UTC 启动，6GiB / 4 CPU，初始执行高度 40,497,612，chainId=4217。镜像 `public.ecr.aws/b2h7a5c4/chaintable/tempo-writer:c67046ca` 的 amd64/arm64 CI [run 36088652540](https://github.com/Chaintable/tempo/actions/runs/36088652540) 已通过，公开 manifest digest 为 `sha256:28138252d13109ea2811f702a3b45ba21770893a88d2f85344e2b2a57a4f40ce`。

实际运行的独立 Compose：

```yaml
name: tempo-upstream-v1150
services:
  node:
    image: public.ecr.aws/b2h7a5c4/chaintable/tempo-writer:c67046ca
    container_name: tempo-upstream-v1150-node
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
    restart: unless-stopped
    stop_grace_period: 5m
    mem_limit: 6g
    cpus: 4
    logging:
      driver: json-file
      options:
        max-size: "100m"
        max-file: "5"
    volumes:
      - /opt/apps/tempo-merge-v1150/writer:/var/data
    ports:
      - "127.0.0.1:18675:8545"
      - "127.0.0.1:18676:8546"
      - "31415:30303/tcp"
      - "31415:30303/udp"
    networks: [tempo-upstream]
networks:
  tempo-upstream:
    ipam:
      config:
        - subnet: 10.99.75.0/24
```

## 4. 部署后测试情况

- `cargo check -p debank-rpc -p tempo-node -p tempo` 与 `cargo build --locked --bin tempo` 通过。
- `cargo test --locked -p debank-rpc`：34 passed；`cargo test --locked -p tempo-node --lib -- --test-threads=2`：27 passed；`tempo-consensus`、`tempo-evm`、`tempo-precompiles` 的 combined lib test 退出码 0，其中 precompiles 1016 passed、1 个上游既有 ignored。nightly rustfmt check 与 diff --check 通过。
- 双架构 PR CI 全部通过，测试机已拉取并启动对应 amd64 镜像。
- 历史区间 40,497,000–40,497,019：与官方 RPC 的 20 块 hash、parentHash、stateRoot、receiptsRoot、transactionsRoot 全部匹配；5 个非空块 40,497,004/005/008/010/011 与当前 Chaintable main 基线的完整 `trace_debankBlock`、events、state_diff 匹配；40,497,011 的 `eth_multiCall`、`pre_traceMany` 匹配。
- 从快照 40,497,612 执行至 41,118,351 后，MerkleExecute 及全部索引阶段完成；03:36:34 UTC Finish 追至 41,120,422，开始跟随新区块。快照后区间 40,497,613–632、近 head 区间 41,120,440–459：各 20 块 hash/四项根均与官方 RPC 匹配，也与 Chaintable main 基线匹配。各区间 5 个非空块的完整 `trace_debankBlock`/events/state_diff，以及各一次 `eth_multiCall`、`pre_traceMany`，均与基线匹配。三段合计 60 块 hash/根、15 个非空块 trace/state_diff、6 次模拟 RPC。
- 03:37:00–03:39:01 UTC 连续 5 次采样均 `eth_syncing=false`，与同次采样的官方 RPC 相差 1 块，26 peers；从启动到首次追平约 20 分钟。容器 0 restart、未 OOM，启动后日志未发现 panic/FATAL/ERROR、invalid block 或 state root mismatch。收尾内存约 2.5GiB / 6GiB，卷剩余约 23GiB。
- 历史索引未完成时，快照后首块 `trace_debankBlock(40497613)` 曾返回 `ExpiringNonceReplay`，当时 `eth_syncing` 的 Finish 和历史索引仍停在快照高度；03:36:34 UTC Finish 后同块复测成功，完整 trace/state_diff 与基线一致。该次失败属未完成同步时的查询结果，不作为同步完成后的 RPC 回归。
- RPC 对账仅去除节点本地 `process_start_timestamp`、`eth_multiCall.timeCost` 和模拟交易随机 `transactionHash`，并排序无序的 storage 集合；state_diff 内容及其余字段完整比较。参照节点是 lihe-dev 上运行当前 Chaintable main 的 v1.14.0-ct.2；生产 v1.14.0-debank 尚未包含 PR #10/#13 的 writer 修复，不能直接作为这些 RPC 的逐字段基线。

## 5. 待跟进事项

用户 review/merge [PR #14](https://github.com/Chaintable/tempo/pull/14) 后才进入 release 镜像及 metadata 台账步骤；生产切换由 SRE 执行。本轮测试卷和容器保留供审阅，清理另行确认。

## 6. T4 前 subblock 历史重放兼容修复（2026-09-26）

上游 [#7449](https://github.com/tempoxyz/tempo/pull/7449) 无条件拒绝 `nonceKey` 首字节为 `0x5b` 的交易，同时删除了旧的区块分段、手续费失败处理和元数据校验。此次补丁仅恢复区块执行所需的 T4 前路径；T4 起在区块执行器和 Revm 校验器继续拒绝此类交易。没有恢复共识层的实时 subblock 生产、P2P 转发或 payload 注入。

本地补丁验证：`tempo-evm` 104 项、`tempo-revm` 198 项单测通过；`tempo-node --all-targets` 编译通过；nightly rustfmt 与 `git diff --check` 通过。新增 T3 接受、T4/T11 拒绝的校验测试，并保留 T4 gas 计算及模拟交易豁免测试。交叉 review 后将 RPC 模拟中的 `subblock_transaction` 恢复为旧版的 `false`，避免把普通模拟请求误当作历史 subblock；`tempo-node`、`tempo-alloy` 全 targets 再次编译通过。旧版仅对未最终确认 payload 传入 validator set，新版上游已删除该字段，本补丁未恢复这条共识实时验证路径；已确认区块的历史重放原本也不传 validator set。上文测试机镜像 `c67046ca` 构建于此补丁之前，尚未代表补丁的远端重放结果。两网历史索引未发现 T4 前 `0x5b` 交易；本次仍未从 genesis 全量重放两网，因此不能以本地单测宣称全历史状态根已经逐块核对。
