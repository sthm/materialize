// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Types related to the source ingestion pipeline/framework.

// https://github.com/tokio-rs/prost/issues/237
// #![allow(missing_docs)]

use std::any::Any;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt::Debug;
use std::rc::Rc;
use std::sync::Arc;

use differential_dataflow::Collection;
use mz_expr::PartitionId;
use mz_ore::metrics::{CounterVecExt, DeleteOnDropCounter, DeleteOnDropGauge, GaugeVecExt};
use mz_repr::{Diff, GlobalId, Row};
use mz_rocksdb::RocksDBInstanceMetrics;
use mz_storage_operators::metrics::BackpressureMetrics;
use mz_storage_types::connections::ConnectionContext;
use mz_storage_types::errors::{DecodeError, SourceErrorDetails};
use mz_storage_types::sources::{MzOffset, SourceTimestamp};
use prometheus::core::{AtomicF64, AtomicI64, AtomicU64};
use serde::{Deserialize, Serialize};
use timely::dataflow::{Scope, Stream};
use timely::progress::Antichain;

use crate::healthcheck::{HealthStatusMessage, StatusNamespace};
use crate::source::metrics::{SourceBaseMetrics, UpsertSharedMetrics};
use crate::source::RawSourceCreationConfig;

/// Describes a source that can render itself in a timely scope.
pub trait SourceRender {
    type Key: timely::Data + MaybeLength;
    type Value: timely::Data + MaybeLength;
    type Time: SourceTimestamp;
    const STATUS_NAMESPACE: StatusNamespace;

    /// Renders the source in the provided timely scope.
    ///
    /// The `resume_uppers` stream can be used by the source to observe the overall progress of the
    /// ingestion. When a frontier appears in this stream the source implementation can be certain
    /// that future ingestion instances will request to read the external data only at times beyond
    /// that frontier. Therefore, the source implementation can react to this stream by e.g
    /// committing offsets upstream or advancing the LSN of a replication slot. It is safe to
    /// ignore this argument.
    ///
    /// Rendering a source is expected to return four things.
    ///
    /// First, a source must produce a collection that is produced by the rendered dataflow and
    /// must contain *definite*[^1] data for all times beyond the resumption frontier.
    ///
    /// Second, a source may produce an optional progress stream that will be used to drive
    /// reclocking. This is useful for sources that can query the highest offsets of the external
    /// source before reading the data for those offsets. In those cases it is preferable to
    /// produce this additional stream.
    ///
    /// Third, a source must produce a stream of health status updates.
    ///
    /// Finally, the source is expected to return an opaque token that when dropped will cause the
    /// source to immediately drop all capabilities and advance its frontier to the empty antichain.
    ///
    /// [^1] <https://github.com/MaterializeInc/materialize/blob/main/doc/developer/design/20210831_correctness.md#describing-definite-data>
    fn render<G: Scope<Timestamp = Self::Time>>(
        self,
        scope: &mut G,
        config: RawSourceCreationConfig,
        connection_context: ConnectionContext,
        resume_uppers: impl futures::Stream<Item = Antichain<Self::Time>> + 'static,
        start_signal: impl std::future::Future<Output = ()> + 'static,
    ) -> (
        Collection<
            G,
            (
                usize,
                Result<SourceMessage<Self::Key, Self::Value>, SourceReaderError>,
            ),
            Diff,
        >,
        Option<Stream<G, Infallible>>,
        Stream<G, HealthStatusMessage>,
        Rc<dyn Any>,
    );
}

/// Source-agnostic wrapper for messages. Each source must implement a
/// conversion to Message.
#[derive(Debug, Clone)]
pub struct SourceMessage<Key, Value> {
    /// The message key
    pub key: Key,
    /// The message value
    pub value: Value,
    /// Additional metadata columns requested by the user
    pub metadata: Row,
}

/// A record produced by a source
#[derive(Clone, Serialize, Debug, Deserialize)]
pub struct SourceOutput<K, V> {
    /// The record's key (or some empty/default value for sources without the concept of key)
    pub key: K,
    /// The record's value
    pub value: V,
    /// Additional metadata columns requested by the user
    pub metadata: Row,
    /// The offset position in the partition of a kafka source. This is field is on its way out and
    /// its only valid use is in the upsert operator. Do NOT use it in any new place!
    // TODO(petrosagg): remove this field
    pub position_for_upsert: MzOffset,
}

