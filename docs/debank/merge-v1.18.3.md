# Tempo v1.18.3 合并验证

## 1. 更新内容与升级收益

官方对 RPC 节点和验证者均建议更新 v1.18.3。本版是维护更新，不激活新的网络升级。与本 fork 直接相关的是 AA 调用树恢复 v1.17 形态、legacy/AA 模拟修复，以及 follower finalization 输入校验和默认主网 RPC 地址修正；交易池和构块预热也有有效期、费用、内存计数修复。

官方[安装页](https://tempo.xyz/developers/docs/guide/node/installation)只提供 `<version>` 镜像参数，没有独立主网 bundle pin；[网络升级页](https://tempo.xyz/developers/docs/guide/node/network-upgrades)版本表暂列 v1.18.2，T13 要求 v1.17.0。目标选择依据本次最新 stable release，不把文档表滞后解释为必需升级。

目标 [tempo/v1.18.3](https://github.com/tempoxyz/tempo/releases/tag/tempo/v1.18.3) 为 `5c71a4c64f5f206b99554f6e40fe461b1a928967`，fork 工作线基线为 `e9b3af574919dda2addee50fd312e1508aa841f9`。生产仍运行 v1.17.0 的 `6cfc3dc8`，已经包含在该基线；本次未改生产。

## 2. 合并冲突与保留的 fork 适配

上游 release 分支分叉，以 `tempo/v1.18.2` 为显式 base 合并。14 个冲突为 13 个 workflow 和 Cargo.toml。保留 CT build/release 流程；Cargo.lock 使用目标 tag，只补 debank-rpc、md-5 及必要依赖边，上游 1374 个包的版本/source/checksum 保留。

保留的 fork 适配：

- 三个自定义 RPC 的注册、JSON/RLP 协议，以及 pre 的顺序状态提交和 multi 的独立执行语义。
- Native StorageActions 与真实调用帧的写入归属、失败调用写入标记和普通 EVM SSTORE 标记。slot-write 对每个枚举变体显式分类，余额检查与账户配置承诺不误标为 storage slot 写入。
- Replay 的 gas、receipt root 和六字段 state diff 对 canonical changesets 的校验，以及 bundle 字节码回退；不以 stateRoot 代替 diff 内容。

相对目标 tag，EVM、primitives、payload、consensus 和 alloy 核心源码无 fork 差异；StorageActions 辅助函数只读取动作，不改状态转换。测试产品提交为 `d67671f91528ab794cd9d5416818732fa147e033`。

## 3. 隔离部署

测试机使用独立 100GiB ext4 卷，从 2026-11-11 的生产 writer 快照恢复。首启前移出 P2P 身份，限额 8GiB/4CPU，HTTP/WS/metrics 仅监听 localhost。仅运行 node；生产 ETL 拉取 RPC 后上传，本测试未启动 ETL/Kafka/S3 上传器，未注入生产凭据，禁用 IMDS 凭据回退。

实际运行镜像 `public.ecr.aws/b2h7a5c4/chaintable/tempo:d67671f9-amd64`，digest `sha256:98a209552affdbd85db9f9d177a1edee7441a1871e5cfb9f00b9718a941a552c`。同提交多架构 index `sha256:fa60870f917d0b91605b3581ba2c9d93e953332a47e50c7c1a2f20590ab6be24`，tempo-writer 别名一致。

2026-11-13 02:53:09Z 首启，初始高度 48,456,648；沿用 archive/storage_v1，未转换生产存储格式。follow 使用新版默认同域的 `wss://rpc.mainnet.tempo.xyz`，chain ID 和 finalized WebSocket 订阅已验证。

测试 Compose 全文（TEMPO_IMAGE 使用本节固定镜像）：

```yaml
name: tempo-upstream-v1183
services:
  node:
    image: ${TEMPO_IMAGE:?Set the tested PR image}
    container_name: tempo-upstream-v1183-node
    entrypoint:
      - /usr/local/bin/tempo
      - node
      - --chain=mainnet
      - --datadir=/var/data
      - --follow=wss://rpc.mainnet.tempo.xyz
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
    mem_limit: 8g
    cpus: 4
    stop_grace_period: 5m
    restart: "on-failure:3"
    logging:
      driver: json-file
      options: {max-size: "10m", max-file: "3"}
    networks: [tempo-upstream]
networks:
  tempo-upstream:
    ipam:
      config:
        - subnet: 10.42.20.0/24
```

## 4. 验证结果

本地完整节点 build、36 个 RPC 测试、348 个 EVM 测试、1073 个 precompile 测试通过（1 个上游既有 ignored）。Clippy 通过，仅两条旧 needless-borrow 提示；显式 nightly 的全 workspace 格式检查通过。PR 产品提交 CI43353300726 的 amd64/arm64/manifest 全成功。

真实节点从快照执行 303,280 块到 48,759,928，完成执行、Merkle 和历史索引后追平。

冻结 1000 个不同 canonical 块：保留原 100，另选新快照追块、T13 激活、epoch 边界、繁忙近期及近头各 180；501 个非空块、815 笔交易，其中 630 笔 AA。逐块 hash 五字段、完整 header/blockfile/validation 和 RLP 六字段全部一致：820 块使用生产 S3，180 块使用同块生产 archive RPC。主 S3 比较耗时 120.19 秒，180 块补充 260.18 秒；原始 S3 缺失结果保留。

同批 1000 块的两种模拟接口，各加 4 个固定 view/state override/revert 请求；03:09:45Z–03:31:52Z 共 1326.47 秒：

| 接口 | 请求数 | 严格一致 | 差异 |
|---|---:|---:|---|
| eth_multiCall | 1004 | 1004 | 0 |
| pre_traceMany | 1004 | 954 | 50 个请求中 60 个结果仅 error.msg 不同 |

这 50 个请求均为第 5 节所述上游 expiring-nonce 错误映射变化。完整差异归类后，没有其他字段差异，也没有顶层 RPC error 或传输中断。**pre 严格一致性结论仍为 FAIL，不能把归因当作兼容性豁免。** 原 100 样本 96 个严格一致、4 个仅该文本差异，旧调用树缺失问题未再出现。

含固定调用共 829 个调用结果：pre 为 766 成功、60 个重复 nonce 拒绝、3 个 revert；multi 为 826 个 code=0、3 个 code=-40014。匹配的拒绝/revert 不表述为执行成功。

03:32:06Z 对同镜像、同数据库发送正常 SIGTERM 后重启；旧进程在 follower 收到共识引擎停止错误后 exit=1（不是优雅 exit=0），不到一秒重新启动。03:32:29Z–03:33:29Z 五次采样 syncing=false、lag 0–1；重启后固定 48,764,366–48,764,385 共 20 个新近头块，hash+完整 trace/S3/RLP 六字段 20/20 一致，比较 2.32 秒。首个立即就绪探针仍 syncing=true，随后完成，不作为数据对比成功样本。未发生自动重启或 OOM，镜像 digest 未变。

追块期间曾见官方 RPC 的 -32005 限流并自动恢复；停机时的 forkchoice error 如上保留。保存的后段运行日志无 Rust panic 或其他 ERROR；不以 Docker 有限日志轮转覆盖范围推断整个运行期完全无日志错误。

报告归档提交同时将六处 Antithesis 工具文件恢复为目标 tag 的原始格式；默认 node 产品代码与实际测试提交 `d67671f9` 相同。原始测试脚本、fixture、严格失败和响应仅保存在任务证据目录，未纳入源码 PR。

基准：官方 RPC 逐块校验 canonical hash/parentHash/stateRoot/receiptsRoot/transactionsRoot；完整 trace 先读生产 `4217/f490914c` 的 S3 对象，源对象不存在的固定 180 块用同一生产 v1.17.0-6cfc archive 节点的 trace_debankBlock 补充比较，没有替换样本或将缺失当空值。原 S3 读取失败与补充结果均保留。生产 compactor 配有 delete-source，独立源对象不是长期可用性的保证；未根据配置推定每个删除事件的审计结果。

比较仅忽略 process_start_timestamp、pre 的随机 transactionHash 和 multi 的 timeCost；storage_contracts 和 RLP 六字段内部 map 顺序规范化，缺失/null/空数组保持区别。独立 JSON 键与 rlp 库复核全部 1000 块通过。

pre 传块 N（内部读取 N−1），multi 传 N−1 并覆盖 N 的时间与高度；保留真实交易 nonce、AA fee token/nonce key/授权和有效期。两种模拟接口独立统计，空块请求不计作真实交易覆盖。

## 5. 后续事项与部署注意

- 本版 certified follow/验证者模式禁止与 debug.rpc-consensus-url 或 debug.etherscan 混用；本次部署没有这些 debug 参数。
- T4 前非空 subblock 的历史 trace 仍使用保留支持的旧 CT 版本和兼容数据副本。
- `pre_traceMany` 的重复 expiring nonce 仍被两版拒绝，外层 code 都是 1000；新版上游将其映射为 `ServerError(-32003)` / `expiring nonce transaction already included`，旧版为 `Revm error: nonce manager error: ... ExpiringNonceReplay(ExpiringNonceReplay)`。RPC 直接暴露上游错误的 `to_string()`，本轮未为文本恢复旧映射；下游是否依赖该文本未验证，需在合并前接受这一兼容性变化。Halt 文本和部分 trace 失败双重日志的旧项也仍保留。
- 正常 SIGTERM 停机的 follower 错误导致 exit=1；本次重启恢复和输出检查通过，但未修复该退出码/停机日志行为。
- 本轮不是 genesis 全历史重放，不覆盖未激活硬分叉的真实主网行为；ETL/Kafka 真实投递和生产 rollout 未执行。
- 官方公告从下一版起将镜像签名验证切至 GitHub provenance；当前 CT 构建发布机制没有因此变更。
