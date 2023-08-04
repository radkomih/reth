//! Wrapper around revms state.
use reth_db::{
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW},
    models::{AccountBeforeTx, BlockNumberAddress},
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_interfaces::db::DatabaseError;
use reth_primitives::{
    bloom::logs_bloom, keccak256, proofs::calculate_receipt_root_ref, Account, Address,
    BlockNumber, Bloom, Bytecode, Log, Receipt, StorageEntry, H256, U256,
};
use reth_revm_primitives::{
    db::states::{
        BundleState as RevmBundleState, StateChangeset as RevmChange, StateReverts as RevmReverts,
    },
    into_reth_acc, into_revm_acc,
    primitives::AccountInfo,
    to_reth_acc,
};
use reth_trie::{
    hashed_cursor::{HashedPostState, HashedPostStateCursorFactory, HashedStorage},
    StateRoot, StateRootError,
};
use std::collections::HashMap;

/// Bundle state of post execution changes and reverts
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct BundleState {
    /// Bundle state with reverts.
    bundle: RevmBundleState,
    /// Receipts
    receipts: Vec<Vec<Receipt>>,
    /// First block o bundle state.
    first_block: BlockNumber,
}

/// Type used to initialize revms bundle state.
pub type BundleStateInit =
    HashMap<Address, (Option<Account>, Option<Account>, HashMap<H256, (U256, U256)>)>;

/// Types used inside RevertsInit to initialize revms reverts.
pub type AccountRevertInit = (Option<Option<Account>>, Vec<StorageEntry>);

/// Type used to initialize revms reverts.
pub type RevertsInit = HashMap<BlockNumber, HashMap<Address, AccountRevertInit>>;

impl BundleState {
    /// Create Bundle State.
    pub fn new(
        bundle: RevmBundleState,
        receipts: Vec<Vec<Receipt>>,
        first_block: BlockNumber,
    ) -> Self {
        Self { bundle, receipts, first_block }
    }

    /// Create new bundle state with receipts.
    pub fn new_init(
        state_init: BundleStateInit,
        revert_init: RevertsInit,
        contracts_init: Vec<(H256, Bytecode)>,
        receipts: Vec<Vec<Receipt>>,
        first_block: BlockNumber,
    ) -> Self {
        // initialize revm bundle
        let bundle = RevmBundleState::new(
            state_init.into_iter().map(|(address, (original, present, storage))| {
                (
                    address,
                    original.map(into_revm_acc),
                    present.map(into_revm_acc),
                    storage.into_iter().map(|(k, s)| (k.into(), s)).collect(),
                )
            }),
            revert_init.into_iter().map(|(_, reverts)| {
                reverts.into_iter().map(|(address, (original, storage))| {
                    (
                        address,
                        original.map(|i| i.map(into_revm_acc)),
                        storage.into_iter().map(|entry| (entry.key.into(), entry.value)),
                    )
                })
            }),
            contracts_init.into_iter().map(|(code_hash, bytecode)| (code_hash, bytecode.0)),
        );

        Self { bundle, receipts, first_block }
    }

    /// Return revm bundle state.
    pub fn state(&self) -> &RevmBundleState {
        &self.bundle
    }

    /// Return iterator over all accounts
    pub fn accounts_iter(&self) -> impl Iterator<Item = (Address, Option<&AccountInfo>)> {
        self.bundle.state().iter().map(|(a, acc)| (*a, acc.info.as_ref())).into_iter()
    }

    /// Get account if account is known.
    pub fn account(&self, address: &Address) -> Option<Option<Account>> {
        self.bundle.account(address).map(|a| a.info.as_ref().map(to_reth_acc))
    }

    /// Get storage if value is known.
    ///
    /// This means that depending on status we can potentially return U256::ZERO.
    pub fn storage(&self, address: &Address, storage_key: U256) -> Option<U256> {
        self.bundle.account(address).and_then(|a| a.storage_slot(storage_key))
    }

    /// Return bytecode if known.
    pub fn bytecode(&self, code_hash: &H256) -> Option<Bytecode> {
        self.bundle.bytecode(code_hash).map(|b| Bytecode(b))
    }

    /// Hash all changed accounts and storage entries that are currently stored in the post state.
    ///
    /// # Returns
    ///
    /// The hashed post state.
    fn hash_state_slow(&self) -> HashedPostState {
        //let mut storages = BTreeMap::default();
        let mut hashed_state = HashedPostState::default();
        for (address, account) in self.bundle.state() {
            let hashed_address = keccak256(address);
            if let Some(account) = &account.info {
                hashed_state.insert_account(hashed_address, to_reth_acc(account))
            } else {
                hashed_state.insert_cleared_account(hashed_address);
            }

            // insert storage.
            let mut hashed_storage = HashedStorage::new(account.status.was_destroyed());
            for (key, value) in account.storage.iter() {
                let hashed_key = keccak256(H256(key.to_be_bytes()));
                if value.present_value == U256::ZERO {
                    hashed_storage.insert_zero_valued_slot(hashed_key);
                } else {
                    hashed_storage.insert_non_zero_valued_storage(hashed_key, value.present_value);
                }
            }
            hashed_state.insert_hashed_storage(hashed_address, hashed_storage)
        }
        hashed_state.sorted()
    }

