// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use crate::dfs;
use crate::table::*;

use super::builder::*;
use super::iterator::TableIterator;
use crate::table::table::Result;
use byteorder::ByteOrder;
use byteorder::LittleEndian;
use bytes::BytesMut;
use bytes::{Buf, Bytes};
use moka::sync::SegmentedCache;
use slog_global::info;
use std::cmp::Ordering;
use std::path::PathBuf;
use std::slice;
use std::sync::Arc;

#[derive(Clone)]
pub struct SSTable {
    file: Arc<dyn dfs::File>,
    cache: SegmentedCache<BlockCacheKey, Bytes>,
    footer: Footer,
    smallest_buf: Bytes,
    biggest_buf: Bytes,
    pub idx: Index,
    pub old_idx: Index,
}

impl SSTable {
    pub fn new(
        file: Arc<dyn dfs::File>,
        cache: SegmentedCache<BlockCacheKey, Bytes>,
    ) -> Result<Self> {
        let mut footer = Footer::default();
        if file.size() < FOOTER_SIZE as u64 {
            return Err(table::Error::InvalidFileSize);
        }
        let footer_data = file.read(file.size() - FOOTER_SIZE as u64, FOOTER_SIZE)?;
        footer.unmarshal(footer_data.chunk());
        if footer.magic != MAGIC_NUMBER {
            return Err(table::Error::InvalidMagicNumber);
        }
        let idx_data = file.read(footer.index_offset as u64, footer.index_len())?;
        let idx = Index::new(idx_data.clone(), footer.checksum_type)?;
        let old_idx_data = file.read(footer.old_index_offset as u64, footer.old_index_len())?;
        let old_idx = Index::new(old_idx_data.clone(), footer.checksum_type)?;
        let props_data = file.read(
            footer.properties_offset as u64,
            footer.properties_len(file.size() as usize),
        )?;
        let mut prop_slice = props_data.chunk();
        validate_checksum(prop_slice, footer.checksum_type)?;
        prop_slice = &prop_slice[4..];
        let mut smallest_buf = Bytes::new();
        let mut biggest_buf = Bytes::new();
        while prop_slice.len() > 0 {
            let (key, val, remain) = parse_prop_data(prop_slice);
            prop_slice = remain;
            if key == PROP_KEY_SMALLEST.as_bytes() {
                smallest_buf = Bytes::copy_from_slice(val);
            } else if key == PROP_KEY_BIGGEST.as_bytes() {
                biggest_buf = Bytes::copy_from_slice(val);
            }
        }
        Ok(Self {
            file,
            cache,
            footer,
            smallest_buf,
            biggest_buf,
            idx,
            old_idx,
        })
    }

    pub fn load_block(&self, pos: usize) -> Result<Bytes> {
        let addr = self.idx.block_addrs[pos];
        let length: usize;
        if pos + 1 < self.idx.num_blocks() {
            length = (self.idx.block_addrs[pos + 1].curr_off - addr.curr_off) as usize;
        } else {
            length = self.footer.data_len() - addr.curr_off as usize;
        }
        self.load_block_by_addr_len(addr, length)
    }

    fn load_block_by_addr_len(&self, addr: BlockAddress, length: usize) -> Result<Bytes> {
        let cache_key = BlockCacheKey::new(addr.origin_fid, addr.origin_off);
        if let Some(block) = self.cache.get(&cache_key) {
            return Ok(block);
        }
        let raw_block = self.file.read(addr.curr_off as u64, length)?;
        validate_checksum(raw_block.chunk(), self.footer.checksum_type)?;
        let block = raw_block.slice(4..);
        self.cache.insert(cache_key, block.clone());
        Ok(block)
    }

    pub fn load_old_block(&self, pos: usize) -> Result<Bytes> {
        let addr = self.old_idx.block_addrs[pos];
        let length: usize;
        if pos + 1 < self.old_idx.num_blocks() {
            length = (self.old_idx.block_addrs[pos + 1].curr_off - addr.curr_off) as usize;
        } else {
            length = self.footer.index_offset as usize - addr.curr_off as usize;
        }
        self.load_block_by_addr_len(addr, length)
    }
}

