// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use std::sync::{atomic::Ordering, mpsc, Arc};

use byteorder::{ByteOrder, LittleEndian};
use bytes::{Buf, Bytes, BytesMut};
use moka::sync::SegmentedCache;
use protobuf::RepeatedField;
use slog_global::error;

use crate::dfs;
use crate::table::{
    search,
    sstable::{self, SSTable},
};
use crate::*;
use crossbeam_epoch as epoch;
use kvenginepb as pb;

#[derive(Clone)]
pub(crate) struct CompactionClient {
    dfs: Arc<dyn dfs::DFS>,
    remote_url: String,
}

impl CompactionClient {
    pub(crate) fn new(dfs: Arc<dyn dfs::DFS>, remote_url: String) -> Self {
        Self { dfs, remote_url }
    }

    pub(crate) fn compact(&self, req: CompactionRequest) -> Result<pb::Compaction> {
        let mut comp = pb::Compaction::new();
        comp.set_top_deletes(req.tops.clone());
        comp.set_cf(req.cf as i32);
        comp.set_level(req.level as u32);
        if req.level == 0 {
            let tbls = compact_l0(&req, self.dfs.clone())?;
            comp.set_table_creates(RepeatedField::from_vec(tbls));
            let mut bot_dels = vec![];
            for cf_bot_dels in &req.multi_cf_bottoms {
                bot_dels.extend_from_slice(cf_bot_dels.as_slice());
            }
            comp.set_bottom_deletes(bot_dels);
            return Ok(comp);
        }
        let tbls = compact_tables(&req, self.dfs.clone())?;
        comp.set_table_creates(RepeatedField::from_vec(tbls));
        comp.set_bottom_deletes(req.bottoms.clone());
        return Ok(comp);
    }
}

pub(crate) struct CompactionRequest {
    pub(crate) cf: isize,
    pub(crate) level: usize,
    pub(crate) tops: Vec<u64>,

    pub(crate) shard_id: u64,
    pub(crate) shard_ver: u64,

    // Used for L1+ compaction.
    pub(crate) bottoms: Vec<u64>,

    // Used for L0 compaction.
    pub(crate) multi_cf_bottoms: Vec<Vec<u64>>,

    pub(crate) overlap: bool,
    pub(crate) safe_ts: u64,
    pub(crate) block_size: usize,
    pub(crate) max_table_size: usize,
    pub(crate) bloom_fpr: f64,
    pub(crate) instance_id: u32,
    pub(crate) start_id: u64,
    pub(crate) end_id: u64,
}

impl CompactionRequest {
    pub(crate) fn get_table_builder_options(&self) -> sstable::TableBuilderOptions {
        sstable::TableBuilderOptions {
            block_size: self.block_size,
            max_table_size: self.max_table_size as usize,
            bloom_fpr: self.bloom_fpr,
        }
    }
}

pub struct CompactDef {
    pub(crate) cf: usize,
    pub(crate) level: usize,

    pub(crate) top: Vec<sstable::SSTable>,
    pub(crate) bot: Vec<sstable::SSTable>,

    pub(crate) has_overlap: bool,

    this_range: KeyRange,
    next_range: KeyRange,

    top_size: u64,
    top_left_idx: usize,
    top_right_idx: usize,
    bot_size: u64,
    bot_left_idx: usize,
    bot_right_idx: usize,
}

impl CompactDef {
    pub(crate) fn new(cf: usize, level: usize) -> Self {
        Self {
            cf,
            level,
            top: vec![],
            bot: vec![],
            has_overlap: false,
            this_range: KeyRange::default(),
            next_range: KeyRange::default(),
            top_size: 0,
            top_left_idx: 0,
            top_right_idx: 0,
            bot_size: 0,
            bot_left_idx: 0,
            bot_right_idx: 0,
        }
    }

    pub(crate) fn smallest(&self) -> &[u8] {
        if self.bot.len() > 0 && self.next_range.left < self.this_range.left {
            return self.next_range.left.chunk();
        }
        self.this_range.left.chunk()
    }

    pub(crate) fn biggest(&self) -> &[u8] {
        if self.bot.len() > 0 && self.next_range.right > self.this_range.right {
            return self.next_range.right.chunk();
        }
        self.this_range.right.chunk()
    }

