# Tempo 通用节点支持方案

## 目标

为 Tempo 节点实现 `trace_debankBlock` RPC，接入通用节点数据管线：

```
Tempo 节点 (trace_debankBlock RPC)
       ↓ HTTP
background-tracer (已有 sidecar)
       ↓                    ↓
   S3 上传                Kafka 发布
  (header/stateDiff/     (BlockChangeNotify)
   blockFile/validation)
       ↓                    ↓
   leafage-evm          DeBankCore
  (state query)      (blockfile traces)
```

## 实现状态

### 已完成

**新增文件**:
- `crates/debank-rpc/src/debank_trace.rs` — DeBank 类型定义（BlockStorageDiff, BlockFile, DebankOutPut 等）+ trace 树构建 + state diff 提取
- `crates/debank-rpc/src/state_diff_db.rs` — StateDiffTraceDB 双写数据库（记录 EVM state 变化用于 diff 提取）
- `crates/debank-rpc/src/trace_block.rs` — `trace_debankBlock` RPC 实现

**修改文件**:
- `crates/debank-rpc/src/lib.rs` — 新增 DebankTraceApi trait
- `crates/debank-rpc/Cargo.toml` — 添加 alloy-rlp, md-5, sha1, reth-chainspec 等依赖
- `crates/node/src/node.rs` — 注册 trace_debankBlock RPC

**编译通过**: `cargo check -p tempo-node` 零 warning 零 error。

### trace_debankBlock 执行流程

```
1. recovered_block(block_id) → DebankBlock + Header

2. block == 0 → genesis 特殊处理
   从 chain_spec.genesis() 构建 synthetic txs/traces + state_diff

3. block_receipts(block_id) → Vec<DebankTransaction>

4. parent.state_root == block.state_root → 空 state_diff（无状态变化）

5. spawn_blocking_io_fut:
   a. state_at_block_id(parent) × 2 → pre_db + StateDiffTraceDB(db)
   b. 对每笔 tx:
      - TracingInspector(parity + SSTORE filter) + TempoEvm handler
      - eth_api.inspect() → ResultAndState
      - commit state
      - build_debank_traces(CallTraceArena) → traces/events
   c. get_storage_diffs_from_cache(diff.cache, pre_db) → RLP-encoded state_diff

6. 组装 DebankOutPut { block_file, header, state_diff, validation_hash }
```

### 关键设计决策

**state_diff 包含完整状态变化（含 fee）**:
`eth_api.inspect()` 走 `TempoEvm::transact_raw()` → `inner.inspect_tx()` → 完整 TempoEvmHandler 路径（`validate_against_state_and_deduct_caller` + `reimburse_caller`）。handler 层的 fee 操作（TIP-20 storage 修改、fee Transfer log）包含在 `ResultAndState` 中，state_diff 和 events 自动完整。

这与 `pre_traceMany` 不同 — pre_traceMany 走 `prepare_call_env` → `inspect()` 路径（eth_call 模式），handler 的 fee 逻辑被跳过。

**AA tx (type=0x76) 的 DebankTransaction 字段来源**:

`to_addr` 和 `input` 对 AA tx 与普通 tx 表现不同：

| 字段 | 来源 | 普通 tx | AA tx (0x76) |
|------|------|--------|-------------|
| to_addr | `ReceiptResponse::to()` | tx.to = 合约地址 | receipt.to = 解包后的实际调用目标 |
| input | `Transaction::input()` | tx.input = call data | TempoTxEnvelope trait 返回 AA 内部调用数据（非信封 payload） |

`eth_getBlockByNumber` 返回 AA 信封层原始数据（to=null, input=短 AA payload），而 debankBlock 返回解包后的业务数据。这是 Tempo 的 `Transaction` trait 实现决定的行为（reth-x 无 AA tx，不存在此差异）。DeBankCore 需要解包后的数据，当前行为正确。

**Revert tx 的 fee log 获取**:

成功 tx: fee log 在 `ExecutionResult::Success { logs }` 中，直接可得。
Revert tx: `ExecutionResult::Revert` 没有 logs 字段。handler 的 fee log 存在 `TempoEvm.logs` 中，但 `inspect()` 后 EVM 被丢弃。实际通过 `eth_getTransactionReceipt`（已存储的 receipt）的 logs 补回，使用 `serde_json::from_value::<Vec<alloy_rpc_types_eth::Log>>` 反序列化。

**trace/event 分类: per-node success + receipt status 修正**:

