use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_eth::Log;
use alloy_rpc_types_trace::parity::LocalizedTransactionTrace;
use serde::{Deserialize, Serialize};

// ========== pre_traceMany types ==========

#[repr(i64)]
#[derive(Clone, Eq, PartialEq, Debug, Deserialize)]
pub enum PreErrorCode {
    UnKnown = 1000,
    InsufficientBalance = 1001,
    Reverted = 1002,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct PreError {
    pub code: i64,
    pub msg: String,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct PreResult {
    pub trace: Vec<LocalizedTransactionTrace>,
    pub logs: Vec<Log>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PreError>,
    #[serde(rename = "gasUsed")]
    pub gas_used: u64,
}

impl From<PreError> for PreResult {
    fn from(error: PreError) -> Self {
        Self {
            trace: vec![],
            logs: vec![],
            error: Some(error),
            gas_used: 0,
        }
    }
}

// ========== eth_multiCall types ==========

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SingleCallResult {
    pub code: i32,
    pub err: String,
    pub from_cache: bool,
    pub result: Bytes,
    pub gas_used: i64,
    pub time_cost: f64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiCallStats {
    pub block_num: u64,
    pub block_hash: B256,
    pub block_time: u64,
    pub success: bool,
    pub cache_enabled: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiCallResp {
    pub results: Vec<SingleCallResult>,
    pub stats: MultiCallStats,
}

#[repr(i32)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MultiCallErrorCode {
    Success = 0,
    CodeTxArgs = -40000,
    NativeMethodNotFound = -40001,
    #[allow(dead_code)]
    NativeMethodInput = -40002,
    #[allow(dead_code)]
    NativeMethodInputAddress = -40003,
    #[allow(dead_code)]
    NativeMethodOutput = -40010,
    #[allow(dead_code)]
    NativeMethodStateError = -40011,
    #[allow(dead_code)]
    MessageExecuting = -40012,
    EVMCancelled = -40013,
    EVMReverted = -40014,
    EVMFastFailed = -40015,
    #[allow(dead_code)]
    UnderlyingDB = -40020,
    #[allow(dead_code)]
    LoadingState = -40021,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_result_json_roundtrip() {
        let result = PreResult {
            trace: vec![],
            logs: vec![],
            error: None,
            gas_used: 21000,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["gasUsed"], 21000);
        // error: None should be omitted (skip_serializing_if)
        assert!(json.get("error").is_none());

        let decoded: PreResult = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.gas_used, 21000);
    }

    #[test]
    fn pre_result_with_error_serializes() {
        let result = PreResult::from(PreError {
            code: PreErrorCode::Reverted as i64,
            msg: "revert".to_string(),
        });
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["error"]["code"], 1002);
        assert_eq!(json["error"]["msg"], "revert");
        assert_eq!(json["gasUsed"], 0);
    }

    #[test]
    fn single_call_result_camel_case() {
        let result = SingleCallResult {
            code: 0,
            err: String::new(),
            from_cache: true,
            result: Bytes::from(vec![0x01]),
            gas_used: 50000,
            time_cost: 0.123,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["fromCache"], true);
        assert_eq!(json["gasUsed"], 50000);
        assert_eq!(json["timeCost"], 0.123);
        // Verify camelCase keys exist, snake_case keys don't
        assert!(json.get("from_cache").is_none());
        assert!(json.get("gas_used").is_none());
        assert!(json.get("time_cost").is_none());

        let decoded: SingleCallResult = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.gas_used, 50000);
        assert!(decoded.from_cache);
    }

    #[test]
    fn multi_call_stats_camel_case() {
        let stats = MultiCallStats {
            block_num: 100,
            block_hash: B256::ZERO,
            block_time: 1700000000,
            success: true,
            cache_enabled: false,
        };
        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["blockNum"], 100);
        assert_eq!(json["blockTime"], 1700000000u64);
        assert_eq!(json["cacheEnabled"], false);
        assert!(json.get("block_num").is_none());
        assert!(json.get("block_time").is_none());
        assert!(json.get("cache_enabled").is_none());

        let decoded: MultiCallStats = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.block_num, 100);
    }

    #[test]
    fn multi_call_resp_roundtrip() {
        let resp = MultiCallResp {
            results: vec![SingleCallResult {
                code: -40014,
                err: "revert".to_string(),
                from_cache: false,
                result: Bytes::default(),
                gas_used: 21000,
                time_cost: 0.001,
            }],
            stats: MultiCallStats {
                block_num: 42,
                block_hash: B256::ZERO,
                block_time: 1700000000,
                success: false,
                cache_enabled: true,
            },
        };
        let json_str = serde_json::to_string(&resp).unwrap();
        let decoded: MultiCallResp = serde_json::from_str(&json_str).unwrap();
        assert_eq!(decoded.results.len(), 1);
        assert_eq!(decoded.results[0].code, -40014);
        assert_eq!(decoded.stats.block_num, 42);
        assert!(!decoded.stats.success);
    }
}