    pub(crate) fn fill_table(
        &mut self,
        this_level: &LevelHandler,
        next_level: &LevelHandler,
    ) -> bool {
        if this_level.tables.len() == 0 {
            return false;
        }
        let this = this_level.tables.clone();
        let next = next_level.tables.clone();

        // First pick one table has max topSize/bottomSize ratio.
        let mut candidate_ratio = 0f64;
        for (i, tbl) in this.iter().enumerate() {
            let (left, right) = get_tables_in_range(&next, tbl.smallest(), tbl.biggest());
            let bot_size: u64 = Self::sume_tbl_size(&next[left..right]);
            let ratio = Self::calc_ratio(tbl.size(), bot_size);
            if candidate_ratio < ratio {
                candidate_ratio = ratio;
                self.top_left_idx = i;
                self.top_right_idx = i + 1;
                self.top_size = tbl.size();
                self.bot_left_idx = left;
                self.bot_right_idx = right;
                self.bot_size = bot_size;
            }
        }
        if self.top_left_idx == self.top_right_idx {
            return false;
        }
        // Expand to left to include more tops as long as the ratio doesn't decrease and the total size
        // do not exceed maxCompactionExpandSize.
        for i in (0..self.top_left_idx).rev() {
            let t = &this[i];
            let (left, right) = get_tables_in_range(&next, t.smallest(), t.biggest());
            if right < self.bot_left_idx {
                // A bottom table is skipped, we can compact in another run.
                break;
            }
            let new_top_size = t.size() + self.top_size;
            let new_bot_size = Self::sume_tbl_size(&next[left..self.bot_left_idx]) + self.bot_size;
            let new_ratio = Self::calc_ratio(new_top_size, new_bot_size);
            if new_ratio > candidate_ratio {
                self.top_left_idx -= 1;
                self.bot_left_idx = left;
                self.top_size = new_top_size;
                self.bot_size = new_bot_size;
            } else {
                break;
            }
        }
        // Expand to right to include more tops as long as the ratio doesn't decrease and the total size
        // do not exceeds maxCompactionExpandSize.
        for i in self.top_right_idx..this.len() {
            let t = &this[i];
            let (left, right) = get_tables_in_range(&next, t.smallest(), t.biggest());
            if left > self.bot_right_idx {
                // A bottom table is skipped, we can compact in another run.
                break;
            }
            let new_top_size = t.size() + self.top_size;
            let new_bot_size =
                Self::sume_tbl_size(&next[self.bot_right_idx..right]) + self.bot_size;
            let new_ratio = Self::calc_ratio(new_top_size, new_bot_size);
            if new_ratio > candidate_ratio {
                self.top_right_idx += 1;
                self.bot_right_idx = right;
                self.top_size = new_bot_size;
                self.bot_size = new_bot_size;
            } else {
                break;
            }
        }
        self.top = this[self.top_left_idx..self.top_right_idx].to_vec();
        self.bot = next[self.bot_left_idx..self.bot_right_idx].to_vec();
        self.this_range = KeyRange {
            left: Bytes::copy_from_slice(self.top[0].smallest()),
            right: Bytes::copy_from_slice(self.top.last().unwrap().biggest()),
        };
        if self.bot.len() > 0 {
            self.next_range = KeyRange {
                left: Bytes::copy_from_slice(self.bot[0].smallest()),
                right: Bytes::copy_from_slice(self.bot.last().unwrap().biggest()),
            };
        } else {
            self.next_range = self.this_range.clone();
        }
        true
    }

    fn sume_tbl_size(tbls: &[sstable::SSTable]) -> u64 {
        tbls.iter().map(|tbl| tbl.size()).sum()
    }

    fn calc_ratio(top_size: u64, bot_size: u64) -> f64 {
        if bot_size == 0 {
            return top_size as f64;
        }
        top_size as f64 / bot_size as f64
    }

    pub(crate) fn move_down(&self) -> bool {
        self.level > 0 && self.bot.len() == 0
    }
}

impl Engine {
    pub fn update_managed_safe_ts(&self, ts: u64) {
        loop {
            let old = load_u64(&self.managed_safe_ts);
            if old < ts {
                if self
                    .managed_safe_ts
                    .compare_exchange(old, ts, Ordering::Release, Ordering::Relaxed)
                    .is_err()
                {
                    continue;
                }
            }
            break;
        }
    }

