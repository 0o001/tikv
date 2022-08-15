// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use std::sync::Arc;

use bytes::Buf;
use engine_traits::RaftEngineReadOnly;
use kvengine::{Engine, Shard, ShardMeta};
use kvenginepb::ChangeSet;
use kvproto::{metapb, raft_cmdpb::RaftCmdRequest, raft_serverpb};
use protobuf::Message;
use raft_proto::eraftpb;
use slog_global::info;
use tikv_util::warn;

use crate::store::{
    parse_region_state_key, raft_state_key, rlog, Applier, ApplyContext, PeerTag, RaftApplyState,
    RaftState, RegionIDVer, KV_ENGINE_META_KEY, REGION_META_KEY_BYTE, STORE_IDENT_KEY, TERM_KEY,
};

#[derive(Clone)]
pub struct RecoverHandler {
    rf_engine: rfengine::RfEngine,
    store_id: u64,
}

impl RecoverHandler {
    pub fn new(rf_engine: rfengine::RfEngine) -> Self {
        let store_id = match load_store_ident(&rf_engine) {
            Some(ident) => ident.store_id,
            None => 0,
        };
        Self {
            rf_engine,
            store_id,
        }
    }

    fn load_region_meta(
        &self,
        shard_id: u64,
        shard_ver: u64,
    ) -> kvengine::Result<(metapb::Region, u64)> {
        let mut region: Option<metapb::Region> = None;
        let _ =
            self.rf_engine
                .iterate_region_states(shard_id, true, |k, v| -> rfengine::Result<()> {
                    if k[0] != REGION_META_KEY_BYTE {
                        return Ok(());
                    }
                    let (meta_ver, _) = parse_region_state_key(k);
                    if meta_ver != shard_ver {
                        return Ok(());
                    }
                    let mut state = raft_serverpb::RegionLocalState::new();
                    if state.merge_from_bytes(v).is_err() {
                        return Err(rfengine::Error::ParseError);
                    }
                    region = Some(state.take_region());
                    Err(rfengine::Error::EOF)
                });
        let tag = PeerTag::new(self.store_id, RegionIDVer::new(shard_id, shard_ver));
        if let Some(region) = region {
            let raft_state_key = raft_state_key(region.get_region_epoch().get_version());
            let val = self
                .rf_engine
                .get_state(region.get_id(), &raft_state_key)
                .unwrap_or_else(|| {
                    panic!(
                        "{} failed to get raft state key, state keys {:?}",
                        tag,
                        self.get_state_keys(shard_id)
                    );
                });
            let mut raft_state = RaftState::default();
            raft_state.unmarshal(val.as_ref());
            return Ok((region, raft_state.commit));
        }
        let err_msg = format!(
            "{} failed to load region meta, state keys {:?}",
            tag,
            self.get_state_keys(shard_id),
        );
        Err(kvengine::Error::ErrOpen(err_msg))
    }

    fn get_state_keys(&self, region_id: u64) -> Vec<Vec<u8>> {
        let mut state_keys = vec![];
        let _ = self.rf_engine.iterate_region_states(
            region_id,
            false,
            |k, _| -> rfengine::Result<()> {
                state_keys.push(k.to_vec());
                Ok(())
            },
        );
        state_keys
    }

    fn execute_admin_request(
        applier: &mut Applier,
        ctx: &mut ApplyContext,
        req: RaftCmdRequest,
    ) -> kvengine::Result<()> {
        let admin_req = req.get_admin_request();
        if admin_req.has_change_peer() {
            applier
                .exec_change_peer(ctx, admin_req)
                .map_err(|x| kvengine::Error::ErrOpen(format!("{}", x)))?;
        }
        Ok(())
    }
}

fn load_store_ident(rf_engine: &rfengine::RfEngine) -> Option<raft_serverpb::StoreIdent> {
    let val = rf_engine.get_state(0, STORE_IDENT_KEY);
    val.as_ref()?;
    let mut ident = raft_serverpb::StoreIdent::new();
    ident.merge_from_bytes(val.unwrap().chunk()).unwrap();
    Some(ident)
}

