// Copyright 2022 TiKV Project Authors. Licensed under Apache-2.0.

use lazy_static::lazy_static;
use prometheus::*;
use prometheus_static_metric::*;

use crate::*;

make_static_metric! {
    pub label_enum LogQueueKind {
        rewrite,
        append,
    }

    pub struct LogQueueHistogramVec: Histogram {
        "type" => LogQueueKind,
    }

    pub struct LogQueueCounterVec: IntCounter {
        "type" => LogQueueKind,
    }

    pub struct LogQueueGaugeVec: IntGauge {
        "type" => LogQueueKind,
    }
}

pub fn flush_engine_properties(_engine: &RfEngine, _name: &str) {}

lazy_static! {
    pub static ref ENGINE_PERSIST_DURATION_HISTOGRAM: Histogram = register_histogram!(
        "raft_engine_persist_duration_seconds",
        "Bucketed histogram of Raft Engine persist duration",
        exponential_buckets(0.00005, 1.8, 26).unwrap()
    )
    .unwrap();
    pub static ref ENGINE_ROTATE_DURATION_HISTOGRAM: Histogram = register_histogram!(
        "raft_engine_rotate_duration_seconds",
        "Bucketed histogram of Raft Engine rotate duration",
        exponential_buckets(0.00005, 1.8, 26).unwrap()
    )
    .unwrap();
    pub static ref ENGINE_WAL_WRITE_DURATION_HISTOGRAM: Histogram = register_histogram!(
        "raft_engine_wal_write_duration_seconds",
        "Bucketed histogram of Raft Engine wal write duration",
        exponential_buckets(0.00005, 1.8, 26).unwrap()
    )
    .unwrap();
    pub static ref ENGINE_APPLY_DURATION_HISTOGRAM: Histogram = register_histogram!(
        "raft_engine_apply_duration_seconds",
        "Bucketed histogram of Raft Engine apply duration",
        exponential_buckets(0.00005, 1.8, 26).unwrap()
    )
    .unwrap();
    pub static ref ENGINE_TRUNCATE_DURATION_HISTOGRAM: Histogram = register_histogram!(
        "raft_engine_truncate_duration_seconds",
        "Bucketed histogram of Raft Engine truncate duration",
        exponential_buckets(0.00005, 1.8, 26).unwrap()
    )
    .unwrap();
    pub static ref ENGINE_FETCH_ENTRIES_DURATION_HISTOGRAM: Histogram = register_histogram!(
        "raft_engine_fetch_entries_duration_seconds",
        "Bucketed histogram of Raft Engine fetch entries duration",
        exponential_buckets(0.00005, 1.8, 26).unwrap()
    )
    .unwrap();
    pub static ref ENGINE_COMPACT_WAL_DURATION_HISTOGRAM: Histogram = register_histogram!(
        "raft_engine_compact_wal_duration_seconds",
        "Bucketed histogram of Raft Engine compact WAL duration",
        exponential_buckets(0.0005, 1.8, 26).unwrap()
    )
    .unwrap();
}