    pub(crate) fn run_compaction(&self) {
        let mut results = Vec::new();
        loop {
            self.get_compaction_priorities(&mut results);
            let cnt = results.len().min(self.opts.num_compactors);
            for i in 0..cnt {
                let pri = results[i].clone();
                if let Ok(shard) = self.get_shard_with_ver(pri.shard_id, pri.shard_ver) {
                    store_bool(&shard.compacting, true);
                }
                let res = self.compact(&pri);
                if let Err(err) = res {
                    error!(
                        "compact shard {}:{} failed, {:?}",
                        pri.shard_id, pri.shard_ver, err
                    );
                }
            }
        }
    }

    fn get_compaction_priorities(&self, results: &mut Vec<CompactionPriority>) {
        results.truncate(0);
        for entry in self.shards.iter() {
            let shard = entry.value().clone();
            if shard.is_active() && !load_bool(&shard.compacting) {
                let pri = self.get_compaction_priority(&shard);
                if pri.score > 1.0 {
                    results.push(pri);
                }
            }
        }
        results.sort_by(|i, j| i.score.partial_cmp(&j.score).unwrap().reverse());
    }

    fn get_compaction_priority(&self, shard: &Shard) -> CompactionPriority {
        let mut max_pri = CompactionPriority::default();
        max_pri.shard_id = shard.id;
        max_pri.shard_ver = shard.ver;
        let g = &epoch::pin();
        let l0s = shard.get_l0_tbls(g);
        let size_score = l0s.total_size() as f64 / self.opts.base_size as f64;
        let num_tbl_score = l0s.tbls.len() as f64 / 5.0;
        max_pri.score = size_score * 0.7 + num_tbl_score * 0.3;
        max_pri.cf = -1;

        for cf in 0..NUM_CFS {
            let scf = shard.get_cf(cf, g);
            for lh in &scf.levels {
                let score = lh.total_size as f64
                    / ((self.opts.base_size as f64) * 10f64.powf((lh.level - 1) as f64));
                if max_pri.score < score {
                    max_pri.score = score;
                    max_pri.level = lh.level;
                    max_pri.cf = cf as isize;
                }
            }
        }
        max_pri
    }

    pub(crate) fn compact(&self, pri: &CompactionPriority) -> Result<()> {
        let shard = self.get_shard_with_ver(pri.shard_id, pri.shard_ver)?;
        if shard.is_splitting() {
            info!("avoid compaction for splitting shard");
            return Ok(());
        }
        if !shard.is_active() {
            info!("avoid passive shard compaction");
            return Ok(());
        }
        if pri.cf == -1 {
            info!(
                "compaction shard multi-cf shard {}:{} score:{}",
                shard.id, shard.ver, pri.score
            );
            let req = self.build_compact_l0_request(&shard)?;
            let comp = self.comp_client.compact(req)?;
            info!(
                "compact shard {}:{} level {}, cf {} done",
                shard.id, shard.ver, 0, -1
            );
            self.handle_compact_response(comp, &shard);
            return Ok(());
        }
        info!(
            "start compaction shard {}:{} leve:{} cf:{}, score:{}",
            shard.id, shard.ver, pri.level, pri.cf, pri.score
        );
        let g = &epoch::pin();
        let scf = shard.get_cf(pri.cf as usize, g);
        let this_level = &scf.levels[pri.level - 1];
        let next_level = &scf.levels[pri.level];
        let mut cd = CompactDef::new(pri.cf as usize, pri.level);
        let ok = cd.fill_table(this_level, next_level);
        assert!(ok);
        scf.set_has_overlapping(&mut cd);
        let req = self.build_compact_ln_request(&shard, &cd)?;
        if req.bottoms.len() == 0 && req.cf as usize == WRITE_CF {
            // Move down. only write CF benefits from this optimization.
            let mut comp = pb::Compaction::new();
            comp.set_cf(req.cf as i32);
            comp.set_level(req.level as u32);
            comp.set_top_deletes(req.tops.clone());
            let mut tbl_creates = vec![];
            for top_tbl in &cd.top {
                let mut tbl_create = pb::TableCreate::new();
                tbl_create.set_id(top_tbl.id());
                tbl_create.set_cf(req.cf as i32);
                tbl_create.set_level(req.level as u32 + 1);
                tbl_create.set_smallest(top_tbl.smallest().to_vec());
                tbl_create.set_biggest(top_tbl.biggest().to_vec());
                tbl_creates.push(tbl_create);
            }
            comp.set_table_creates(RepeatedField::from_vec(tbl_creates));
            self.handle_compact_response(comp, &shard);
            return Ok(());
        }
        let comp = self.comp_client.compact(req)?;
        self.handle_compact_response(comp, &shard);
        Ok(())
    }

