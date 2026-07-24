use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{SolType, sol_data};
use revm::DatabaseRef;

use crate::types::{MultiCallErrorCode, SingleCallResult};

/// Sentinel address representing the native token as an ERC-20.
///
/// `0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE` — industry-standard placeholder
/// for native tokens, originated from Kyber Network.
pub const NATIVE_TOKEN_ADDRESS: Address = Address::new([0xee; 20]);

/// Handles ERC-20 method calls to the native token sentinel address (`0xeeee...eeee`).
///
/// Simulates `balanceOf`, `totalSupply`, `decimals`, `name`, and `symbol`
/// by reading native balances directly from state.
pub fn eth_erc20_handle<DB: DatabaseRef>(state: &DB, input: Option<&[u8]>) -> SingleCallResult {
    let Some(data) = input else {
        return SingleCallResult {
            code: MultiCallErrorCode::CodeTxArgs as i32,
            err: "tx input missing".to_string(),
            ..Default::default()
        };
    };

    if data.len() < 4 {
        return SingleCallResult {
            code: MultiCallErrorCode::CodeTxArgs as i32,
            err: "tx input less than 4 bytes".to_string(),
            ..Default::default()
        };
    }

    let selector = &data[0..4];
    match selector {
        // balanceOf(address) = 0x70a08231
        [0x70, 0xa0, 0x82, 0x31] => {
            if data.len() < 36 {
                return SingleCallResult {
                    code: MultiCallErrorCode::CodeTxArgs as i32,
                    err: "balanceOf: input too short".to_string(),
                    ..Default::default()
                };
            }
            let mut addr_bytes = [0u8; 20];
            addr_bytes.copy_from_slice(&data[16..36]);
            let user_addr = Address::from(addr_bytes);
            let balance = state
                .basic_ref(user_addr)
                .ok()
                .flatten()
                .map(|acc| acc.balance)
                .unwrap_or_default();
            SingleCallResult {
                code: MultiCallErrorCode::Success as i32,
                err: String::new(),
                result: Bytes::from(balance.to_be_bytes_vec()),
                ..Default::default()
            }
        }
        // totalSupply() = 0x18160ddd
        [0x18, 0x16, 0x0d, 0xdd] => SingleCallResult {
            code: MultiCallErrorCode::Success as i32,
            err: String::new(),
            result: Bytes::from(U256::from(1u32).to_be_bytes_vec()),
            ..Default::default()
        },
        // decimals() = 0x313ce567
        [0x31, 0x3c, 0xe5, 0x67] => SingleCallResult {
            code: MultiCallErrorCode::Success as i32,
            err: String::new(),
            result: Bytes::from(U256::from(18u32).to_be_bytes_vec()),
            ..Default::default()
        },
        // name() = 0x06fdde03, symbol() = 0x95d89b41
        [0x06, 0xfd, 0xde, 0x03] | [0x95, 0xd8, 0x9b, 0x41] => SingleCallResult {
            code: MultiCallErrorCode::Success as i32,
            err: String::new(),
            result: Bytes::from(sol_data::String::abi_encode("ETH")),
            ..Default::default()
        },
        _ => SingleCallResult {
            code: MultiCallErrorCode::NativeMethodNotFound as i32,
            err: "method not found".to_string(),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use revm::state::AccountInfo;
    use std::collections::HashMap;

    /// Minimal in-memory DB for testing.
    struct MockDb {
        accounts: HashMap<Address, AccountInfo>,
    }

    impl MockDb {
        fn new() -> Self {
            Self {
                accounts: HashMap::new(),
            }
        }

        fn insert(&mut self, addr: Address, balance: U256) {
            self.accounts.insert(
                addr,
                AccountInfo {
                    balance,
                    ..Default::default()
                },
            );
        }
    }

    impl DatabaseRef for MockDb {
        type Error = std::convert::Infallible;

        fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Ok(self.accounts.get(&address).cloned())
        }

        fn code_by_hash_ref(
            &self,
            _: alloy_primitives::B256,
        ) -> Result<revm::bytecode::Bytecode, Self::Error> {
            Ok(Default::default())
        }

        fn storage_ref(&self, _: Address, _: U256) -> Result<U256, Self::Error> {
            Ok(U256::ZERO)
        }

        fn block_hash_ref(&self, _: u64) -> Result<alloy_primitives::B256, Self::Error> {
            Ok(Default::default())
        }
    }

    const SUCCESS: i32 = MultiCallErrorCode::Success as i32;
    const ERR_ARGS: i32 = MultiCallErrorCode::CodeTxArgs as i32;
    const ERR_NOT_FOUND: i32 = MultiCallErrorCode::NativeMethodNotFound as i32;

    // --- balanceOf(address) = 0x70a08231 ---

    #[test]
    fn balance_of_returns_account_balance() {
        let mut db = MockDb::new();
        let user = Address::new([0x11; 20]);
        db.insert(user, U256::from(42_000_000_000_000_000_000u128));

        // ABI-encode: selector + 12 zero bytes + 20-byte address
        let mut input = vec![0x70, 0xa0, 0x82, 0x31];
        input.extend_from_slice(&[0u8; 12]);
        input.extend_from_slice(user.as_slice());

        let res = eth_erc20_handle(&db, Some(&input));
        assert_eq!(res.code, SUCCESS);
        let balance = U256::from_be_slice(&res.result);
        assert_eq!(balance, U256::from(42_000_000_000_000_000_000u128));
    }

    #[test]
    fn balance_of_unknown_account_returns_zero() {
        let db = MockDb::new();
        let mut input = vec![0x70, 0xa0, 0x82, 0x31];
        input.extend_from_slice(&[0u8; 12]);
        input.extend_from_slice(&[0xaa; 20]);

        let res = eth_erc20_handle(&db, Some(&input));
        assert_eq!(res.code, SUCCESS);
        assert_eq!(U256::from_be_slice(&res.result), U256::ZERO);
    }

    #[test]
    fn balance_of_input_too_short() {
        let db = MockDb::new();
        // selector + only 10 bytes (need 32)
        let input = vec![
            0x70, 0xa0, 0x82, 0x31, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let res = eth_erc20_handle(&db, Some(&input));
        assert_eq!(res.code, ERR_ARGS);
        assert!(res.err.contains("too short"));
    }

    #[test]
    fn balance_of_extra_trailing_bytes_ok() {
        let mut db = MockDb::new();
        let user = Address::new([0x22; 20]);
        db.insert(user, U256::from(1u64));

        let mut input = vec![0x70, 0xa0, 0x82, 0x31];
        input.extend_from_slice(&[0u8; 12]);
        input.extend_from_slice(user.as_slice());
        input.extend_from_slice(&[0xff; 32]); // extra trailing data

        let res = eth_erc20_handle(&db, Some(&input));
        assert_eq!(res.code, SUCCESS);
        assert_eq!(U256::from_be_slice(&res.result), U256::from(1u64));
    }

    // --- totalSupply() = 0x18160ddd ---

    #[test]
    fn total_supply_returns_one() {
        let db = MockDb::new();
        let input = [0x18, 0x16, 0x0d, 0xdd];
        let res = eth_erc20_handle(&db, Some(&input));
        assert_eq!(res.code, SUCCESS);
        assert_eq!(U256::from_be_slice(&res.result), U256::from(1u32));
    }

    // --- decimals() = 0x313ce567 ---

    #[test]
    fn decimals_returns_18() {
        let db = MockDb::new();
        let input = [0x31, 0x3c, 0xe5, 0x67];
        let res = eth_erc20_handle(&db, Some(&input));
        assert_eq!(res.code, SUCCESS);
        assert_eq!(U256::from_be_slice(&res.result), U256::from(18u32));
    }

    // --- name() = 0x06fdde03, symbol() = 0x95d89b41 ---

    #[test]
    fn name_returns_eth() {
        let db = MockDb::new();
        let input = [0x06, 0xfd, 0xde, 0x03];
        let res = eth_erc20_handle(&db, Some(&input));
        assert_eq!(res.code, SUCCESS);
        let decoded = sol_data::String::abi_decode(&res.result).unwrap();
        assert_eq!(decoded, "ETH");
    }

    #[test]
    fn symbol_returns_eth() {
        let db = MockDb::new();
        let input = [0x95, 0xd8, 0x9b, 0x41];
        let res = eth_erc20_handle(&db, Some(&input));
        assert_eq!(res.code, SUCCESS);
        let decoded = sol_data::String::abi_decode(&res.result).unwrap();
        assert_eq!(decoded, "ETH");
    }

    // --- error cases ---

    #[test]
    fn none_input_returns_error() {
        let db = MockDb::new();
        let res = eth_erc20_handle(&db, None);
        assert_eq!(res.code, ERR_ARGS);
        assert!(res.err.contains("missing"));
    }

    #[test]
    fn short_input_less_than_4_bytes() {
        let db = MockDb::new();
        let res = eth_erc20_handle(&db, Some(&[0x70, 0xa0]));
        assert_eq!(res.code, ERR_ARGS);
        assert!(res.err.contains("less than 4"));
    }

    #[test]
    fn unknown_selector_returns_not_found() {
        let db = MockDb::new();
        let input = [0xde, 0xad, 0xbe, 0xef];
        let res = eth_erc20_handle(&db, Some(&input));
        assert_eq!(res.code, ERR_NOT_FOUND);
    }
}
