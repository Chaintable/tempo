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
use reth_rpc_eth_types::EthApiError;
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
use tempo_precompiles::storage::StorageActions;
use tempo_primitives::transaction::Call;
use tempo_revm::evm::TempoContext;

use crate::debank_trace::*;

type DebankInspector = (TracingInspector, NativeStorageChangeInspector);

#[derive(Debug)]
struct NativeCallFrame {
    change_index: usize,
    journal_start: usize,
    action_cursor: usize,
}

/// Records storage writes performed inside native precompile frames.
///
/// Native precompiles write through the journal directly, so they do not execute an SSTORE opcode
/// that [`TracingInspector`] can attach to a call trace. Call records are kept in the same
/// insertion order as real tracing arena nodes and merged into the DeBank trace after execution.
#[derive(Debug)]
struct NativeStorageChangeInspector {
    frames: Vec<NativeCallFrame>,
    changes: Vec<(Address, NativeStorageWrites)>,
    actions: StorageActions,
}

impl Default for NativeStorageChangeInspector {
    fn default() -> Self {
        Self::new(StorageActions::disabled())
    }
}

impl NativeStorageChangeInspector {
    fn new(actions: StorageActions) -> Self {
        Self {
            frames: Vec::new(),
            changes: Vec::new(),
            actions,
        }
    }

    fn start_frame<DB: Database>(&mut self, context: &TempoContext<DB>, address: Address) {
        let change_index = self.changes.len();
        self.changes.push((address, NativeStorageWrites::default()));
        self.frames.push(NativeCallFrame {
            change_index,
            journal_start: context.journaled_state.journal.len(),
            action_cursor: self.actions.cursor(),
        });
    }

    fn finish_frame<DB: Database>(&mut self, context: &TempoContext<DB>, is_precompile: bool) {
        let Some(frame) = self.frames.pop() else {
            return;
        };
        if !is_precompile {
            return;
        }

        // Tempo precompiles only accept direct calls, so the frame address is the account whose
        // storage the precompile owns. Storage actions survive a journal revert, so writes of a
        // failed call are still attributed, like an SSTORE executed before a revert.
        let (frame_address, writes) = &mut self.changes[frame.change_index];
        let journal_writes = context
            .journaled_state
            .journal
            .get(frame.journal_start..)
            .unwrap_or_default()
            .iter()
            .filter_map(|entry| match entry {
                JournalEntry::StorageChanged { address, .. } => Some(*address),
                _ => None,
            });
        for written in journal_writes.chain(self.actions.storage_writes_since(frame.action_cursor))
        {
            writes.any = true;
            writes.own |= written == *frame_address;
        }
    }

    fn into_changes(self) -> Vec<(Address, NativeStorageWrites)> {
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
        self.start_frame(context, address);
        None
    }

    fn call_end(
        &mut self,
        context: &mut TempoContext<DB>,
        _inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        // revm sets this when a precompile served the call, including the Tempo precompiles
        // registered through the precompile lookup. The journal's warm precompile set only holds
        // the statically registered Ethereum precompiles and cannot identify them.
        self.finish_frame(context, outcome.was_precompile_called);
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
        self.start_frame(context, inputs.created_address(nonce));
        None
    }

    fn create_end(
        &mut self,
        context: &mut TempoContext<DB>,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        self.finish_frame(context, false);
    }
}