impl kvengine::RecoverHandler for RecoverHandler {
    fn recover(
        &self,
        engine: &Engine,
        shard: &Arc<Shard>,
        meta: &ShardMeta,
    ) -> kvengine::Result<()> {
        let applied_index = shard.get_write_sequence();
        let mut ctx = ApplyContext::new(engine.clone(), None);
        let applied_index_term = shard.get_property(TERM_KEY).unwrap().get_u64_le();
        let apply_state = RaftApplyState::new(applied_index, applied_index_term);
        let (region_meta, commit_idx) = self.load_region_meta(shard.id, shard.ver)?;
        let low_idx = applied_index + 1;
        let high_idx = commit_idx + 1;
        info!(
            "{} recover from applied {} to committed {}",
            shard.tag(),
            applied_index,
            commit_idx
        );
        let mut entries = Vec::with_capacity((high_idx.saturating_sub(low_idx)) as usize);
        self.rf_engine
            .fetch_entries_to(shard.id, low_idx, high_idx, None, &mut entries)
            .map_err(|e| {
                let stats = self.rf_engine.get_region_stats(shard.id);
                let err_msg = format!(
                    "{} entries unavailable err: {:?}, stats {:?}, low: {}, high {}",
                    shard.tag(),
                    e,
                    stats,
                    low_idx,
                    high_idx
                );
                kvengine::Error::ErrOpen(err_msg)
            })?;

        let snap = shard.new_snap_access();
        let mut applier = Applier::new_for_recover(self.store_id, region_meta, snap, apply_state);

        for e in &entries {
            if e.data.is_empty() || e.entry_type != eraftpb::EntryType::EntryNormal {
                continue;
            }
            ctx.exec_log_index = e.get_index();
            ctx.exec_log_term = e.get_term();
            let mut req = RaftCmdRequest::new();
            req.merge_from_bytes(e.data.chunk()).unwrap();
            if req.get_header().get_region_epoch().version != shard.ver {
                continue;
            }
            if req.has_admin_request() {
                if req.get_admin_request().has_splits() {
                    // We are recovering an parent shard, we need to switch the mem-table for
                    // children to copy.
                    engine.switch_mem_table(&shard, meta.base_version + ctx.exec_log_index);
                    // It is the last command for a parent shard, we should return here.
                    return Ok(());
                }
                Self::execute_admin_request(&mut applier, &mut ctx, req)?;
            } else if let Some(custom) = rlog::get_custom_log(&req) {
                if rlog::is_engine_meta_log(custom.data.chunk()) {
                    let mut cs = custom.get_change_set().unwrap();
                    cs.sequence = e.get_index();
                    if !meta.is_duplicated_change_set(&mut cs) {
                        // We don't have a background region worker now, should do it synchronously.
                        let cs = engine.prepare_change_set(cs, false)?;
                        engine.apply_change_set(cs)?;
                    }
                } else if let Err(e) = applier.exec_custom_log(&mut ctx, &custom) {
                    // Only duplicated pre-split may fail, we can ignore this error.
                    warn!("failed to execute custom log {:?}", e);
                }
            }
            applier.apply_state.applied_index = ctx.exec_log_index;
            applier.apply_state.applied_index_term = ctx.exec_log_term;
        }
        Ok(())
    }
}

impl kvengine::MetaIterator for RecoverHandler {
    fn iterate<F>(&self, mut f: F) -> kvengine::Result<()>
    where
        F: FnMut(ChangeSet),
    {
        let mut err_msg = None;
        self.rf_engine
            .iterate_all_states(false, |_region_id, key, val| {
                if key[0] == KV_ENGINE_META_KEY[0] {
                    let mut cs = kvenginepb::ChangeSet::new();
                    if let Err(e) = cs.merge_from_bytes(val) {
                        err_msg = Some(e.to_string());
                        return false;
                    }
                    f(cs);
                }
                true
            });
        if let Some(err) = err_msg {
            return Err(kvengine::Error::ErrOpen(err));
        }
        Ok(())
    }

    fn engine_id(&self) -> u64 {
        self.store_id
    }
}
