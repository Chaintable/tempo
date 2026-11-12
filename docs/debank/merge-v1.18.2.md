# Tempo v1.18.2 合并验证

## 1. 更新内容与升级收益

上游建议 RPC 节点更新 v1.18.2。本次是从生产同线 v1.17.0 升级的维护版本，包含 EVM 调用输入加固、trie iterator seek 修复、DKG/finalization/restart 可靠性及依赖安全更新；不引入新的网络激活时间。

目标 [tempo/v1.18.2](https://github.com/tempoxyz/tempo/releases/tag/tempo/v1.18.2) 为 `9d605438d6b05f58376db8b0f00ba9d2c37abf0e`，fork 基线为 `6cfc3dc8bb4810ac31e90ac589641be7d321c887`。本轮通过 [PR #19](https://github.com/Chaintable/tempo/pull/19) 保留 upstream 历史，最终测试代码为 `f87ec7e78a7b41b5f687b84409c16e43a0f2182d`。

## 2. 合并冲突与保留的 fork 适配

合并处理 46 个冲突文件，其中 23 个 workflow 文件。保留 CT 的双架构构建和 release 流程，以及以下业务适配：

- `trace_debankBlock`、`pre_traceMany`、`eth_multiCall` 注册及既有 JSON/RLP 输出协议，适配目标 release 锁定的 EVM2/inspectors。
- 顺序模拟提交前笔交易保留的状态变化，独立 multicall 不提交前笔 call 状态。
- AA 合成根、多顶层 call、静态 Ethereum precompile 与动态 Tempo native 合约过滤，以及无活动调用帧协议日志的 receipt 对账。
- Native StorageActions 与调用帧关联、失败调用写入标记、普通 EVM SSTORE 标记；完整 replay/canonical state diff、receipt root、gas 校验。
- Cargo.lock 使用目标 upstream 基底，仅增加 fork 依赖及必要的版本消歧。

## 3. 隔离部署

在独立测试机使用生产快照恢复的任务专属 100GiB ext4 数据卷，首启前移出 P2P 身份文件。仅运行 node；生产投递由独立 ETL 拉 RPC 完成，本次不启动 ETL/Kafka/S3 上传器。HTTP/WS/metrics 仅绑定 localhost，不挂载生产凭据并禁用 IMDS 凭据回退。

最终镜像 `public.ecr.aws/b2h7a5c4/chaintable/tempo:f87ec7e7-amd64`，amd64 digest `sha256:72511540898f0425e8b57a8164621920a5ca41b9966e2d4c78bdc40d29b5d42d`。沿用快照 storage_v1 archive；2026-11-12 06:31:50Z 计划内切换后 clientVersion 确认为 `tempo/v1.18.2-f87ec7e/x86_64-unknown-linux-gnu`。

测试 Compose 全文（部署时使用本节固定镜像设置 `TEMPO_IMAGE`）：

```yaml
name: tempo-upstream-v1182
services:
  node:
    image: ${TEMPO_IMAGE:?Set the tested PR image}
    container_name: tempo-upstream-v1182-node
    entrypoint:
      - /usr/local/bin/tempo
      - node
      - --chain=mainnet
      - --datadir=/var/data
      - --follow=wss://rpc.tempo.xyz
      - --log.stdout.filter=info
      - --log.file.directory=/var/data/logs
      - --log.file.max-size=100
      - --log.file.max-files=3
      - --http
      - --http.addr=0.0.0.0
      - --http.port=8545
      - --http.api=all
      - --ws
      - --ws.addr=0.0.0.0
      - --ws.port=8546
      - --ws.api=all
      - --metrics=0.0.0.0:9001
      - --port=30303
      - --discovery.port=30303
    environment:
      AWS_EC2_METADATA_DISABLED: "true"
      AWS_SHARED_CREDENTIALS_FILE: /nonexistent
      AWS_CONFIG_FILE: /nonexistent
    user: "0:0"
    volumes:
      - ./writer:/var/data
    ports:
      - "127.0.0.1:18645:8545"
      - "127.0.0.1:18646:8546"
      - "127.0.0.1:29268:9001"
      - "31403:30303/tcp"
      - "31403:30303/udp"
    mem_limit: 8589934592
    cpus: 4
    stop_grace_period: 5m
    restart: "on-failure:3"
    logging:
      driver: json-file
      options:
        max-size: "10m"
        max-file: "3"
    networks: [tempo-upstream]
networks:
  tempo-upstream:
    ipam:
      config:
        - subnet: 10.42.19.0/24
```

## 4. 验证结果

| 项目 | 结果 |
|---|---|
| 完整 tempo 构建 | `cargo build --locked --bin tempo` 通过 |
| debank-rpc | 36 passed，含真实 AA 多调用、parity 节点、native/SSTORE 和帧外协议日志 |
| tempo-evm / tempo-precompiles | 341 / 1071 passed，precompiles 1 个上游既有 ignored |
| nightly fmt / Clippy | 通过；Clippy 保留 2 条原有 needless-borrow 提示 |
| 最终代码双架构 PR 镜像 | CI 42498791526 amd64/arm64/manifest 全通过 |
| 快照后真实同步 | 1,484,386 块执行、Merkle 和历史索引完成，覆盖 T13 激活 |
| 最终 f87 镜像 1000 不同 canonical 块 | hash/parentHash/stateRoot/receiptsRoot/transactionsRoot 全部匹配官方 |
| 完整 blockfile/header/validation | 1000/1000 对生产落盘对象匹配 |
| state_diff 完整 RLP 六字段 | 1000/1000 匹配，归档后独立 RLP 库再次复核通过 |
| 最终镜像同步 | 连续 5 次 syncing=false，lag 0/0/0/0/1 |
| 最终镜像容器状态 | 无自动重启或 OOM，最终观察日志无 ERROR/panic/FATAL |

1000 块覆盖原冻结的全部 100 个复测样本，以及新增追块段、T13 边界、epoch 边界、繁忙近期和近 head 各 180 块。共 478 个非空块、723 笔交易，其中 575 笔 AA。完整采集/比较在 2026-11-12 06:36:16Z–06:38:17Z 完成，墙钟 121.05 秒，不含准备和归档时间。

基准只读来自生产 `4217/f490914c` 的 blockfile/validation/header/stateDiff 对象，逐块向官方 RPC 校验 canonical 身份。生产当前 node 为 `6cfc3dc8`，ETL 为 `amd64-v0.1.48`；历史对象不带 producer-build metadata，不能认定每个历史对象均由当前 node 版本生成。完整字段协议与对象映射已经核对；本轮不要求生产重建历史状态。比较仅忽略 process_start_timestamp，storage_contracts 与 RLP 内 map 字段按无序集合比较，未以 stateRoot 代替 state diff 内容。

Claude Opus 5.5 对 f87 独立只读审查结论为代码层面 Approve，无确定 P0/P1；非阻塞意见及其复核归档在本地任务证据中。审查不是完整上游共识审计。

**模拟 RPC 覆盖限制：** 最终 f87 的 pre/multi 完整回归未运行。旧 70c511a 全轮 pre 为 56/104 匹配，b0 后五个定向块 pre/multi 均匹配；旧 pre 的其余差异尚未逐项由 f87 完整回归闭环。旧 multicall 全轮 104/104 匹配。1000 块 trace 成功不代替这些模拟接口的最终验证。用户在知悉此缺口后明确授权合并、release 与测试资源清理；不把该授权写成测试通过。

## 5. 后续事项与部署注意

- 本版不维护 T4 前非空 subblock 的历史执行；需要此类历史 trace 时使用保留支持的历史 CT 版本和兼容数据副本。
- 部署需显式使用已验证的 `--follow=wss://rpc.tempo.xyz`。auto 的旧默认域名在测试环境无法解析。
- Halt 错误码保持，但文本由旧 TempoHaltReason 的 Debug 改成 EVM2 stop 文本；消费者是否依赖文本尚未验证。
- 部分 trace 失败会重复记录 error 日志；转换前日志用于保留具体执行错误。
- 最终模拟 RPC 全轮覆盖仍未完成；ETL/Kafka 的实际投递及生产 rollout 不在本次测试范围。
