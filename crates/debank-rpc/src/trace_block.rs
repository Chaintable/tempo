//! `trace_debankBlock` RPC implementation.
//!
//! Replays all transactions in a block, collecting DeBank-format traces, events,
//! and state diffs for consumption by background-tracer → S3/Kafka → leafage-evm.

use alloy_consensus::{BlockHeader, Transaction, TxReceipt, transaction::TxHashRef};
use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::Header;
use jsonrpsee::core::RpcResult;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_errors::{BlockExecutionError, RethError};
use reth_evm::{ConfigureEvm, Evm, block::TxResult, execute::BlockExecutor};
use reth_primitives_traits::BlockBody;
use reth_provider::ChainSpecProvider;
use reth_revm::{State, database::StateProviderDatabase};
use reth_rpc_eth_api::{
    EthApiTypes, FromEthApiError, RpcNodeCore,
    helpers::{
        EthBlocks, EthTransactions, LoadBlock, LoadReceipt, LoadState, SpawnBlocking, TraceExt,
    },
};
use reth_rpc_eth_types::{EthApiError, cache::db::StateProviderTraitObjWrapper};
use revm::{
    Database, Inspector, JournalEntry,
    bytecode::opcode::OpCode,
    context::{ContextTr, JournalTr},
    database::states::bundle_state::BundleRetention,
    interpreter::{CallInputs, CallOutcome, CallScheme, CreateInputs, CreateOutcome},
};
use revm_inspectors::tracing::{OpcodeFilter, TracingInspector, TracingInspectorConfig};
use std::{mem, str::FromStr};
use tempo_evm::TempoEvmConfig;
use tempo_revm::evm::TempoContext;

use crate::debank_trace::*;

type DebankInspector = (TracingInspector, NativeStorageChangeInspector);

#[derive(Debug)]
struct NativeCallFrame {
    change_index: usize,
    journal_start: usize,
    is_precompile: bool,
}

/// Records storage writes performed inside native precompile frames.
///
/// Native precompiles write through the journal directly, so they do not execute an SSTORE opcode
/// that [`TracingInspector`] can attach to a call trace. Call records are kept in the same
/// insertion order as the tracing arena and merged into the DeBank trace after execution.
#[derive(Debug, Default)]
struct NativeStorageChangeInspector {
    frames: Vec<NativeCallFrame>,
    changes: Vec<(Address, bool)>,
}

impl NativeStorageChangeInspector {
    fn start_frame<DB: Database>(
        &mut self,
        context: &TempoContext<DB>,
        address: Address,
        is_precompile: bool,
    ) {
        let change_index = self.changes.len();
        self.changes.push((address, false));
        self.frames.push(NativeCallFrame {
            change_index,
            journal_start: context.journaled_state.journal.len(),
            is_precompile,
        });
    }

    fn finish_frame<DB: Database>(&mut self, context: &TempoContext<DB>) {
        let Some(frame) = self.frames.pop() else {
            return;
        };
        if !frame.is_precompile {
            return;
        }

        let storage_changed = context
            .journaled_state
            .journal
            .get(frame.journal_start..)
            .is_some_and(|entries| {
                entries
                    .iter()
                    .any(|entry| matches!(entry, JournalEntry::StorageChanged { .. }))
            });
        self.changes[frame.change_index].1 = storage_changed;
    }

    fn into_changes(self) -> Vec<(Address, bool)> {
        self.changes
    }
}

impl<DB: Database> Inspector<TempoContext<DB>> for NativeStorageChangeInspector {
    fn call(
        &mut self,
        context: &mut TempoContext<DB>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        let address = match inputs.scheme {
            CallScheme::DelegateCall | CallScheme::CallCode => inputs.bytecode_address,
            _ => inputs.target_address,
        };
        let is_precompile = context
            .journal_ref()
            .precompile_addresses()
            .contains(&address);
        self.start_frame(context, address, is_precompile);
        None
    }

    fn call_end(
        &mut self,
        context: &mut TempoContext<DB>,
        _inputs: &CallInputs,
        _outcome: &mut CallOutcome,
    ) {
        self.finish_frame(context);
    }

    fn create(
        &mut self,
        context: &mut TempoContext<DB>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        // TracingInspector runs first in the inspector tuple and has already loaded the caller.
        // Repeat the lookup to derive the same address and preserve one-to-one arena ordering.
        let nonce = context
            .journal_mut()
            .load_account(inputs.caller())
            .ok()?
            .info
            .nonce;
        self.start_frame(context, inputs.created_address(nonce), false);
        None
    }

