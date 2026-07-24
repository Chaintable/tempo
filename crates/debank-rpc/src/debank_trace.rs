//! DeBank block trace types and trace building functions.
//!
//! Ported from reth-x `crates/rpc/rpc-eth-types/src/debank.rs` with Tempo adaptations.

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{
    Address, B256 as H256, BlockHash, BlockNumber, Bytes, U256, hex, keccak256,
};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use alloy_rpc_types_eth::Header;
use reth_revm::db::{AccountState, Cache};
use revm::DatabaseRef;
use revm_inspectors::tracing::{
    CallTraceArena,
    types::{CallKind, CallLog, CallTraceNode, TraceMemberOrder},
};
use serde::{Deserialize, Serialize};
use sha1::{Digest as Sha1Digest, Sha1};
use std::str::FromStr;

// ---------------------------------------------------------------------------
// State diff types (RLP-encoded for S3 storage)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable, Default)]
pub struct BlockStorageDiff {
    pub hash: H256,
    pub parent_hash: H256,
    pub new_accounts: Vec<NewAccount>,
    pub deleted_accounts: Vec<H256>,
    pub storage_diffs: Vec<AccountStorageDiff>,
    pub new_codes: Vec<NewCode>,
}

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct NewAccount {
    pub address: H256,
    pub balance: U256,
    pub nonce: u64,
    pub code_hash: H256,
}

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct AccountStorageDiff {
    pub address: H256,
    pub diffs: Vec<IndexValuePair>,
}

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct IndexValuePair {
    pub index: H256,
    pub value: U256,
}

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct NewCode {
    pub code_hash: H256,
    pub code: Bytes,
}