    /// Calculate the state root for this [PostState].
    /// Internally, function calls [Self::hash_state_slow] to obtain the [HashedPostState].
    /// Afterwards, it retrieves the prefixsets from the [HashedPostState] and uses them to
    /// calculate the incremental state root.
    ///
    /// # Example
    ///
    /// ```
    /// use reth_primitives::{Address, Account};
    /// use reth_provider::PostState;
    /// use reth_db::{mdbx::{EnvKind, WriteMap, test_utils::create_test_db}, database::Database};
    ///
    /// // Initialize the database
    /// let db = create_test_db::<WriteMap>(EnvKind::RW);
    ///
    /// // Initialize the post state
    /// let mut post_state = PostState::new();
    ///
    /// // Create an account
    /// let block_number = 1;
    /// let address = Address::random();
    /// post_state.create_account(1, address, Account { nonce: 1, ..Default::default() });
    ///
    /// // Calculate the state root
    /// let tx = db.tx().expect("failed to create transaction");
    /// let state_root = post_state.state_root_slow(&tx);
    /// ```
    ///
    /// # Returns
    ///
    /// The state root for this [PostState].
    pub fn state_root_slow<'a, 'tx, TX: DbTx<'tx>>(
        &self,
        tx: &'a TX,
    ) -> Result<H256, StateRootError> {
        let hashed_post_state = self.hash_state_slow();
        let (account_prefix_set, storage_prefix_set) = hashed_post_state.construct_prefix_sets();
        let hashed_cursor_factory = HashedPostStateCursorFactory::new(tx, &hashed_post_state);
        StateRoot::new(tx)
            .with_hashed_cursor_factory(&hashed_cursor_factory)
            .with_changed_account_prefixes(account_prefix_set)
            .with_changed_storage_prefixes(storage_prefix_set)
            .root()
    }

    /// Transform block number to the index of block.
    fn block_number_to_index(&self, block_number: BlockNumber) -> Option<usize> {
        if self.first_block > block_number {
            return None
        }
        let index = block_number - self.first_block;
        if index >= self.receipts.len() as u64 {
            return None
        }
        Some(index as usize)
    }

    /// Returns an iterator over all block logs.
    pub fn logs(&self, block_number: BlockNumber) -> Option<impl Iterator<Item = &Log>> {
        let index = self.block_number_to_index(block_number)?;
        Some(self.receipts[index].iter().flat_map(|r| r.logs.iter()))
    }

    /// Return blocks logs bloom
    pub fn block_logs_bloom(&self, block_number: BlockNumber) -> Option<Bloom> {
        Some(logs_bloom(self.logs(block_number)?))
    }

    /// Returns the receipt root for all recorded receipts.
    /// Note: this function calculated Bloom filters for every receipt and created merkle trees
    /// of receipt. This is a expensive operation.
    pub fn receipts_root_slow(&self, block_number: BlockNumber) -> Option<H256> {
        let index = self.block_number_to_index(block_number)?;
        Some(calculate_receipt_root_ref(&self.receipts[index]))
    }

    /// Return reference to receipts.
    pub fn receipts(&self) -> &Vec<Vec<Receipt>> {
        &self.receipts
    }

    /// Return all block receipts
    pub fn receipts_by_block(&self, block_number: BlockNumber) -> &[Receipt] {
        let Some(index) = self.block_number_to_index(block_number) else { return &[] };
        self.receipts[index].as_slice()
    }

    /// Number of blocks in bundle state.
    pub fn len(&self) -> usize {
        self.receipts.len()
    }

    /// Return first block of the bundle
    pub fn first_block(&self) -> BlockNumber {
        self.first_block
    }

    /// Return last block of the bundle.
    pub fn last_block(&self) -> BlockNumber {
        self.first_block + self.len() as BlockNumber
    }

    /// Revert to given block number.
    ///
    /// Note: Give Block number will stay inside the bundle state.
    pub fn revert_to(&mut self, block_number: BlockNumber) {
        let Some(index) = self.block_number_to_index(block_number) else { return };

        // +1 is for number of blocks that we have as index is included.
        let new_len = self.len() - (index + 1);
        let rm_trx: usize = self.len() - new_len;

        // remove receipts
        self.receipts.truncate(new_len);
        // Revert last n reverts.
        self.bundle.revert(rm_trx);
    }

    /// This will detach lower part of the chain and return it back.
    /// Specified block number will be included in detachment
    ///
    /// Detached part BundleState will become broken as it will not contain plain state.
    ///
    /// This plain state will contains some additional informations.
    ///
    /// If block number is in future, return None.
    pub fn detach_lower_part_at(&mut self, block_number: BlockNumber) -> Option<Self> {
        let last_block = self.last_block();
        let first_block = self.first_block;
        if block_number >= last_block {
            return None
        }
        if block_number < first_block {
            return Some(Self::default())
        }

        // detached number should be included so we are adding +1 to it.
        // for example if block number is same as first_block then
        // number of detached block shoud be 1.
        let num_of_detached_block = (block_number - first_block) + 1;

        let mut detached_bundle_state: BundleState = self.clone();
        detached_bundle_state.revert_to(block_number);

        // split is done as [0, num) and [num, len]
        let (_, this) = self.receipts.split_at(num_of_detached_block as usize);

        self.receipts = this.to_vec().clone();
        self.bundle
            .detach_lower_part_reverts(num_of_detached_block as usize)
            .expect("there should be detachments");

        self.first_block = block_number + 1;

        Some(detached_bundle_state)
    }

    /// Extend one state from another
    ///
    /// For state this is very sensitive opperation and should be used only when
    /// we know that other state was build on top of this one.
    /// In most cases this would be true.
    pub fn extend(&mut self, other: Self) {
        self.bundle.extend(other.bundle);
        self.receipts.extend(other.receipts);
    }

    /// Write bundle state to database.
    ///
    /// `omit_changed_check` should be set to true of bundle has some of it data
    /// detached, This would make some original values not known.
    pub fn write_to_db<'a, TX: DbTxMut<'a> + DbTx<'a>>(
        mut self,
        tx: &TX,
        omit_changed_check: bool,
    ) -> Result<(), DatabaseError> {
        // write receipts
        let mut receipts_cursor = tx.cursor_write::<tables::Receipts>()?;
        let mut next_number = receipts_cursor.last()?.map(|(i, _)| i + 1).unwrap_or_default();
        for block_receipts in self.receipts.into_iter() {
            for receipt in block_receipts {
                receipts_cursor.append(next_number, receipt)?;
                next_number += 1;
            }
        }
        StateReverts(self.bundle.take_reverts()).write_to_db(tx, self.first_block)?;

        StateChange(self.bundle.take_sorted_plain_change_inner(omit_changed_check))
            .write_to_db(tx)?;

        Ok(())
    }
}

