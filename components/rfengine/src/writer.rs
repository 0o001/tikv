// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use crate::*;
use std::{
    fs::File,
    fs::{self, OpenOptions},
    mem,
    os::unix::fs::OpenOptionsExt,
    os::unix::prelude::FileExt,
    path::{Path, PathBuf},
};

use bytes::BufMut;

pub const BATCH_HEADER_SIZE: usize = 12;
pub(crate) const ALIGN_SIZE: usize = 4096;
pub(crate) const ALIGN_MASK: u64 = 0xffff_f000;
pub(crate) const INITIAL_BUF_SIZE: usize = 8 * 1024 * 1024;
pub(crate) const RECYCLE_DIR: &str = "recycle";

#[repr(C, align(4096))]
struct AlignTo4K([u8; ALIGN_SIZE]);

pub fn alloc_aligned(n_bytes: usize) -> Vec<u8> {
    let n_units = (n_bytes + ALIGN_SIZE - 1) / ALIGN_SIZE;

    let mut aligned: Vec<AlignTo4K> = Vec::with_capacity(n_units);

    let ptr = aligned.as_mut_ptr();
    let cap_units = aligned.capacity();

    mem::forget(aligned);
    let result =
        unsafe { Vec::from_raw_parts(ptr as *mut u8, 0, cap_units * mem::size_of::<AlignTo4K>()) };
    result
}

pub(crate) struct WALWriter {
    dir: PathBuf,
    pub(crate) epoch_id: u32,
    pub(crate) wal_size: usize,
    fd: File,
    buf: Vec<u8>,
    // file_off is always aligned.
    file_off: u64,
}

impl WALWriter {
    pub(crate) fn new(dir: &Path, epoch_id: u32, wal_size: usize) -> Result<Self> {
        let wal_size = (wal_size + ALIGN_SIZE - 1) & ALIGN_MASK as usize;
        let file_path = get_wal_file_path(dir, epoch_id)?;
        let fd = open_direct_file(file_path, true)?;
        let mut buf = alloc_aligned(INITIAL_BUF_SIZE);
        buf.resize(BATCH_HEADER_SIZE, 0);
        Ok(Self {
            dir: dir.to_path_buf(),
            epoch_id,
            wal_size,
            fd,
            buf,
            file_off: 0,
        })
    }

    pub(crate) fn seek(&mut self, file_offset: u64) {
        assert_eq!(file_offset & ALIGN_MASK.reverse_bits(), 0);
        self.file_off = file_offset;
    }

    pub(crate) fn reallocate(&mut self) {
        let new_cap = self.buf.capacity() * 2;
        let mut new_buf = alloc_aligned(new_cap);
        new_buf.truncate(0);
        new_buf.extend_from_slice(self.buf.as_slice());
        let _ = mem::replace(&mut self.buf, new_buf);
    }

    pub(crate) fn append_region_data(&mut self, region_data: &RegionData) {
        if self.buf.len() + region_data.encoded_len() > self.buf.capacity() {
            self.reallocate();
        }
        region_data.encode_to(&mut self.buf);
    }

    pub(crate) fn flush(&mut self) -> Result<bool> {
        let mut rotated = false;
        if aligned_len(self.buf.len()) + self.file_off as usize > self.wal_size {
            self.rotate()?;
            rotated = true;
        }
        let batch = &mut self.buf[..];
        let (mut batch_header, batch_payload) = batch.split_at_mut(BATCH_HEADER_SIZE);
        let checksum = crc32c::crc32c(batch_payload);
        batch_header.put_u32_le(self.epoch_id);
        batch_header.put_u32_le(checksum);
        batch_header.put_u32_le(batch_payload.len() as u32);
        self.buf.resize(aligned_len(self.buf.len()), 0);
        self.fd.write_all_at(&self.buf[..], self.file_off)?;
        self.file_off += self.buf.len() as u64;
        self.reset_batch();
        Ok(rotated)
    }

    pub(crate) fn reset_batch(&mut self) {
        self.buf.truncate(BATCH_HEADER_SIZE);
    }

    fn rotate(&mut self) -> Result<()> {
        self.epoch_id += 1;
        self.open_file()
    }

    pub(crate) fn open_file(&mut self) -> Result<()> {
        let filename = get_wal_file_path(&self.dir, self.epoch_id)?;
        let file = open_direct_file(filename, true)?;
        file.set_len(self.wal_size as u64)?;
        self.fd = file;
        self.file_off = 0;
        Ok(())
    }
}

pub(crate) fn get_wal_file_path(dir: &Path, epoch_id: u32) -> Result<PathBuf> {
    let filename = wal_file_name(dir, epoch_id);
    if !filename.exists() {
        if let Ok(Some(recycle_filename)) = find_recycled_file(dir) {
            fs::rename(recycle_filename, filename.clone())?;
        }
    }
    Ok(filename)
}

pub(crate) fn open_direct_file(filename: PathBuf, sync: bool) -> Result<File> {
    let mut flag = o_direct_flag();
    if sync {
        flag |= libc::O_DSYNC;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .custom_flags(flag)
        .open(filename)?;
    Ok(file)
}

pub(crate) fn aligned_len(origin_len: usize) -> usize {
    ((origin_len + ALIGN_SIZE - 1) as u64 & ALIGN_MASK) as usize
}

pub(crate) fn find_recycled_file(dir: &Path) -> Result<Option<PathBuf>> {
    let recycle_dir = dir.join(RECYCLE_DIR);
    let read_dir = recycle_dir.read_dir()?;
    let mut recycle_file = None;
    for x in read_dir {
        let dir_entry = x?;
        if dir_entry.path().is_file() {
            recycle_file = Some(dir_entry.path())
        }
    }
    Ok(recycle_file)
}

pub(crate) fn o_direct_flag() -> i32 {
    if std::env::consts::OS != "linux" {
        return 0;
    }
    if std::env::consts::ARCH == "aarch64" {
        0x10000
    } else {
        0x4000
    }
}
