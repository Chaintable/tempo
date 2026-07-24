//! A database wrapper that captures all state changes for diff extraction.
//!
//! Ported from reth-x `crates/rpc/rpc-eth-types/src/cache/db.rs`.

use alloy_primitives::{Address, B256, U256, map::AddressMap};
use revm::{
    Database, DatabaseCommit, DatabaseRef,
    database::InMemoryDB,
    state::{Account, AccountInfo, Bytecode},
};

/// A database that wraps an external database and an in-memory diff database.
///
/// - `Database` (mutable) reads/writes go through `db` (typically a `StateCacheDb`
///   which has an internal cache). During block replay, subsequent txs see prior
///   commits via this cache.
/// - `DatabaseRef` (immutable) reads also delegate to `db`, but `State`'s
///   `DatabaseRef` impl bypasses its cache and reads the underlying provider
///   directly. This means `basic_ref()` returns the original parent-block state,
///   not state modified by prior commits. This is fine for our use case — only
///   `pre_db` uses `DatabaseRef` for diff comparison.
/// - `DatabaseCommit` writes to both `diff` (captures all changes for state-diff
///   extraction) and `db` (updates state for subsequent txs).
#[derive(Debug, Clone)]
pub struct StateDiffTraceDB<ExtDB> {
    /// The diff that stores all state changes.
    pub diff: InMemoryDB,
    /// The underlying database (read+write).
    pub db: ExtDB,
}

impl<ExtDB> StateDiffTraceDB<ExtDB> {
    pub fn new(db: ExtDB) -> Self {
        Self {
            diff: InMemoryDB::default(),
            db,
        }
    }
}

impl<ExtDB: Database> Database for StateDiffTraceDB<ExtDB> {
    type Error = ExtDB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.db.basic(address)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.db.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.db.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash(number)
    }
}

impl<ExtDB: DatabaseRef> DatabaseRef for StateDiffTraceDB<ExtDB> {
    type Error = ExtDB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.db.basic_ref(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.db.code_by_hash_ref(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.db.storage_ref(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash_ref(number)
    }
}

impl<ExtDB: DatabaseCommit> DatabaseCommit for StateDiffTraceDB<ExtDB> {
    fn commit(&mut self, changes: AddressMap<Account>) {
        self.diff.commit(changes.clone());
        self.db.commit(changes);
    }
}