impl<K, V> SourceOutput<K, V> {
    /// Build a new SourceOutput
    pub fn new(
        key: K,
        value: V,
        metadata: Row,
        position_for_upsert: MzOffset,
    ) -> SourceOutput<K, V> {
        SourceOutput {
            key,
            value,
            metadata,
            position_for_upsert,
        }
    }
}

/// The output of the decoding operator
#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct DecodeResult {
    /// The decoded key
    pub key: Option<Result<Row, DecodeError>>,
    /// The decoded value, as well as the the
    /// differential `diff` value for this value, if the value
    /// is present and not and error.
    pub value: Option<Result<Row, DecodeError>>,
    /// Additional metadata requested by the user
    pub metadata: Row,
    /// The offset position in the partition of a kafka source. This is field is on its way out and
    /// its only valid use is in the upsert operator. Do NOT use it in any new place!
    // TODO(petrosagg): remove this field
    pub position_for_upsert: MzOffset,
}

/// A structured error for `SourceReader::get_next_message` implementors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceReaderError {
    pub inner: SourceErrorDetails,
}

impl SourceReaderError {
    /// This is an unclassified but definite error. This is typically only appropriate
    /// when the error is permanently fatal for the source... some critical invariant
    /// is violated or data is corrupted, for example.
    pub fn other_definite(e: anyhow::Error) -> SourceReaderError {
        SourceReaderError {
            inner: SourceErrorDetails::Other(format!("{}", e)),
        }
    }
}

/// Source-specific metrics in the persist sink
pub struct SourcePersistSinkMetrics {
    pub(crate) progress: DeleteOnDropGauge<'static, AtomicI64, Vec<String>>,
    pub(crate) row_inserts: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) row_retractions: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) error_inserts: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) error_retractions: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) processed_batches: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
}

impl SourcePersistSinkMetrics {
    /// Initialises source metrics used in the `persist_sink`.
    pub fn new(
        base: &SourceBaseMetrics,
        _source_id: GlobalId,
        parent_source_id: GlobalId,
        worker_id: usize,
        shard_id: &mz_persist_client::ShardId,
        output_index: usize,
    ) -> SourcePersistSinkMetrics {
        let shard = shard_id.to_string();
        SourcePersistSinkMetrics {
            progress: base.source_specific.progress.get_delete_on_drop_gauge(vec![
                parent_source_id.to_string(),
                output_index.to_string(),
                shard.clone(),
                worker_id.to_string(),
            ]),
            row_inserts: base
                .source_specific
                .row_inserts
                .get_delete_on_drop_counter(vec![
                    parent_source_id.to_string(),
                    output_index.to_string(),
                    shard.clone(),
                    worker_id.to_string(),
                ]),
            row_retractions: base
                .source_specific
                .row_retractions
                .get_delete_on_drop_counter(vec![
                    parent_source_id.to_string(),
                    output_index.to_string(),
                    shard.clone(),
                    worker_id.to_string(),
                ]),
            error_inserts: base
                .source_specific
                .error_inserts
                .get_delete_on_drop_counter(vec![
                    parent_source_id.to_string(),
                    output_index.to_string(),
                    shard.clone(),
                    worker_id.to_string(),
                ]),
            error_retractions: base
                .source_specific
                .error_retractions
                .get_delete_on_drop_counter(vec![
                    parent_source_id.to_string(),
                    output_index.to_string(),
                    shard.clone(),
                    worker_id.to_string(),
                ]),
            processed_batches: base
                .source_specific
                .persist_sink_processed_batches
                .get_delete_on_drop_counter(vec![
                    parent_source_id.to_string(),
                    output_index.to_string(),
                    shard,
                    worker_id.to_string(),
                ]),
        }
    }
}

