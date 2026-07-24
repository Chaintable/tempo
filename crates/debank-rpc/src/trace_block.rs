//! `trace_debankBlock` RPC implementation.
//!
//! Replays all transactions in a block, collecting DeBank-format traces, events,
//! and state diffs for consumption by background-tracer → S3/Kafka → leafage-evm.

use alloy_consensus::{BlockHeader, Transaction, transaction::TxHashRef};
use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::Header;
use jsonrpsee::core::RpcResult;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_evm::ConfigureEvm;
use reth_primitives_traits::BlockBody;
use reth_provider::ChainSpecProvider;
use reth_revm::{State, database::StateProviderDatabase};
use reth_rpc_eth_api::{
    EthApiTypes,
    helpers::{
        EthBlocks, EthTransactions, LoadBlock, LoadReceipt, LoadState, SpawnBlocking, TraceExt,
    },
};
use reth_rpc_eth_types::{EthApiError, cache::db::StateProviderTraitObjWrapper};
use revm::DatabaseCommit;
use revm::bytecode::opcode::OpCode;
use revm_inspectors::tracing::{OpcodeFilter, TracingInspector, TracingInspectorConfig};
use std::str::FromStr;

use crate::debank_trace::*;
use crate::state_diff_db::StateDiffTraceDB;

/// `trace` namespace API implementation for `debankBlock`.
#[derive(Clone)]
pub struct DebankTraceBlock<Eth> {
    eth_api: Eth,
}

impl<Eth> DebankTraceBlock<Eth> {
    pub fn new(eth_api: Eth) -> Self {
        Self { eth_api }
    }
}

