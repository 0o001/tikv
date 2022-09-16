// Copyright 2022 TiKV Project Authors. Licensed under Apache-2.0.

use std::{thread, time::Duration};

use test_cloud_server::{try_wait, ServerCluster};
use tikv_util::config::ReadableSize;

use crate::alloc_node_id;

#[test]
fn test_engine_auto_switch() {
    test_util::init_log_for_test();
    let node_id = alloc_node_id();
    let mut cluster = ServerCluster::new(vec![node_id], |_, conf| {
        conf.rocksdb.writecf.write_buffer_size = ReadableSize::kb(256);
    });
    let mut client = cluster.new_client();
    client.put_kv(0..100, i_to_key, i_to_val);
    client.put_kv(100..200, i_to_key, i_to_val);
    client.put_kv(200..300, i_to_key, i_to_val);
    let region_id = client.get_region_id(&[]);
    let engine = cluster.get_kvengine(node_id);
    let stats = engine.get_shard_stat(region_id);
    assert!(stats.mem_table_count + stats.l0_table_count > 1);
    cluster.stop();
}

fn i_to_key(i: usize) -> Vec<u8> {
    format!("key_{:03}", i).into_bytes()
}

fn i_to_val(i: usize) -> Vec<u8> {
    format!("val_{:03}", i).into_bytes().repeat(100)
}

#[test]
fn test_split_by_key() {
    test_util::init_log_for_test();
    let node_id = alloc_node_id();
    let mut cluster = ServerCluster::new(vec![node_id], |_, conf| {
        conf.coprocessor.region_split_keys = Some(3);
    });
    let mut client = cluster.new_client();
    client.put_kv(0..5, i_to_key, i_to_key);
    let engine = cluster.get_kvengine(node_id);
    try_wait(|| engine.get_all_shard_id_vers().len() == 2, 10);
    let shard_stats = engine.get_all_shard_stats();
    assert!(shard_stats.len() == 2, "{:?}", &shard_stats);
    client.put_kv(6..15, i_to_key, i_to_key);
    try_wait(|| engine.get_all_shard_id_vers().len() == 5, 10);
    let shard_stats = engine.get_all_shard_stats();
    assert!(shard_stats.len() == 5, "{:?}", &shard_stats);
    cluster.stop();
}

#[test]
fn test_remove_and_add_peer() {
    test_util::init_log_for_test();
    let node_ids = vec![alloc_node_id(), alloc_node_id(), alloc_node_id()];
    let mut cluster = ServerCluster::new(node_ids.clone(), |_, _| {});
    cluster.wait_region_replicated(&[], 3);
    let mut client = cluster.new_client();
    let split_key = i_to_key(5);
    client.split(&split_key);
    // Wait for region heartbeat to update region epoch in PD.
    sleep();

    client.put_kv(0..10, i_to_key, i_to_key);
    let pd = cluster.get_pd_client();
    cluster.wait_pd_region_count(2);
    pd.disable_default_operator();
    let &first_node = node_ids.first().unwrap();
    cluster.remove_node_peers(first_node);
    // After one store has removed peer, the cluster is still available.
    let mut client = cluster.new_client();
    client.put_kv(0..10, i_to_key, i_to_key);
    cluster.stop_node(first_node);
    thread::sleep(Duration::from_millis(100));
    cluster.start_node(first_node, |_, _| {});
    pd.enable_default_operator();
    cluster.wait_region_replicated(&[], 3);
    cluster.wait_region_replicated(&split_key, 3);
    cluster.stop();
}

#[test]
fn test_increasing_put_and_split() {
    test_util::init_log_for_test();
    let node_id = alloc_node_id();
    let mut cluster = ServerCluster::new(vec![node_id], |_, _| {});
    let mut client = cluster.new_client();
    client.put_kv(0..50, i_to_key, i_to_val);
    for i in 1..5 {
        let split_idx = i * 10;
        let split_key = i_to_key(split_idx);
        client.split(&split_key);
        for _ in 0..10 {
            client.put_kv(split_idx..split_idx + 5, i_to_key, i_to_val);
        }
    }
    cluster.stop()
}

fn sleep() {
    std::thread::sleep(Duration::from_millis(100));
}