    pub(crate) fn build_compact_l0_request(&self, shard: &Shard) -> Result<CompactionRequest> {
        let g = &epoch::pin();
        let l0s = shard.get_l0_tbls(g);
        let mut req = self.new_compact_request(shard, -1, 0);
        let mut total_size = 0;
        for l0 in &l0s.tbls {
            req.tops.push(l0.id());
            total_size += l0.size();
        }
        for cf in 0..NUM_CFS {
            let lh = &shard.get_cf(cf, g).levels[0];
            let mut bottoms = vec![];
            for tbl in lh.tables.iter() {
                bottoms.push(tbl.id());
                total_size += tbl.size();
            }
            req.multi_cf_bottoms.push(bottoms);
        }
        self.set_alloc_ids_for_request(&mut req, total_size)?;
        Ok(req)
    }

    pub(crate) fn set_alloc_ids_for_request(
        &self,
        req: &mut CompactionRequest,
        total_size: u64,
    ) -> Result<()> {
        let id_cnt = total_size as usize / req.max_table_size + 16; // Add 16 here just in case we run out of ID.
        let ids = self
            .id_allocator
            .alloc_id(id_cnt)
            .map_err(|e| Error::ErrAllocID(e))?;
        req.start_id = ids[0];
        req.end_id = ids[ids.len() - 1] + 1;
        Ok(())
    }

    pub(crate) fn new_compact_request(
        &self,
        shard: &Shard,
        cf: isize,
        level: usize,
    ) -> CompactionRequest {
        CompactionRequest {
            shard_id: shard.id,
            shard_ver: shard.ver,
            cf,
            level,
            instance_id: self.opts.instance_id,
            safe_ts: load_u64(&self.managed_safe_ts),
            block_size: self.opts.table_builder_options.block_size,
            max_table_size: self.opts.table_builder_options.max_table_size,
            bloom_fpr: self.opts.table_builder_options.bloom_fpr,
            overlap: level == 0,
            tops: vec![],
            bottoms: vec![],
            multi_cf_bottoms: vec![],
            start_id: 0,
            end_id: 0,
        }
    }

    pub(crate) fn build_compact_ln_request(
        &self,
        shard: &Shard,
        cd: &CompactDef,
    ) -> Result<CompactionRequest> {
        let mut req = self.new_compact_request(shard, cd.cf as isize, cd.level);
        req.overlap = cd.has_overlap;
        for top in &cd.top {
            req.tops.push(top.id());
        }
        for bot in &cd.bot {
            req.bottoms.push(bot.id());
        }
        self.set_alloc_ids_for_request(&mut req, cd.top_size + cd.bot_size)?;
        Ok(req)
    }

    pub(crate) fn handle_compact_response(&self, comp: pb::Compaction, shard: &Shard) {
        let mut cs = new_change_set(shard.id, shard.ver, shard.get_split_stage());
        cs.set_compaction(comp);
        self.meta_change_listener.on_change_set(cs);
    }
}

#[derive(Clone, Default)]
pub(crate) struct CompactionPriority {
    pub cf: isize,
    pub level: usize,
    pub score: f64,
    pub shard_id: u64,
    pub shard_ver: u64,
}

#[derive(Clone, Default)]
pub(crate) struct KeyRange {
    pub left: Bytes,
    pub right: Bytes,
}

pub(crate) fn get_key_range(tables: &Vec<SSTable>) -> KeyRange {
    let mut smallest = tables[0].smallest();
    let mut biggest = tables[0].biggest();
    for i in 1..tables.len() {
        let tbl = &tables[i];
        if tbl.smallest() < smallest {
            smallest = tbl.smallest();
        }
        if tbl.biggest() > tbl.biggest() {
            biggest = tbl.biggest();
        }
    }
    KeyRange {
        left: Bytes::copy_from_slice(smallest),
        right: Bytes::copy_from_slice(biggest),
    }
}