impl<Eth> DebankTraceBlock<Eth>
where
    Eth: EthApiTypes
        + EthBlocks
        + LoadBlock
        + LoadReceipt
        + LoadState
        + SpawnBlocking
        + TraceExt
        + 'static,
    Eth::Provider: ChainSpecProvider<ChainSpec: EthChainSpec + EthereumHardforks>,
{
    /// Build `DebankOutPut` for the given block.
    async fn trace_debank_block(&self, block_id: BlockId) -> Result<DebankOutPut, Eth::Error> {
        let block = self.eth_api.recovered_block(block_id).await?;
        let Some(block) = block else {
            return Err(EthApiError::HeaderNotFound(block_id).into());
        };

        let debank_block = DebankBlock {
            id: block.hash(),
            height: block.number(),
            parent_id: block.parent_hash(),
            base_fee_per_gas: block.base_fee_per_gas(),
            miner: block.beneficiary(),
            gas_limit: block.gas_limit(),
            gas_used: block.gas_used(),
            timestamp: block.timestamp(),
            process_start_timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        };

        let debank_header = Header {
            inner: alloy_consensus::Header {
                parent_hash: block.parent_hash(),
                ommers_hash: block.ommers_hash(),
                beneficiary: block.beneficiary(),
                state_root: block.state_root(),
                transactions_root: block.transactions_root(),
                receipts_root: block.receipts_root(),
                logs_bloom: block.logs_bloom(),
                difficulty: block.difficulty(),
                number: block.number(),
                gas_limit: block.gas_limit(),
                gas_used: block.gas_used(),
                timestamp: block.timestamp(),
                extra_data: block.extra_data().clone(),
                mix_hash: block.mix_hash().unwrap_or_default(),
                nonce: block.nonce().unwrap_or_default(),
                base_fee_per_gas: block.base_fee_per_gas(),
                withdrawals_root: block.withdrawals_root(),
                blob_gas_used: block.blob_gas_used(),
                excess_blob_gas: block.excess_blob_gas(),
                parent_beacon_block_root: block.parent_beacon_block_root(),
                requests_hash: block.requests_hash(),
                block_access_list_hash: block.block_access_list_hash(),
                slot_number: block.slot_number(),
            },
            hash: block.hash(),
            total_difficulty: None,
            size: None,
        };

        // Genesis block: synthetic txs from chain spec
        if block.number() == 0 {
            let chain_spec = self.eth_api.provider().chain_spec();
            let genesis = chain_spec.genesis();
            let mut state_diff: BlockStorageDiff = genesis.into();
            state_diff.hash = block.state_root();
            let (transactions, traces) = build_genesis_txs_and_traces(genesis);
            let block_file = BlockFile {
                block: debank_block,
                transactions,
                traces,
                storage_contracts: get_storage_contracts_from_genesis(genesis),
                ..Default::default()
            };
            let validation_hash = block_file.validation().validation_hash;
            return Ok(DebankOutPut {
                block_file,
                header: debank_header,
                state_diff: alloy_rlp::encode(state_diff).into(),
                validation_hash,
            });
        }

        // Build DebankTransactions from receipts
        use alloy_network::ReceiptResponse;

        let receipts = self.eth_api.block_receipts(block_id).await?;
        let Some(receipts) = receipts else {
            return Err(EthApiError::HeaderNotFound(block_id).into());
        };

        let block_txs = block.body().transactions();
        let mut debank_txs: Vec<DebankTransaction> = Vec::with_capacity(block_txs.len());

        for index in 0..block_txs.len() {
            let tx = &block_txs[index];
            let receipt = &receipts[index];

            // Extract 0x76 AA tx fields via serde round-trip.
            // Cannot use tempo_primitives directly (workspace feature unification
            // causes reth_codecs::Compact compile errors). Serialize the tx and
            // extract AA-specific fields from the JSON.
            // Extract 0x76 AA tx fields from serde JSON.
            let tx_json = serde_json::to_value(tx).unwrap_or_default();
            let is_aa = tx_json.get("type").and_then(|t| t.as_str()) == Some("0x76");

            let parse_hex_u64 = |v: &serde_json::Value| -> Option<u64> {
                v.as_str()
                    .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            };

            let mut dtx = DebankTransaction {
                id: receipt.transaction_hash().to_string(),
                from: receipt.from(),
                to: receipt.to().unwrap_or_default(),
                gas_limit: tx.gas_limit(),
                gas_price: receipt.effective_gas_price(),
                gas_used: receipt.gas_used(),
                status: receipt.status(),
                gas_fee_cap: tx.max_fee_per_gas(),
                gas_tip_cap: tx.max_priority_fee_per_gas().unwrap_or_default(),
                input: tx.input().clone(),
                nonce: tx.nonce(),
                transaction_index: receipt.transaction_index().unwrap_or(0),
                value: tx.value(),
                ..Default::default()
            };

            if is_aa {
                // AA tx has no top-level to/input/value; real data is in calls.
                // Clear the degraded values filled by trait methods from calls[0].
                dtx.to = Address::ZERO;
                dtx.input = Default::default();
                dtx.value = U256::ZERO;
                dtx.chain_id = tx_json.get("chainId").and_then(&parse_hex_u64);
                dtx.calls = tx_json
                    .get("calls")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                dtx.fee_token = tx_json
                    .get("feeToken")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok());
                dtx.nonce_key = tx_json
                    .get("nonceKey")
                    .and_then(|v| v.as_str())
                    .and_then(|s| U256::from_str(s).ok());
                dtx.valid_before = tx_json.get("validBefore").and_then(&parse_hex_u64);
                dtx.valid_after = tx_json.get("validAfter").and_then(parse_hex_u64);
                // Signature JSON has two formats:
                // - v2 keychain: {signature: {type, r, s, ...}, version, keyId, userAddress}
                // - direct: {type, r, s, pubKeyX, pubKeyY, webauthnData}
                dtx.signature_type = tx_json
                    .get("signature")
                    .and_then(|sig| {
                        sig.get("signature")
                            .and_then(|inner| inner.get("type"))
                            .or_else(|| sig.get("type"))
                    })
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                dtx.signature = tx_json.get("signature").cloned();
                dtx.fee_payer_signature = tx_json
                    .get("feePayerSignature")
                    .filter(|v| !v.is_null())
                    .cloned();
                dtx.key_authorization = tx_json
                    .get("keyAuthorization")
                    .filter(|v| !v.is_null())
                    .cloned();
                dtx.aa_authorization_list = tx_json
                    .get("aaAuthorizationList")
                    .and_then(|v| v.as_array().cloned())
                    .filter(|v| !v.is_empty());
                dtx.access_list = tx_json
                    .get("accessList")
                    .and_then(|v| v.as_array().cloned())
                    .filter(|v| !v.is_empty());
            }

            debank_txs.push(dtx);
        }

        // Receipt statuses + logs per tx.
        // ReceiptResponse trait doesn't expose logs(). Extract via serde
        // round-trip to alloy_rpc_types_eth::Log (a standard, stable type).
        let tx_statuses: Vec<bool> = receipts.iter().map(|r| r.status()).collect();
        let receipt_logs_per_tx: Vec<Vec<DebankEvent>> = receipts
            .iter()
            .map(|receipt| {
                let logs: Vec<alloy_rpc_types_eth::Log> = serde_json::to_value(receipt)
                    .ok()
                    .and_then(|v| v.get("logs").cloned())
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                logs.iter()
                    .enumerate()
                    .map(|(log_idx, log)| {
                        let selector = log
                            .topics()
                            .first()
                            .map(|h| h.to_string())
                            .unwrap_or_default();
                        let topics: Vec<String> =
                            log.topics().iter().skip(1).map(|h| h.to_string()).collect();
                        DebankEvent {
                            contract_id: log.address(),
                            selector,
                            topics,
                            data: log.data().data.clone(),
                            idx: log_idx,
                            ..Default::default()
                        }
                    })
                    .collect()
            })
            .collect();

        let parent_hash = block.parent_hash();
        let parent_block = self.eth_api.recovered_block(parent_hash.into()).await?;
        let Some(parent_block) = parent_block else {
            return Err(EthApiError::HeaderNotFound(block_id).into());
        };

        let mut block_file = BlockFile {
            block: debank_block,
            transactions: debank_txs,
            ..Default::default()
        };

        // Prepare block replay
        // No empty block shortcut: Tempo has a system tx in every block
        // (subblock metadata, gas=0) that doesn't change state but has a
        // trace in trace_transaction. Always replay to stay consistent.
        let block_state_root = block.state_root();
        let parent_state_root = parent_block.state_root();

        // Collect tx hashes before move
        let tx_hashes: Vec<B256> = block_txs.iter().map(|tx| *tx.tx_hash()).collect();

        let (evm_env, _) = self.eth_api.evm_env_at(block_id).await?;

        let parent_block_id = BlockId::hash(parent_hash);
        let tx_statuses_clone = tx_statuses.clone();

        let (traces_result, state_diff, change_addresses) = self
            .eth_api
            .spawn_blocking_io_fut(move |eth_api| async move {
                // Two independent state providers from the same parent block:
                // pre_db for diff comparison, db for tx execution
                let state1 = eth_api.state_at_block_id(parent_block_id).await?;
                let state2 = eth_api.state_at_block_id(parent_block_id).await?;

                let pre_db = State::builder()
                    .with_database(StateProviderDatabase::new(StateProviderTraitObjWrapper(
                        state1,
                    )))
                    .build();
                let cache_db = State::builder()
                    .with_database(StateProviderDatabase::new(StateProviderTraitObjWrapper(
                        state2,
                    )))
                    .build();
                let mut diff_db = StateDiffTraceDB::new(cache_db);

                let log_index = std::cell::RefCell::new(0usize);
                // (traces, error_traces, events, error_events, receipt_log_count)
                type PerTxResult = (
                    Vec<DebankTrace>,
                    Vec<DebankTrace>,
                    Vec<DebankEvent>,
                    Vec<DebankEvent>,
                    usize,
                );
                let mut all_results: Vec<PerTxResult> = Vec::new();

                for (idx, tx) in block.transactions_recovered().enumerate() {
                    let tx_hash = tx_hashes[idx];

                    let mut trace_cfg = TracingInspectorConfig::default_parity()
                        .set_steps(true)
                        .set_record_logs(true)
                        .set_exclude_precompile_calls(true);
                    trace_cfg.record_opcodes_filter =
                        Some(OpcodeFilter::new().enabled(OpCode::SSTORE));
                    let mut inspector = TracingInspector::new(trace_cfg);

                    let tx_env = eth_api.evm_config().tx_env(tx);

                    let revm::context::result::ResultAndState {
                        result: exec_result,
                        state,
                    } = eth_api.inspect(&mut diff_db, evm_env.clone(), tx_env, &mut inspector)?;
                    diff_db.commit(state);

                    let exec_logs = exec_result.into_logs();

                    let arena = inspector.into_traces();
                    let (traces, error_traces, events, error_events) =
                        build_debank_traces(tx_hash, arena, &log_index);

                    // Append fee logs not captured by the inspector.
                    //
                    // For successful txs: exec_logs (from ExecutionResult::Success)
                    // contains all logs including handler fee logs. Extra logs beyond
                    // what the inspector captured are fee events.
                    //
                    // For reverted txs: ExecutionResult::Revert has NO logs (exec_logs
                    // is empty). ALL receipt logs are handler-injected fee logs (EVM
                    // logs are reverted and don't enter the receipt). Use receipt logs
                    // directly — do NOT compare with evm_event_count, because the
                    // inspector may have captured N error_events from pre-revert emits,
                    // and receipt_log_count (fee only) < N would cause fee log loss.
                    let evm_event_count = events.len() + error_events.len();
                    let tx_reverted = !tx_statuses_clone.get(idx).copied().unwrap_or(true);
                    let receipt_logs = receipt_logs_per_tx.get(idx).cloned().unwrap_or_default();

                    all_results.push((
                        traces,
                        error_traces,
                        events,
                        error_events,
                        receipt_logs.len(),
                    ));

                    // Determine fee log source: exec_logs for success, receipt for revert.
                    // Use block-global log_index for idx (not tx-local offset).
                    let extra_log_source: Vec<DebankEvent> = if tx_reverted {
                        // Revert path: all receipt logs are fee logs
                        receipt_logs
                            .iter()
                            .map(|rl| {
                                let current_idx = *log_index.borrow();
                                *log_index.borrow_mut() += 1;
                                DebankEvent {
                                    contract_id: rl.contract_id,
                                    selector: rl.selector.clone(),
                                    topics: rl.topics.clone(),
                                    data: rl.data.clone(),
                                    idx: current_idx,
                                    ..Default::default()
                                }
                            })
                            .collect()
                    } else if exec_logs.len() > evm_event_count {
                        // Success path: use exec_logs beyond inspector-captured events
                        exec_logs[evm_event_count..]
                            .iter()
                            .map(|log| {
                                let selector = log
                                    .topics()
                                    .first()
                                    .map(|h| h.to_string())
                                    .unwrap_or_default();
                                let topics = if log.topics().len() > 1 {
                                    log.topics()[1..].iter().map(|h| h.to_string()).collect()
                                } else {
                                    vec![]
                                };
                                let current_idx = *log_index.borrow();
                                *log_index.borrow_mut() += 1;
                                DebankEvent {
                                    contract_id: log.address,
                                    selector,
                                    topics,
                                    data: log.data.data.clone(),
                                    idx: current_idx,
                                    ..Default::default()
                                }
                            })
                            .collect()
                    } else {
                        vec![]
                    };

                    if !extra_log_source.is_empty() {
                        let last = all_results.last().unwrap();
                        let root_trace_id = last
                            .0
                            .first()
                            .or(last.1.first())
                            .map(|t| t.id.clone())
                            .unwrap_or_default();
                        // Compute base pos from root trace's subtraces + all events
                        // already attached to it, to avoid pos collision with EVM events.
                        let root_subtraces = last
                            .0
                            .first()
                            .or(last.1.first())
                            .map(|t| t.subtraces)
                            .unwrap_or(0);
                        let existing_events_on_root = last
                            .2
                            .iter()
                            .chain(last.3.iter())
                            .filter(|e| e.parent_trace_id == root_trace_id)
                            .count();
                        let mut fee_pos = root_subtraces + existing_events_on_root;

                        for mut fee_event in extra_log_source {
                            fee_event.parent_trace_id = root_trace_id.clone();
                            fee_event.pos_in_parent_trace = fee_pos;
                            fee_event.id = fee_event.debank_id();
                            all_results.last_mut().unwrap().2.push(fee_event);
                            fee_pos += 1;
                        }
                    }
                }

                let change_addresses = get_storage_contracts_from_cache(&diff_db.diff.cache);
                let state_diff = get_storage_diffs_from_cache(diff_db.diff.cache, pre_db);
                Ok((all_results, state_diff, change_addresses))
            })
            .await?;

        // Assemble block file.
        // Classification uses per-node success from build_debank_traces, with
        // receipt status as override:
        //
        // 1. Successful tx, root trace correctly classified (in traces):
        //    Keep per-node classification. Internal revert sub-calls
        //    (try/catch) stay in error lists. Matches reth-x behavior.
        //
        // 2. Successful tx, root trace misclassified (in error_traces):
        //    AA tx — CallTraceArena marks the handler wrapper and its
        //    children as success=false even though the tx succeeds. The
        //    arena's success flags are unreliable for the entire tree,
        //    so merge all error_traces/events into success lists.
        //
        // 3. Failed tx: all traces/events go to error lists.
        for (idx, (mut trace, mut error_trace, mut event, mut error_event, _)) in
            traces_result.into_iter().enumerate()
        {
            let tx_success = tx_statuses.get(idx).copied().unwrap_or(true);
            if tx_success {
                let root_misclassified = error_trace.iter().any(|t| t.trace_address.is_empty());
                if root_misclassified {
                    // AA tx: arena success flags unreliable, merge all
                    trace.extend(error_trace);
                    event.extend(error_event);
                } else {
                    // Normal tx: keep per-node classification (try/catch)
                    block_file.error_traces.extend(error_trace);
                    block_file.error_events.extend(error_event);
                }
                block_file.traces.extend(trace);
                block_file.events.extend(event);
            } else {
                // Tx failed: all traces/events go to error lists
                error_trace.extend(trace);
                error_event.extend(event);
                block_file.error_traces.extend(error_trace);
                block_file.error_events.extend(error_event);
            }
        }

        // Reassign event idx to ensure block-global continuity (no gaps).
        // build_debank_traces always increments log_index (needed for AA tx),
        // but classification may split events between events/error_events,
        // leaving gaps in idx. Sort by current idx (preserves original
        // per-block order) and reassign [0, 1, 2, ...] sequentially.
        let mut idx_map: Vec<(usize, bool, usize)> = block_file
            .events
            .iter()
            .enumerate()
            .map(|(pos, e)| (e.idx, false, pos))
            .chain(
                block_file
                    .error_events
                    .iter()
                    .enumerate()
                    .map(|(pos, e)| (e.idx, true, pos)),
            )
            .collect();
        idx_map.sort_by_key(|(old_idx, _, _)| *old_idx);
        for (new_idx, (_, is_error, pos)) in idx_map.into_iter().enumerate() {
            if is_error {
                block_file.error_events[pos].idx = new_idx;
            } else {
                block_file.events[pos].idx = new_idx;
            }
        }

        let mut state_diff = state_diff;
        state_diff.hash = block_state_root;
        state_diff.parent_hash = parent_state_root;
        block_file.storage_contracts = change_addresses;

        let validation_hash = block_file.validation().validation_hash;
        Ok(DebankOutPut {
            block_file,
            header: debank_header,
            state_diff: alloy_rlp::encode(state_diff).into(),
            validation_hash,
        })
    }
}

// ---------------------------------------------------------------------------
// jsonrpsee server trait implementation
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl<Eth> crate::DebankTraceApiServer for DebankTraceBlock<Eth>
where
    Eth: EthApiTypes
        + EthBlocks
        + EthTransactions
        + LoadBlock
        + LoadReceipt
        + LoadState
        + SpawnBlocking
        + TraceExt
        + 'static,
    Eth::Provider: ChainSpecProvider<ChainSpec: EthChainSpec + EthereumHardforks>,
{
    async fn trace_debank_block(&self, block_id: BlockId) -> RpcResult<DebankOutPut> {
        Self::trace_debank_block(self, block_id)
            .await
            .map_err(Into::into)
    }
}

impl<Eth> std::fmt::Debug for DebankTraceBlock<Eth> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebankTraceBlock").finish_non_exhaustive()
    }
}
