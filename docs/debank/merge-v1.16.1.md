# Tempo v1.16.1 upstream 合并验证

日期：2026-10-16。分支 `merge-v1.16.1`，审阅入口 [PR #17](https://github.com/Chaintable/tempo/pull/17)。本轮编译、原生测试和两条主网 RPC 路径对账通过，等待 review；未合并 PR、未创建 release。

## 1. 升级内容与必要性

[上游 v1.16.1](https://github.com/tempoxyz/tempo/releases/tag/v1.16.1) 于 2026-10-14 发布，是主网 network identity 维护更新。内置 BLS 门限公钥和起始 epoch 从 genesis identity / 0 更新为主网轮换后的 identity / 2042。官方对主网 RPC 节点的措辞为 recommended，没有强制激活期限；不能把 follower 需要验签直接推导为本版本必须升级。已有 follower 可以从可信起始区块和经过验证的 epoch 边界获取公钥。

其余改动：workspace 版本更新；上游 release workflow 仅下载 `tempo-*` artifacts（自有 ECR workflow 不适用）；新增四项 rustls advisory 例外，继续保留 rustls 0.23.45。依赖未升级，本次不是这些公告的安全修复版。

## 2. Merge 冲突与影响面

从 Chaintable `main=c15ed64e5ce3b3d5864aaa50d9fdb66576d44a7b` 合入上游 `v1.16.1=acfc30e248d22092c6a14c1b4dd9cf515b88a10f`，merge commit `18f8645c158e36f7159ae1e1ce7763b9586a65ee`。v1.16.0 是新 tag 的祖先，采用普通三方合并。

仅 `.github/workflows/release.yml` 冲突，保留 Chaintable 原 ECR workflow。合并相对 main 仅改四个文件：`Cargo.toml`、`Cargo.lock`、`crates/chainspec/src/network_identity.rs`、`deny.toml`；fork 的 RPC、EVM inspector 和构建设置没有改变。`debank-rpc` 的 workspace 继承版本同步为 1.16.1。

逐包对比 Cargo.lock：所有上游包的版本、来源、checksum 和依赖均与 v1.16.1 一致，只保留 fork 的 debank-rpc、md-5 和 tempo-node / tempo-revm 的两处依赖（相对 upstream +52 行）。`deny.toml` 保留 fork 已有 alloy-rs/core allow-git，新增 advisory exceptions 与上游相同。没有共识执行、trace 输出或模拟 RPC 的代码变更；未发现 fork patch 在新代码路径下引入的新增行为。

## 3. 部署情况

生产参照为 `arn:aws:eks:ap-northeast-1:294354037686:cluster/production` / `blockchain-tempo`：通用节点 `nodex-node-f490914c-0` 与常规节点 `hybrid-0`，两套 workload 及其 seed 都运行 `tempo-writer:v1.16.0-ct.1`。验证前固定两者为旧版参照，不修改生产。

测试部署在 lihe-dev (`i-0c8c99e496eda09b4`) 的 `/opt/apps/tempo-merge-v1161`。卷 `vol-0a648634bca9715e1` 从 10-09 的生产 hybrid-seed-0 快照 `snap-09f57f6cbccc910a2` 恢复，100GiB gp3，300MiB/s 初始化完成；离线文件系统检查通过，ext4 扩容后可用容量 99G、剩余 40G。源快照的 discovery-secret / known-peers 及备份已移出 datadir，首次启动生成新身份。

首次启动生产 v1 数据库成功，日志确认新公钥和 epoch 2042。该快照落后约百万块，官方 consensus_getFinalization 返回 -32005 限流，18:29:18–18:33:54 UTC 只接收约 23,300 个历史 payload，执行阶段尚未追平。该观察不能证明总同步将需要数小时，因为 Reth 还会进入批量同步；原 v1 快照的完整追平耗时未实测。为缩短本轮快照到链头的距离，使用同一测试卷，通过原生 `tempo download --archive --force --non-interactive` 重新初始化当天官方 archive 快照：高度 44,281,380，storage v2，manifest 为 `https://tempo-node-snapshots.tempoxyz.dev/tempo-4217-44281380-1792080264/manifest.json`，下载 19,794,468,437B、输出 54,540,532,760B；保留全部 archive 组件和证书验证。该修改仅覆盖本 run 测试数据，原生产 EBS 快照不变。测试卷临时调到 6000 IOPS / 500MiB/s，不是生产配置。后续主网对账使用 v2 archive；生产 v1 数据库完整追平未在本轮验证。

只运行 node，没有 ETL、Kafka、S3 或 etcd 配置，也不注入 AWS 凭证。RPC/WS 绑定 loopback。Compose：

```yaml
name: tempo-upstream-v1161
services:
  node:
    image: public.ecr.aws/b2h7a5c4/chaintable/tempo:18f8645c-amd64
    container_name: tempo-upstream-v1161-node
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
      - /opt/apps/tempo-merge-v1161/writer:/var/data
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
        - subnet: 10.99.79.0/24
```

## 4. 部署后测试情况

本地：`cargo check --locked --workspace --all-targets --exclude tempo-xtask`、nightly rustfmt 和 git diff 检查通过。原生单测：debank-rpc 36、tempo-chainspec 21、tempo-consensus 394，全部通过；包括证书与区块绑定、epoch 校验及边界 DKG identity 更新。

[双架构 CI](https://github.com/Chaintable/tempo/actions/runs/38783772018) 的 amd64、arm64、manifest 三项均通过；ECR 的 `tempo:18f8645c` 与 `tempo-writer:18f8645c` 指向同一多架构 manifest。测试运行该 manifest 的 amd64 镜像。

**同步与健康**：官方 archive 高度 44,281,380，原生下载与解压耗时 8m55.75s，exit 0。18:42:58 UTC 启动，18:44:24 追平到 44,299,128；18:44:24、18:45:56、18:46:57 的采样均 syncing=false、lag 0/1/0，restart 0、OOM false。追块时有官方 `get_finalization` -32005 限流，内置重试后继续；对账结束前最后90秒无 WARN/ERROR。首次手动停止时的 exit 1 另列于第5节。

**固定样本**：最近 epoch 边界 44,280,000 前后20块（44,279,990–44,280,009）、近500块中交易最多的40个不重复块、近 head 连续40块（44,299,129–44,299,168），共100块；54个非空块，85笔交易，其中58笔 AA。全部是 T12 激活后的主网块，样本固定后没有替换。

- **区块 hash / 根**：60/60 与官方 RPC 相同；比较 hash、parentHash、stateRoot、receiptsRoot、transactionsRoot。
- **通用节点 trace_debankBlock**：100/100 与生产 nodex 完整输出一致。state_diff 按 RLP 解码六字段比较，对无序集合排序，未用 header.stateRoot 替代；仅忽略 process_start_timestamp。
- **常规节点 pre_traceMany**：104/104 请求对 nodex 和 hybrid 均一致。
- **常规节点 eth_multiCall**：104/104 请求对 nodex 和 hybrid 均一致。

两个常规 RPC 各有100次真实块交易批量请求和4次固定请求（两个高度各测试 TIP-20 view / state override 返回值，以及 revert）。真实交易使用父块状态，保留 nonce / validBefore / validAfter，并覆盖为交易所属块的时间；pre 仅忽略随机 transactionHash，multicall 仅忽略 timeCost。

每个接口包含99个交易/固定调用结果：pre 81个成功结果（413条 trace），6个 ExpiringNonceReplay、12个 revert；multicall 90个成功、9个 -40014。所有失败结果也与两套生产参照一致，不把“接口输出一致”写成“所有交易执行成功”。ExpiringNonceReplay 是旧版已记录的行为，本次未修复。

对账采样阶段 18:45:04–18:46:19 UTC（约75秒）；此前的候选块准备未单独计时。原始块、hash、trace、两接口响应和完整汇总仅保存在任务目录 `runs/tempo/v1.16.1/mainnet-evidence/`，`summary-final.json` 固定记录100块和逐项结果。测试脚本及原始证据不进入 PR。

## 5. 其他问题

- 本版本按上游加入 RUSTSEC-2026-0346 / 0347 / 0348 / 0349 的忽略项，rustls 仍为 0.23.45；未证明这些公告在当前部署下不可利用，不能把 ignore 当作修复。
- `tempo-xtask` 测试的 `include_str!` 引用已删除的上游 `.github/workflows/bench.yml`。该源码及缺失文件与 main 完全相同，是既有编译限制，本次 workspace 检查明确排除此 package；未修改或删除测试。
- 完整 genesis 重放不在本轮范围；ETL、Kafka、S3 链路未测试（node 不投递，ETL 版本不变）。旧版已记录的其他 RPC 问题不由本次公钥维护更新修复。
- 首次追块手动 SIGTERM 时，executor 在 beacon engine 停止后继续提交 payload，出现 `beacon consensus engine task stopped`，随后 5s shutdown timeout，exit 1 / OOM false。相关 executor 和 shutdown 代码相对 main 未改；未做基线动态复测，未修复，不记作正常停止通过。