/// Source-specific Prometheus metrics
pub struct SourceMetrics {
    /// Value of the capability associated with this source
    pub(crate) capability: DeleteOnDropGauge<'static, AtomicU64, Vec<String>>,
    /// The resume_upper for a source.
    pub(crate) resume_upper: DeleteOnDropGauge<'static, AtomicI64, Vec<String>>,
    /// Per-partition Prometheus metrics.
    pub(crate) partition_metrics: BTreeMap<PartitionId, PartitionMetrics>,
    /// The number of in-memory remap bindings that reclocking a time needs to iterate over.
    pub(crate) inmemory_remap_bindings: DeleteOnDropGauge<'static, AtomicU64, Vec<String>>,
    source_name: String,
    source_id: GlobalId,
    base_metrics: SourceBaseMetrics,
}

impl SourceMetrics {
    /// Initialises source metrics for a given (source_id, worker_id)
    pub fn new(
        base: &SourceBaseMetrics,
        source_name: &str,
        source_id: GlobalId,
        worker_id: &str,
    ) -> SourceMetrics {
        let labels = &[
            source_name.to_string(),
            source_id.to_string(),
            worker_id.to_string(),
        ];
        SourceMetrics {
            capability: base
                .source_specific
                .capability
                .get_delete_on_drop_gauge(labels.to_vec()),
            resume_upper: base
                .source_specific
                .resume_upper
                .get_delete_on_drop_gauge(vec![source_id.to_string()]),
            inmemory_remap_bindings: base
                .source_specific
                .inmemory_remap_bindings
                .get_delete_on_drop_gauge(vec![source_id.to_string(), worker_id.to_string()]),
            partition_metrics: Default::default(),
            source_name: source_name.to_string(),
            source_id,
            base_metrics: base.clone(),
        }
    }

    /// Log updates to which offsets / timestamps read up to.
    pub fn record_partition_offsets(
        &mut self,
        offsets: BTreeMap<PartitionId, (MzOffset, mz_repr::Timestamp, i64)>,
    ) {
        for (partition, (offset, timestamp, count)) in offsets {
            let metric = self
                .partition_metrics
                .entry(partition.clone())
                .or_insert_with(|| {
                    PartitionMetrics::new(
                        &self.base_metrics,
                        &self.source_name,
                        self.source_id,
                        &partition,
                    )
                });

            metric.messages_ingested.inc_by(count);

            metric.record_offset(
                &self.source_name,
                self.source_id,
                &partition,
                offset.offset,
                i64::try_from(timestamp).expect("materialize exists after 250M AD"),
            );
        }
    }
}

/// Partition-specific metrics, recorded to both Prometheus and a system table
pub struct PartitionMetrics {
    /// Highest offset that has been received by the source and timestamped
    pub(crate) offset_ingested: DeleteOnDropGauge<'static, AtomicU64, Vec<String>>,
    /// Highest offset that has been received by the source
    pub(crate) offset_received: DeleteOnDropGauge<'static, AtomicU64, Vec<String>>,
    /// Value of the highest timestamp that is closed (for which all messages have been ingested)
    pub(crate) closed_ts: DeleteOnDropGauge<'static, AtomicU64, Vec<String>>,
    /// Total number of messages that have been received by the source and timestamped
    pub(crate) messages_ingested: DeleteOnDropCounter<'static, AtomicI64, Vec<String>>,
    pub(crate) last_offset: u64,
    pub(crate) last_timestamp: i64,
}

impl PartitionMetrics {
    /// Record the latest offset ingested high-water mark
    fn record_offset(
        &mut self,
        _source_name: &str,
        _source_id: GlobalId,
        _partition_id: &PartitionId,
        offset: u64,
        timestamp: i64,
    ) {
        self.offset_received.set(offset);
        self.offset_ingested.set(offset);
        self.last_offset = offset;
        self.last_timestamp = timestamp;
    }

    /// Initialises partition metrics for a given (source_id, partition_id)
    pub fn new(
        base_metrics: &SourceBaseMetrics,
        source_name: &str,
        source_id: GlobalId,
        partition_id: &PartitionId,
    ) -> PartitionMetrics {
        let labels = &[
            source_name.to_string(),
            source_id.to_string(),
            partition_id.to_string(),
        ];
        let base = &base_metrics.partition_specific;
        PartitionMetrics {
            offset_ingested: base
                .offset_ingested
                .get_delete_on_drop_gauge(labels.to_vec()),
            offset_received: base
                .offset_received
                .get_delete_on_drop_gauge(labels.to_vec()),
            closed_ts: base.closed_ts.get_delete_on_drop_gauge(labels.to_vec()),
            messages_ingested: base
                .messages_ingested
                .get_delete_on_drop_counter(labels.to_vec()),
            last_offset: 0,
            last_timestamp: 0,
        }
    }
}