`build_debank_traces()` 内部按 `CallTraceNode.trace.success` 分类 traces/events 到 success 或 error 列表。组装 block_file 时根据 receipt status 做两种修正：

1. **成功 tx, root trace 在 traces 中（正常 tx）**: 保留 per-node 分类。内部 revert 子调用（try/catch 场景）保留在 error 列表。与 reth-x 行为一致。
2. **成功 tx, root trace 在 error_traces 中（AA tx）**: `CallTraceArena` 将 handler 包装和子调用都标记为 `success=false`，arena 的 success 标志对整个调用树不可靠。将所有 error_traces/error_events 合并回 success 列表。
3. **失败 tx (status=0x0)**: 所有 traces/events 归入 error 列表。

**与 reth-x 的差异**:

| 项 | reth-x | Tempo |
|---|--------|-------|
| 区块重放 | `EvmFactory::create_tracer().try_trace_many()` | 手动循环 + `inspect()` + `commit()` |
| 错误处理 | iterator 返回 None 停止 | `?` 直接返回错误（一致） |
| pre_db | 两次 `state_at_block_id` | 同上（一致） |
| fee 处理 | 标准以太坊（无特殊） | TempoEvmHandler 自动处理 TIP-20 fee |
| revert tx fee log | 无此问题 | 从 receipt serde 反序列化补回 |
| trace 分类 | CallTraceArena success 标志 | receipt status 最终决定 |
| AA tx | 无 | to_addr/input 来自解包后数据 |
| exclude_precompile_calls | `false` | `false`（标准预编译调用保留在 trace 中，与 native inspector 的逐调用记录一一对应） |
| deposit_nonce | OP Stack 支持 | 不需要（非 OP Stack） |

**与 pipeline Go 版的字段兼容性**:

逐字段递归对比 pipeline (`/chaintable/pipeline/types/`) 与 Tempo 实现，结论:
- JSON 字段名: 全部一致（除 Header 的 `requestsHash` vs `requestsRoot`，reth 系通用差异，消费方不使用）
- 字段数量: 全部一致（BlockValidation 的 count 字段由 background-tracer 独立计算，不在 RPC 返回中）
- 数值类型: Go `*big.Int` vs Rust `u64`/`u128`，JSON 序列化结果一致

## 验证状态

已在 dev 环境 (blockchain-misc-x3, 镜像 `blockchain/tempo:e13d513`) 完成全部验证。详见 `docs/test-plan-generic-node.md`，136 项测试 132 PASS + 200 blocks 批量回归 (1557 tests, 0 FAIL)。

