use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::iter::once;

use ethereum_types::{Address, H256, U256};
use evm_arithmetization::generation::mpt::{AccountRlp, LegacyReceiptRlp};
use zk_evm_common::{EMPTY_CODE_HASH, EMPTY_TRIE_HASH};

use crate::hash;
use crate::typed_mpt::TrieKey;
use crate::PartialTriePreImages;
use crate::{ContractCodeUsage, TxnInfo};

const FIRST_PRECOMPILE_ADDRESS: U256 = U256([1, 0, 0, 0]);
const LAST_PRECOMPILE_ADDRESS: U256 = U256([10, 0, 0, 0]);

#[derive(Debug)]
pub(crate) struct ProcessedBlockTrace {
    pub tries: PartialTriePreImages,
    pub txn_info: Vec<ProcessedTxnInfo>,
    pub withdrawals: Vec<(Address, U256)>,
}

#[derive(Debug)]
pub(crate) struct ProcessedBlockTracePreImages {
    pub tries: PartialTriePreImages,
    pub extra_code_hash_mappings: Option<HashMap<H256, Vec<u8>>>,
}

#[derive(Debug, Default)]
pub(crate) struct ProcessedTxnInfo {
    pub nodes_used_by_txn: NodesUsedByTxn,
    pub contract_code_accessed: HashMap<H256, Vec<u8>>,
    pub meta: TxnMetaState,
}

pub(crate) struct CodeHashResolving<F> {
    /// If we have not seen this code hash before, use the resolve function that
    /// the client passes down to us. This will likely be an rpc call/cache
    /// check.
    pub client_code_hash_resolve_f: F,

    /// Code hash mappings that we have constructed from parsing the block
    /// trace. If there are any txns that create contracts, then they will also
    /// get added here as we process the deltas.
    pub extra_code_hash_mappings: HashMap<H256, Vec<u8>>,
}

impl<F: Fn(&H256) -> Vec<u8>> CodeHashResolving<F> {
    fn resolve(&mut self, c_hash: &H256) -> Vec<u8> {
        match self.extra_code_hash_mappings.get(c_hash) {
            Some(code) => code.clone(),
            None => (self.client_code_hash_resolve_f)(c_hash),
        }
    }

    fn insert_code(&mut self, c_hash: H256, code: Vec<u8>) {
        self.extra_code_hash_mappings.insert(c_hash, code);
    }
}