impl SSTable {
    pub fn id(&self) -> u64 {
        return self.file.id();
    }

    pub fn size(&self) -> u64 {
        return self.file.size();
    }

    pub fn smallest(&self) -> &[u8] {
        return self.smallest_buf.chunk();
    }

    pub fn biggest(&self) -> &[u8] {
        return self.biggest_buf.chunk();
    }

    pub fn new_iterator(&self, reversed: bool) -> Box<TableIterator> {
        let it = TableIterator::new(&self, reversed);
        Box::new(it)
    }

    pub fn get(&self, key: &[u8], version: u64, _key_hash: u64) -> table::Value {
        let mut it = self.new_iterator(false);
        it.seek(key);
        if !it.valid() || key != it.key() {
            return table::Value::new();
        }
        while it.value().version > version {
            if !it.next_version() {
                return table::Value::new();
            }
        }
        it.value()
    }

    pub fn has_overlap(&self, start: &[u8], end: &[u8], include_end: bool) -> bool {
        if start > self.biggest() {
            return false;
        }
        match end.cmp(self.smallest()) {
            Ordering::Less => {
                return false;
            }
            Ordering::Equal => {
                return include_end;
            }
            _ => {}
        }
        let mut it = self.new_iterator(false);
        it.seek(start);
        if !it.valid() {
            return it.error().is_some();
        }
        match it.key().cmp(end) {
            Ordering::Greater => false,
            Ordering::Equal => include_end,
            _ => true,
        }
    }

    pub fn get_suggest_split_key(&self) -> Option<Bytes> {
        let num_blocks = self.idx.num_blocks();
        if num_blocks > 0 {
            let diff_key = self.idx.block_diff_key(num_blocks / 2);
            let mut split_key = BytesMut::new();
            split_key.extend_from_slice(self.idx.common_prefix.chunk());
            split_key.extend_from_slice(diff_key);
            return Some(split_key.freeze());
        }
        None
    }
}

#[derive(Clone)]
pub struct Index {
    bin: Bytes,
    common_prefix: Bytes,
    block_key_offs: &'static [u32],
    block_addrs: &'static [BlockAddress],
    block_keys: Bytes,
}

impl Index {
    fn new(bin: Bytes, checksum_type: u8) -> Result<Self> {
        let data = bin.chunk();
        validate_checksum(data, checksum_type)?;
        let mut offset = 4 as usize;
        let num_blocks = LittleEndian::read_u32(&data[offset..]) as usize;
        offset += 4;
        let block_key_offs = unsafe {
            let ptr = data[offset..].as_ptr() as *mut u32;
            slice::from_raw_parts(ptr, num_blocks)
        };
        offset += 4 * num_blocks;

        let block_addrs = unsafe {
            let ptr = data[offset..].as_ptr() as *mut BlockAddress;
            slice::from_raw_parts(ptr, num_blocks)
        };
        offset += BLOCK_ADDR_SIZE * num_blocks;
        let common_prefix_len = LittleEndian::read_u16(&data[offset..]) as usize;
        offset += 2;
        let common_prefix = bin.slice(offset..offset + common_prefix_len);
        offset += common_prefix_len;
        let block_key_len = LittleEndian::read_u32(&data[offset..]) as usize;
        offset += 4;
        let block_keys = bin.slice(offset..offset + block_key_len);
        Ok(Self {
            bin,
            common_prefix,
            block_key_offs,
            block_addrs,
            block_keys,
        })
    }

    pub fn num_blocks(&self) -> usize {
        self.block_key_offs.len()
    }

    pub fn seek_block(&self, key: &[u8]) -> usize {
        if key.len() <= self.common_prefix.len() {
            if key <= self.common_prefix.chunk() {
                return 0;
            }
            return self.num_blocks();
        }
        let cmp = key[..self.common_prefix.len()].cmp(self.common_prefix.chunk());
        match cmp {
            Ordering::Less => 0,
            Ordering::Equal => {
                let diff_key = &key[self.common_prefix.len()..];
                search(self.num_blocks(), |i| self.block_diff_key(i) > diff_key)
            }
            Ordering::Greater => self.num_blocks(),
        }
    }

