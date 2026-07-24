//! DeBank custom RPC extensions for Tempo.
//!
//! Provides `pre_traceMany`, `eth_multiCall`, and `trace_debankBlock` endpoints.

// `multi_call` has 8 params dictated by the deployed RPC schema; we can't merge them.
// `#[allow]` on the trait method doesn't reach into jsonrpsee::rpc macro-generated impl,
// so allow at module scope.
#![allow(clippy::too_many_arguments)]

pub mod debank_trace;
pub mod erc20_handle;
pub mod multi_call;
pub mod pre;
pub mod state_diff_db;
pub mod trace_block;
pub mod types;

// Re-export key types for registration
pub use debank_trace::DebankOutPut;
pub use multi_call::DebankEthExt;
pub use pre::PreApi;
pub use trace_block::DebankTraceBlock;
pub use types::{MultiCallResp, PreResult};

use alloy_eips::BlockId;
use alloy_rpc_types_eth::{BlockOverrides, state::StateOverride};
use jsonrpsee::core::RpcResult;

/// RPC trait for `pre` namespace.
#[jsonrpsee::proc_macros::rpc(server, namespace = "pre")]
pub trait DebankPreApi<TxReq> {
    /// Execute multiple transactions sequentially, returning execution traces for each.
    #[method(name = "traceMany")]
    async fn trace_many(
        &self,
        transactions: Vec<TxReq>,
        block_id: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<Vec<PreResult>>;
}

/// RPC trait extending `eth` namespace with `multiCall`.
#[jsonrpsee::proc_macros::rpc(server, namespace = "eth")]
pub trait DebankEthExtApi<TxReq> {
    /// Execute multiple calls in a single request, returning results for each.
    #[method(name = "multiCall")]
    async fn multi_call(
        &self,
        requests: Vec<TxReq>,
        block_number: Option<BlockId>,
        fast_fail: Option<bool>,
        use_parallel: Option<bool>,
        disable_cache: Option<bool>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<MultiCallResp>;
}

/// RPC trait for `trace` namespace — `debankBlock`.
#[jsonrpsee::proc_macros::rpc(server, namespace = "trace")]
pub trait DebankTraceApi {
    /// Returns DeBank trace/event/state-diff output for the given block.
    #[method(name = "debankBlock")]
    async fn trace_debank_block(&self, block_id: BlockId) -> RpcResult<DebankOutPut>;
}