// ---------------------------------------------------------------------------
// BlockFile types (JSON for S3 / background-tracer)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[serde(default)]
pub struct DebankBlock {
    pub id: BlockHash,
    pub height: BlockNumber,
    pub parent_id: BlockHash,
    pub base_fee_per_gas: Option<u64>,
    pub miner: Address,
    pub gas_limit: u64,
    pub gas_used: u64,
    pub timestamp: u64,
    pub process_start_timestamp: u128,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[serde(default)]
pub struct DebankTransaction {
    pub id: String,
    #[serde(rename = "from_addr")]
    pub from: Address,
    #[serde(rename = "to_addr")]
    pub to: Address,
    pub gas_limit: u64,
    pub gas_price: u128,
    pub gas_used: u64,
    pub status: bool,
    #[serde(rename = "max_fee_per_gas")]
    pub gas_fee_cap: u128,
    #[serde(rename = "max_priority_fee_per_gas")]
    pub gas_tip_cap: u128,
    pub input: Bytes,
    pub nonce: u64,
    #[serde(rename = "idx")]
    pub transaction_index: u64,
    pub value: U256,
    // Tempo 0x76 (AA tx) fields — None/empty for standard tx types.
    // Aligned with TempoTransaction in crates/primitives/src/transaction/tempo_transaction.rs
    /// Chain ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<u64>,
    /// All calls in the AA tx. Standard txs have a single call derived from to/value/input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calls: Option<Vec<TempoCall>>,
    /// TIP-20 token address used to pay gas fees.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_token: Option<Address>,
    /// 2D nonce key for parallelizable transactions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce_key: Option<U256>,
    /// Transaction validity window (unix timestamp upper bound).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_before: Option<u64>,
    /// Transaction validity window (unix timestamp lower bound).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_after: Option<u64>,
    /// Signature type: "secp256k1", "p256", or "webAuthn".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_type: Option<String>,
    /// Full signature object (format varies by signature_type).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<serde_json::Value>,
    /// Fee payer signature for gas sponsorship.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_payer_signature: Option<serde_json::Value>,
    /// Key authorization data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_authorization: Option<serde_json::Value>,
    /// AA authorization list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aa_authorization_list: Option<Vec<serde_json::Value>>,
    /// EIP-2930 access list (also used by 0x76 AA tx).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_list: Option<Vec<serde_json::Value>>,
}

/// A single call within a Tempo AA transaction.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TempoCall {
    pub to: Address,
    pub value: U256,
    pub input: Bytes,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DebankEvent {
    pub id: String,
    pub contract_id: Address,
    pub selector: String,
    pub topics: Vec<String>,
    pub data: Bytes,
    pub parent_trace_id: String,
    pub pos_in_parent_trace: usize,
    pub idx: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DebankTrace {
    pub id: String,
    pub from_addr: Address,
    pub gas_limit: u64,
    pub input: Bytes,
    pub to_addr: Address,
    pub value: U256,
    pub gas_used: u64,
    pub output: Bytes,
    #[serde(rename = "type")]
    pub call_create_type: String,
    pub call_type: String,
    pub tx_id: String,
    pub parent_trace_id: String,
    pub pos_in_parent_trace: usize,
    pub self_storage_change: bool,
    pub storage_change: bool,
    pub subtraces: usize,
    pub trace_address: Vec<usize>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct BlockValidation {
    pub validation_hash: i64,
    pub is_fork: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[serde(default)]
pub struct BlockFile {
    pub block: DebankBlock,
    #[serde(rename = "txs")]
    pub transactions: Vec<DebankTransaction>,
    pub events: Vec<DebankEvent>,
    pub traces: Vec<DebankTrace>,
    pub error_events: Vec<DebankEvent>,
    pub error_traces: Vec<DebankTrace>,
    pub storage_contracts: Vec<Address>,
}

impl BlockFile {
    pub fn validation(&self) -> BlockValidation {
        let mut ids = Vec::new();
        ids.push(self.block.id.to_string());
        for transaction in &self.transactions {
            ids.push(transaction.id.to_string());
        }
        for event in &self.events {
            ids.push(event.id.clone());
        }
        for trace in &self.traces {
            ids.push(trace.id.clone());
        }
        BlockValidation {
            validation_hash: calc_validation_hash(&ids),
            is_fork: false,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DebankOutPut {
    pub block_file: BlockFile,
    pub header: Header,
    pub state_diff: Bytes,
    pub validation_hash: i64,
}

// ---------------------------------------------------------------------------
// ID calculation
// ---------------------------------------------------------------------------

pub trait DebankID {
    fn debank_id(&self) -> String;

    fn calculate_id(args: Vec<&str>) -> String {
        use md5::{Digest, Md5};
        let mut hasher = Md5::new();
        for arg in args {
            hasher.update(arg.as_bytes());
        }
        let result = hasher.finalize();
        format!("{result:x}")
    }
}

impl DebankID for DebankEvent {
    fn debank_id(&self) -> String {
        Self::calculate_id(vec![
            &self.parent_trace_id,
            &self.pos_in_parent_trace.to_string(),
        ])
    }
}

impl DebankID for DebankTrace {
    fn debank_id(&self) -> String {
        Self::calculate_id(vec![
            &self.tx_id,
            &self.parent_trace_id,
            &self.pos_in_parent_trace.to_string(),
        ])
    }
}

pub fn calc_validation_hash(ids: &[String]) -> i64 {
    let mut sha1_sum = U256::from(0);
    for each in ids {
        let mut hasher = Sha1::new();
        hasher.update(each.as_bytes());
        let hash_int = U256::from_str_radix(&hex::encode(hasher.finalize()), 16)
            .unwrap_or_else(|_| panic!("Failed to convert id {each} to U256"));
        sha1_sum += hash_int;
    }
    let sha1_sum_str = sha1_sum.to_string();
    let last_6_digits = if sha1_sum_str.len() >= 6 {
        &sha1_sum_str[sha1_sum_str.len().saturating_sub(6)..]
    } else {
        &sha1_sum_str
    };
    i64::from_str(last_6_digits).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Error message formatting
// ---------------------------------------------------------------------------

pub(crate) fn fmt_error_msg(res: revm::interpreter::InstructionResult) -> Option<String> {
    use revm::interpreter::InstructionResult;
    if res.is_ok() {
        return None;
    }
    let msg = match res {
        InstructionResult::Revert => "Reverted".to_string(),
        InstructionResult::OutOfGas
        | InstructionResult::PrecompileOOG
        | InstructionResult::MemoryOOG
        | InstructionResult::MemoryLimitOOG
        | InstructionResult::InvalidOperandOOG
        | InstructionResult::ReentrancySentryOOG => "Out of gas".to_string(),
        InstructionResult::OutOfFunds => "Insufficient balance for transfer".to_string(),
        InstructionResult::OpcodeNotFound | InstructionResult::InvalidFEOpcode => {
            "Bad instruction".to_string()
        }
        InstructionResult::StackOverflow => "Out of stack".to_string(),
        InstructionResult::InvalidJump => "Bad jump destination".to_string(),
        InstructionResult::PrecompileError => "Built-in failed".to_string(),
        status => format!("{status:?}"),
    };
    Some(msg)
}

// ---------------------------------------------------------------------------
// Trace node conversion: CallTraceNode → DebankTrace
// ---------------------------------------------------------------------------

impl From<&CallTraceNode> for DebankTrace {
    fn from(call_trace: &CallTraceNode) -> Self {
        let trace = &call_trace.trace;
        let call_create_type = match trace.kind {
            CallKind::Call
            | CallKind::StaticCall
            | CallKind::CallCode
            | CallKind::DelegateCall
            | CallKind::AuthCall => "call".to_string(),
            CallKind::Create => "create".to_string(),
            CallKind::Create2 => "create2".to_string(),
        };
        let mut call_type = String::new();
        if call_create_type == "call" {
            call_type = trace.kind.to_string().to_lowercase();
        }
        let error = trace.status.and_then(fmt_error_msg);
        let mut debank_trace = Self {
            from_addr: trace.caller,
            gas_limit: trace.gas_limit,
            input: trace.data.clone(),
            to_addr: trace.address,
            value: trace.value,
            gas_used: trace.gas_used,
            output: trace.output.clone(),
            call_create_type,
            call_type,
            subtraces: call_trace.children.len(),
            error: error.unwrap_or_default(),
            ..Default::default()
        };
        for op in &trace.steps {
            if op.op == revm::bytecode::opcode::OpCode::SSTORE {
                debank_trace.self_storage_change = true;
                debank_trace.storage_change = true;
                break;
            }
        }
        debank_trace
    }
}

impl From<&CallLog> for DebankEvent {
    fn from(log: &CallLog) -> Self {
        let selector = log
            .raw_log
            .topics()
            .first()
            .map(|h| h.to_string())
            .unwrap_or_default();
        let topics = if log.raw_log.topics().len() > 1 {
            log.raw_log.topics()[1..]
                .iter()
                .map(|h| h.to_string())
                .collect()
        } else {
            vec![]
        };
        Self {
            selector,
            topics,
            data: log.raw_log.data.clone(),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Trace tree building
// ---------------------------------------------------------------------------

// Trace 节点比 Event 大 ~376 vs ~168 bytes; 整树只在一笔 tx 内构建后即消费，
// 不进入持久存储，不 Box 以避免热路径上的一次 heap 分配。
#[allow(clippy::large_enum_variant)]
enum DebankTraceOrLog {
    Trace(DebankTraceNode),
    Log(DebankEvent),
}

struct DebankTraceNode {
    trace: DebankTrace,
    children: Vec<DebankTraceOrLog>,
    success: bool,
}

#[allow(clippy::too_many_arguments)]
fn build_trace_node(
    tx_id: String,
    parent_trace_id: String,
    pos_in_parent_trace: usize,
    node: &CallTraceNode,
    nodes: &[CallTraceNode],
    parent_success: bool,
    trace_address: Vec<usize>,
    log_index: &mut usize,
) -> DebankTraceNode {
    let mut debank_node = DebankTraceNode {
        trace: node.into(),
        children: Vec::new(),
        success: node.trace.success && parent_success,
    };
    debank_node.trace.trace_address = trace_address.clone();
    debank_node.trace.parent_trace_id = parent_trace_id;
    debank_node.trace.pos_in_parent_trace = pos_in_parent_trace;
    debank_node.trace.tx_id = tx_id.clone();
    debank_node.trace.id = debank_node.trace.debank_id();

    let id = debank_node.trace.id.clone();
    let contract_id = node.execution_address();

    let mut child_trace_address = Vec::new();
    for pos in &node.ordering {
        match pos {
            TraceMemberOrder::Call(i) => {
                let child_node = &nodes[node.children[*i]];
                let mut ta = trace_address.clone();
                ta.push(*i);
                child_trace_address = ta.clone();
                let child_trace = build_trace_node(
                    tx_id.clone(),
                    id.clone(),
                    debank_node.children.len(),
                    child_node,
                    nodes,
                    parent_success && debank_node.success,
                    ta,
                    log_index,
                );
                if child_trace.trace.storage_change && child_node.trace.success {
                    debank_node.trace.storage_change = true;
                }
                debank_node
                    .children
                    .push(DebankTraceOrLog::Trace(child_trace));
            }
            TraceMemberOrder::Log(i) => {
                let mut child_event: DebankEvent = (&node.logs[*i]).into();
                child_event.pos_in_parent_trace = debank_node.children.len();
                child_event.contract_id = contract_id;
                child_event.parent_trace_id = id.clone();
                child_event.id = child_event.debank_id();
                child_event.idx = *log_index;
                // Always increment log_index regardless of trace success,
                // because final success/error classification is based on
                // receipt status (not CallTraceArena success). See trace_block.rs.
                *log_index += 1;
                debank_node
                    .children
                    .push(DebankTraceOrLog::Log(child_event));
            }
            _ => {}
        }
    }
    // selfdestruct handling
    if node.is_selfdestruct() {
        // Build trace_address for selfdestruct: parent's trace_address + next child index.
        // child_trace_address tracks the last child call's address, but if there are no
        // child calls it stays empty. Fall back to parent trace_address + child count.
        let selfdestruct_ta = if child_trace_address.is_empty() {
            let mut ta = trace_address;
            ta.push(node.children.len());
            ta
        } else {
            if let Some(last) = child_trace_address.last_mut() {
                *last += 1;
            }
            child_trace_address
        };
        debank_node.trace.subtraces += 1;
        let mut selfdestruct_trace = DebankTrace {
            from_addr: node.trace.selfdestruct_address.unwrap_or_default(),
            to_addr: node.trace.selfdestruct_refund_target.unwrap_or_default(),
            value: node
                .trace
                .selfdestruct_transferred_value
                .unwrap_or_default(),
            trace_address: selfdestruct_ta,
            parent_trace_id: id,
            pos_in_parent_trace: debank_node.children.len(),
            tx_id,
            call_create_type: "suicide".to_string(),
            ..Default::default()
        };
        selfdestruct_trace.id = selfdestruct_trace.debank_id();
        debank_node
            .children
            .push(DebankTraceOrLog::Trace(DebankTraceNode {
                trace: selfdestruct_trace,
                children: vec![],
                success: parent_success && debank_node.success,
            }));
    }
    debank_node
}

fn finish_build_traces(
    node: &mut DebankTraceNode,
    traces: &mut Vec<DebankTrace>,
    error_traces: &mut Vec<DebankTrace>,
    events: &mut Vec<DebankEvent>,
    error_events: &mut Vec<DebankEvent>,
) {
    if node.success {
        traces.push(node.trace.clone());
    } else {
        error_traces.push(node.trace.clone());
    }
    for child in &mut node.children {
        match child {
            DebankTraceOrLog::Trace(trace) => {
                trace.trace.parent_trace_id = node.trace.id.clone();
                finish_build_traces(trace, traces, error_traces, events, error_events);
            }
            DebankTraceOrLog::Log(log) => {
                if node.success {
                    events.push(log.clone());
                } else {
                    error_events.push(log.clone());
                }
            }
        }
    }
}

/// Build DeBank traces and events from a revm `CallTraceArena`.
///
/// Returns `(traces, error_traces, events, error_events)`.
pub fn build_debank_traces(
    tx_id: H256,
    traces: CallTraceArena,
    log_index: &std::cell::RefCell<usize>,
) -> (
    Vec<DebankTrace>,
    Vec<DebankTrace>,
    Vec<DebankEvent>,
    Vec<DebankEvent>,
) {
    let nodes = traces.into_nodes();
    if nodes.is_empty() {
        return (vec![], vec![], vec![], vec![]);
    }
    let mut top = build_trace_node(
        tx_id.to_string(),
        String::new(),
        0,
        &nodes[0],
        &nodes,
        true,
        vec![],
        &mut log_index.borrow_mut(),
    );
    let mut traces = vec![];
    let mut error_traces = vec![];
    let mut events = vec![];
    let mut error_events = vec![];
    finish_build_traces(
        &mut top,
        &mut traces,
        &mut error_traces,
        &mut events,
        &mut error_events,
    );
    (traces, error_traces, events, error_events)
}

// ---------------------------------------------------------------------------
// State diff extraction from execution cache
// ---------------------------------------------------------------------------

pub fn get_storage_contracts_from_cache(cache: &Cache) -> Vec<Address> {
    cache
        .accounts
        .iter()
        .filter(|(_, account)| !account.storage.is_empty())
        .map(|(address, _)| *address)
        .collect()
}

pub fn get_storage_diffs_from_cache<DB: DatabaseRef>(cache: Cache, pre_db: DB) -> BlockStorageDiff {
    let mut new_accounts = Vec::new();
    let mut deleted_accounts = Vec::new();
    let mut storage_diffs = Vec::new();
    let mut new_codes = Vec::new();

    for (address, db_account) in cache.accounts {
        if db_account.account_state == AccountState::NotExisting {
            deleted_accounts.push(keccak256(address.0));
            continue;
        }

        new_accounts.push(NewAccount {
            address: keccak256(address.0),
            balance: db_account.info.balance,
            nonce: db_account.info.nonce,
            code_hash: db_account.info.code_hash,
        });

        if !db_account.storage.is_empty() {
            let diffs: Vec<IndexValuePair> = db_account
                .storage
                .into_iter()
                .map(|(key, value)| IndexValuePair {
                    index: keccak256::<[u8; 32]>(key.to_be_bytes()),
                    value,
                })
                .collect();
            if !diffs.is_empty() {
                storage_diffs.push(AccountStorageDiff {
                    address: keccak256(address.0),
                    diffs,
                });
            }
        }

        if let Some(code) = db_account.info.code {
            let code_hash = db_account.info.code_hash;
            if let Ok(Some(account)) = pre_db.basic_ref(address)
                && account.code_hash == code_hash
            {
                continue;
            }
            new_codes.push(NewCode {
                code_hash,
                code: code.original_bytes(),
            });
        }
    }

    BlockStorageDiff {
        hash: H256::ZERO,
        parent_hash: H256::ZERO,
        new_accounts,
        deleted_accounts,
        storage_diffs,
        new_codes,
    }
}

// ---------------------------------------------------------------------------
// Genesis block helpers
// ---------------------------------------------------------------------------

pub fn get_storage_contracts_from_genesis(genesis: &alloy_genesis::Genesis) -> Vec<Address> {
    genesis
        .alloc
        .iter()
        .filter(|(_, account)| account.storage.is_some())
        .map(|(address, _)| *address)
        .collect()
}

impl From<&alloy_genesis::Genesis> for BlockStorageDiff {
    fn from(genesis: &alloy_genesis::Genesis) -> Self {
        let mut new_accounts = Vec::new();
        let mut new_codes = Vec::new();
        let mut storage_diffs = Vec::new();

        for (address, account) in &genesis.alloc {
            let code_hash = if let Some(code) = &account.code {
                let code_hash = keccak256(code);
                new_codes.push(NewCode {
                    code_hash,
                    code: code.clone(),
                });
                code_hash
            } else {
                KECCAK_EMPTY
            };

            new_accounts.push(NewAccount {
                address: keccak256(address.0),
                balance: account.balance,
                nonce: account.nonce.unwrap_or_default(),
                code_hash,
            });

            if let Some(storage) = &account.storage {
                let diffs: Vec<IndexValuePair> = storage
                    .iter()
                    .map(|(key, value)| IndexValuePair {
                        index: keccak256::<[u8; 32]>(key.0),
                        value: U256::from_be_bytes(value.0),
                    })
                    .collect();
                if !diffs.is_empty() {
                    storage_diffs.push(AccountStorageDiff {
                        address: keccak256(address.0),
                        diffs,
                    });
                }
            }
        }

        Self {
            hash: H256::ZERO,
            parent_hash: alloy_consensus::constants::EMPTY_ROOT_HASH,
            new_accounts,
            deleted_accounts: vec![],
            storage_diffs,
            new_codes,
        }
    }
}

/// Build synthetic genesis transactions and traces (balance transfers + code deploys).
pub fn build_genesis_txs_and_traces(
    genesis: &alloy_genesis::Genesis,
) -> (Vec<DebankTransaction>, Vec<DebankTrace>) {
    let zero_addr = Address::ZERO;
    let mut tx_idx: u64 = 0;
    let mut txs = Vec::new();
    let mut traces = Vec::new();

    let mut sorted_addrs: Vec<&Address> = genesis.alloc.keys().collect();
    sorted_addrs.sort_by(|a, b| {
        a.to_string()
            .to_lowercase()
            .cmp(&b.to_string().to_lowercase())
    });

    for addr in sorted_addrs {
        let account = &genesis.alloc[addr];
        let addr_lower = format!("{addr:?}").to_lowercase();

        if account.balance > U256::ZERO {
            let tx_id = format!("0xgenesis01{:013}{}", 0, addr_lower);
            txs.push(DebankTransaction {
                id: tx_id.clone(),
                from: zero_addr,
                to: *addr,
                status: true,
                transaction_index: tx_idx,
                value: account.balance,
                ..Default::default()
            });
            let trace_id = DebankTrace::calculate_id(vec![&tx_id, "", "0"]);
            traces.push(DebankTrace {
                id: trace_id,
                from_addr: zero_addr,
                to_addr: *addr,
                value: account.balance,
                call_create_type: "call".to_string(),
                call_type: "call".to_string(),
                tx_id,
                ..Default::default()
            });
            tx_idx += 1;
        }

        if let Some(ref code) = account.code
            && !code.is_empty()
        {
            let tx_id = format!("0xgenesis02{:013}{}", 0, addr_lower);
            txs.push(DebankTransaction {
                id: tx_id.clone(),
                from: zero_addr,
                to: *addr,
                status: true,
                input: code.clone(),
                transaction_index: tx_idx,
                ..Default::default()
            });
            let trace_id = DebankTrace::calculate_id(vec![&tx_id, "", "0"]);
            traces.push(DebankTrace {
                id: trace_id,
                from_addr: zero_addr,
                to_addr: *addr,
                input: code.clone(),
                output: code.clone(),
                call_create_type: "create".to_string(),
                tx_id,
                ..Default::default()
            });
            tx_idx += 1;
        }
    }

    // Native token contract (0xeeee...eeee)
    let native_addr = Address::from_str("0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee").unwrap();
    let native_addr_lower = format!("{native_addr:?}").to_lowercase();
    let native_tx_id = format!("0xgenesis03{:013}{}", 0, native_addr_lower);
    txs.push(DebankTransaction {
        id: native_tx_id.clone(),
        from: zero_addr,
        to: native_addr,
        status: true,
        transaction_index: tx_idx,
        ..Default::default()
    });
    let native_trace_id = DebankTrace::calculate_id(vec![&native_tx_id, "", "0"]);
    traces.push(DebankTrace {
        id: native_trace_id,
        from_addr: zero_addr,
        to_addr: native_addr,
        call_create_type: "create".to_string(),
        tx_id: native_tx_id,
        ..Default::default()
    });

    (txs, traces)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debank_tx_aa_fields_serialization() {
        let tx = DebankTransaction {
            id: "0xabc".to_string(),
            from: Address::ZERO,
            to: Address::ZERO,
            gas_limit: 100000,
            gas_price: 20000000000,
            gas_used: 50000,
            status: true,
            gas_fee_cap: 24000000000,
            gas_tip_cap: 0,
            input: Bytes::from(vec![0x09, 0x5e, 0xa7, 0xb3]),
            nonce: 1,
            transaction_index: 0,
            value: U256::ZERO,
            calls: Some(vec![
                TempoCall {
                    to: "0x20c0000000000000000000000000000000000000"
                        .parse()
                        .unwrap(),
                    value: U256::ZERO,
                    input: Bytes::from(vec![0x09, 0x5e, 0xa7, 0xb3]),
                },
                TempoCall {
                    to: "0x99979c31c9785c4391dd02c00d981b30319add8f"
                        .parse()
                        .unwrap(),
                    value: U256::ZERO,
                    input: Bytes::from(vec![0xae, 0x77, 0xc2, 0x37]),
                },
            ]),
            fee_token: Some(
                "0x20c0000000000000000000000000000000000000"
                    .parse()
                    .unwrap(),
            ),
            nonce_key: Some(U256::ZERO),
            valid_before: None,
            valid_after: None,
            signature_type: Some("webAuthn".to_string()),
            signature: Some(serde_json::json!({
                "type": "webAuthn",
                "r": "0xabc",
                "s": "0xdef",
                "pubKeyX": "0x111",
                "pubKeyY": "0x222",
                "webauthnData": "0x333"
            })),
            fee_payer_signature: None,
            key_authorization: None,
            aa_authorization_list: None,
            access_list: None,
            chain_id: Some(4217),
        };

        let json = serde_json::to_value(&tx).unwrap();

        // Verify AA fields present
        assert_eq!(json["chain_id"], 4217);
        assert_eq!(json["calls"].as_array().unwrap().len(), 2);
        assert_eq!(
            json["calls"][0]["to"],
            "0x20c0000000000000000000000000000000000000"
        );
        assert_eq!(
            json["calls"][1]["to"],
            "0x99979c31c9785c4391dd02c00d981b30319add8f"
        );
        assert_eq!(
            json["fee_token"],
            "0x20c0000000000000000000000000000000000000"
        );
        assert_eq!(json["signature_type"], "webAuthn");
        assert_eq!(json["signature"]["type"], "webAuthn");

        // Verify None fields omitted
        assert!(json.get("valid_before").is_none());
        assert!(json.get("valid_after").is_none());
        assert!(json.get("fee_payer_signature").is_none());
        assert!(json.get("key_authorization").is_none());
        assert!(json.get("aa_authorization_list").is_none());
        assert!(json.get("access_list").is_none());

        // Round-trip
        let deserialized: DebankTransaction = serde_json::from_value(json).unwrap();
        assert_eq!(deserialized.calls.as_ref().unwrap().len(), 2);
        assert_eq!(deserialized.fee_token, tx.fee_token);
        assert_eq!(deserialized.signature_type, tx.signature_type);
        assert!(deserialized.signature.is_some());
    }

    #[test]
    fn test_debank_tx_standard_omits_aa_fields() {
        let tx = DebankTransaction {
            id: "0xdef".to_string(),
            from: Address::ZERO,
            to: "0xf851abca1d0fd1df8eaba6de466a102996b7d7b2"
                .parse()
                .unwrap(),
            gas_limit: 21000,
            gas_price: 20000000000,
            gas_used: 21000,
            status: true,
            input: Bytes::default(),
            ..Default::default()
        };

        let json = serde_json::to_value(&tx).unwrap();

        // AA fields should be absent (skip_serializing_if = None)
        assert!(json.get("calls").is_none());
        assert!(json.get("fee_token").is_none());
        assert!(json.get("nonce_key").is_none());
        assert!(json.get("valid_before").is_none());
        assert!(json.get("valid_after").is_none());
        assert!(json.get("signature_type").is_none());
        assert!(json.get("signature").is_none());
        assert!(json.get("fee_payer_signature").is_none());
        assert!(json.get("key_authorization").is_none());
        assert!(json.get("aa_authorization_list").is_none());
        assert!(json.get("access_list").is_none());
        assert!(json.get("chain_id").is_none());
    }

    #[test]
    fn test_debank_tx_deserialize_ignores_unknown_fields() {
        // Simulate old consumer receiving new fields — should not fail
        let json = r#"{
            "id": "0x123",
            "from_addr": "0x0000000000000000000000000000000000000000",
            "to_addr": "0x0000000000000000000000000000000000000000",
            "gas_limit": 100000,
            "gas_price": 20000000000,
            "gas_used": 50000,
            "status": true,
            "max_fee_per_gas": 24000000000,
            "max_priority_fee_per_gas": 0,
            "input": "0x",
            "nonce": 1,
            "idx": 0,
            "value": "0x0",
            "calls": [{"to": "0x20c0000000000000000000000000000000000000", "value": "0x0", "input": "0x095ea7b3"}],
            "fee_token": "0x20c0000000000000000000000000000000000000",
            "nonce_key": "0x0",
            "signature_type": "webAuthn",
            "some_future_field": "should be ignored"
        }"#;

        let tx: DebankTransaction = serde_json::from_str(json).unwrap();
        assert_eq!(tx.calls.as_ref().unwrap().len(), 1);
        assert_eq!(tx.signature_type, Some("webAuthn".to_string()));
    }

    #[test]
    fn test_debank_tx_backward_compatible_deserialize() {
        // Simulate new consumer reading old format without AA fields
        let json = r#"{
            "id": "0x456",
            "from_addr": "0x0000000000000000000000000000000000000000",
            "to_addr": "0x0000000000000000000000000000000000000000",
            "gas_limit": 21000,
            "gas_price": 20000000000,
            "gas_used": 21000,
            "status": true,
            "max_fee_per_gas": 0,
            "max_priority_fee_per_gas": 0,
            "input": "0x",
            "nonce": 0,
            "idx": 0,
            "value": "0x0"
        }"#;

        let tx: DebankTransaction = serde_json::from_str(json).unwrap();
        assert!(tx.calls.is_none());
        assert!(tx.fee_token.is_none());
        assert!(tx.nonce_key.is_none());
        assert!(tx.signature_type.is_none());
    }
}
