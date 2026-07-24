use alloy_consensus::BlockHeader;
use alloy_eips::BlockId;
use alloy_primitives::B256;
use alloy_rpc_types_eth::{BlockOverrides, Log, TransactionInfo, state::StateOverride};
use jsonrpsee::core::RpcResult;
use reth_evm::EvmEnvFor;
use reth_rpc_convert::RpcTxReq;
use reth_rpc_eth_api::{
    EthApiTypes,
    helpers::{EthTransactions, TraceExt},
};
use reth_rpc_eth_types::{EthApiError, StateCacheDb};
use revm::{DatabaseCommit, context::result::ResultAndState};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

use crate::types::{PreError, PreErrorCode, PreResult};

/// `pre` namespace API implementation.
#[derive(Clone)]
pub struct PreApi<Eth> {
    eth_api: Eth,
}

impl<Eth> PreApi<Eth> {
    /// Create a new instance of the [`PreApi`].
    pub fn new(eth_api: Eth) -> Self {
        Self { eth_api }
    }
}

impl<Eth> PreApi<Eth>
where
    Eth: EthApiTypes + TraceExt + 'static,
{
    /// Execute a single transaction with tracing, committing state changes to the db.
    fn trace_transaction(
        &self,
        evm_env: EvmEnvFor<Eth::Evm>,
        tx_env: reth_evm::TxEnvFor<Eth::Evm>,
        db: &mut StateCacheDb,
        tx_info: TransactionInfo,
        block_timestamp: u64,
    ) -> PreResult {
        let result: Result<PreResult, PreError> = (|| {
            let mut inspector = TracingInspector::new(TracingInspectorConfig::default_parity());

            // Execute with inspector, then commit state changes so subsequent txs see effects.
            let ResultAndState { result, state } = self
                .eth_api
                .inspect(&mut *db, evm_env, tx_env, &mut inspector)
                .map_err(|e| PreError {
                    code: PreErrorCode::UnKnown as i64,
                    msg: e.to_string(),
                })?;
            db.commit(state);

            match result {
                revm::context::result::ExecutionResult::Success {
                    gas,
                    logs: exec_logs,
                    ..
                } => {
                    let traces = inspector
                        .into_parity_builder()
                        .into_localized_transaction_traces(tx_info);

                    let logs: Vec<Log> = exec_logs
                        .into_iter()
                        .enumerate()
                        .map(|(i, log)| Log {
                            inner: log,
                            block_hash: tx_info.block_hash,
                            block_number: tx_info.block_number,
                            block_timestamp: Some(block_timestamp),
                            transaction_hash: tx_info.hash,
                            transaction_index: tx_info.index,
                            log_index: Some(i as u64),
                            removed: false,
                        })
                        .collect();

                    Ok(PreResult {
                        trace: traces,
                        logs,
                        error: None,
                        gas_used: gas.tx_gas_used(),
                    })
                }
                revm::context::result::ExecutionResult::Halt { reason, .. } => Err(PreError {
                    code: PreErrorCode::InsufficientBalance as i64,
                    msg: format!("halt: {reason:?}"),
                }),
                revm::context::result::ExecutionResult::Revert { .. } => Err(PreError {
                    code: PreErrorCode::Reverted as i64,
                    msg: "revert".to_string(),
                }),
            }
        })();

        match result {
            Ok(pre_result) => pre_result,
            Err(error) => PreResult::from(error),
        }
    }

    /// Execute multiple transactions sequentially, accumulating state changes.
    async fn trace_many(
        &self,
        transactions: Vec<RpcTxReq<Eth::NetworkTypes>>,
        block_id: Option<BlockId>,
        mut state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> Result<Vec<PreResult>, Eth::Error> {
        use alloy_rpc_types_eth::state::EvmOverrides;

        let target_block = block_id.unwrap_or_default();
        let ((evm_env, _), block) = futures::try_join!(
            self.eth_api.evm_env_at(target_block),
            self.eth_api.recovered_block(target_block),
        )?;
        let block = block.ok_or(EthApiError::HeaderNotFound(target_block))?;
        let block_number: u64 = block.number();
        let block_timestamp: u64 = block.timestamp();
        let parent_hash = block.parent_hash();
        let base_fee = block.base_fee_per_gas();

        self.eth_api
            .spawn_with_state_at_block(BlockId::hash(parent_hash), move |eth_api, mut db| {
                let this = Self::new(eth_api);
                let mut results: Vec<PreResult> = Vec::with_capacity(transactions.len());

                for (tx_index, tx) in transactions.into_iter().enumerate() {
                    let overrides =
                        EvmOverrides::new(state_overrides.take(), block_overrides.clone());
                    let (current_evm_env, tx_env) =
                        this.eth_api
                            .prepare_call_env(evm_env.clone(), tx, &mut db, overrides)?;

                    let tx_info = TransactionInfo {
                        hash: Some(B256::random()),
                        index: Some(tx_index as u64),
                        block_hash: Some(parent_hash),
                        block_number: Some(block_number),
                        block_timestamp: Some(block_timestamp),
                        base_fee,
                    };

                    let res = this.trace_transaction(
                        current_evm_env,
                        tx_env,
                        &mut db,
                        tx_info,
                        block_timestamp,
                    );
                    results.push(res);
                }

                Ok(results)
            })
            .await
    }
}

/// jsonrpsee server trait implementation.
#[async_trait::async_trait]
impl<Eth> crate::DebankPreApiServer<RpcTxReq<Eth::NetworkTypes>> for PreApi<Eth>
where
    Eth: EthApiTypes + EthTransactions + TraceExt + 'static,
{
    async fn trace_many(
        &self,
        transactions: Vec<RpcTxReq<Eth::NetworkTypes>>,
        block_id: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<Vec<PreResult>> {
        Self::trace_many(
            self,
            transactions,
            block_id,
            state_overrides,
            block_overrides,
        )
        .await
        .map_err(Into::into)
    }
}

impl<Eth> std::fmt::Debug for PreApi<Eth> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreApi").finish_non_exhaustive()
    }
}