fn new_debank_inspector(actions: StorageActions) -> DebankInspector {
    let mut trace_cfg = TracingInspectorConfig::default_parity()
        .set_steps(true)
        .set_record_logs(true)
        .set_exclude_precompile_calls(false);
    trace_cfg.record_opcodes_filter = Some(OpcodeFilter::new().enabled(OpCode::SSTORE));
    (
        TracingInspector::new(trace_cfg),
        NativeStorageChangeInspector::new(actions),
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
    native_changes: Vec<(Address, NativeStorageWrites)>,
    tx_success: bool,
) -> Option<Vec<(Address, NativeStorageWrites)>> {
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
    aligned.push((Address::ZERO, NativeStorageWrites::default()));
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

/// Converts AA calls to their blockfile form and picks the transaction target.
///
/// A CREATE call keeps `to: None` (`null`), matching Tempo's `TxKind`. Only the first call can be
/// a CREATE, so the target is the created contract when that call deployed one; otherwise it stays
/// zero and the real targets are in `calls`.
fn debank_aa_calls_and_target(
    calls: &[Call],
    contract_address: Option<Address>,
) -> (Vec<TempoCall>, Address) {
    let target = calls
        .first()
        .filter(|call| call.to.is_create())
        .and(contract_address)
        .unwrap_or_default();
    let calls = calls
        .iter()
        .map(|call| TempoCall {
            to: call.to.to().copied(),
            value: call.value,
            input: call.input.clone(),
        })
        .collect();
    (calls, target)
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

/// Inspector-only logs are not in the receipt, so they take the index of the next receipt log
/// without consuming it. Receipt logs therefore keep `idx == logIndex`, as in reth-x.
fn with_pending_event_index(mut event: DebankEvent, next_event_index: usize) -> DebankEvent {
    event.idx = next_event_index;
    event
}

/// Reconciles inspector-visible logs with the authoritative logs in the replayed receipt.
///
/// Tempo transaction handlers can emit logs both before and after EVM calls. Inspector-only logs
/// come from reverted frames; receipt-only logs come from handler code outside the interpreter.
/// Greedily matching equal payloads in order preserves call-trace parents without assuming that
/// handler logs form a suffix. Error events are eligible only for a successful transaction whose
/// synthetic root was misclassified; otherwise an identical reverted log must not steal a receipt
/// log from its successful frame.
fn reconcile_persisted_events(
    events: Vec<DebankEvent>,
    error_events: Vec<DebankEvent>,
    persisted_events: Vec<DebankEvent>,
    root_trace: Option<&DebankTrace>,
    match_error_events: bool,
    next_event_index: &mut usize,
) -> (Vec<DebankEvent>, Vec<DebankEvent>) {
    struct Candidate {
        event: DebankEvent,
        receipt_eligible: bool,
    }

    let mut candidates = events
        .into_iter()
        .map(|event| Candidate {
            event,
            receipt_eligible: true,
        })
        .chain(error_events.into_iter().map(|event| Candidate {
            event,
            receipt_eligible: match_error_events,
        }))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| candidate.event.idx);

    let mut candidate_indices: HashMap<EventPayloadKey, VecDeque<usize>> = HashMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        if !candidate.receipt_eligible {
            continue;
        }
        candidate_indices
            .entry((&candidate.event).into())
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
            .filter(|candidate| candidate.event.parent_trace_id == root_trace_id)
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
        for candidate in &candidates[candidate_cursor..candidate_index] {
            let event = candidate.event.clone();
            reconciled_error_events.push(with_pending_event_index(event, *next_event_index));
        }

        reconciled_events.push(with_event_index(
            candidates[candidate_index].event.clone(),
            next_event_index,
        ));
        receipt_cursor = receipt_index + 1;
        candidate_cursor = candidate_index + 1;
    }

    // Inspector-only trailing logs execute before handler-generated receipt suffix logs.
    for candidate in &candidates[candidate_cursor..] {
        let event = candidate.event.clone();
        reconciled_error_events.push(with_pending_event_index(event, *next_event_index));
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

            // Extract 0x76 AA tx fields from serde JSON; calls come from the typed transaction.
            let tx_json = serde_json::to_value(tx).unwrap_or_default();
            let aa_tx = tx.as_aa();

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

            if let Some(aa_tx) = aa_tx {
                // AA tx has no top-level to/input/value; real data is in calls.
                // Clear the degraded values filled by trait methods from calls[0].
                let (calls, target) =
                    debank_aa_calls_and_target(&aa_tx.tx().calls, receipt.contract_address());
                dtx.to = target;
                dtx.input = Default::default();
                dtx.value = U256::ZERO;
                dtx.chain_id = tx_json.get("chainId").and_then(&parse_hex_u64);
                dtx.calls = Some(calls);
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
                    .with_database(StateProviderDatabase::new(parent_state))
                    .with_bundle_update()
                    .build();

                let evm = eth_api
                    .evm_config()
                    .evm_with_env(&mut replay_state, evm_env)
                    .with_actions();
                let storage_actions = evm.storage_actions();
                let evm = evm.with_inspector(new_debank_inspector(storage_actions));
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
                executor.evm_mut().clear_actions();
                let storage_actions = executor.evm_mut().storage_actions();
                *executor.evm_mut().components_mut().1 =
                    new_debank_inspector(storage_actions);

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

                    let storage_actions = executor.evm_mut().storage_actions();
                    let next_inspector = new_debank_inspector(storage_actions);
                    let (mut inspector, native_storage_inspector) =
                        mem::replace(executor.evm_mut().components_mut().1, next_inspector);
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
                    executor.evm_mut().clear_actions();
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
                    let match_error_events = replay_tx_statuses[idx]
                        && root_trace_misclassified(&error_traces);
                    let (events, error_events) = reconcile_persisted_events(
                        events,
                        error_events,
                        persisted_events,
                        root_trace,
                        match_error_events,
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
                    StateProviderDatabase::new(post_state),
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

        // Event idx values are final: reconcile_persisted_events numbered receipt logs with their
        // block-level logIndex (including failed-tx handler logs) and gave inspector-only logs the
        // next receipt index without consuming it.

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
    use tempo_precompiles::storage::StorageAction;
    use tempo_revm::TempoTxEnv;

    const OWN_WRITE: NativeStorageWrites = NativeStorageWrites {
        own: true,
        any: true,
    };

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

        inspector.start_frame(&context, root);
        inspector.start_frame(&context, precompile);
        context.journal_mut().load_account(precompile).unwrap();
        context
            .journal_mut()
            .sstore(precompile, U256::ZERO, U256::from(1))
            .unwrap();
        inspector.finish_frame(&context, true);
        inspector.finish_frame(&context, false);

        assert_eq!(
            inspector.into_changes(),
            vec![
                (root, NativeStorageWrites::default()),
                (precompile, OWN_WRITE)
            ]
        );
    }

    #[test]
    fn native_inspector_separates_writes_to_other_accounts() {
        let precompile = Address::repeat_byte(0x22);
        let other = Address::repeat_byte(0x33);
        let actions = StorageActions::enabled();
        let mut context: TempoContext<_> = Context::mainnet()
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_block(TempoBlockEnv::default())
            .with_cfg(Default::default())
            .with_tx(TempoTxEnv::default());
        let mut inspector = NativeStorageChangeInspector::new(actions.clone());

        // Journaled write to another account, e.g. TIP20Factory initializing a new token.
        inspector.start_frame(&context, precompile);
        context.journal_mut().load_account(other).unwrap();
        context
            .journal_mut()
            .sstore(other, U256::ZERO, U256::from(1))
            .unwrap();
        inspector.finish_frame(&context, true);

        // Write to another account whose journal entry is gone, as after a revert: only the
        // storage action remains.
        inspector.start_frame(&context, precompile);
        actions.record(StorageAction::Sstore(
            other,
            U256::ZERO,
            U256::ZERO,
            U256::ONE,
        ));
        inspector.finish_frame(&context, true);

        let other_only = NativeStorageWrites {
            own: false,
            any: true,
        };
        assert_eq!(
            inspector.into_changes(),
            vec![(precompile, other_only), (precompile, other_only)]
        );
    }

    #[test]
    fn native_inspector_retains_storage_actions_after_journal_revert() {
        let precompile = Address::repeat_byte(0x22);
        let actions = StorageActions::enabled();
        let context: TempoContext<_> = Context::mainnet()
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_block(TempoBlockEnv::default())
            .with_cfg(Default::default())
            .with_tx(TempoTxEnv::default());
        let mut inspector = NativeStorageChangeInspector::new(actions.clone());

        inspector.start_frame(&context, precompile);
        actions.record(StorageAction::Sstore(
            precompile,
            U256::ZERO,
            U256::ZERO,
            U256::ONE,
        ));
        // A failed precompile has already reverted its journal checkpoint before call_end.
        assert!(context.journaled_state.journal.is_empty());
        inspector.finish_frame(&context, true);

        assert_eq!(inspector.into_changes(), vec![(precompile, OWN_WRITE)]);
    }

    #[test]
    fn debank_trace_marks_storage_writes_end_to_end() {
        use alloy_primitives::{Bytes, bytes};
        use alloy_sol_types::SolCall;
        use reth_evm::EvmEnv;
        use revm::{
            DatabaseCommit,
            bytecode::Bytecode,
            context::{CfgEnv, TxEnv},
            primitives::TxKind,
            state::AccountInfo,
        };
        use tempo_chainspec::hardfork::TempoHardfork;
        use tempo_evm::evm::TempoEvm;
        use tempo_precompiles::{
            PATH_USD_ADDRESS, storage::StorageCtx, test_util::TIP20Setup, tip20::ITIP20,
        };
        use tempo_revm::gas_params::tempo_gas_params;

        let sender = Address::repeat_byte(0x01);
        let recipient = Address::repeat_byte(0x02);
        let spec = TempoHardfork::T11;
        let mut evm = TempoEvm::new(
            CacheDB::new(EmptyDB::default()),
            EvmEnv::new(
                CfgEnv::new_with_spec_and_gas_params(spec, tempo_gas_params(spec)),
                TempoBlockEnv::default(),
            ),
        );
        StorageCtx::enter_ctx(evm.ctx_mut(), StorageActions::disabled(), || {
            TIP20Setup::path_usd(sender)
                .with_issuer(sender)
                .with_mint(sender, U256::from(1_000_000))
                .apply()
        })
        .unwrap();
        let setup_state = evm.ctx_mut().journaled_state.finalize();
        evm.db_mut().commit(setup_state);
        // PUSH1 1 PUSH1 0 SSTORE
        let sstore_contract = Address::repeat_byte(0x03);
        evm.db_mut().insert_account_info(
            sstore_contract,
            AccountInfo::from_bytecode(Bytecode::new_raw(bytes!("6001600055"))),
        );

        // Same wiring as trace_debankBlock. Each call runs against the same pre-state.
        let evm = evm.with_actions();
        let storage_actions = evm.storage_actions();
        let mut evm = evm.with_inspector(new_debank_inspector(storage_actions));
        let mut trace_call = |to: Address, data: Bytes| {
            let result = evm
                .transact(TempoTxEnv {
                    inner: TxEnv {
                        caller: sender,
                        gas_limit: 1_000_000,
                        kind: TxKind::Call(to),
                        data,
                        ..Default::default()
                    },
                    fee_token: Some(PATH_USD_ADDRESS),
                    ..Default::default()
                })
                .unwrap();
            assert!(result.result.is_success(), "{:?}", result.result);

            let next_inspector = new_debank_inspector(evm.storage_actions());
            let (inspector, native) = mem::replace(evm.components_mut().1, next_inspector);
            let mut arena = inspector.into_traces();
            let native_changes =
                align_native_storage_changes(&mut arena, native.into_changes(), true).unwrap();
            let (traces, error_traces, _, _) = build_debank_traces(
                B256::ZERO,
                arena,
                &native_changes,
                &std::cell::RefCell::new(0),
            );
            assert!(error_traces.is_empty());
            assert_eq!(traces.len(), 1);
            traces.into_iter().next().unwrap()
        };

        // A TIP-20 transfer writes the token's own balances natively, without an SSTORE. TIP-20
        // tokens are served through the precompile lookup, so only the call outcome identifies
        // them as precompiles.
        let transfer = trace_call(
            PATH_USD_ADDRESS,
            ITIP20::transferCall {
                to: recipient,
                amount: U256::from(100),
            }
            .abi_encode()
            .into(),
        );
        assert!(transfer.self_storage_change);
        assert!(transfer.storage_change);

        // SSTORE flags of an EVM frame must survive merging its (empty) native writes.
        let sstore = trace_call(sstore_contract, Bytes::new());
        assert!(sstore.self_storage_change);
        assert!(sstore.storage_change);
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
            align_native_storage_changes(&mut invalid_arena, vec![(precompile, OWN_WRITE)], true,)
                .is_none()
        );

        let aligned =
            align_native_storage_changes(&mut arena, vec![(precompile, OWN_WRITE)], true).unwrap();
        assert!(arena.nodes()[0].trace.success);
        assert_eq!(
            aligned,
            vec![
                (Address::ZERO, NativeStorageWrites::default()),
                (precompile, OWN_WRITE)
            ]
        );
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
            false,
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
            vec![0, 1, 2, 3]
        );
        assert_eq!(next_event_index, 4);
        assert_eq!(events[0].parent_trace_id, "root");
        assert_eq!(events[0].pos_in_parent_trace, 2);
        assert_eq!(events[1].parent_trace_id, "child");
        assert_eq!(events[3].parent_trace_id, "root");
        assert_eq!(events[3].pos_in_parent_trace, 3);
    }

    #[test]
    fn receipt_reconciliation_does_not_match_a_reverted_duplicate() {
        let root = DebankTrace {
            id: "root".to_string(),
            ..Default::default()
        };
        let reverted = DebankEvent {
            contract_id: Address::repeat_byte(0xaa),
            selector: "0x01".to_string(),
            parent_trace_id: "reverted-child".to_string(),
            idx: 0,
            ..Default::default()
        };
        let successful = DebankEvent {
            parent_trace_id: "successful-child".to_string(),
            idx: 1,
            ..reverted.clone()
        };
        let persisted = DebankEvent {
            parent_trace_id: String::new(),
            idx: 0,
            ..reverted.clone()
        };
        let mut next_event_index = 0;

        let (events, error_events) = reconcile_persisted_events(
            vec![successful],
            vec![reverted],
            vec![persisted],
            Some(&root),
            false,
            &mut next_event_index,
        );

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].parent_trace_id, "successful-child");
        assert_eq!(error_events.len(), 1);
        assert_eq!(error_events[0].parent_trace_id, "reverted-child");
    }

    #[test]
    fn event_idx_follows_block_log_index_across_failed_and_successful_txs() {
        fn event(marker: u8, parent_trace_id: &str) -> DebankEvent {
            DebankEvent {
                contract_id: Address::repeat_byte(marker),
                selector: format!("0x{marker:02x}"),
                parent_trace_id: parent_trace_id.to_string(),
                ..Default::default()
            }
        }
        let root = DebankTrace {
            id: "root".to_string(),
            ..Default::default()
        };
        let mut next_event_index = 0;

        // Failed tx: a log emitted before the revert is not in the receipt, while the handler's
        // fee log is persisted and occupies logIndex 0.
        let (failed_receipt_events, failed_reverted_events) = reconcile_persisted_events(
            Vec::new(),
            vec![event(0xee, "reverted")],
            vec![event(0xfe, "")],
            Some(&root),
            false,
            &mut next_event_index,
        );
        // Successful tx: both logs are persisted and follow the failed tx's fee log.
        let (success_events, success_reverted_events) = reconcile_persisted_events(
            vec![event(0xa0, "child"), event(0xb0, "child")],
            Vec::new(),
            vec![event(0xa0, ""), event(0xb0, "")],
            Some(&root),
            false,
            &mut next_event_index,
        );

        assert_eq!(failed_receipt_events.len(), 1);
        assert_eq!(failed_receipt_events[0].selector, "0xfe");
        assert_eq!(failed_receipt_events[0].idx, 0);
        assert_eq!(failed_reverted_events.len(), 1);
        assert_eq!(failed_reverted_events[0].idx, 0);
        assert!(success_reverted_events.is_empty());
        assert_eq!(
            success_events
                .iter()
                .map(|event| (event.selector.as_str(), event.idx))
                .collect::<Vec<_>>(),
            vec![("0xa0", 1), ("0xb0", 2)]
        );
        assert_eq!(next_event_index, 3);
    }

    #[test]
    fn aa_calls_keep_create_target_as_null() {
        let created = Address::repeat_byte(0x71);
        let callee = Address::repeat_byte(0x20);
        let calls = vec![
            Call {
                to: alloy_primitives::TxKind::Create,
                value: U256::ZERO,
                input: vec![0x60, 0x80].into(),
            },
            Call {
                to: alloy_primitives::TxKind::Call(callee),
                value: U256::from(1),
                input: vec![0x01].into(),
            },
        ];

        let (debank_calls, target) = debank_aa_calls_and_target(&calls, Some(created));
        assert_eq!(target, created);
        assert_eq!(debank_calls[0].to, None);
        assert_eq!(debank_calls[0].input, calls[0].input);
        assert_eq!(debank_calls[1].to, Some(callee));
        assert_eq!(debank_calls[1].value, U256::from(1));
        let json = serde_json::to_value(&debank_calls).unwrap();
        assert_eq!(json[0]["to"], serde_json::Value::Null);

        // A reverted CREATE has no receipt contract address.
        let (_, target) = debank_aa_calls_and_target(&calls, None);
        assert_eq!(target, Address::ZERO);

        // Plain calls keep the AA rule of a zero top-level target.
        let (debank_calls, target) = debank_aa_calls_and_target(&calls[1..], None);
        assert_eq!(target, Address::ZERO);
        assert_eq!(debank_calls[0].to, Some(callee));
    }
}