/// Source reader operator specific Prometheus metrics
pub struct SourceReaderMetrics {
    source_id: GlobalId,
    base_metrics: SourceBaseMetrics,
}

impl SourceReaderMetrics {
    /// Initialises source metrics for a given (source_id, worker_id)
    pub fn new(base: &SourceBaseMetrics, source_id: GlobalId) -> SourceReaderMetrics {
        SourceReaderMetrics {
            source_id,
            base_metrics: base.clone(),
        }
    }

    /// Get metrics struct for offset committing.
    pub fn offset_commit_metrics(&self) -> OffsetCommitMetrics {
        OffsetCommitMetrics::new(&self.base_metrics, self.source_id)
    }
}

/// Metrics about committing offsets
pub struct OffsetCommitMetrics {
    /// The offset-domain resume_upper for a source.
    pub(crate) offset_commit_failures: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
}

impl OffsetCommitMetrics {
    /// Initialises partition metrics for a given (source_id, partition_id)
    pub fn new(base_metrics: &SourceBaseMetrics, source_id: GlobalId) -> OffsetCommitMetrics {
        let base = &base_metrics.source_specific;
        OffsetCommitMetrics {
            offset_commit_failures: base
                .offset_commit_failures
                .get_delete_on_drop_counter(vec![source_id.to_string()]),
        }
    }
}

/// Metrics for the `upsert` operator.
pub struct UpsertMetrics {
    pub(crate) rehydration_latency: DeleteOnDropGauge<'static, AtomicF64, Vec<String>>,
    pub(crate) rehydration_total: DeleteOnDropGauge<'static, AtomicU64, Vec<String>>,
    pub(crate) rehydration_updates: DeleteOnDropGauge<'static, AtomicU64, Vec<String>>,
    pub(crate) rocksdb_autospill_in_use: Arc<DeleteOnDropGauge<'static, AtomicU64, Vec<String>>>,

    pub(crate) merge_snapshot_updates: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) merge_snapshot_inserts: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) merge_snapshot_deletes: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) upsert_inserts: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) upsert_updates: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) upsert_deletes: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) multi_get_size: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) multi_get_result_bytes: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) multi_get_result_count: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) multi_put_size: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,

    pub(crate) legacy_value_errors: DeleteOnDropGauge<'static, AtomicU64, Vec<String>>,

    pub(crate) shared: Arc<UpsertSharedMetrics>,
    pub(crate) rocksdb_shared: Arc<mz_rocksdb::RocksDBSharedMetrics>,
    pub(crate) rocksdb_instance_metrics: Arc<mz_rocksdb::RocksDBInstanceMetrics>,
    // `UpsertMetrics` keeps a reference (through `Arc`'s) to backpressure metrics, so that
    // they are not dropped when the `persist_source` operator is dropped.
    _backpressure_metrics: Option<BackpressureMetrics>,
}

