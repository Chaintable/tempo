# Tempo v1.14.0 upstream 合并验证

日期：2026-09-08。结论：本轮 upstream 合并验证通过，等待用户 review/merge [PR #11](https://github.com/Chaintable/tempo/pull/11)。本地1416项测试、双架构 CI、主网追块、hash 与自定义 RPC 对账均通过；release 尚未创建。

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

用户已明确批准本轮按实时内存余量继续，现场 available 70GiB。独立节点于 2026-09-08 01:29:25Z 在 lihe-dev 启动，6GiB / 4 CPU；未替换或停止供另一套 T11 pipeline 使用的现有 Tempo 节点。测试卷 vol-0582fcb2fe3cf922a 来自生产周期快照 snap-00c445d5bb1442d9c（2026-09-04 05:02 UTC，60GiB），初始化速率 300MiB/s，AWS 已确认初始化 100%，未启用 FSR。新卷独立 UUID 为 bf156665-a228-4d02-8fd4-4644ef0ce51f；P2P 文件移入本 run 的 identity-backup，启动时生成新身份。初始高度 37,909,435，archive 与已有 storage_v2=false 设置得到保留，T11=1789048800 已加载。

部署 Compose 见附录，已通过 config 校验；配置已按镜像 digest 启动。未对生产或其他测试任务做变更。

## 4. 测试结果

- merge 冲突已解决；本次改动的 Rust/Cargo 文件 diff --check 通过。上游 TIP 文档及存量 fork 文档的 whitespace 问题保留原状。
- 首次 check 定位 Trace::inspect API 不兼容，已按第 2 节适配。
- `cargo check --workspace --locked` 通过（2m06s，含 tempo、node、EVM、e2e）；`cargo build --locked --bin tempo` 通过（2m26s）。
- `cargo test --locked -p debank-rpc -p tempo-precompiles -p tempo-evm --lib -- --test-threads=2`：20 + 1005 + 101 = 1126 passed，0 failed；唯一 ignored 是上游原有 TIP-1016 refund 预期差异用例，未改测试或跳过逻辑。
- 本地独立 dev 节点：trace_debankBlock（genesis/空块/非空 TIP-20 转账）、eth_multiCall、pre_traceMany 冒烟通过；转账 block 0x40 的 header hash 匹配 receipt，2 events 对应 2 receipt logs，1 trace、1125-byte state diff。
- T10/T11+ 两份 dev genesis 对照：带32-byte尾随数据的 decimals() 由成功变为 revert；规范调用 pre_traceMany gas=271170→271194，差24，符合输入每 word 6→30 gas。两个测试节点均已停止。
- `cargo test --locked -p tempo-consensus --lib -- --test-threads=2`：290 passed、0 failed，65.06s；本地单测累计1416 passed。双架构 CI 已通过，PR #11 / run 34174121220。
- 01:37:29Z 已执行快照后596,954块至38,506,389；01:39:21Z MerkleExecute 完成，状态根校验通过；01:43:54Z Finish 至38,507,901并进入实时跟随，约14.5分钟追平。连续5次同步采样均syncing=false，生产lag=1、官方lag=2–3；三个端点chainId均4217。
- 新代码导入段37,909,436–455与近head段38,507,800–819：各20块hash及四项根全部匹配；各5个非空块完整trace/事件/state_diff匹配。trace抽样分别为37,909,437/442/447/450/454，以及38,507,801/802/803/805/807。
- 主网历史段 37,909,000–37,909,019：20 块 hash、parentHash、stateRoot、receiptsRoot、transactionsRoot 全匹配。非空块 37,909,028 / 029 / 035 / 038 / 042 的 trace、事件与完整 state_diff 全匹配。三段合计60块hash/各根、15块非空trace/state_diff；eth_multiCall/pre_traceMany每段各一次，6组响应全匹配。
- 对账规则：trace_debankBlock 仅去除 process_start_timestamp，并排序 storage_contracts 和 RLP 内无序集合；BlockStorageDiff 六个字段全部保留，不以 header.stateRoot 替代 diff。eth_multiCall 去除 timeCost；pre_traceMany 去除源码 pre.rs:140 明确随机生成的模拟 transactionHash，其余字段全部比较。
- 运行状态：0 restart、OOM=false，启动以来无panic/FATAL/ERROR/invalid-block/state-root-mismatch；收尾内存672MiB / 6GiB，CPU约0.34核 / 4核，卷available7.5GiB，主机available69GiB。采样仍保留在独立测试目录。
- T11 主网尚未激活；本轮覆盖历史主网与T10/T11+本地fixture，真实激活边界仍须在9月10日观察。

## 5. 待跟进事项

[PR #10](https://github.com/Chaintable/tempo/pull/10) 的完整 executor 生命周期、AA trace/event/state-diff 修复保持独立 review，未并入本分支。此次保持生产分支既有 RPC 行为，不把其已知缺陷误算为 v1.14.0 新增问题；也不宣称 writer-to-Leafage 全链路通过。PR #10 所述 Leafage storage-wipe consumer 依赖需另行验证。

用户 review/merge PR 后才进入 release 镜像构建与交付，生产切换由 SRE 执行。生产需要在T11激活前完成升级；本PR未包含PR #10的独立writer正确性修复。

## 已验证的 PR 镜像

源码 commit `3aae0294107d4d4323c47a0e5244733427161e00`；[CI run 34174121220](https://github.com/Chaintable/tempo/actions/runs/34174121220) 的 amd64、arm64 与 manifest job 全部 success，ECR manifest 平台和 digest 已对账。后续报告提交只改 docs，以下镜像仍对应相同 Rust/Cargo/Dockerfile 源码。

- Registry/repo：`294354037686.dkr.ecr.ap-northeast-1.amazonaws.com/blockchain/tempo`
- `3aae029` manifest：`sha256:fbb035a92bbb1e390df6b857c96aabd0f371c52d75393927706a3ec89d6fc408`
- `amd64-3aae029`：`sha256:f973791d11c936ea68d09084e9f60dc99f408d1ae2459773740c9d9d87ad5015`
- `arm64-3aae029`：`sha256:5e7eee5507b47d3204ee28017b9fffc23e57d08b3bf9c6bab6406b55ebb6a462`

## 附录：实际运行的独立测试 Compose

```yaml
name: tempo-upstream-v1140
services:
  node:
    image: 294354037686.dkr.ecr.ap-northeast-1.amazonaws.com/blockchain/tempo:amd64-3aae029@sha256:f973791d11c936ea68d09084e9f60dc99f408d1ae2459773740c9d9d87ad5015
    container_name: tempo-upstream-v1140-node
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
      - /opt/app/tempo/writer_merge_v1.14.0/data:/var/data
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
