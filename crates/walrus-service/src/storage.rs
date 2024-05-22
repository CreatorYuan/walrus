// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

#![allow(unused)]

use std::{
    collections::{hash_map::Entry, BinaryHeap, HashMap, HashSet},
    fmt::Debug,
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use rocksdb::{ColumnFamily, ColumnFamilyDescriptor, DBCommon, MergeOperands, Options, DB};
use serde::{Deserialize, Serialize};
use sui_sdk::types::{digests::TransactionDigest, event::EventID};
use tracing::Level;
use typed_store::{
    rocks::{
        self,
        errors::typed_store_err_from_bcs_err,
        DBMap,
        MetricConf,
        ReadWriteOptions,
        RocksDB,
    },
    Map,
    TypedStoreError,
};
use walrus_core::{
    merkle::Node as MerkleNode,
    metadata::{
        BlobMetadata,
        BlobMetadataWithId,
        UnverifiedBlobMetadataWithId,
        VerifiedBlobMetadataWithId,
    },
    BlobId,
    ShardIndex,
};
use walrus_sui::types::{Blob, BlobEvent, BlobRegistered};

use self::{
    blob_info::{BlobCertificationStatus, BlobInfo, BlobInfoMergeOperand},
    event_sequencer::EventSequencer,
    shard::ShardStorage,
};
use crate::storage::blob_info::Mergeable as _;

pub(crate) mod blob_info;
mod event_sequencer;
mod shard;

/// Storage backing a [`StorageNode`][crate::StorageNode].
///
/// Enables storing blob metadata, which is shared across all shards. The method
/// [`shard_storage()`][Self::shard_storage] can be used to retrieve shard-specific storage.
#[derive(Debug, Clone)]
pub struct Storage {
    database: Arc<RocksDB>,
    metadata: DBMap<BlobId, BlobMetadata>,
    blob_info: DBMap<BlobId, BlobInfo>,
    event_cursor: DBMap<String, EventID>,
    shards: HashMap<ShardIndex, ShardStorage>,
    event_queue: Arc<Mutex<EventSequencer>>,
}

impl Storage {
    const METADATA_COLUMN_FAMILY_NAME: &'static str = "metadata";
    const BLOBINFO_COLUMN_FAMILY_NAME: &'static str = "blob_info";
    const EVENT_CURSOR_COLUMN_FAMILY_NAME: &'static str = "event_cursor";
    const EVENT_CURSOR_KEY: &'static str = "event_cursor";

    /// Opens the storage database located at the specified path, creating the database if absent.
    pub fn open(path: &Path, metrics_config: MetricConf) -> Result<Self, anyhow::Error> {
        let mut db_opts = Options::default();
        db_opts.create_missing_column_families(true);
        db_opts.create_if_missing(true);

        let existing_shards_ids = ShardStorage::existing_shards(path, &db_opts);
        let mut shard_column_families: Vec<_> = existing_shards_ids
            .iter()
            .copied()
            .map(ShardStorage::slivers_column_family_options)
            .collect();

        let (metadata_cf_name, metadata_options) = Self::metadata_options();
        let (blob_info_cf_name, blob_info_options) = Self::blob_info_options();
        let (event_cursor_cf_name, event_cursor_options) = Self::event_cursor_options();

        let mut expected_column_families: Vec<_> = shard_column_families
            .iter_mut()
            .map(|(name, opts)| (name.as_str(), std::mem::take(opts)))
            .chain([
                (metadata_cf_name, metadata_options),
                (blob_info_cf_name, blob_info_options),
                (event_cursor_cf_name, event_cursor_options),
            ])
            .collect();

        let database = rocks::open_cf_opts(
            path,
            Some(db_opts),
            metrics_config,
            &expected_column_families,
        )?;
        let metadata = DBMap::reopen(
            &database,
            Some(metadata_cf_name),
            &ReadWriteOptions::default(),
        )?;
        let event_cursor = DBMap::reopen(
            &database,
            Some(event_cursor_cf_name),
            &ReadWriteOptions::default(),
        )?;
        let blob_info = DBMap::reopen(
            &database,
            Some(blob_info_cf_name),
            &ReadWriteOptions::default(),
        )?;
        let shards = existing_shards_ids
            .into_iter()
            .map(|id| ShardStorage::create_or_reopen(id, &database).map(|shard| (id, shard)))
            .collect::<Result<_, _>>()?;

        Ok(Self {
            database,
            metadata,
            blob_info,
            event_cursor,
            shards,
            event_queue: Arc::new(Mutex::new(EventSequencer::default())),
        })
    }

    /// Returns the storage for the specified shard, creating it if it does not exist.
    pub fn create_storage_for_shard(
        &mut self,
        shard: ShardIndex,
    ) -> Result<&ShardStorage, TypedStoreError> {
        match self.shards.entry(shard) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                let shard_storage = ShardStorage::create_or_reopen(shard, &self.database)?;
                Ok(entry.insert(shard_storage))
            }
        }
    }

    /// Returns the indices of the shards managed by the storage.
    pub fn shards(&self) -> Vec<ShardIndex> {
        self.shards.keys().copied().collect()
    }

    /// Returns a handle over the storage for a single shard.
    pub fn shard_storage(&self, shard: ShardIndex) -> Option<&ShardStorage> {
        self.shards.get(&shard)
    }

    /// Store the verified metadata.
    pub fn put_verified_metadata(
        &self,
        metadata: &VerifiedBlobMetadataWithId,
    ) -> Result<(), TypedStoreError> {
        self.put_metadata(metadata.blob_id(), metadata.metadata())
    }

    fn put_metadata(
        &self,
        blob_id: &BlobId,
        metadata: &BlobMetadata,
    ) -> Result<(), TypedStoreError> {
        self.metadata.insert(blob_id, metadata)?;
        self.merge_update_blob_info(blob_id, BlobInfoMergeOperand::MarkMetadataStored)
    }

    /// Get the blob info for `blob_id`
    pub fn get_blob_info(&self, blob_id: &BlobId) -> Result<Option<BlobInfo>, TypedStoreError> {
        self.blob_info.get(blob_id)
    }

    /// Get the event cursor for `event_type`
    pub fn get_event_cursor(&self) -> Result<Option<EventID>, TypedStoreError> {
        self.event_cursor.get(&Self::EVENT_CURSOR_KEY.to_string())
    }

    /// Update the blob info for a blob based on the `BlobEvent`
    #[tracing::instrument(level = Level::DEBUG, skip(self))]
    pub fn update_blob_info(&self, event: &BlobEvent) -> Result<(), TypedStoreError> {
        self.merge_update_blob_info(&event.blob_id(), event.into())?;
        Ok(())
    }

    /// Update the blob info for `blob_id` to `new_state` using the merge operation
    fn merge_update_blob_info(
        &self,
        blob_id: &BlobId,
        operation: BlobInfoMergeOperand,
    ) -> Result<(), TypedStoreError> {
        tracing::debug!(?operation, %blob_id, "updating blob info");

        let mut batch = self.blob_info.batch();
        batch.partial_merge_batch(&self.blob_info, [(blob_id, operation.to_bytes())])?;
        batch.write()
    }

    /// Advances the event cursor to the most recent, sequential event observed.
    ///
    /// The sequence-id is a sequence number of the order in which cursors were observed from the
    /// chain, and must start from 0 with the cursor following `get_event_cursor()`
    /// after the database is re-opened.
    ///
    /// For calls to this function such as `(0, cursor0), (2, cursor2), (1, cursor1)`, the cursor
    /// will advance to `cursor0` after the first call since it is the first in next in the
    /// sequence; will remain at `cursor0` after the next call since `cursor2` is not the next in
    /// sequence; and will advance to cursor2 after the 3rd call, since `cursor1` fills the gap as
    /// identified by its sequence number.
    pub fn maybe_advance_event_cursor(
        &self,
        sequence_number: usize,
        cursor: &EventID,
    ) -> Result<(), TypedStoreError> {
        let mut event_queue = self.event_queue.lock().unwrap();

        event_queue.add(sequence_number, *cursor);

        if let Some(cursor) = event_queue.peek() {
            self.event_cursor
                .insert(&Self::EVENT_CURSOR_KEY.to_string(), cursor)?;

            // Only pop from the event queue if the write was successful,
            // otherwise we may lose events.
            let _ = event_queue.pop();
        }

        Ok(())
    }

    /// Returns true if the metadata for the specified blob is stored.
    pub fn has_metadata(&self, blob_id: &BlobId) -> Result<bool, TypedStoreError> {
        Ok(self
            .get_blob_info(blob_id)?
            .map(|info| info.is_metadata_stored)
            .unwrap_or_default())
    }

    /// Gets the metadata for a given [`BlobId`] or None.
    pub fn get_metadata(
        &self,
        blob_id: &BlobId,
    ) -> Result<Option<VerifiedBlobMetadataWithId>, TypedStoreError> {
        Ok(self
            .metadata
            .get(blob_id)?
            .map(|inner| VerifiedBlobMetadataWithId::new_verified_unchecked(*blob_id, inner)))
    }

    /// Returns true if the sliver pairs for the provided blob-id is stored at
    /// all of the storage's shards.
    pub fn is_stored_at_all_shards(&self, blob_id: &BlobId) -> Result<bool, TypedStoreError> {
        for shard in self.shards.values() {
            if !shard.is_sliver_pair_stored(blob_id)? {
                return Ok(false);
            }
        }

        Ok(!self.shards.is_empty())
    }

    /// Returns a list of identifiers of the shards that store their
    /// respective sliver for the specified blob.
    pub fn shards_with_sliver_pairs(
        &self,
        blob_id: &BlobId,
    ) -> Result<Vec<ShardIndex>, TypedStoreError> {
        let mut shards_with_sliver_pairs = Vec::with_capacity(self.shards.len());

        for shard in self.shards.values() {
            if shard.is_sliver_pair_stored(blob_id)? {
                shards_with_sliver_pairs.push(shard.id());
            }
        }

        Ok(shards_with_sliver_pairs)
    }

    fn metadata_options() -> (&'static str, Options) {
        let mut options = Options::default();

        // TODO(jsmith): Tune storage for metadata and slivers (#65)
        options.set_enable_blob_files(true);

        (Self::METADATA_COLUMN_FAMILY_NAME, options)
    }

    fn blob_info_options() -> (&'static str, Options) {
        let mut options = Options::default();
        options.set_merge_operator("merge blob info", merge_blob_info, |_, _, _| None);
        (Self::BLOBINFO_COLUMN_FAMILY_NAME, options)
    }

    fn event_cursor_options() -> (&'static str, Options) {
        let mut options = Options::default();
        (Self::EVENT_CURSOR_COLUMN_FAMILY_NAME, options)
    }

    /// Returns the shards currently present in the storage.
    pub(crate) fn shards_present(&self) -> Vec<ShardIndex> {
        self.shards.keys().copied().collect()
    }
}