impl UpsertMetrics {
    pub fn new(
        base_metrics: &SourceBaseMetrics,
        source_id: GlobalId,
        worker_id: usize,
        backpressure_metrics: Option<BackpressureMetrics>,
    ) -> Self {
        let base = &base_metrics.upsert_specific;
        let source_id_s = source_id.to_string();
        let worker_id = worker_id.to_string();
        Self {
            rehydration_latency: base
                .rehydration_latency
                .get_delete_on_drop_gauge(vec![source_id_s.clone(), worker_id.clone()]),
            rehydration_total: base
                .rehydration_total
                .get_delete_on_drop_gauge(vec![source_id_s.clone(), worker_id.clone()]),
            rehydration_updates: base
                .rehydration_updates
                .get_delete_on_drop_gauge(vec![source_id_s.clone(), worker_id.clone()]),
            rocksdb_autospill_in_use: Arc::new(
                base.rocksdb_autospill_in_use
                    .get_delete_on_drop_gauge(vec![source_id_s.clone(), worker_id.clone()]),
            ),
            merge_snapshot_updates: base
                .merge_snapshot_updates
                .get_delete_on_drop_counter(vec![source_id_s.clone(), worker_id.clone()]),
            merge_snapshot_inserts: base
                .merge_snapshot_inserts
                .get_delete_on_drop_counter(vec![source_id_s.clone(), worker_id.clone()]),
            merge_snapshot_deletes: base
                .merge_snapshot_deletes
                .get_delete_on_drop_counter(vec![source_id_s.clone(), worker_id.clone()]),
            upsert_inserts: base
                .upsert_inserts
                .get_delete_on_drop_counter(vec![source_id_s.clone(), worker_id.clone()]),
            upsert_updates: base
                .upsert_updates
                .get_delete_on_drop_counter(vec![source_id_s.clone(), worker_id.clone()]),
            upsert_deletes: base
                .upsert_deletes
                .get_delete_on_drop_counter(vec![source_id_s.clone(), worker_id.clone()]),
            multi_get_size: base
                .multi_get_size
                .get_delete_on_drop_counter(vec![source_id_s.clone(), worker_id.clone()]),
            multi_get_result_count: base
                .multi_get_result_count
                .get_delete_on_drop_counter(vec![source_id_s.clone(), worker_id.clone()]),
            multi_get_result_bytes: base
                .multi_get_result_bytes
                .get_delete_on_drop_counter(vec![source_id_s.clone(), worker_id.clone()]),
            multi_put_size: base
                .multi_put_size
                .get_delete_on_drop_counter(vec![source_id_s.clone(), worker_id.clone()]),

            legacy_value_errors: base
                .legacy_value_errors
                .get_delete_on_drop_gauge(vec![source_id_s.clone(), worker_id.clone()]),

            shared: base.shared(&source_id),
            rocksdb_shared: base.rocksdb_shared(&source_id),
            rocksdb_instance_metrics: Arc::new(RocksDBInstanceMetrics {
                multi_get_size: base
                    .rocksdb_multi_get_size
                    .get_delete_on_drop_counter(vec![source_id_s.clone(), worker_id.clone()]),
                multi_get_result_count: base
                    .rocksdb_multi_get_result_count
                    .get_delete_on_drop_counter(vec![source_id_s.clone(), worker_id.clone()]),
                multi_get_result_bytes: base
                    .rocksdb_multi_get_result_bytes
                    .get_delete_on_drop_counter(vec![source_id_s.clone(), worker_id.clone()]),
                multi_get_count: base
                    .rocksdb_multi_get_count
                    .get_delete_on_drop_counter(vec![source_id_s.clone(), worker_id.clone()]),
                multi_put_count: base
                    .rocksdb_multi_put_count
                    .get_delete_on_drop_counter(vec![source_id_s.clone(), worker_id.clone()]),
                multi_put_size: base
                    .rocksdb_multi_put_size
                    .get_delete_on_drop_counter(vec![source_id_s, worker_id]),
            }),
            _backpressure_metrics: backpressure_metrics,
        }
    }
}

/// Types that implement this trait expose a length function
pub trait MaybeLength {
    /// Returns the size of the object
    fn len(&self) -> Option<usize>;
}

impl MaybeLength for () {
    fn len(&self) -> Option<usize> {
        None
    }
}

impl MaybeLength for Vec<u8> {
    fn len(&self) -> Option<usize> {
        Some(self.len())
    }
}

impl MaybeLength for mz_repr::Row {
    fn len(&self) -> Option<usize> {
        Some(self.data().len())
    }
}

impl<T: MaybeLength> MaybeLength for Option<T> {
    fn len(&self) -> Option<usize> {
        self.as_ref().and_then(|v| v.len())
    }
}

/*
#[derive(Debug, thiserror::Error)]
pub enum KafkaMessageConsumptionError {
    #[error("{0}")]
    KafkaError(#[from] KafkaError),
    #[error("{0}")]
    DecodeError(DecodeError),
}

impl From<DecodeError> for KafkaMessageConsumptionError {
    fn from(err: DecodeError) -> Self {
        KafkaMessageConsumptionError::DecodeError(err)
    }
}
*/