    fn block_diff_key(&self, i: usize) -> &[u8] {
        let off = self.block_key_offs[i] as usize;
        let end_off: usize;
        if i + 1 < self.num_blocks() {
            end_off = self.block_key_offs[i + 1] as usize;
        } else {
            end_off = self.block_keys.len();
        }
        return &self.block_keys[off..end_off];
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq, Debug)]
pub struct BlockCacheKey {
    origin_id: u64,
    origin_off: u32,
}

impl BlockCacheKey {
    pub fn new(origin_id: u64, origin_off: u32) -> Self {
        Self {
            origin_id,
            origin_off,
        }
    }
}

fn validate_checksum(data: &[u8], checksum_type: u8) -> Result<()> {
    if data.len() < 4 {
        return Err(table::Error::InvalidChecksum(String::from(
            "data is too short",
        )));
    }
    let checksum = LittleEndian::read_u32(data);
    let content = &data[4..];
    if checksum_type == CRC32_CASTAGNOLI {
        let got_checksum = crc32c::crc32c(content);
        if checksum != got_checksum {
            return Err(table::Error::InvalidChecksum(format!(
                "checksum mismatch expect {} got {}",
                checksum, got_checksum
            )));
        }
    }
    Ok(())
}

const FILE_SUFFIX: &str = ".sst";

fn parse_file_id(path: &PathBuf) -> Result<u64> {
    let name = path.file_name().unwrap().to_str().unwrap();
    if name.as_bytes().ends_with(FILE_SUFFIX.as_bytes()) {
        return Err(table::Error::InvalidFileName);
    }
    let digit_part = &name[..name.len() - FILE_SUFFIX.len()];
    if let Ok(id) = u64::from_str_radix(digit_part, 16) {
        return Ok(id);
    }
    Err(table::Error::InvalidFileName)
}

fn parse_prop_data(mut prop_data: &[u8]) -> (&[u8], &[u8], &[u8]) {
    let key_len = LittleEndian::read_u16(prop_data) as usize;
    prop_data = &prop_data[2..];
    let key = &prop_data[..key_len];
    prop_data = &prop_data[key_len..];
    let val_len = LittleEndian::read_u32(prop_data) as usize;
    prop_data = &prop_data[4..];
    let val = &prop_data[..val_len];
    let remained = &prop_data[val_len..];
    (key, val, remained)
}

pub fn id_to_filename(id: u64) -> String {
    format!("{:016x}.sst", id)
}

