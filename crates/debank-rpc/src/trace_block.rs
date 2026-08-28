//! `trace_debankBlock` RPC implementation.
//!
//! Replays all transactions in a block, collecting DeBank-format traces, events,
//! and state diffs for consumption by background-tracer → S3/Kafka → leafage-evm.

use alloy_consensus::{BlockHeader, Transaction, TxReceipt, transaction::TxHashRef};
use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_rpc_types_eth::Header;
use jsonrpsee::core::RpcResult;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_errors::{BlockExecutionError, RethError};
use reth_evm::{ConfigureEvm, Evm, execute::BlockExecutor};
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
use reth_storage_api::{ChangeSetReader, StorageChangeSetReader};
use revm::{
    Database, Inspector, JournalEntry,
    bytecode::opcode::OpCode,
    context::{ContextTr, JournalTr},
    database::states::bundle_state::BundleRetention,
    interpreter::{CallInputs, CallOutcome, CallScheme, CreateInputs, CreateOutcome},
};
use revm_inspectors::tracing::{
    CallTraceArena, OpcodeFilter, TracingInspector, TracingInspectorConfig,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    mem,
    str::FromStr,
};
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
/// insertion order as real tracing arena nodes and merged into the DeBank trace after execution.
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

/// Aligns native inspector calls with tracing arena nodes.
///
/// Standard transactions enter the EVM at journal depth zero, so the first inspector call fills
/// arena node zero and both inspectors have the same number of records. Tempo AA transactions
/// execute their calls at depth one under a synthetic transaction root. `TracingInspector` keeps
/// its preallocated node zero for that root, but no `Inspector::call` callback exists for it. The
/// only accepted offset is therefore one verified synthetic root followed by an exact address
/// match for every real call/create frame.
fn align_native_storage_changes(
    arena: &mut CallTraceArena,
    native_changes: Vec<(Address, bool)>,
    tx_success: bool,
) -> Option<Vec<(Address, bool)>> {
    let nodes = arena.nodes();
    let trace_node_count = nodes.len();
    if trace_node_count == native_changes.len()
        && nodes
            .iter()
            .zip(&native_changes)
            .all(|(node, (address, _))| node.trace.address == *address)
    {
        return Some(native_changes);
    }

    let has_synthetic_root = trace_node_count == native_changes.len() + 1
        && nodes.first().is_some_and(|root| {
            root.parent.is_none()
                && root.trace.depth == 0
                && root.trace.address.is_zero()
                && root.trace.data.is_empty()
                && root.trace.value.is_zero()
                && root.trace.status.is_none()
        })
        && nodes[1..]
            .iter()
            .zip(&native_changes)
            .all(|(node, (address, _))| node.trace.address == *address);
    if !has_synthetic_root {
        return None;
    }

    arena.nodes_mut()[0].trace.success = tx_success;
    let mut aligned = Vec::with_capacity(trace_node_count);
    aligned.push((Address::ZERO, false));
    aligned.extend(native_changes);
    Some(aligned)
}

fn root_trace_misclassified(error_traces: &[DebankTrace]) -> bool {
    error_traces
        .iter()
        .any(|trace| trace.trace_address.is_empty())
}

