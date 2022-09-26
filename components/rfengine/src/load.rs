// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use byteorder::{ByteOrder, LittleEndian};
use bytes::Bytes;
use slog_global::info;

use crate::{write_batch::RegionBatch, *};

const EPOCH_LEN: usize = 8;
const REGION_ID_LEN: usize = 16;
const START_INDEX_LEN: usize = 16;
const END_INDEX_LEN: usize = 16;
const REGION_ID_OFFSET: usize = EPOCH_LEN + 1;
const START_INDEX_OFFSET: usize = REGION_ID_OFFSET + 1 + REGION_ID_LEN;
const END_INDEX_OFFSET: usize = START_INDEX_OFFSET + 1 + START_INDEX_LEN;

pub(crate) struct Epoch {
    pub(crate) id: u32,
    pub(crate) has_state_file: bool,
    pub(crate) has_wal_file: bool,
    pub(crate) raft_log_files: Mutex<HashMap<u64, (u64, u64)>>,
}

pub(crate) fn get_epoch(epoches: &mut HashMap<u32, Epoch>, epoch_id: u32) -> &mut Epoch {
    if let std::collections::hash_map::Entry::Vacant(e) = epoches.entry(epoch_id) {
        let ep = Epoch::new(epoch_id);
        e.insert(ep);
        epoches.get_mut(&epoch_id).unwrap()
    } else {
        epoches.get_mut(&epoch_id).unwrap()
    }
}

impl Epoch {
    pub(crate) fn new(id: u32) -> Self {
        Self {
            id,
            has_state_file: false,
            has_wal_file: false,
            raft_log_files: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn add_file(&mut self, filename: PathBuf) -> Result<()> {
        let extention = filename.extension().unwrap();
        if extention == "wal" {
            self.has_wal_file = true;
        } else if extention == "states" {
            self.has_state_file = true;
        } else if extention == "rlog" {
            let filename_str = filename.file_name().unwrap().to_str().unwrap();
            let region_id_buf = &filename_str[REGION_ID_OFFSET..REGION_ID_OFFSET + REGION_ID_LEN];
            let region_id = u64::from_str_radix(region_id_buf, 16)?;
            let start_index_buf =
                &filename_str[START_INDEX_OFFSET..START_INDEX_OFFSET + START_INDEX_LEN];
            let start_index = u64::from_str_radix(start_index_buf, 16)?;
            let end_index_buf = &filename_str[END_INDEX_OFFSET..END_INDEX_OFFSET + END_INDEX_LEN];
            let end_index = u64::from_str_radix(end_index_buf, 16)?;
            self.raft_log_files
                .lock()
                .unwrap()
                .insert(region_id, (start_index, end_index));
        }
        Ok(())
    }
}

pub(crate) fn read_epoches(dir: &Path) -> Result<Vec<Epoch>> {
    let mut epoch_map = HashMap::new();
    let recycle_path = dir.join(RECYCLE_DIR);
    let entries = fs::read_dir(dir)?;
    for e in entries {
        let entry = e?;
        let path = entry.path();
        if path.starts_with(&recycle_path) {
            continue;
        }
        let filename = path.file_name().unwrap().to_str().unwrap();
        let epoch_id = u32::from_str_radix(&filename[..8], 16)?;
        let ep = get_epoch(&mut epoch_map, epoch_id);
        ep.add_file(path)?;
    }
    let mut epoches = Vec::new();
    for (_, v) in epoch_map.drain() {
        epoches.push(v);
    }
    epoches.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(epoches)
}

impl RfEngine {
    pub(crate) fn load_epoch(&mut self, ep: &mut Epoch, prev_has_state_file: bool) -> Result<u64> {
        info!(
            "{}: load epoch {}, rlog files {}, has_wal {}, has_state {}",
            self.get_engine_id(),
            ep.id,
            ep.raft_log_files.lock().unwrap().len(),
            ep.has_wal_file,
            ep.has_state_file
        );
        if ep.has_wal_file && ep.has_state_file && !prev_has_state_file {
            // The compact job:
            //   1. write rlog files.
            //   2. write new state file.
            //   3. remove old state file.
            //   4. remove the WAL file.
            //
            // If process crashed between 3 and 4, i.e., the prev epoch doesn't have state
            // file and the current epoch has WAL and state file, the compact job is actually finished
            // so we remove the WAL file directly.
            fs::remove_file(wal_file_name(&self.dir, ep.id))?;
            ep.has_wal_file = false;
        }
        let mut wal_off: u64 = 0;
        if ep.has_wal_file {
            wal_off = self.load_wal_file(ep.id)?;
        } else {
            let raft_log_files = ep.raft_log_files.lock().unwrap();
            for (k, (first, end)) in raft_log_files.iter() {
                self.load_raft_log_file(ep.id, *k, *first, *end)?;
            }
            if ep.has_state_file {
                self.load_state_file(ep.id)?;
            }
        }
        Ok(wal_off)
    }

    pub(crate) fn load_wal_file(&mut self, epoch_id: u32) -> Result<u64> {
        let mut it = WALIterator::new(self.dir.clone(), epoch_id);
        it.iterate(|new_data| {
            let region_ref = self.get_or_init_region_data(new_data.region_id);
            let mut region_data = region_ref.write().unwrap();
            let _ = region_data.apply(&new_data);
        })?;
        Ok(it.offset)
    }

    pub(crate) fn load_state_file(&mut self, epoch_id: u32) -> Result<()> {
        let filename = states_file_name(&self.dir, epoch_id);
        let bin = fs::read(filename)?;
        let mut data = bin.as_slice();
        let _header = StatesHeader::decode(data)?;
        data = &data[StatesHeader::len()..];
        let payload_len = data.len() - 4;
        let checksum = LittleEndian::read_u32(&data[payload_len..]);
        data = &data[..payload_len];
        if crc32fast::hash(data) != checksum {
            return Err(Error::Corruption("checksum mismatch".to_owned()));
        }
        while !data.is_empty() {
            let region_id = LittleEndian::read_u64(data);
            data = &data[8..];
            let key_len = LittleEndian::read_u16(data) as usize;
            data = &data[2..];
            let key = &data[..key_len];
            data = &data[key_len..];
            let val_len = LittleEndian::read_u32(data) as usize;
            data = &data[4..];
            let val = &data[..val_len];
            data = &data[val_len..];
            let region_ref = self.get_or_init_region_data(region_id);
            let mut region_data = region_ref.write().unwrap();
            region_data
                .states
                .insert(Bytes::copy_from_slice(key), Bytes::copy_from_slice(val));
        }
        Ok(())
    }

    pub(crate) fn load_raft_log_file(
        &mut self,
        epoch_id: u32,
        region_id: u64,
        first: u64,
        end: u64,
    ) -> Result<()> {
        let rlog_filename = raft_log_file_name(&self.dir, epoch_id, region_id, first, end);
        let bin = fs::read(&rlog_filename)?;
        let _header = RlogHeader::decode(bin.as_slice())?;
        let mut data = &bin[RlogHeader::len()..];
        let checksum = LittleEndian::read_u32(&data[data.len() - 4..]);
        data = &data[..data.len() - 4];
        if crc32fast::hash(data) != checksum {
            return Err(Error::Corruption("checksum mismatch".to_owned()));
        }
        let new_data = RegionBatch::decode(data);
        let old_data_ref = self.get_or_init_region_data(new_data.region_id);
        let mut old_data = old_data_ref.write().unwrap();
        let _ = old_data.apply(&new_data);
        Ok(())
    }
}