pub fn new_filename(id: u64, dir: &PathBuf) -> PathBuf {
    dir.join(id_to_filename(id))
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use rand::Rng;
    use std::sync::atomic::AtomicU64;

    use crate::{table::sstable::ConcatIterator, Iterator};

    use super::*;
    use std::sync::atomic::Ordering;

    static ID_ALLOC: AtomicU64 = AtomicU64::new(1);

    fn default_builder_opts() -> TableBuilderOptions {
        TableBuilderOptions {
            block_size: 4096,
            bloom_fpr: 0.01,
            max_table_size: 32 * 1024,
        }
    }

    fn key(prefix: &str, i: usize) -> String {
        format!("{}{:04}", prefix, i)
    }

    fn generate_key_values(prefix: &str, n: usize) -> Vec<(String, String)> {
        let mut results = Vec::with_capacity(n);
        assert!(n <= 10000);
        for i in 0..n {
            let k = key(prefix, i);
            let v = format!("{}", i);
            results.push((k, v));
        }
        results
    }

    fn new_table_builder_for_test(id: u64) -> Builder {
        let opts = default_builder_opts();
        Builder::new(id, opts)
    }

    fn new_cache() -> SegmentedCache<BlockCacheKey, Bytes> {
        SegmentedCache::new(1024, 4)
    }

    fn build_table(key_vals: Vec<(String, String)>) -> Arc<dyn dfs::File> {
        let id = ID_ALLOC.fetch_add(1, Ordering::Relaxed) + 1;
        let mut builder = new_table_builder_for_test(id);
        for (k, v) in key_vals {
            let val_buf = Value::encode_buf('A' as u8, &[0], 0, v.as_bytes());
            builder.add(k.as_bytes(), Value::decode(val_buf.as_slice()));
        }
        let mut buf = BytesMut::with_capacity(builder.estimated_size());
        builder.finish(&mut buf);
        Arc::new(dfs::InMemFile::new(id, buf.freeze()))
    }

    fn build_test_table(prefix: &str, n: usize) -> Arc<dyn dfs::File> {
        let kvs = generate_key_values(prefix, n);
        build_table(kvs)
    }

    fn build_multi_vesion_table(
        mut key_vals: Vec<(String, String)>,
    ) -> (Arc<dyn dfs::File>, usize) {
        let id = ID_ALLOC.fetch_add(1, Ordering::Relaxed) + 1;
        let mut builder = new_table_builder_for_test(id);
        key_vals.sort_by(|a, b| a.0.cmp(&b.0));
        let mut all_cnt = key_vals.len();
        for (k, v) in &key_vals {
            let val_str = format!("{}_{}", v, 9);
            let val_buf = Value::encode_buf('A' as u8, &[0], 9, val_str.as_bytes());
            builder.add(k.as_bytes(), Value::decode(val_buf.as_slice()));
            let mut r = rand::thread_rng();
            for i in (1..=8).rev() {
                if r.gen_range(0..4) == 0 {
                    let val_str = format!("{}_{}", v, i);
                    let val_buf = Value::encode_buf('A' as u8, &[0], i, val_str.as_bytes());
                    builder.add(k.as_bytes(), Value::decode(val_buf.as_slice()));
                    all_cnt += 1;
                }
            }
        }
        let mut buf = BytesMut::with_capacity(builder.estimated_size());
        builder.finish(&mut buf);
        (Arc::new(dfs::InMemFile::new(id, buf.freeze())), all_cnt)
    }

    #[test]
    fn test_table_iterator() {
        for n in 99..=101 {
            let file = build_test_table("key", n);
            let t = SSTable::new(file, new_cache()).unwrap();
            let mut it = t.new_iterator(false);
            let mut count = 0;
            it.rewind();
            while it.valid() {
                let k = it.key();
                assert_eq!(k, key("key", count).as_bytes());
                let v = it.value();
                assert_eq!(v.get_value(), format!("{}", count).as_bytes());
                count += 1;
                it.next()
            }
        }
    }

    #[test]
    fn test_point_get() {
        let kvs = generate_key_values("key", 8000);
        let tf = build_table(kvs);
        let t = SSTable::new(tf, new_cache()).unwrap();
        for i in 0..100 {
            let k = key("key", 0);
            let k_h = farmhash::fingerprint64(k.as_bytes());
            let v = t.get(k.as_bytes(), u64::MAX, k_h);
            assert!(!v.is_empty())
        }

        for i in 0..8000 {
            let k = key("key", i);
            let k_h = farmhash::fingerprint64(k.as_bytes());
            let v = t.get(k.as_bytes(), u64::MAX, k_h);
            assert!(!v.is_empty())
        }
        for i in 8000..10000 {
            let k = key("key", i);
            let k_h = farmhash::fingerprint64(k.as_bytes());
            let v = t.get(k.as_bytes(), u64::MAX, k_h);
            assert!(v.is_empty())
        }
    }

    #[test]
    fn test_seek_to_first() {
        let nums = &[99, 100, 101, 199, 200, 250, 9999, 10000];
        for n in nums {
            let tf = build_test_table("key", *n);
            let t = SSTable::new(tf, new_cache()).unwrap();
            let mut it = t.new_iterator(false);
            it.rewind();
            assert!(it.valid());
            let v = it.value();
            assert_eq!(v.get_value(), "0".as_bytes());
            assert_eq!(v.meta, 'A' as u8);
            assert_eq!(v.user_meta(), &[0u8]);
        }
    }

    struct TestData {
        input: &'static str,
        valid: bool,
        output: &'static str,
    }
    impl TestData {
        fn new(input: &'static str, valid: bool, output: &'static str) -> Self {
            Self {
                input,
                valid,
                output,
            }
        }
    }

    #[test]
    fn test_seek_to_last() {
        let nums = vec![99, 100, 101, 199, 200, 250, 9999, 10000];
        for n in nums {
            let tf = build_test_table("key", n);
            let t = SSTable::new(tf, new_cache()).unwrap();
            let mut it = t.new_iterator(true);
            it.rewind();
            assert!(it.valid());
            let v = it.value();
            assert_eq!(v.get_value(), format!("{}", n - 1).as_bytes());
            assert_eq!(v.meta, 'A' as u8);
            assert_eq!(v.user_meta(), &[0u8]);
            it.next();
            assert!(it.valid());
            let v = it.value();
            assert_eq!(v.get_value(), format!("{}", n - 2).as_bytes());
            assert_eq!(v.meta, 'A' as u8);
            assert_eq!(v.user_meta(), &[0u8]);
        }
    }

    #[test]
    fn test_seek_basic() {
        let test_datas: Vec<TestData> = vec![
            TestData::new("abc", true, "k0000"),
            TestData::new("k0100", true, "k0100"),
            TestData::new("k0100b", true, "k0101"),
            TestData::new("k1234", true, "k1234"),
            TestData::new("k1234b", true, "k1235"),
            TestData::new("k9999", true, "k9999"),
            TestData::new("z", false, ""),
        ];
        let tf = build_test_table("k", 10000);
        let t = SSTable::new(tf, new_cache()).unwrap();
        let mut it = t.new_iterator(false);
        for td in test_datas {
            it.seek(td.input.as_bytes());
            if !td.valid {
                assert!(!it.valid());
                continue;
            }
            assert!(it.valid());
            assert_eq!(it.key(), td.output.as_bytes());
        }
    }

    #[test]
    fn test_seek_for_prev() {
        let test_datas: Vec<TestData> = vec![
            TestData::new("abc", false, ""),
            TestData::new("k0100", true, "k0100"),
            TestData::new("k0100b", true, "k0100"),
            TestData::new("k1234", true, "k1234"),
            TestData::new("k1234b", true, "k1234"),
            TestData::new("k9999", true, "k9999"),
            TestData::new("z", true, "k9999"),
        ];
        let tf = build_test_table("k", 10000);
        let t = SSTable::new(tf, new_cache()).unwrap();
        let mut it = t.new_iterator(true);
        for td in test_datas {
            it.seek(td.input.as_bytes());
            if !td.valid {
                assert!(!it.valid());
                continue;
            }
            assert!(it.valid());
            assert_eq!(it.key(), td.output.as_bytes());
        }
    }

    #[test]
    fn test_iterate_from_start() {
        let nums = vec![99, 100, 101, 199, 200, 250, 9999, 10000];
        for n in nums {
            let file = build_test_table("key", n);
            let t = SSTable::new(file, new_cache()).unwrap();
            let mut it = t.new_iterator(false);
            let mut count = 0;
            it.rewind();
            assert!(it.valid());
            while it.valid() {
                let k = it.key();
                assert_eq!(k, key("key", count).as_bytes());
                let v = it.value();
                assert_eq!(v.get_value(), format!("{}", count).as_bytes());
                assert_eq!(v.meta, 'A' as u8);
                count += 1;
                it.next()
            }
        }
    }

    #[test]
    fn test_iterate_from_end() {
        let nums = vec![99, 100, 101, 199, 200, 250, 9999, 10000];
        for n in nums {
            let file = build_test_table("key", n);
            let t = SSTable::new(file, new_cache()).unwrap();
            let mut it = t.new_iterator(true);
            it.seek("zzzzzz".as_bytes()); // Seek to end, an invalid element.
            assert!(it.valid());
            it.rewind();
            for i in (0..n).rev() {
                assert!(it.valid());
                let v = it.value();
                assert_eq!(v.get_value(), format!("{}", i).as_bytes());
                assert_eq!(v.meta, 'A' as u8);
                it.next();
            }
            it.next();
            assert!(!it.valid());
        }
    }

    #[test]
    fn test_table() {
        let tf = build_test_table("key", 10000);
        let t = SSTable::new(tf, new_cache()).unwrap();
        let mut it = t.new_iterator(false);
        let mut kid = 1010 as usize;
        let seek = key("key", kid);
        it.seek(seek.as_bytes());
        while it.valid() {
            assert_eq!(it.key(), key("key", kid).as_bytes());
            kid += 1;
            it.next()
        }
        assert_eq!(kid, 10000);

        it.seek(key("key", 99999).as_bytes());
        assert!(!it.valid());

        it.seek(key("kex", 0).as_bytes());
        assert!(it.valid());
        assert_eq!(it.key(), key("key", 0).as_bytes());
    }

    #[test]
    fn test_iterate_back_and_forth() {
        let tf = build_test_table("key", 10000);
        let t = SSTable::new(tf, new_cache()).unwrap();

        let seek = key("key", 1010);
        let mut it = t.new_iterator(false);
        it.seek(seek.as_bytes());
        assert!(it.valid());
        assert_eq!(it.key(), seek.as_bytes());

        it.set_reversed(true);
        it.next();
        it.next();
        assert!(it.valid());
        assert_eq!(it.key(), key("key", 1008).as_bytes());

        it.set_reversed(false);
        it.next();
        it.next();
        assert_eq!(it.valid(), true);
        assert_eq!(it.key(), key("key", 1010).as_bytes());

        it.seek(key("key", 2000).as_bytes());
        assert_eq!(it.valid(), true);
        assert_eq!(it.key(), key("key", 2000).as_bytes());

        it.set_reversed(true);
        it.next();
        assert_eq!(it.valid(), true);
        assert_eq!(it.key(), key("key", 1999).as_bytes());

        it.set_reversed(false);
        it.rewind();
        assert_eq!(it.key(), key("key", 0).as_bytes());
    }

    #[test]
    fn test_iterate_multi_version() {
        let num = 4000;
        let (tf, all_cnt) = build_multi_vesion_table(generate_key_values("key", num));
        let t = SSTable::new(tf, new_cache()).unwrap();
        let mut it = t.new_iterator(false);
        let mut it_cnt = 0;
        let mut last_key = BytesMut::new();
        it.rewind();
        while it.valid() {
            if last_key.len() > 0 {
                assert!(last_key < it.key());
            }
            last_key.truncate(0);
            last_key.extend_from_slice(it.key());
            it_cnt += 1;
            while it.next_version() {
                it_cnt += 1;
            }
            it.next();
        }
        assert_eq!(it_cnt, all_cnt);
        let mut r = rand::thread_rng();
        for _ in 0..1000 {
            let k = key("key", r.gen_range(0..num));
            let ver = 5 + r.gen_range(0..5) as u64;
            let k_h = farmhash::fingerprint64(k.as_bytes());
            let val = t.get(k.as_bytes(), ver, k_h);
            if !val.is_empty() {
                assert!(val.version <= ver);
            }
        }
        let mut rev_it = t.new_iterator(true);
        last_key.truncate(0);
        rev_it.rewind();
        while rev_it.valid() {
            if last_key.len() > 0 {
                assert!(last_key > rev_it.key());
            }
            last_key.truncate(0);
            last_key.extend_from_slice(rev_it.key());
            rev_it.next();
        }
        for _ in 0..1000 {
            let k = key("key", r.gen_range(0..num));
            // reverse iterator never seek to the same key with smaller version.
            rev_it.seek(k.as_bytes());
            if !rev_it.valid() {
                continue;
            }
            assert_eq!(rev_it.value().version, 9);
            assert!(rev_it.key() <= k.as_bytes());
        }
    }

    #[test]
    fn test_uni_iterator() {
        let tf = build_test_table("key", 10000);
        let t = SSTable::new(tf, new_cache()).unwrap();
        {
            let mut it = t.new_iterator(false);
            let mut cnt = 0;
            it.rewind();
            while it.valid() {
                let v = it.value();
                assert_eq!(v.get_value(), format!("{}", cnt).as_bytes());
                assert_eq!(v.meta, 'A' as u8);
                cnt += 1;
                it.next();
            }
            assert_eq!(cnt, 10000);
        }
        {
            let mut it = t.new_iterator(true);
            let mut cnt = 0;
            it.rewind();
            while it.valid() {
                let v = it.value();
                assert_eq!(v.get_value(), format!("{}", 10000 - 1 - cnt).as_bytes());
                assert_eq!(v.meta, 'A' as u8);
                cnt += 1;
                it.next();
            }
        }
    }

    #[test]
    fn test_concat_iterator_one_table() {
        let tf = build_table(vec![
            ("k1".to_string(), "a1".to_string()),
            ("k2".to_string(), "a2".to_string()),
        ]);
        let t = SSTable::new(tf, new_cache()).unwrap();
        let tbls = vec![t];
        let mut it = ConcatIterator::new(&tbls, false);
        it.rewind();
        assert_eq!(it.valid(), true);
        assert_eq!(it.key(), "k1".as_bytes());
        let v = it.value();
        assert_eq!(v.get_value(), "a1".as_bytes());
        assert_eq!(v.meta, 'A' as u8);
    }

    #[test]
    fn test_concat_iterator() {
        let tf1 = build_test_table("keya", 10000);
        let tf2 = build_test_table("keyb", 10000);
        let tf3 = build_test_table("keyc", 10000);
        let t1 = SSTable::new(tf1, new_cache()).unwrap();
        let t2 = SSTable::new(tf2, new_cache()).unwrap();
        let t3 = SSTable::new(tf3, new_cache()).unwrap();
        let tbls = vec![t1, t2, t3];
        {
            let mut it = ConcatIterator::new(&tbls, false);
            it.rewind();
            assert_eq!(it.valid(), true);
            let mut cnt = 0;
            while it.valid() {
                let v = it.value();
                assert_eq!(v.get_value(), format!("{}", cnt % 10000).as_bytes());
                assert_eq!(v.meta, 'A' as u8);
                cnt += 1;
                it.next();
            }
            assert_eq!(cnt, 30000);
            it.seek("a".as_bytes());
            assert_eq!(it.key(), "keya0000".as_bytes());
            assert_eq!(it.value().get_value(), "0".as_bytes());

            it.seek("keyb".as_bytes());
            assert_eq!(it.key(), "keyb0000".as_bytes());
            assert_eq!(it.value().get_value(), "0".as_bytes());

            it.seek("keyb9999b".as_bytes());
            assert_eq!(it.key(), "keyc0000".as_bytes());
            assert_eq!(it.value().get_value(), "0".as_bytes());

            it.seek("keyd".as_bytes());
            assert_eq!(it.valid(), false);
        }
        {
            let mut it = ConcatIterator::new(&tbls, true);
            it.rewind();
            assert_eq!(it.valid(), true);
            let mut cnt = 0;
            while it.valid() {
                let v = it.value();
                assert_eq!(
                    v.get_value(),
                    format!("{}", 10000 - (cnt % 10000) - 1).as_bytes()
                );
                assert_eq!(v.meta, 'A' as u8);
                cnt += 1;
                it.next();
            }
            assert_eq!(cnt, 30000);

            it.seek("a".as_bytes());
            assert_eq!(it.valid(), false);

            it.seek("keyb".as_bytes());
            assert_eq!(it.key(), "keya9999".as_bytes());
            assert_eq!(it.value().get_value(), "9999".as_bytes());

            it.seek("keyb9999b".as_bytes());
            assert_eq!(it.key(), "keyb9999".as_bytes());
            assert_eq!(it.value().get_value(), "9999".as_bytes());

            it.seek("keyd".as_bytes());
            assert_eq!(it.key(), "keyc9999".as_bytes());
            assert_eq!(it.value().get_value(), "9999".as_bytes());
        }
    }
}
