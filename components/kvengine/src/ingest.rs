// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use crate::table::sstable::{L0Table, SSTable};
use crate::*;
use crossbeam_epoch::Owned;
use std::iter::Iterator;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

pub struct IngestTree {
    pub change_set: kvenginepb::ChangeSet,
    pub active: bool,
}

impl Engine {
    pub fn ingest(&self, tree: IngestTree) -> Result<()> {
        let (l0s, mut scfs) = self.create_level_tree_level_handlers(&tree)?;
        let shard = Shard::new_for_ingest(tree.change_set, self.opts.clone());
        shard.set_active(tree.active);
        shard.l0_tbls.store(Owned::new(Arc::new(l0s)), Relaxed);
        for (cf, scf) in scfs.drain(..).enumerate() {
            store_resource(&shard.cfs[cf], Arc::new(scf));
        }
        shard.refresh_estimated_size();
        self.shards.insert(shard.id, Arc::new(shard));
        Ok(())
    }

    fn create_level_tree_level_handlers(
        &self,
        tree: &IngestTree,
    ) -> Result<(L0Tables, Vec<ShardCF>)> {
        let mut l0_tbls = vec![];
        let snap = tree.change_set.get_snapshot();
        let fs_opts = dfs::Options::new(tree.change_set.shard_id, tree.change_set.shard_ver);
        for l0_create in snap.get_l0_creates() {
            let file = self.fs.open(l0_create.id, fs_opts)?;
            let l0_tbl = L0Table::new(file, self.cache.clone())?;
            l0_tbls.push(l0_tbl);
        }
        l0_tbls.sort_by(|a, b| b.version().cmp(&a.version()));
        let mut scfs = vec![];
        for i in 0..NUM_CFS {
            let num_level = self.opts.cfs[i].max_levels;
            let scf = ShardCF::new(num_level);
            scfs.push(scf);
        }
        for table_create in snap.get_table_creates() {
            let scf = &mut scfs[table_create.cf as usize];
            let level = &mut scf.levels[table_create.level as usize - 1];
            let file = self.fs.open(table_create.id, fs_opts)?;
            let tbl = SSTable::new(file, self.cache.clone())?;
            level.total_size += tbl.size();
            level.tables.push(tbl);
        }
        for cf in 0..NUM_CFS {
            let scf = &mut scfs[cf];
            for l in &mut scf.levels {
                l.tables.sort_by(|x, y| x.smallest().cmp(y.smallest()))
            }
        }
        Ok((L0Tables::new(l0_tbls), scfs))
    }
}