pub(crate) fn get_tables_in_range(
    tables: &Vec<SSTable>,
    start: &[u8],
    end: &[u8],
) -> (usize, usize) {
    let left = search(tables.len(), |i| start <= tables[i].biggest());
    let right = search(tables.len(), |i| end < tables[i].smallest());
    (left, right)
}

pub(crate) fn compact_l0(
    req: &CompactionRequest,
    fs: Arc<dyn dfs::DFS>,
) -> Result<Vec<pb::TableCreate>> {
    let opts = dfs::Options::new(req.shard_id, req.shard_ver);
    let l0_files = load_table_files(&req.tops, fs.clone(), opts)?;
    let cache = SegmentedCache::new(256, 1);
    let l0_tbls = in_mem_files_to_l0_tables(&l0_files, cache.clone());

    let mut mult_cf_bot_tbls = vec![];
    for cf in 0..NUM_CFS {
        let bot_ids = &req.multi_cf_bottoms[cf];
        let bot_files = load_table_files(&bot_ids, fs.clone(), opts)?;
        let bot_tbls = in_mem_files_to_tables(&bot_files, cache.clone());
        mult_cf_bot_tbls.push(bot_tbls);
    }

    let channel_cap = req.end_id - req.start_id;
    let (tx, rx) = mpsc::sync_channel(channel_cap as usize);
    let mut id = req.start_id;
    for cf in 0..NUM_CFS {
        let mut iter = build_compact_l0_iterator(
            cf,
            l0_tbls.clone(),
            std::mem::take(&mut mult_cf_bot_tbls[cf]),
        );
        let mut helper = CompactL0Helper::new(cf, req);
        loop {
            let (tbl_create, data) = helper.build_one(&mut iter, id)?;
            if data.is_empty() {
                break;
            }
            let afs = fs.clone();
            let atx = tx.clone();
            fs.get_future_pool().spawn_ok(async move {
                if let Err(err) = afs.create(tbl_create.id, data, opts).await {
                    atx.send(Err(err)).unwrap();
                } else {
                    atx.send(Ok(tbl_create)).unwrap();
                }
            });
            id += 1;
        }
    }
    let mut table_creates = vec![];
    let cnt = id - req.start_id;
    for _ in 0..cnt {
        let tbl_create = rx.recv().unwrap()?;
        table_creates.push(tbl_create);
    }
    Ok(table_creates)
}

pub(crate) fn load_table_files(
    tbl_ids: &Vec<u64>,
    fs: Arc<dyn dfs::DFS>,
    opts: dfs::Options,
) -> Result<Vec<dfs::InMemFile>> {
    let mut files = vec![];
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Bytes>>(tbl_ids.len());
    for id in tbl_ids {
        let aid = *id;
        let atx = tx.clone();
        let afs = fs.clone();
        fs.get_future_pool().spawn_ok(async move {
            let res = afs
                .read_file(aid, opts)
                .await
                .map_err(|e| Error::DFSError(e));
            atx.send(res).unwrap();
        });
    }
    for id in tbl_ids {
        let data = rx.recv().unwrap()?;
        let file = dfs::InMemFile::new(*id, data);
        files.push(file);
    }
    Ok(files)
}

fn in_mem_files_to_l0_tables(
    files: &Vec<dfs::InMemFile>,
    cache: SegmentedCache<sstable::BlockCacheKey, Bytes>,
) -> Vec<sstable::L0Table> {
    let mut tables = vec![];
    for file in files {
        let table = sstable::L0Table::new(Arc::new(file.clone()), cache.clone()).unwrap();
        tables.push(table);
    }
    tables
}

fn in_mem_files_to_tables(
    files: &Vec<dfs::InMemFile>,
    cache: SegmentedCache<sstable::BlockCacheKey, Bytes>,
) -> Vec<sstable::SSTable> {
    let mut tables = vec![];
    for file in files {
        let table = sstable::SSTable::new(Arc::new(file.clone()), cache.clone()).unwrap();
        tables.push(table);
    }
    tables
}