#[tracing::instrument(level = Level::DEBUG, skip(operands))]
fn merge_blob_info(
    key: &[u8],
    existing_val: Option<&[u8]>,
    operands: &MergeOperands,
) -> Option<Vec<u8>> {
    let mut current_val: Option<BlobInfo> = existing_val.and_then(deserialize_from_db);

    for operand_bytes in operands {
        let Some(operand) = deserialize_from_db::<BlobInfoMergeOperand>(operand_bytes) else {
            continue;
        };
        tracing::debug!("updating {current_val:?} with {operand:?}");

        current_val = if let Some(info) = current_val {
            Some(info.merge(operand))
        } else if let BlobInfoMergeOperand::ChangeStatus { end_epoch, status } = operand {
            Some(BlobInfo::new(end_epoch, status))
        } else {
            // TODO(jsmith): Deserialize the key.
            tracing::error!(?key, "attempted to mutate the info for an untracked blob");
            None
        };
    }

    current_val.map(BlobInfo::to_bytes)
}

fn deserialize_from_db<'de, T>(val: &'de [u8]) -> Option<T>
where
    T: Deserialize<'de>,
{
    bcs::from_bytes(val)
        .inspect_err(|error| {
            tracing::error!(?error, data=?val, "failed to deserialize value stored in database")
        })
        .ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use prometheus::Registry;
    use sui_sdk::types::digests::TransactionDigest;
    use tempfile::TempDir;
    use tokio::runtime::Runtime;
    use walrus_core::{
        encoding::{EncodingAxis, Sliver as TypedSliver},
        Sliver,
        SliverIndex,
        SliverType,
    };
    use walrus_sui::test_utils::event_id_for_testing;
    use walrus_test_utils::{async_param_test, param_test, Result as TestResult, WithTempDir};

    use super::*;
    use crate::test_utils::empty_storage_with_shards;

    type StorageSpec<'a> = &'a [(ShardIndex, Vec<(BlobId, WhichSlivers)>)];

    pub(crate) enum WhichSlivers {
        Primary,
        Secondary,
        Both,
    }

    pub(crate) const BLOB_ID: BlobId = BlobId([7; 32]);
    pub(crate) const SHARD_INDEX: ShardIndex = ShardIndex(3);
    pub(crate) const OTHER_SHARD_INDEX: ShardIndex = ShardIndex(9);

    /// Returns an empty storage, with the column families for [`SHARD_INDEX`] already created.
    pub(crate) fn empty_storage() -> WithTempDir<Storage> {
        typed_store::metrics::DBMetrics::init(&Registry::new());
        empty_storage_with_shards(&[SHARD_INDEX])
    }

    pub(crate) fn get_typed_sliver<E: EncodingAxis>(seed: u8) -> TypedSliver<E> {
        TypedSliver::new(
            vec![seed; usize::from(seed) * 512],
            16.try_into().unwrap(),
            SliverIndex(0),
        )
    }

    pub(crate) fn get_sliver(sliver_type: SliverType, seed: u8) -> Sliver {
        match sliver_type {
            SliverType::Primary => Sliver::Primary(get_typed_sliver(seed)),
            SliverType::Secondary => Sliver::Secondary(get_typed_sliver(seed)),
        }
    }

    pub(crate) fn populated_storage(spec: StorageSpec) -> TestResult<WithTempDir<Storage>> {
        let mut storage = empty_storage();

        let mut seed = 10u8;
        for (shard, sliver_list) in spec {
            storage.as_mut().create_storage_for_shard(*shard)?;
            let shard_storage = storage.as_ref().shard_storage(*shard).unwrap();

            for (blob_id, which) in sliver_list.iter() {
                if matches!(*which, WhichSlivers::Primary | WhichSlivers::Both) {
                    shard_storage.put_sliver(blob_id, &get_sliver(SliverType::Primary, seed))?;
                    seed += 1;
                }
                if matches!(*which, WhichSlivers::Secondary | WhichSlivers::Both) {
                    shard_storage.put_sliver(blob_id, &get_sliver(SliverType::Secondary, seed))?;
                    seed += 1;
                }
            }
        }

        Ok(storage)
    }

    #[tokio::test]
    async fn can_write_then_read_metadata() -> TestResult {
        let storage = empty_storage();
        let storage = storage.as_ref();
        let metadata = walrus_core::test_utils::verified_blob_metadata();
        let blob_id = metadata.blob_id();
        let expected = VerifiedBlobMetadataWithId::new_verified_unchecked(
            *blob_id,
            metadata.metadata().clone(),
        );

        storage.put_metadata(metadata.blob_id(), metadata.metadata())?;
        let retrieved = storage.get_metadata(blob_id)?;

        assert_eq!(retrieved, Some(expected));

        Ok(())
    }

    #[tokio::test]
    async fn update_blob_info() -> TestResult {
        let storage = empty_storage();
        let storage = storage.as_ref();
        let blob_id = BLOB_ID;

        let state0 = BlobInfo::new(42, BlobCertificationStatus::Registered);
        let state1 = BlobInfo::new(42, BlobCertificationStatus::Certified);

        storage.merge_update_blob_info(
            &blob_id,
            BlobInfoMergeOperand::ChangeStatus {
                end_epoch: 42,
                status: BlobCertificationStatus::Registered,
            },
        )?;
        assert_eq!(storage.get_blob_info(&blob_id)?, Some(state0));
        storage.merge_update_blob_info(
            &blob_id,
            BlobInfoMergeOperand::ChangeStatus {
                end_epoch: 42,
                status: BlobCertificationStatus::Certified,
            },
        )?;
        assert_eq!(storage.get_blob_info(&blob_id)?, Some(state1));
        Ok(())
    }

    #[tokio::test]
    async fn update_blob_info_stored() -> TestResult {
        let storage = empty_storage();
        let storage = storage.as_ref();
        let blob_id = BLOB_ID;

        let state0 = BlobInfo::new(42, BlobCertificationStatus::Registered);
        let state1 = BlobInfo {
            is_metadata_stored: true,
            ..state0
        };

        storage.merge_update_blob_info(
            &blob_id,
            BlobInfoMergeOperand::ChangeStatus {
                end_epoch: 42,
                status: BlobCertificationStatus::Registered,
            },
        )?;
        assert_eq!(storage.get_blob_info(&blob_id)?, Some(state0));
        storage.merge_update_blob_info(&blob_id, BlobInfoMergeOperand::MarkMetadataStored)?;
        assert_eq!(storage.get_blob_info(&blob_id)?, Some(state1));

        Ok(())
    }

    async_param_test! {
        maybe_advance_event_cursor_order -> TestResult: [
            in_order: (&[0, 1, 2], &[0, 1, 2]),
            out_of_order: (&[0, 3, 2, 1], &[0, 0, 0, 3]),
        ]
    }
    async fn maybe_advance_event_cursor_order(
        sequence_ids: &[usize],
        expected_sequence: &[usize],
    ) -> TestResult {
        let storage = empty_storage();
        let storage = storage.as_ref();

        let cursors: Vec<_> = sequence_ids
            .iter()
            .map(|&seq_id| (seq_id, event_id_for_testing()))
            .collect();
        let cursor_lookup: HashMap<_, _> = cursors.clone().into_iter().collect();

        for ((seq_id, cursor), expected_observed) in cursors.iter().zip(expected_sequence) {
            storage.maybe_advance_event_cursor(*seq_id, cursor)?;

            assert_eq!(
                storage.get_event_cursor()?,
                Some(cursor_lookup[expected_observed])
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn maybe_advance_event_cursor_missed_zero() -> TestResult {
        let storage = empty_storage();
        let storage = storage.as_ref();

        storage.maybe_advance_event_cursor(1, &event_id_for_testing());
        assert_eq!(storage.get_event_cursor()?, None);

        Ok(())
    }

    mod shards_with_sliver_pairs {
        use walrus_test_utils::async_param_test;

        use super::*;

        async_param_test! {
            returns_shard_if_it_stores_both -> TestResult: [
                both: (WhichSlivers::Both, true),
                only_primary: (WhichSlivers::Primary, false),
                only_secondary: (WhichSlivers::Secondary, false),
            ]
        }
        async fn returns_shard_if_it_stores_both(
            which: WhichSlivers,
            is_retrieved: bool,
        ) -> TestResult {
            let storage = populated_storage(&[(SHARD_INDEX, vec![(BLOB_ID, which)])])?;

            let result: Vec<_> = storage.as_ref().shards_with_sliver_pairs(&BLOB_ID)?;

            if is_retrieved {
                assert_eq!(result, &[SHARD_INDEX]);
            } else {
                assert!(result.is_empty());
            }

            Ok(())
        }

        #[tokio::test]
        async fn identifies_all_shards_storing_sliver_pairs() -> TestResult {
            let storage = populated_storage(&[
                (SHARD_INDEX, vec![(BLOB_ID, WhichSlivers::Both)]),
                (OTHER_SHARD_INDEX, vec![(BLOB_ID, WhichSlivers::Both)]),
            ])?;

            let mut result: Vec<_> = storage.as_ref().shards_with_sliver_pairs(&BLOB_ID)?;

            result.sort();

            assert_eq!(result, [SHARD_INDEX, OTHER_SHARD_INDEX]);

            Ok(())
        }

        #[tokio::test]
        async fn ignores_shards_without_both_sliver_pairs() -> TestResult {
            let storage = populated_storage(&[
                (SHARD_INDEX, vec![(BLOB_ID, WhichSlivers::Primary)]),
                (OTHER_SHARD_INDEX, vec![(BLOB_ID, WhichSlivers::Both)]),
            ])?;

            let result: Vec<_> = storage.as_ref().shards_with_sliver_pairs(&BLOB_ID)?;

            assert_eq!(result, [OTHER_SHARD_INDEX]);

            Ok(())
        }
    }

    /// Open and populate the storage, then close it.
    ///
    /// Runs in its own runtime to ensure that all tasked spawned by typed_store
    /// are dropped to free the storage lock.
    #[tokio::main(flavor = "current_thread")]
    async fn populate_storage_then_close(spec: StorageSpec) -> TestResult<TempDir> {
        let WithTempDir { inner, temp_dir } = populated_storage(spec)?;
        Ok(temp_dir)
    }

    #[test]
    fn can_reopen_storage_with_shards_and_access_data() -> TestResult {
        let directory = populate_storage_then_close(&[
            (SHARD_INDEX, vec![(BLOB_ID, WhichSlivers::Both)]),
            (OTHER_SHARD_INDEX, vec![(BLOB_ID, WhichSlivers::Both)]),
        ])?;

        let result = Runtime::new()?.block_on(async move {
            let storage = Storage::open(directory.path(), MetricConf::default())?;

            for shard_id in [SHARD_INDEX, OTHER_SHARD_INDEX] {
                let Some(shard) = storage.shard_storage(SHARD_INDEX) else {
                    panic!("shard {shard_id} should exist");
                };

                for sliver_type in [SliverType::Primary, SliverType::Secondary] {
                    let _ = shard
                        .get_sliver(&BLOB_ID, sliver_type)
                        .expect("sliver lookup should not err")
                        .expect("sliver should be present");
                }
            }

            Result::<(), anyhow::Error>::Ok(())
        });

        Ok(())
    }
}