/// Revert of the state.
#[derive(Default)]
pub struct StateReverts(pub RevmReverts);

impl From<RevmReverts> for StateReverts {
    fn from(revm: RevmReverts) -> Self {
        Self(revm)
    }
}

impl StateReverts {
    /// Write reverts to database.
    ///
    /// Note:: Reverts will delete all wiped storage from plain state.
    pub fn write_to_db<'a, TX: DbTxMut<'a> + DbTx<'a>>(
        self,
        tx: &TX,
        first_block: BlockNumber,
    ) -> Result<(), DatabaseError> {
        // Write storage changes
        tracing::trace!(target: "provider::reverts", "Writing storage changes");
        let mut storages_cursor = tx.cursor_dup_write::<tables::PlainStorageState>()?;
        let mut storage_changeset_cursor = tx.cursor_dup_write::<tables::StorageChangeSet>()?;
        for (block_number, storage_changes) in self.0.storage.into_iter().enumerate() {
            let block_number = first_block + block_number as BlockNumber;

            tracing::trace!(target: "provider::reverts", block_number=block_number,"Writing block change");
            for (address, wipe_storage, storage) in storage_changes.into_iter() {
                let storage_id = BlockNumberAddress((block_number, address));
                tracing::trace!(target: "provider::reverts","Writting revert for {:?}", address);
                // If we are writing the primary storage wipe transition, the pre-existing plain
                // storage state has to be taken from the database and written to storage history.
                // See [StorageWipe::Primary] for more details.
                let mut wiped_storage: Vec<(U256, U256)> = Vec::new();
                if wipe_storage {
                    tracing::trace!(target: "provider::reverts", "wipe storage storage changes");
                    if let Some((_, entry)) = storages_cursor.seek_exact(address)? {
                        wiped_storage.push((entry.key.into(), entry.value));
                        while let Some(entry) = storages_cursor.next_dup_val()? {
                            wiped_storage.push((entry.key.into(), entry.value))
                        }
                        // delete all values
                        storages_cursor.seek_exact(address)?;
                        storages_cursor.delete_current_duplicates()?;
                    }
                }
                tracing::trace!(target: "provider::reverts", "storage changes: {:?}",storage);
                // if empty just write storage reverts.
                if wiped_storage.is_empty() {
                    for (slot, old_value) in storage {
                        storage_changeset_cursor.append_dup(
                            storage_id,
                            StorageEntry { key: H256(slot.to_be_bytes()), value: old_value },
                        )?;
                    }
                } else {
                    // if there is some of wiped storage, they are both sorted, intersect both of
                    // them and in conflict use change from revert (discard values from wiped
                    // storage).
                    let mut wiped_iter = wiped_storage.into_iter();
                    let mut revert_iter = storage.into_iter();

                    // items to apply. both iterators are sorted.
                    let mut wiped_item = wiped_iter.next();
                    let mut revert_item = revert_iter.next();
                    loop {
                        let apply = match (wiped_item, revert_item) {
                            (None, None) => break,
                            (Some(w), None) => {
                                wiped_item = wiped_iter.next();
                                w
                            }
                            (None, Some(r)) => {
                                revert_item = revert_iter.next();
                                r
                            }
                            (Some(w), Some(r)) => {
                                match w.0.cmp(&r.0) {
                                    std::cmp::Ordering::Less => {
                                        // next key is from revert storage
                                        wiped_item = wiped_iter.next();
                                        w
                                    }
                                    std::cmp::Ordering::Greater => {
                                        // next key is from wiped storage
                                        revert_item = revert_iter.next();
                                        r
                                    }
                                    std::cmp::Ordering::Equal => {
                                        // priority goes for storage if key is same.
                                        wiped_item = wiped_iter.next();
                                        revert_item = revert_iter.next();
                                        r
                                    }
                                }
                            }
                        };

                        storage_changeset_cursor.append_dup(
                            storage_id,
                            StorageEntry { key: H256(apply.0.to_be_bytes()), value: apply.1 },
                        )?;
                    }
                }
            }
        }

        // Write account changes
        tracing::trace!(target: "provider::reverts", "Writing account changes");
        let mut account_changeset_cursor = tx.cursor_dup_write::<tables::AccountChangeSet>()?;
        for (block_number, account_block_reverts) in self.0.accounts.into_iter().enumerate() {
            let block_number = first_block + block_number as BlockNumber;
            for (address, info) in account_block_reverts {
                account_changeset_cursor.append_dup(
                    block_number,
                    AccountBeforeTx { address, info: info.map(into_reth_acc) },
                )?;
            }
        }

        Ok(())
    }
}