| 验证项 | 状态 |
|--------|------|
| 字段完整性 (block/txs/traces/events/state_diff/header) | PASS (132/136) |
| traces 与 trace_transaction 逐字段对比 (11 字段 × 21 条) | PASS |
| events 与 eth_getTransactionReceipt.logs 数量一致 | PASS (含 fee log) |
| revert tx with EVM events: fee log 捕获 (CR #2) | PASS (block 0x99e15c) |
| revert tx error_traces/error_events 分类 | PASS |
| AA tx trace 分类: root-trace 检测 (CR #3) | PASS |
| event idx 全局递增无重复 (CR #4) | PASS (200 blocks) |
| state_diff RLP 解码 (hash/parent_hash/new_accounts/storage_diffs/new_codes) | PASS |
| genesis/空块/CREATE 块/AA tx 块/最新块 | PASS |
| Receipt converter 回归 (CR #8) | PASS (dev vs prod `eth_getTransactionReceipt` 输出 diff 为空) |
| 性能 (单次调用 12ms) | PASS |
| background-tracer dry-run | 未执行 (需 binary 部署, 上线阻塞项) |

## 部署

### Dev 环境 (已完成)

- 机器: blockchain-misc-x3
- 数据: `/data/tempo/` (snapshot 导入, 20G)
- 镜像: `blockchain/tempo:5e3c190` (CI 自动构建)
- 端口: 8566 (HTTP RPC)
- docker-compose: `/data/tempo/docker-compose.yml`

### 生产部署步骤

1. 合并 `feature/debank_rpc` → `debank` 分支
2. 创建 release tag → CI 构建镜像 (amd64 + arm64)
3. 更新线上 2 台机器 docker compose (172.22.141.54 / 172.22.179.114)
4. 验证 `trace_debankBlock` RPC 可用
5. 部署 background-tracer sidecar (配置 Kafka/S3 + chain_id=4217)

## 已知限制 / 关注点

### exclude_precompile_calls 设置 (CTO CR #1 — 已撤回)

`trace_block.rs` 曾设为 `true`，排除标准预编译 (0x01-0x09) 的 call trace；a0407b3e68 引入 `NativeStorageChangeInspector` 时改回 `false`，标准预编译调用也保留在 trace 中，使 trace 节点与 native inspector 的逐调用记录一一对应。CTO 初始认为设为 `true` 会丢失 Tempo 自定义预编译 trace，**经确认：Tempo 自定义预编译 (TIP-20, FeeManager 等) 通过 `set_precompile_lookup` 注册，其地址不在 `warm_addresses()` 中，不受 `exclude_precompile_calls` 影响**。

### per-trace storage_change 对预编译无效 (CTO CR #7, 已修复)

字段语义按写入本身判定，不只看 opcode：
- `self_storage_change`：本调用写了自己的 storage，即调用执行所在账户的 storage：CALL / STATICCALL 为 `to_addr`，DELEGATECALL / CALLCODE 为调用方 `from_addr`。
- `storage_change`：本调用写了任何账户的 storage，或者有成功的子调用 `storage_change=true`；失败子调用的写入已回滚，不向上传播。

EVM 合约代码只能用 SSTORE 写当前执行上下文的 storage，`debank_trace.rs` 按 SSTORE opcode 置位即符合上述语义，与 geth pipeline 实现一致。Tempo 自定义预编译（TIP-20、FeeManager 等）用 Rust 代码直接写 state，不执行 SSTORE，只看 opcode 时调用预编译的 trace 两个字段都是 false，并导致父调用的 `storage_change` 也为 false。

**修复**: `trace_block.rs` 的 `NativeStorageChangeInspector` 对预编译调用收集 journal 的 `StorageChanged` 与 storage action 中的写入地址；调用 revert 后 journal 条目会回滚，但 storage action 仍保留，与 revert 前执行过 SSTORE 的处理一致：
- 写入地址等于调用地址时置 `self_storage_change`。Tempo 预编译只接受直接调用，调用地址就是它的 storage 所在账户。
- 有任何写入时置 `storage_change`。预编译可以在 Rust 里直接写其他账户，例如 `TIP20Factory.createToken` 初始化新 token、FeeManager `distribute_fees` 修改 token 余额；这类写入只体现在 `storage_change`，所以不能用 `self_storage_change` + `to_addr` 推出完整的写入账户集合。

是否为预编译调用以 revm 返回的 `CallOutcome::was_precompile_called` 为准，在 `call_end` 中读取：revm 在 precompile 实际处理调用时置位，包括通过 `set_precompile_lookup` 注册的 Tempo 预编译。journal 的 `precompile_addresses()` 只包含 `PrecompilesMap::warm_addresses()` 中静态注册的标准预编译（见上节 CR #1），不能用来识别 Tempo 预编译。

block 级 `storage_contracts` 由 `get_storage_contracts_from_bundle` 从回放后的 bundle state 计算，不依赖这两个字段；它只包含已提交的 storage 变更，不包含只在失败调用中写过的账户。

### event idx 连续性保证 (CTO 新增关注, 已修复)

`build_debank_traces` 中 `*log_index += 1` 对所有 event 无条件递增（AA tx 必需），但分类后 events/error_events 拆分可能导致 idx 有 gap。

**修复**: 在分类完成后，对所有 events + error_events 按原始 idx 排序，重新分配连续的 `[0, 1, 2, ...]` idx。保证 block 内 idx 全局连续无 gap。

### 多 call AA tx 的 DebankTransaction 只展示第一个 call

Tempo 0x76 tx 支持 `calls: Vec<Call>`（多个调用原子执行）。`DebankTransaction` 的 `to`/`input`/`value` 取自第一个 call（`receipt.to()` / `Transaction::input()` / `tx.value()`），其余 call 的信息只在 `traces` 中可见。

**链上已存在**: block 0x9eeb98 有 2-call AA tx（approve TIP-20 + swap 合约），`txs` 中 `to=null, input=第一个call`，第二个 call 仅在 traces `trace_address=[1]` 可见。

**影响**: leafage-evm 和 DeBankCore 都消费 blockfile.txs 字段。to/input 不完整会导致 leafage tx 索引/state 关联缺失、DeBankCore tx 解析不完整。上线前需修复或确认消费方可容忍。

### genesis native token 无实际意义 (CTO CR #11)

genesis 块创建了 `0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee` native token 的 synthetic tx。Tempo 实际无 native token（gas 用 TIP-20 支付），该资产永远无转账。不影响功能，DeBankCore 链配置层面建议标注忽略此资产。