fn build_compact_l0_iterator(
    cf: usize,
    top_tbls: Vec<sstable::L0Table>,
    bot_tbls: Vec<sstable::SSTable>,
) -> Box<dyn table::Iterator> {
    let mut iters: Vec<Box<dyn table::Iterator>> = vec![];
    for top_tbl in top_tbls {
        if let Some(tbl) = top_tbl.get_cf(cf) {
            let iter = tbl.new_iterator(false);
            iters.push(iter);
        }
    }
    if bot_tbls.len() > 0 {
        let iter = ConcatIterator::new_with_tables(bot_tbls, false);
        iters.push(Box::new(iter));
    }
    let mut iter = table::new_merge_iterator(iters, false);
    iter.rewind();
    iter
}

struct CompactL0Helper {
    cf: usize,
    builder: sstable::Builder,
    last_key: BytesMut,
    skip_key: BytesMut,
    safe_ts: u64,
    opts: sstable::TableBuilderOptions,
}

impl CompactL0Helper {
    fn new(cf: usize, req: &CompactionRequest) -> Self {
        Self {
            cf,
            builder: sstable::Builder::new(0, req.get_table_builder_options()),
            last_key: BytesMut::new(),
            skip_key: BytesMut::new(),
            safe_ts: req.safe_ts,
            opts: req.get_table_builder_options(),
        }
    }

    fn build_one<'a>(
        &mut self,
        iter: &mut Box<dyn table::Iterator + 'a>,
        id: u64,
    ) -> Result<(pb::TableCreate, Bytes)> {
        self.builder.reset(id);
        self.last_key.truncate(0);
        self.skip_key.truncate(0);

        while iter.valid() {
            // See if we need to skip this key.
            let key = iter.key();
            if self.skip_key.len() > 0 {
                if key == self.skip_key {
                    iter.next_all_version();
                    continue;
                } else {
                    self.skip_key.truncate(0);
                }
            }
            if key != self.last_key {
                // We only break on table size.
                if self.builder.estimated_size() > self.opts.max_table_size {
                    break;
                }
                self.last_key.extend_from_slice(key);
            }

            // Only consider the versions which are below the safeTS, otherwise, we might end up discarding the
            // only valid version for a running transaction.
            let val = iter.value();
            if val.version <= self.safe_ts {
                // key is the latest readable version of this key, so we simply discard all the rest of the versions.
                self.skip_key.truncate(0);
                self.skip_key.extend_from_slice(key);
                if !val.is_deleted() {
                    match filter(self.safe_ts, self.cf, val) {
                        Decision::Keep => {}
                        Decision::Drop => {
                            iter.next_all_version();
                            continue;
                        }
                        Decision::MarkTombStone => {
                            // There may have old versions for this key, so convert to delete tombstone.
                            self.builder
                                .add(key, table::Value::new_tombstone(val.version));
                            iter.next_all_version();
                            continue;
                        }
                    }
                }
            }
            self.builder.add(key, val);
            iter.next_all_version();
        }
        if self.builder.is_empty() {
            return Ok((pb::TableCreate::new(), Bytes::new()));
        }
        let mut buf = BytesMut::with_capacity(self.builder.estimated_size());
        let res = self.builder.finish(&mut buf);
        let mut table_create = pb::TableCreate::new();
        table_create.set_id(id);
        table_create.set_cf(self.cf as i32);
        table_create.set_smallest(res.smallest);
        table_create.set_biggest(res.biggest);
        Ok((table_create, buf.freeze()))
    }

    fn set_fid(&mut self, id: u64) {
        self.builder.reset(id);
    }
}

