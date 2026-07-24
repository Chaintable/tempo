use alloy_consensus::BlockHeader;
use alloy_eips::BlockId;
use alloy_rpc_types_eth::{BlockOverrides, TransactionRequest, state::StateOverride};
use jsonrpsee::core::RpcResult;
use reth_rpc_convert::RpcTxReq;
use reth_rpc_eth_api::{EthApiTypes, helpers::EthCall};
use reth_rpc_eth_types::EthApiError;
use revm::context::result::ExecutionResult;

use crate::erc20_handle::{NATIVE_TOKEN_ADDRESS, eth_erc20_handle};
use crate::types::{MultiCallErrorCode, MultiCallResp, MultiCallStats, SingleCallResult};

/// `eth_multiCall` API implementation.
#[derive(Clone)]
pub struct DebankEthExt<Eth> {
    eth_api: Eth,
}

impl<Eth> DebankEthExt<Eth> {
    /// Create a new instance of the [`DebankEthExt`].
    pub fn new(eth_api: Eth) -> Self {
        Self { eth_api }
    }
}

impl<Eth> DebankEthExt<Eth>
where
    Eth: EthApiTypes + EthCall + 'static,
    RpcTxReq<Eth::NetworkTypes>: AsRef<TransactionRequest>,
{
    /// Execute multiple calls in a single request.
    #[allow(clippy::too_many_arguments)]
    async fn multi_call(
        &self,
        requests: Vec<RpcTxReq<Eth::NetworkTypes>>,
        block_number: Option<BlockId>,
        fast_fail: Option<bool>,
        _use_parallel: Option<bool>,
        disable_cache: Option<bool>,
        mut state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> Result<MultiCallResp, Eth::Error> {
        use alloy_rpc_types_eth::state::EvmOverrides;

        let target_block = block_number.unwrap_or_default();
        let fast_fail = fast_fail.unwrap_or(false);
        let disable_cache = disable_cache.unwrap_or(false);

        let ((evm_env, _), block) = futures::try_join!(
            self.eth_api.evm_env_at(target_block),
            self.eth_api.recovered_block(target_block),
        )?;
        let block = block.ok_or(EthApiError::HeaderNotFound(target_block))?;

        let mut multi_call_stats = MultiCallStats {
            block_num: block.number(),
            block_hash: block.hash(),
            block_time: block.timestamp(),
            success: true,
            cache_enabled: !disable_cache,
        };

        self.eth_api
            .spawn_with_state_at_block(target_block, move |eth_api, mut db| {
                let mut result_response: Vec<SingleCallResult> = Vec::with_capacity(requests.len());

                for request in requests {
                    let start = std::time::Instant::now();

                    // Fast-fail: if previous call failed, skip with dedicated error code
                    if fast_fail
                        && !result_response.is_empty()
                        && result_response.last().unwrap().code
                            != MultiCallErrorCode::Success as i32
                    {
                        result_response.push(SingleCallResult {
                            code: MultiCallErrorCode::EVMFastFailed as i32,
                            err: "skipped due to fast_fail".to_string(),
                            ..Default::default()
                        });
                        continue;
                    }

                    // Check if this is a call to the native token sentinel address
                    let to_addr = request.as_ref().to.as_ref().and_then(|kind| kind.to());
                    if to_addr == Some(&NATIVE_TOKEN_ADDRESS) {
                        let input = request.as_ref().input.input();
                        let mut res = eth_erc20_handle(&db, input.map(|b| b.as_ref()));
                        if res.code != MultiCallErrorCode::Success as i32 {
                            multi_call_stats.success = false;
                        }
                        res.time_cost = start.elapsed().as_secs_f64();
                        result_response.push(res);
                        continue;
                    }

                    // Regular EVM call
                    let overrides =
                        EvmOverrides::new(state_overrides.take(), block_overrides.clone());
                    let (current_evm_env, prepared_tx) =
                        eth_api.prepare_call_env(evm_env.clone(), request, &mut db, overrides)?;

                    let execute_result = eth_api.transact(&mut db, current_evm_env, prepared_tx)?;

                    let mut res = match execute_result.result {
                        ExecutionResult::Success { output, gas, .. } => SingleCallResult {
                            code: MultiCallErrorCode::Success as i32,
                            err: String::new(),
                            from_cache: false,
                            result: output.into_data(),
                            gas_used: gas.tx_gas_used() as i64,
                            time_cost: 0.0,
                        },
                        ExecutionResult::Revert { output, gas, .. } => SingleCallResult {
                            code: MultiCallErrorCode::EVMReverted as i32,
                            err: alloy_sol_types::decode_revert_reason(&output)
                                .unwrap_or_else(|| "Reason Unknown".to_string()),
                            from_cache: false,
                            result: alloy_primitives::Bytes::default(),
                            gas_used: gas.tx_gas_used() as i64,
                            time_cost: 0.0,
                        },
                        ExecutionResult::Halt { reason, gas, .. } => SingleCallResult {
                            code: MultiCallErrorCode::EVMCancelled as i32,
                            err: format!("Halted: {reason:?}"),
                            from_cache: false,
                            result: alloy_primitives::Bytes::default(),
                            gas_used: gas.tx_gas_used() as i64,
                            time_cost: 0.0,
                        },
                    };

                    res.time_cost = start.elapsed().as_secs_f64();
                    if res.code != MultiCallErrorCode::Success as i32 {
                        multi_call_stats.success = false;
                    }
                    result_response.push(res);
                }

                Ok(MultiCallResp {
                    results: result_response,
                    stats: multi_call_stats,
                })
            })
            .await
    }
}

/// jsonrpsee server trait implementation.
#[async_trait::async_trait]
impl<Eth> crate::DebankEthExtApiServer<RpcTxReq<Eth::NetworkTypes>> for DebankEthExt<Eth>
where
    Eth: EthApiTypes + EthCall + 'static,
    RpcTxReq<Eth::NetworkTypes>: AsRef<TransactionRequest>,
{
    async fn multi_call(
        &self,
        requests: Vec<RpcTxReq<Eth::NetworkTypes>>,
        block_number: Option<BlockId>,
        fast_fail: Option<bool>,
        use_parallel: Option<bool>,
        disable_cache: Option<bool>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<MultiCallResp> {
        Self::multi_call(
            self,
            requests,
            block_number,
            fast_fail,
            use_parallel,
            disable_cache,
            state_overrides,
            block_overrides,
        )
        .await
        .map_err(Into::into)
    }
}

impl<Eth> std::fmt::Debug for DebankEthExt<Eth> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebankEthExt").finish_non_exhaustive()
    }
}