fn debank_transaction_target(
    recipient: Option<Address>,
    contract_address: Option<Address>,
) -> Address {
    recipient.or(contract_address).unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct EventPayloadKey {
    contract_id: Address,
    selector: String,
    topics: Vec<String>,
    data: alloy_primitives::Bytes,
}

impl From<&DebankEvent> for EventPayloadKey {
    fn from(event: &DebankEvent) -> Self {
        Self {
            contract_id: event.contract_id,
            selector: event.selector.clone(),
            topics: event.topics.clone(),
            data: event.data.clone(),
        }
    }
}

fn with_event_index(mut event: DebankEvent, next_event_index: &mut usize) -> DebankEvent {
    event.idx = *next_event_index;
    *next_event_index += 1;
    event
}

/// Reconciles inspector-visible logs with the authoritative logs in the replayed receipt.
///
/// Tempo transaction handlers can emit logs both before and after EVM calls. Inspector-only logs
/// come from reverted frames; receipt-only logs come from handler code outside the interpreter.
/// Greedily matching equal payloads in order preserves call-trace parents without assuming that
/// handler logs form a suffix.
fn reconcile_persisted_events(
    events: Vec<DebankEvent>,
    error_events: Vec<DebankEvent>,
    persisted_events: Vec<DebankEvent>,
    root_trace: Option<&DebankTrace>,
    next_event_index: &mut usize,
) -> (Vec<DebankEvent>, Vec<DebankEvent>) {
    let mut candidates = events;
    candidates.extend(error_events);
    candidates.sort_by_key(|event| event.idx);

    let mut candidate_indices: HashMap<EventPayloadKey, VecDeque<usize>> = HashMap::new();
    for (index, event) in candidates.iter().enumerate() {
        candidate_indices
            .entry(event.into())
            .or_default()
            .push_back(index);
    }

    let mut matches = Vec::new();
    let mut last_candidate_index = None;
    for (receipt_index, event) in persisted_events.iter().enumerate() {
        let Some(indices) = candidate_indices.get_mut(&event.into()) else {
            continue;
        };
        while indices
            .front()
            .is_some_and(|index| last_candidate_index.is_some_and(|last| *index <= last))
        {
            indices.pop_front();
        }
        if let Some(candidate_index) = indices.pop_front() {
            matches.push((receipt_index, candidate_index));
            last_candidate_index = Some(candidate_index);
        }
    }

    let root_trace_id = root_trace.map(|trace| trace.id.clone()).unwrap_or_default();
    let mut next_root_position = root_trace.map(|trace| trace.subtraces).unwrap_or_default()
        + candidates
            .iter()
            .filter(|event| event.parent_trace_id == root_trace_id)
            .count();
    let mut reconciled_events = Vec::with_capacity(persisted_events.len());
    let mut reconciled_error_events = Vec::new();
    let mut receipt_cursor = 0;
    let mut candidate_cursor = 0;

    for (receipt_index, candidate_index) in matches {
        for mut event in persisted_events[receipt_cursor..receipt_index]
            .iter()
            .cloned()
        {
            event.parent_trace_id = root_trace_id.clone();
            event.pos_in_parent_trace = next_root_position;
            next_root_position += 1;
            event.id = event.debank_id();
            reconciled_events.push(with_event_index(event, next_event_index));
        }
        for event in candidates[candidate_cursor..candidate_index]
            .iter()
            .cloned()
        {
            reconciled_error_events.push(with_event_index(event, next_event_index));
        }

        reconciled_events.push(with_event_index(
            candidates[candidate_index].clone(),
            next_event_index,
        ));
        receipt_cursor = receipt_index + 1;
        candidate_cursor = candidate_index + 1;
    }

    // Inspector-only trailing logs execute before handler-generated receipt suffix logs.
    for event in candidates[candidate_cursor..].iter().cloned() {
        reconciled_error_events.push(with_event_index(event, next_event_index));
    }
    for mut event in persisted_events[receipt_cursor..].iter().cloned() {
        event.parent_trace_id = root_trace_id.clone();
        event.pos_in_parent_trace = next_root_position;
        next_root_position += 1;
        event.id = event.debank_id();
        reconciled_events.push(with_event_index(event, next_event_index));
    }

    (reconciled_events, reconciled_error_events)
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
    Eth::Provider: ChainSpecProvider<ChainSpec: EthChainSpec + EthereumHardforks>
        + ChangeSetReader
        + StorageChangeSetReader,
{
    /// Build `DebankOutPut` for the given block.
    async fn trace_debank_block(&self, block_id: BlockId) -> Result<DebankOutPut, Eth::Error> {
        let block = self.eth_api.recovered_block(block_id).await?;
        let Some(block) = block else {
            return Err(EthApiError::HeaderNotFound(block_id).into());
        };
        // Resolve dynamic tags such as `latest` once. Every subsequent lookup must refer to the
        // same block even if the canonical head advances while this RPC is running.
        let resolved_block_id = BlockId::hash(block.hash());

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

        let receipts = self.eth_api.block_receipts(resolved_block_id).await?;
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
                to: debank_transaction_target(receipt.to(), receipt.contract_address()),
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

        // Receipt statuses are used for final success/error classification. Event payloads come
        // from the receipts produced by the replay executor below, so they are covered by the
        // receipts-root validation rather than a fallible RPC serde round-trip.
        let tx_statuses: Vec<bool> = receipts.iter().map(|r| r.status()).collect();
        let replay_tx_statuses = tx_statuses.clone();
        let tx_gas_used: Vec<u64> = receipts.iter().map(|r| r.gas_used()).collect();

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
        // No empty block shortcut: Tempo can change state in block-level pre/post execution even
        // when the transaction list is empty (for example, hardfork activation deployments).
        // Pre-T4 blocks can also contain a subblock metadata system transaction.
        let block_state_root = block.state_root();
        let parent_state_root = parent_block.state_root();

        // Collect tx hashes before move
        let tx_hashes: Vec<B256> = block_txs.iter().map(|tx| *tx.tx_hash()).collect();

        let (evm_env, _) = self.eth_api.evm_env_at(resolved_block_id).await?;

        let parent_block_id = BlockId::hash(parent_hash);

        let (traces_result, state_diff, change_addresses) = self
            .eth_api
            .spawn_blocking_io_fut(move |eth_api| async move {
                let parent_state = eth_api.state_at_block_id(parent_block_id).await?;
                let post_state = eth_api
                    .state_at_block_id(BlockId::hash(block.hash()))
                    .await?;
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

                let mut next_event_index = 0usize;
                // (traces, error_traces, events, error_events)
                type PerTxResult = (
                    Vec<DebankTrace>,
                    Vec<DebankTrace>,
                    Vec<DebankEvent>,
                    Vec<DebankEvent>,
                );
                let mut all_results: Vec<PerTxResult> = Vec::new();

                for (idx, tx) in block.transactions_recovered().enumerate() {
                    let tx_hash = tx_hashes[idx];

                    let output = executor
                        .execute_transaction_without_commit(tx)
                        .map_err(Eth::Error::from_eth_err)?;

                    let (mut inspector, native_storage_inspector) = mem::replace(
                        executor.evm_mut().components_mut().1,
                        new_debank_inspector(),
                    );
                    inspector.set_transaction_gas_limit(tx.gas_limit());
                    inspector.set_transaction_gas_used(tx_gas_used[idx]);
                    inspector.set_transaction_caller(Address::from(*tx.signer()));
                    let mut arena = inspector.into_traces();
                    let raw_native_storage_changes = native_storage_inspector.into_changes();
                    let trace_node_count = arena.nodes().len();
                    let native_frame_count = raw_native_storage_changes.len();
                    let Some(native_storage_changes) = align_native_storage_changes(
                        &mut arena,
                        raw_native_storage_changes,
                        replay_tx_statuses[idx],
                    ) else {
                        return Err(Eth::Error::from_eth_err(BlockExecutionError::msg(format!(
                            "trace_debankBlock native inspector mismatch for transaction {tx_hash}: trace nodes {trace_node_count}, native frames {native_frame_count}",
                        ))));
                    };
                    let inspector_log_index = std::cell::RefCell::new(0usize);
                    let (traces, error_traces, events, error_events) =
                        build_debank_traces(
                            tx_hash,
                            arena,
                            &native_storage_changes,
                            &inspector_log_index,
                        );

                    executor.commit_transaction(output);
                    let persisted_events = executor
                        .receipts()
                        .get(idx)
                        .ok_or_else(|| {
                            Eth::Error::from_eth_err(BlockExecutionError::msg(format!(
                                "trace_debankBlock missing replayed receipt for transaction {tx_hash}"
                            )))
                        })?
                        .logs()
                        .iter()
                        .map(DebankEvent::from)
                        .collect();
                    let root_trace = traces
                        .iter()
                        .chain(&error_traces)
                        .find(|trace| trace.trace_address.is_empty());
                    let (events, error_events) = reconcile_persisted_events(
                        events,
                        error_events,
                        persisted_events,
                        root_trace,
                        &mut next_event_index,
                    );

                    all_results.push((traces, error_traces, events, error_events));
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
                let change_addresses = get_storage_contracts_from_bundle(&bundle_state);
                let destroyed_addresses = bundle_state
                    .state
                    .iter()
                    .filter_map(|(address, account)| {
                        (account.was_destroyed() && account.original_info.is_some())
                            .then_some(*address)
                    })
                    .collect::<HashSet<_>>();
                let replayed_state_diff =
                    get_storage_diffs_from_bundle(bundle_state, &replay_state.database);

                let account_changesets = eth_api
                    .provider()
                    .account_block_changeset(block.number())
                    .map_err(Eth::Error::from_eth_err)?;
                let storage_changesets = eth_api
                    .provider()
                    .storage_changeset(block.number())
                    .map_err(Eth::Error::from_eth_err)?;
                let canonical_state_diff = get_storage_diffs_from_changesets(
                    account_changesets,
                    storage_changesets,
                    &destroyed_addresses,
                    StateProviderDatabase::new(StateProviderTraitObjWrapper(post_state)),
                )
                .map_err(|err| {
                    Eth::Error::from_eth_err(BlockExecutionError::msg(format!(
                        "trace_debankBlock failed to build canonical state diff for block {}: {err}",
                        block.number(),
                    )))
                })?;

                if replayed_state_diff != canonical_state_diff {
                    let replayed_hash = keccak256(alloy_rlp::encode(&replayed_state_diff));
                    let canonical_hash = keccak256(alloy_rlp::encode(&canonical_state_diff));
                    return Err(Eth::Error::from_eth_err(BlockExecutionError::msg(format!(
                        "trace_debankBlock state diff mismatch for block {}: replayed {} (accounts {}, deleted {}, storage {}, codes {}), canonical {} (accounts {}, deleted {}, storage {}, codes {})",
                        block.number(),
                        replayed_hash,
                        replayed_state_diff.new_accounts.len(),
                        replayed_state_diff.deleted_accounts.len(),
                        replayed_state_diff.storage_diffs.len(),
                        replayed_state_diff.new_codes.len(),
                        canonical_hash,
                        canonical_state_diff.new_accounts.len(),
                        canonical_state_diff.deleted_accounts.len(),
                        canonical_state_diff.storage_diffs.len(),
                        canonical_state_diff.new_codes.len(),
                    ))));
                }

                Ok((all_results, canonical_state_diff, change_addresses))
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
        //    children as success=false even though the tx succeeds. Merge
        //    traces, but keep inspector-only logs in error_events because
        //    receipt reconciliation has already identified persisted events.
        //
        // 3. Failed tx: all traces/events go to error lists.
        for (idx, (mut trace, mut error_trace, event, mut error_event)) in
            traces_result.into_iter().enumerate()
        {
            let tx_success = tx_statuses.get(idx).copied().unwrap_or(true);
            if tx_success {
                let root_misclassified = root_trace_misclassified(&error_trace);
                if root_misclassified {
                    // AA tx: arena call success flags are unreliable, so merge traces. Events were
                    // already reconciled against the replayed receipt; inspector-only events must
                    // remain in the error list.
                    trace.extend(error_trace);
                    block_file.error_events.extend(error_event);
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
    Eth::Provider: ChainSpecProvider<ChainSpec: EthChainSpec + EthereumHardforks>
        + ChangeSetReader
        + StorageChangeSetReader,
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
    use revm_inspectors::tracing::types::{CallTrace, CallTraceNode, TraceMemberOrder};
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
    fn transaction_target_uses_created_address_for_create() {
        let created = Address::repeat_byte(0x33);

        assert_eq!(debank_transaction_target(None, Some(created)), created);
        assert_eq!(
            debank_transaction_target(Some(Address::repeat_byte(0x44)), Some(created)),
            Address::repeat_byte(0x44)
        );
    }

    #[test]
    fn native_changes_align_with_verified_aa_synthetic_root() {
        let precompile = Address::repeat_byte(0x22);
        let mut arena = CallTraceArena::default();
        arena.nodes_mut()[0].children = vec![1];
        arena.nodes_mut()[0].ordering = vec![TraceMemberOrder::Call(0)];
        arena.nodes_mut().push(CallTraceNode {
            parent: Some(0),
            idx: 1,
            trace: CallTrace {
                depth: 1,
                address: precompile,
                ..Default::default()
            },
            ..Default::default()
        });

        let mut invalid_arena = arena.clone();
        invalid_arena.nodes_mut()[0].trace.address = Address::repeat_byte(0x11);
        assert!(
            align_native_storage_changes(&mut invalid_arena, vec![(precompile, true)], true,)
                .is_none()
        );

        let aligned =
            align_native_storage_changes(&mut arena, vec![(precompile, true)], true).unwrap();
        assert!(arena.nodes()[0].trace.success);
        assert_eq!(aligned, vec![(Address::ZERO, false), (precompile, true)]);
    }

    #[test]
    fn receipt_reconciliation_handles_handler_and_reverted_logs() {
        fn event(marker: u8, idx: usize, parent_trace_id: &str) -> DebankEvent {
            DebankEvent {
                contract_id: Address::repeat_byte(marker),
                selector: format!("0x{marker:02x}"),
                topics: vec![format!("0x{marker:064x}")],
                data: vec![marker].into(),
                parent_trace_id: parent_trace_id.to_string(),
                idx,
                ..Default::default()
            }
        }

        let root = DebankTrace {
            id: "root".to_string(),
            subtraces: 2,
            ..Default::default()
        };
        let captured_b = event(0xb0, 0, "child");
        let reverted = event(0xee, 1, "child");
        let captured_c = event(0xc0, 2, "child");
        let persisted = vec![
            event(0xa0, 0, ""),
            event(0xb0, 0, ""),
            event(0xc0, 0, ""),
            event(0xd0, 0, ""),
        ];
        let mut next_event_index = 0;

        let (events, error_events) = reconcile_persisted_events(
            vec![captured_b, captured_c],
            vec![reverted],
            persisted,
            Some(&root),
            &mut next_event_index,
        );

        assert_eq!(
            events
                .iter()
                .map(|event| event.selector.as_str())
                .collect::<Vec<_>>(),
            vec!["0xa0", "0xb0", "0xc0", "0xd0"]
        );
        assert_eq!(error_events.len(), 1);
        assert_eq!(error_events[0].selector, "0xee");
        assert_eq!(error_events[0].idx, 2);
        assert_eq!(
            events.iter().map(|event| event.idx).collect::<Vec<_>>(),
            vec![0, 1, 3, 4]
        );
        assert_eq!(events[0].parent_trace_id, "root");
        assert_eq!(events[0].pos_in_parent_trace, 2);
        assert_eq!(events[1].parent_trace_id, "child");
        assert_eq!(events[3].parent_trace_id, "root");
        assert_eq!(events[3].pos_in_parent_trace, 3);
    }
}