impl TxnInfo {
    pub(crate) fn into_processed_txn_info<F: Fn(&H256) -> Vec<u8>>(
        self,
        tries: &PartialTriePreImages,
        all_accounts_in_pre_image: &[(H256, AccountRlp)],
        extra_state_accesses: &[H256],
        code_hash_resolver: &mut CodeHashResolving<F>,
    ) -> ProcessedTxnInfo {
        let mut nodes_used_by_txn = NodesUsedByTxn::default();
        let mut contract_code_accessed = create_empty_code_access_map();

        for (addr, trace) in self.traces {
            let hashed_addr = hash(addr.as_bytes());

            let storage_writes = trace.storage_written.unwrap_or_default();

            let storage_read_keys = trace
                .storage_read
                .into_iter()
                .flat_map(|reads| reads.into_iter());

            let storage_write_keys = storage_writes.keys();
            let storage_access_keys = storage_read_keys.chain(storage_write_keys.copied());

            nodes_used_by_txn.storage_accesses.push((
                hashed_addr,
                storage_access_keys
                    .map(|H256(bytes)| TrieKey::from_hash(hash(bytes)))
                    .collect(),
            ));

            let storage_trie_change = !storage_writes.is_empty();
            let code_change = trace.code_usage.is_some();
            let state_write_occurred = trace.balance.is_some()
                || trace.nonce.is_some()
                || storage_trie_change
                || code_change;

            if state_write_occurred {
                let state_trie_writes = StateTrieWrites {
                    balance: trace.balance,
                    nonce: trace.nonce,
                    storage_trie_change,
                    code_hash: trace.code_usage.as_ref().map(|usage| usage.get_code_hash()),
                };

                nodes_used_by_txn
                    .state_writes
                    .push((hashed_addr, state_trie_writes))
            }

            let storage_writes_vec = storage_writes
                .into_iter()
                .map(|(k, v)| (TrieKey::from_hash(k), rlp::encode(&v).to_vec()))
                .collect();

            nodes_used_by_txn
                .storage_writes
                .push((hashed_addr, storage_writes_vec));

            let is_precompile = (FIRST_PRECOMPILE_ADDRESS..LAST_PRECOMPILE_ADDRESS)
                .contains(&U256::from_big_endian(&addr.0));

            // Trie witnesses will only include accessed precompile accounts as hash
            // nodes if the transaction calling them reverted. If this is the case, we
            // shouldn't include them in this transaction's `state_accesses` to allow the
            // decoder to build a minimal state trie without hitting any hash node.
            if !is_precompile
                || tries
                    .state
                    .get_by_key(TrieKey::from_hash(hashed_addr))
                    .is_some()
            {
                nodes_used_by_txn.state_accesses.push(hashed_addr);
            }

            if let Some(c_usage) = trace.code_usage {
                match c_usage {
                    ContractCodeUsage::Read(c_hash) => {
                        contract_code_accessed
                            .entry(c_hash)
                            .or_insert_with(|| code_hash_resolver.resolve(&c_hash));
                    }
                    ContractCodeUsage::Write(c_bytes) => {
                        let c_hash = hash(&c_bytes);

                        contract_code_accessed.insert(c_hash, c_bytes.clone());
                        code_hash_resolver.insert_code(c_hash, c_bytes);
                    }
                }
            }

            if trace.self_destructed.unwrap_or_default() {
                nodes_used_by_txn.self_destructed_accounts.push(hashed_addr);
            }
        }

        for &hashed_addr in extra_state_accesses {
            nodes_used_by_txn.state_accesses.push(hashed_addr);
        }

        let accounts_with_storage_accesses: HashSet<_> = HashSet::from_iter(
            nodes_used_by_txn
                .storage_accesses
                .iter()
                .filter(|(_, slots)| !slots.is_empty())
                .map(|(addr, _)| *addr),
        );

        let all_accounts_with_non_empty_storage = all_accounts_in_pre_image
            .iter()
            .filter(|(_, data)| data.storage_root != EMPTY_TRIE_HASH);

        let accounts_with_storage_but_no_storage_accesses = all_accounts_with_non_empty_storage
            .filter(|&(addr, _data)| !accounts_with_storage_accesses.contains(addr))
            .map(|(addr, data)| (*addr, data.storage_root));

        nodes_used_by_txn
            .state_accounts_with_no_accesses_but_storage_tries
            .extend(accounts_with_storage_but_no_storage_accesses);

        let txn_bytes = match self.meta.byte_code.is_empty() {
            false => Some(self.meta.byte_code),
            true => None,
        };

        let receipt_node_bytes =
            process_rlped_receipt_node_bytes(self.meta.new_receipt_trie_node_byte);

        let new_meta_state = TxnMetaState {
            txn_bytes,
            receipt_node_bytes,
            gas_used: self.meta.gas_used,
        };

        ProcessedTxnInfo {
            nodes_used_by_txn,
            contract_code_accessed,
            meta: new_meta_state,
        }
    }
}

fn process_rlped_receipt_node_bytes(raw_bytes: Vec<u8>) -> Vec<u8> {
    match rlp::decode::<LegacyReceiptRlp>(&raw_bytes) {
        Ok(_) => raw_bytes,
        Err(_) => {
            // Must be non-legacy.
            rlp::decode::<Vec<u8>>(&raw_bytes).unwrap()
        }
    }
}

fn create_empty_code_access_map() -> HashMap<H256, Vec<u8>> {
    HashMap::from_iter(once((EMPTY_CODE_HASH, Vec::new())))
}

/// Note that "*_accesses" includes writes.
#[derive(Debug, Default)]
pub(crate) struct NodesUsedByTxn {
    pub state_accesses: Vec<H256>,
    pub state_writes: Vec<(H256, StateTrieWrites)>,

    // Note: All entries in `storage_writes` also appear in `storage_accesses`.
    pub storage_accesses: Vec<(H256, Vec<TrieKey>)>,
    #[allow(clippy::type_complexity)]
    pub storage_writes: Vec<(H256, Vec<(TrieKey, Vec<u8>)>)>,
    pub state_accounts_with_no_accesses_but_storage_tries: HashMap<H256, H256>,
    pub self_destructed_accounts: Vec<H256>,
}

#[derive(Debug)]
pub(crate) struct StateTrieWrites {
    pub balance: Option<U256>,
    pub nonce: Option<U256>,
    pub storage_trie_change: bool,
    pub code_hash: Option<H256>,
}

#[derive(Debug, Default)]
pub(crate) struct TxnMetaState {
    pub txn_bytes: Option<Vec<u8>>,
    pub receipt_node_bytes: Vec<u8>,
    pub gas_used: u64,
}