pub(crate) fn compact_tables(
    req: &CompactionRequest,
    fs: Arc<dyn dfs::DFS>,
) -> Result<Vec<pb::TableCreate>> {
    let cache = SegmentedCache::new(256, 1);
    let opts = dfs::Options::new(req.shard_id, req.shard_ver);
    let top_files = load_table_files(&req.tops, fs.clone(), opts)?;
    let top_tables = in_mem_files_to_tables(&top_files, cache.clone());
    let bot_files = load_table_files(&req.bottoms, fs.clone(), opts)?;
    let bot_tables = in_mem_files_to_tables(&bot_files, cache.clone());
    let top_iter = Box::new(ConcatIterator::new_with_tables(top_tables, false));
    let bot_iter = Box::new(ConcatIterator::new_with_tables(bot_tables, false));
    let mut iter = table::new_merge_iterator(vec![top_iter, bot_iter], false);
    iter.rewind();

    let mut last_key = BytesMut::new();
    let mut skip_key = BytesMut::new();
    let mut builder = sstable::Builder::new(0, req.get_table_builder_options());
    let mut alloc_id = req.start_id;
    let (tx, rx) = mpsc::sync_channel((req.end_id - req.start_id) as usize);
    while iter.valid() {
        assert!(alloc_id < req.end_id);
        let id = alloc_id;
        builder.reset(id);
        last_key.truncate(0);
        while iter.valid() {
            let val = iter.value();
            let key = iter.key();
            let kv_size = val.encoded_size() + key.len();
            // See if we need to skip this key.
            if skip_key.len() > 0 {
                if key == skip_key {
                    continue;
                } else {
                    skip_key.truncate(0);
                }
            }
            if key != last_key {
                if last_key.len() > 0 && builder.estimated_size() + kv_size > req.max_table_size {
                    break;
                }
                last_key.truncate(0);
                last_key.extend_from_slice(key);
            }

            // Only consider the versions which are below the minReadTs, otherwise, we might end up discarding the
            // only valid version for a running transaction.
            if req.cf as usize == LOCK_CF || val.version <= req.safe_ts {
                // key is the latest readable version of this key, so we simply discard all the rest of the versions.
                skip_key.truncate(0);
                skip_key.extend_from_slice(key);

                if val.is_deleted() {
                    // If this key range has overlap with lower levels, then keep the deletion
                    // marker with the latest version, discarding the rest. We have set skipKey,
                    // so the following key versions would be skipped. Otherwise discard the deletion marker.
                    if !req.overlap {
                        continue;
                    }
                } else {
                    match filter(req.safe_ts, req.cf as usize, val) {
                        Decision::Keep => {}
                        Decision::Drop => continue,
                        Decision::MarkTombStone => {
                            if req.overlap {
                                // There may have old versions for this key, so convert to delete tombstone.
                                builder.add(key, table::Value::new_tombstone(val.version));
                            }
                            continue;
                        }
                    }
                }
            }
            builder.add(key, val);
            iter.next_all_version();
        }
        if builder.is_empty() {
            continue;
        }
        let mut buf = BytesMut::with_capacity(builder.estimated_size());
        let res = builder.finish(&mut buf);
        let mut tbl_create = pb::TableCreate::new();
        tbl_create.set_id(id);
        tbl_create.set_cf(req.cf as i32);
        tbl_create.set_level(req.level as u32 + 1);
        tbl_create.set_smallest(res.smallest);
        tbl_create.set_biggest(res.biggest);
        let afs = fs.clone();
        let atx = tx.clone();
        fs.get_future_pool().spawn_ok(async move {
            if let Err(err) = afs.create(tbl_create.id, buf.freeze(), opts).await {
                atx.send(Err(err)).unwrap();
            } else {
                atx.send(Ok(tbl_create)).unwrap();
            }
        });
        alloc_id += 1;
    }
    let cnt = alloc_id - req.start_id;
    let mut tbl_creates = vec![];
    for _ in 0..cnt {
        let tbl_create = rx.recv().unwrap()?;
        tbl_creates.push(tbl_create);
    }
    Ok(tbl_creates)
}

enum Decision {
    Keep,
    MarkTombStone,
    Drop,
}

const WRITE_CF: usize = 0;
const LOCK_CF: usize = 1;
const EXTRA_CF: usize = 2;

// filter implements the badger.CompactionFilter interface.
// Since we use txn ts as badger version, we only need to filter Delete, Rollback and Op_Lock.
// It is called for the first valid version before safe point, older versions are discarded automatically.
fn filter(safe_ts: u64, cf: usize, val: table::Value) -> Decision {
    let user_meta = val.user_meta();
    if cf == WRITE_CF {
        if user_meta.len() == 16 {
            let commit_ts = LittleEndian::read_u64(&user_meta[8..]);
            if commit_ts < safe_ts && val.get_value().len() == 0 {
                return Decision::MarkTombStone;
            }
        }
    } else if cf == LOCK_CF {
        return Decision::Keep;
    } else {
        assert_eq!(cf, EXTRA_CF);
        if user_meta.len() == 16 {
            let start_ts = LittleEndian::read_u64(user_meta);
            if start_ts < safe_ts {
                return Decision::Drop;
            }
        }
    }
    // Older version are discarded automatically, we need to keep the first valid version.
    return Decision::Keep;
}