    fn create_end(
        &mut self,
        context: &mut TempoContext<DB>,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        self.finish_frame(context);
    }
}

fn new_debank_inspector() -> DebankInspector {
    let mut trace_cfg = TracingInspectorConfig::default_parity()
        .set_steps(true)
        .set_record_logs(true)
        .set_exclude_precompile_calls(false);
    trace_cfg.record_opcodes_filter = Some(OpcodeFilter::new().enabled(OpCode::SSTORE));
    (
        TracingInspector::new(trace_cfg),
        NativeStorageChangeInspector::default(),
    )
}

fn root_trace_misclassified(error_traces: &[DebankTrace]) -> bool {
    error_traces
        .iter()
        .any(|trace| trace.trace_address.is_empty())
}

fn successful_receipt_event_count(
    error_traces: &[DebankTrace],
    events: &[DebankEvent],
    error_events: &[DebankEvent],
) -> usize {
    if root_trace_misclassified(error_traces) {
        // Tempo AA execution can mark the whole arena as failed even when the receipt succeeds.
        // In that case all inspector events are persisted receipt events.
        events.len() + error_events.len()
    } else {
        // Logs emitted by a reverted internal call are inspector-visible but absent from the
        // receipt, so they must not hide handler-injected fee logs at the end of exec_logs.
        events.len()
    }
}

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
        + RpcNodeCore<Evm = TempoEvmConfig, Primitives = <TempoEvmConfig as ConfigureEvm>::Primitives>
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

        let block_txs = BlockBody::transactions(block.body());
        if receipts.len() != block_txs.len() {
            return Err(Eth::Error::from_eth_err(BlockExecutionError::msg(format!(
                "trace_debankBlock receipt count mismatch for block {}: receipts {}, transactions {}",
                block.number(),
                receipts.len(),
                block_txs.len(),
            ))));
        }
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
                let parent_state = eth_api.state_at_block_id(parent_block_id).await?;
                let mut replay_state = State::builder()
                    .with_database(StateProviderDatabase::new(StateProviderTraitObjWrapper(
                        parent_state,
                    )))
                    .with_bundle_update()
                    .build();

                let evm = eth_api.evm_config().evm_with_env_and_inspector(
                    &mut replay_state,
                    evm_env,
                    new_debank_inspector(),
                );
                let execution_ctx = eth_api
                    .evm_config()
                    .context_for_block(block.sealed_block())
                    .map_err(RethError::other)
                    .map_err(Eth::Error::from_eth_err)?;
                let mut executor = eth_api.evm_config().create_executor(evm, execution_ctx);
                executor
                    .apply_pre_execution_changes()
                    .map_err(Eth::Error::from_eth_err)?;

                // Pre-execution system calls are block-level state changes, not transaction
                // traces. Discard anything recorded while applying them and start each
                // transaction with a fresh inspector.
                *executor.evm_mut().components_mut().1 = new_debank_inspector();

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

                    let output = executor
                        .execute_transaction_without_commit(tx)
                        .map_err(Eth::Error::from_eth_err)?;
                    let exec_logs = output.result().result.logs().to_vec();

                    let (inspector, native_storage_inspector) = mem::replace(
                        executor.evm_mut().components_mut().1,
                        new_debank_inspector(),
                    );
                    let arena = inspector.into_traces();
                    let native_storage_changes = native_storage_inspector.into_changes();
                    let (traces, error_traces, events, error_events) =
                        build_debank_traces(
                            tx_hash,
                            arena,
                            &native_storage_changes,
                            &log_index,
                        );

                    executor.commit_transaction(output);

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
                    let tx_reverted = !tx_statuses_clone.get(idx).copied().unwrap_or(true);
                    let persisted_evm_event_count = successful_receipt_event_count(
                        &error_traces,
                        &events,
                        &error_events,
                    );
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
                    } else if exec_logs.len() > persisted_evm_event_count {
                        // Success path: use exec_logs beyond inspector-captured events
                        exec_logs[persisted_evm_event_count..]
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
                        let fee_pos = root_subtraces + existing_events_on_root;

                        for (offset, mut fee_event) in extra_log_source.into_iter().enumerate() {
                            fee_event.parent_trace_id = root_trace_id.clone();
                            fee_event.pos_in_parent_trace = fee_pos + offset;
                            fee_event.id = fee_event.debank_id();
                            all_results.last_mut().unwrap().2.push(fee_event);
                        }
                    }
                }

                let (evm, execution_result) = executor
                    .finish()
                    .map_err(Eth::Error::from_eth_err)?;
                drop(evm);

                if execution_result.gas_used != block.gas_used() {
                    return Err(Eth::Error::from_eth_err(BlockExecutionError::msg(format!(
                        "trace_debankBlock gas mismatch for block {}: replayed {}, header {}",
                        block.number(),
                        execution_result.gas_used,
                        block.gas_used(),
                    ))));
                }

                let receipts_with_bloom = execution_result
                    .receipts
                    .iter()
                    .map(|receipt| receipt.with_bloom_ref())
                    .collect::<Vec<_>>();
                let replayed_receipts_root =
                    alloy_consensus::proofs::calculate_receipt_root(&receipts_with_bloom);
                if replayed_receipts_root != block.receipts_root() {
                    return Err(Eth::Error::from_eth_err(BlockExecutionError::msg(format!(
                        "trace_debankBlock receipts root mismatch for block {}: replayed {}, header {}",
                        block.number(),
                        replayed_receipts_root,
                        block.receipts_root(),
                    ))));
                }

                replay_state.merge_transitions(BundleRetention::PlainState);
                let bundle_state = replay_state.take_bundle();
                let parent_state_provider = &replay_state.database.0.0;
                let hashed_state = parent_state_provider.hashed_post_state(&bundle_state);
                let replayed_state_root = parent_state_provider
                    .state_root(hashed_state)
                    .map_err(Eth::Error::from_eth_err)?;
                if replayed_state_root != block_state_root {
                    return Err(Eth::Error::from_eth_err(BlockExecutionError::msg(format!(
                        "trace_debankBlock state root mismatch for block {}: replayed {}, header {}",
                        block.number(), replayed_state_root, block_state_root,
                    ))));
                }

                let change_addresses = get_storage_contracts_from_bundle(&bundle_state);
                let state_diff =
                    get_storage_diffs_from_bundle(bundle_state, &replay_state.database);
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
                let root_misclassified = root_trace_misclassified(&error_trace);
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
        + RpcNodeCore<Evm = TempoEvmConfig, Primitives = <TempoEvmConfig as ConfigureEvm>::Primitives>
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
        let _permit = self
            .eth_api
            .acquire_owned_tracing()
            .await
            .map_err(RethError::other)
            .map_err(EthApiError::Internal)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use reth_revm::MainContext;
    use revm::{
        Context,
        database::{CacheDB, EmptyDB},
    };
    use tempo_evm::TempoBlockEnv;
    use tempo_revm::TempoTxEnv;

    #[test]
    fn native_inspector_attributes_journaled_storage_to_precompile_frame() {
        let root = Address::repeat_byte(0x11);
        let precompile = Address::repeat_byte(0x22);
        let mut context: TempoContext<_> = Context::mainnet()
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_block(TempoBlockEnv::default())
            .with_cfg(Default::default())
            .with_tx(TempoTxEnv::default());
        let mut inspector = NativeStorageChangeInspector::default();

        inspector.start_frame(&context, root, false);
        inspector.start_frame(&context, precompile, true);
        context.journal_mut().load_account(precompile).unwrap();
        context
            .journal_mut()
            .sstore(precompile, U256::ZERO, U256::from(1))
            .unwrap();
        inspector.finish_frame(&context);
        inspector.finish_frame(&context);

        assert_eq!(
            inspector.into_changes(),
            vec![(root, false), (precompile, true)]
        );
    }

    #[test]
    fn reverted_internal_events_do_not_hide_fee_logs() {
        let internal_error_trace = DebankTrace {
            trace_address: vec![0],
            ..Default::default()
        };
        assert_eq!(
            successful_receipt_event_count(
                &[internal_error_trace],
                &[DebankEvent::default()],
                &[DebankEvent::default()],
            ),
            1
        );

        let misclassified_root = DebankTrace::default();
        assert_eq!(
            successful_receipt_event_count(
                &[misclassified_root],
                &[DebankEvent::default()],
                &[DebankEvent::default()],
            ),
            2
        );
    }
}