/// A change to the state of the world.
#[derive(Default)]
pub struct StateChange(pub RevmChange);

impl From<RevmChange> for StateChange {
    fn from(revm: RevmChange) -> Self {
        Self(revm)
    }
}

impl StateChange {
    /// Write the post state to the database.
    pub fn write_to_db<'a, TX: DbTxMut<'a> + DbTx<'a>>(self, tx: &TX) -> Result<(), DatabaseError> {
        // Write new storage state
        tracing::trace!(target: "provider::post_state", len = self.0.storage.len(), "Writing new storage state");
        let mut storages_cursor = tx.cursor_dup_write::<tables::PlainStorageState>()?;
        for (address, (_wipped, storage)) in self.0.storage.into_iter() {
            // Wipping of storage is done when appling the reverts.

            for (key, value) in storage.into_iter() {
                tracing::trace!(target: "provider::post_state", ?address, ?key, "Updating plain state storage");
                let key: H256 = key.into();
                if let Some(entry) = storages_cursor.seek_by_key_subkey(address, key)? {
                    if entry.key == key {
                        storages_cursor.delete_current()?;
                    }
                }

                if value != U256::ZERO {
                    storages_cursor.upsert(address, StorageEntry { key, value })?;
                }
            }
        }

        // Write new account state
        tracing::trace!(target: "provider::post_state", len = self.0.accounts.len(), "Writing new account state");
        let mut accounts_cursor = tx.cursor_write::<tables::PlainAccountState>()?;
        for (address, account) in self.0.accounts.into_iter() {
            if let Some(account) = account {
                tracing::trace!(target: "provider::post_state", ?address, "Updating plain state account");
                accounts_cursor.upsert(address, into_reth_acc(account))?;
            } else if accounts_cursor.seek_exact(address)?.is_some() {
                tracing::trace!(target: "provider::post_state", ?address, "Deleting plain state account");
                accounts_cursor.delete_current()?;
            }
        }

        // Write bytecode
        tracing::trace!(target: "provider::post_state", len = self.0.contracts.len(), "Writing bytecodes");
        let mut bytecodes_cursor = tx.cursor_write::<tables::Bytecodes>()?;
        for (hash, bytecode) in self.0.contracts.into_iter() {
            bytecodes_cursor.upsert(hash, Bytecode(bytecode))?;
        }
        Ok(())
    }
}
