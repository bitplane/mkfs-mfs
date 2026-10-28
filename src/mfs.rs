//! Macintosh File System (MFS) disk image writer.
//!
//! Reference: Apple, "Inside Macintosh, Volume II" (1985); CiderPress2 MFS
//! format documentation.
//!
//! Layout (512-byte sectors throughout):
//!
//! ```text
//! sectors 0..2:                         boot blocks
//! sectors 2..(2 + mdb_blocks):          MDB volume info + 12-bit alloc map
//! sectors (2 + mdb_blocks)..dir_end:    file directory
//! sectors dir_end..(end - mdb_blocks):  allocation blocks (variable size)
//! sectors (end - mdb_blocks)..end:      backup MDB (mirror of primary)
//! ```
//!
//! All on-disk values are big-endian. The 12-bit allocation map packs two
//! entries into three bytes (`AB CD EF` -> `0xABC, 0xDEF`).

use anyhow::Context;

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const BLOCK_SIZE: usize = 512;
const MIN_ALLOC_BLK_SIZE: u32 = 1024;
const MDB_BLOCKS: u32 = 2;
const MAP_CAPACITY: u32 = (MDB_BLOCKS * BLOCK_SIZE as u32 - 64) * 2 / 3;
pub const SIGNATURE: u16 = 0xD2D7;
const MAX_FILES: u32 = 4094;
// Block numbers 0, 1, and 0xFFF are reserved.
const MAX_ALLOC_BLKS: u32 = 4093;
const ABLK_FREE: u16 = 0x000;
const ABLK_LAST: u16 = 0x001;
/// Seconds between 1904-01-01 and 1970-01-01 (Mac vs. Unix epoch).
const MAC_EPOCH_OFFSET: u32 = 2_082_844_800;

/// Default volume label when none is given.
const DEFAULT_VOLUME_NAME: &str = "Untitled";

fn put_be16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_be_bytes());
}
fn put_be32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

// Unicode mapping for Macintosh Roman bytes 0x80..=0xFF.
const MAC_ROMAN_HIGH: [char; 128] = [
    '\u{c4}', '\u{c5}', '\u{c7}', '\u{c9}', '\u{d1}', '\u{d6}', '\u{dc}', '\u{e1}', '\u{e0}',
    '\u{e2}', '\u{e4}', '\u{e3}', '\u{e5}', '\u{e7}', '\u{e9}', '\u{e8}', '\u{ea}', '\u{eb}',
    '\u{ed}', '\u{ec}', '\u{ee}', '\u{ef}', '\u{f1}', '\u{f3}', '\u{f2}', '\u{f4}', '\u{f6}',
    '\u{f5}', '\u{fa}', '\u{f9}', '\u{fb}', '\u{fc}', '\u{2020}', '\u{b0}', '\u{a2}', '\u{a3}',
    '\u{a7}', '\u{2022}', '\u{b6}', '\u{df}', '\u{ae}', '\u{a9}', '\u{2122}', '\u{b4}', '\u{a8}',
    '\u{2260}', '\u{c6}', '\u{d8}', '\u{221e}', '\u{b1}', '\u{2264}', '\u{2265}', '\u{a5}',
    '\u{b5}', '\u{2202}', '\u{2211}', '\u{220f}', '\u{3c0}', '\u{222b}', '\u{aa}', '\u{ba}',
    '\u{3a9}', '\u{e6}', '\u{f8}', '\u{bf}', '\u{a1}', '\u{ac}', '\u{221a}', '\u{192}', '\u{2248}',
    '\u{2206}', '\u{ab}', '\u{bb}', '\u{2026}', '\u{a0}', '\u{c0}', '\u{c3}', '\u{d5}', '\u{152}',
    '\u{153}', '\u{2013}', '\u{2014}', '\u{201c}', '\u{201d}', '\u{2018}', '\u{2019}', '\u{f7}',
    '\u{25ca}', '\u{ff}', '\u{178}', '\u{2044}', '\u{20ac}', '\u{2039}', '\u{203a}', '\u{fb01}',
    '\u{fb02}', '\u{2021}', '\u{b7}', '\u{201a}', '\u{201e}', '\u{2030}', '\u{c2}', '\u{ca}',
    '\u{c1}', '\u{cb}', '\u{c8}', '\u{cd}', '\u{ce}', '\u{cf}', '\u{cc}', '\u{d3}', '\u{d4}',
    '\u{f8ff}', '\u{d2}', '\u{da}', '\u{db}', '\u{d9}', '\u{131}', '\u{2c6}', '\u{2dc}', '\u{af}',
    '\u{2d8}', '\u{2d9}', '\u{2da}', '\u{b8}', '\u{2dd}', '\u{2db}', '\u{2c7}',
];

fn encode_mac_roman(name: &str) -> anyhow::Result<Vec<u8>> {
    name.chars()
        .map(|c| {
            if c.is_ascii() {
                Ok(c as u8)
            } else {
                MAC_ROMAN_HIGH
                    .iter()
                    .position(|&mapped| mapped == c)
                    .map(|i| i as u8 + 128)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "character {c:?} in {name:?} is not representable in MacRoman"
                        )
                    })
            }
        })
        .collect()
}

// Read one extra byte to detect growth beyond the capacity checked at open.
fn read_bounded(reader: impl Read, capacity: u64) -> anyhow::Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.take(capacity + 1).read_to_end(&mut data)?;
    if data.len() as u64 > capacity {
        anyhow::bail!("file exceeds remaining image capacity");
    }
    Ok(data)
}

/// Stats reported after writing.
pub struct Stats {
    pub num_alloc_blks: u32,
    pub file_count: u32,
}

pub struct Mfs {
    image: Vec<u8>,
    image_size: u32,
    num_alloc_blks: u32,
    alloc_blk_size: u32,
    /// First allocation block, expressed in 512-byte sectors.
    alloc_blk_start: u32,
    /// First sector of the directory area.
    dir_start: u32,
    /// Number of directory sectors.
    dir_blocks: u32,
    /// Number of 512-byte sectors the MDB+map occupies (also the size of
    /// the backup MDB at the end of the volume).
    mdb_blocks: u32,
    /// 12-bit allocation map, indexed from 0.
    blk_map: Vec<u16>,
    next_alloc_blk: u32,
    /// Bytes used in the directory area so far.
    dir_used: u32,
    next_file_num: u32,
    file_count: u32,
    file_names: HashSet<Vec<u8>>,
    /// Volume name (1..=27 bytes).
    volume_name: Vec<u8>,
    /// Current time encoded as Mac epoch seconds.
    now_mac: u32,
}

impl Mfs {
    pub fn new(size_kb: u32) -> anyhow::Result<Self> {
        if size_kb == 0 {
            anyhow::bail!("size must be positive");
        }
        let bytes = u64::from(size_kb) * 1024;
        if bytes > u64::from(u32::MAX) {
            anyhow::bail!("image size exceeds 4 GB");
        }
        let image_size = bytes as u32;
        let total_blocks = image_size / BLOCK_SIZE as u32;
        if total_blocks < 20 {
            anyhow::bail!("volume too small");
        }

        let dir_blocks = if size_kb > 2048 {
            48
        } else if size_kb > 400 {
            24
        } else {
            12
        };

        // Keep the MDB in sectors 2 and 3 for classic readers. Grow allocation
        // units, rather than the map, to cover larger volumes.
        let mdb_blocks = MDB_BLOCKS;
        let dir_start = 2 + mdb_blocks;
        let alloc_blk_start = dir_start + dir_blocks;
        if total_blocks < alloc_blk_start + mdb_blocks + 2 {
            anyhow::bail!("volume too small for layout");
        }
        let alloc_area = (total_blocks - alloc_blk_start - mdb_blocks) * BLOCK_SIZE as u32;
        let limit = MAP_CAPACITY.min(MAX_ALLOC_BLKS);
        let alloc_blk_size =
            alloc_area.div_ceil(limit).div_ceil(BLOCK_SIZE as u32) * BLOCK_SIZE as u32;
        let alloc_blk_size = alloc_blk_size.max(MIN_ALLOC_BLK_SIZE);
        let num_alloc_blks = alloc_area / alloc_blk_size;

        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);
        let now_mac = now_unix.wrapping_add(MAC_EPOCH_OFFSET);

        Ok(Self {
            image: vec![0u8; image_size as usize],
            image_size,
            num_alloc_blks,
            alloc_blk_size,
            alloc_blk_start,
            dir_start,
            dir_blocks,
            mdb_blocks,
            blk_map: vec![ABLK_FREE; num_alloc_blks as usize],
            next_alloc_blk: 0,
            dir_used: 0,
            next_file_num: 1,
            file_count: 0,
            file_names: HashSet::new(),
            volume_name: DEFAULT_VOLUME_NAME.as_bytes().to_vec(),
            now_mac,
        })
    }

    pub fn set_volume_name(&mut self, name: &str) -> anyhow::Result<()> {
        if name.is_empty() {
            anyhow::bail!("volume name must not be empty");
        }
        let encoded = encode_mac_roman(name)?;
        if encoded.len() > 27 {
            anyhow::bail!("volume name too long (max 27 bytes)");
        }
        self.volume_name = encoded;
        Ok(())
    }

    pub fn populate_root(&mut self, src: &Path) -> anyhow::Result<()> {
        self.populate(src, "")
    }

    fn populate(&mut self, src_path: &Path, prefix: &str) -> anyhow::Result<()> {
        for entry in fs::read_dir(src_path)? {
            let entry = entry?;
            let path = entry.path();
            let raw_name = entry.file_name();
            let name = raw_name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 filename: {:?}", raw_name))?;

            // ':' is the Mac path separator and would be ambiguous after flattening.
            if name.contains(':') {
                anyhow::bail!("filename contains ':' (Mac path separator): {name}");
            }

            let flat_name = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}:{name}")
            };

            let meta = fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                self.populate(&path, &flat_name)?;
            } else if meta.is_file() {
                let file = File::open(&path)?;
                let remaining = u64::from(self.num_alloc_blks - self.next_alloc_blk)
                    * u64::from(self.alloc_blk_size);
                if file.metadata()?.len() > remaining {
                    anyhow::bail!("file exceeds remaining image capacity: {}", path.display());
                }
                let data = read_bounded(file, remaining)
                    .with_context(|| format!("reading {}", path.display()))?;
                let size = u32::try_from(data.len())
                    .map_err(|_| anyhow::anyhow!("file exceeds 4 GB: {}", path.display()))?;
                let (type_code, creator) = type_for_ext(name);
                self.add_file(&flat_name, &data, size, type_code, creator)?;
            }
            // Symlinks, devices, etc. are skipped.
        }
        Ok(())
    }

    fn add_file(
        &mut self,
        name: &str,
        data: &[u8],
        size: u32,
        type_code: &[u8; 4],
        creator: &[u8; 4],
    ) -> anyhow::Result<()> {
        if self.file_count >= MAX_FILES {
            anyhow::bail!("too many files");
        }

        let encoded = encode_mac_roman(name)?;
        let raw_len = encoded.len();
        let name_len = if raw_len > 255 {
            eprintln!("mkfs-mfs: warning: name truncated to 255 bytes: {name}");
            255usize
        } else {
            raw_len
        };

        let stored_name = encoded[..name_len].to_vec();
        if self.file_names.contains(&stored_name) {
            anyhow::bail!("duplicate filename after truncation: {name}");
        }

        // Entry: 0x32 fixed header + 1 length byte + name, padded to even.
        let entry_size_unpadded = 0x32 + 1 + name_len;
        let entry_size = if entry_size_unpadded & 1 == 1 {
            entry_size_unpadded + 1
        } else {
            entry_size_unpadded
        } as u32;

        let dir_area_size = self.dir_blocks * BLOCK_SIZE as u32;

        // Entries don't cross sector boundaries — skip to next sector if
        // this one doesn't have room.
        let block_offset = self.dir_used % BLOCK_SIZE as u32;
        if block_offset + entry_size > BLOCK_SIZE as u32 {
            self.dir_used = (self.dir_used / BLOCK_SIZE as u32 + 1) * BLOCK_SIZE as u32;
        }

        if self.dir_used + entry_size > dir_area_size {
            anyhow::bail!("directory full");
        }

        // Allocate and copy data fork.
        let mut first_blk: u32 = 0;
        let mut phys_len: u32 = 0;
        if size > 0 {
            let num_blocks = size.div_ceil(self.alloc_blk_size);
            first_blk = self.alloc_blocks(num_blocks)?;
            phys_len = num_blocks * self.alloc_blk_size;

            let mut remaining = size as usize;
            let mut src_off = 0usize;
            for i in 0..num_blocks {
                let off = (self.alloc_blk_start as usize) * BLOCK_SIZE
                    + (first_blk as usize + i as usize) * self.alloc_blk_size as usize;
                let chunk = remaining.min(self.alloc_blk_size as usize);
                self.image[off..off + chunk].copy_from_slice(&data[src_off..src_off + chunk]);
                src_off += chunk;
                remaining -= chunk;
            }
        }

        // Build the directory entry.
        let dir_base = self.dir_start as usize * BLOCK_SIZE;
        let ent_off = dir_base + self.dir_used as usize;
        let ent = &mut self.image[ent_off..ent_off + entry_size as usize];

        ent[0x00] = 0x80; // flFlags: in use
        ent[0x01] = 0x00; // flTyp: version 0
                          // flUsrWds: Finder info
        ent[0x02..0x06].copy_from_slice(type_code); // fdType
        ent[0x06..0x0A].copy_from_slice(creator); // fdCreator
                                                  // fdFlags (0x0A..0x0C), fdLocation (0x0C..0x10), fdFldr (0x10..0x12) = 0
        put_be32(ent, 0x12, self.next_file_num); // flFlNum
        self.next_file_num += 1;

        // Data fork
        if size > 0 {
            put_be16(ent, 0x16, (first_blk + 2) as u16); // flStBlk (+2 for reserved values)
        }
        put_be32(ent, 0x18, size); // flLgLen
        put_be32(ent, 0x1C, phys_len); // flPyLen
                                       // Resource fork left at zero (flRStBlk, flRLgLen, flRPyLen).
        put_be32(ent, 0x2A, self.now_mac); // flCrDat
        put_be32(ent, 0x2E, self.now_mac); // flMdDat

        // Filename
        ent[0x32] = name_len as u8;
        ent[0x33..0x33 + name_len].copy_from_slice(&stored_name);

        self.dir_used += entry_size;
        self.file_count += 1;
        self.file_names.insert(stored_name);
        Ok(())
    }

    fn alloc_blocks(&mut self, count: u32) -> anyhow::Result<u32> {
        if count == 0 {
            anyhow::bail!("alloc_blocks called with count=0");
        }
        if self.next_alloc_blk + count > self.num_alloc_blks {
            anyhow::bail!("no free allocation blocks");
        }
        let first = self.next_alloc_blk;
        // Chain blocks: each points to the next; the last gets ABLK_LAST.
        // Map values are offset by 2 because 0/1 are reserved (FREE/LAST).
        for i in 0..count - 1 {
            self.blk_map[(first + i) as usize] = (first + i + 1 + 2) as u16;
        }
        self.blk_map[(first + count - 1) as usize] = ABLK_LAST;
        self.next_alloc_blk += count;
        Ok(first)
    }

    pub fn write_to(mut self, path: &Path) -> anyhow::Result<Stats> {
        self.write_mdb();

        let mut f = File::create(path)?;
        f.write_all(&self.image)?;
        f.sync_all()?;

        Ok(Stats {
            num_alloc_blks: self.num_alloc_blks,
            file_count: self.file_count,
        })
    }

    fn write_mdb(&mut self) {
        // Volume Information (64 bytes at the start of the MDB).
        let mdb_off = 2 * BLOCK_SIZE;

        let mut free_blks: u32 = 0;
        for &m in &self.blk_map {
            if m == ABLK_FREE {
                free_blks += 1;
            }
        }
        let free_blks = free_blks as u16;

        let mdb = &mut self.image[mdb_off..mdb_off + 64];
        put_be16(mdb, 0x00, SIGNATURE); // drSigWord
        put_be32(mdb, 0x02, self.now_mac); // drCrDate
        put_be32(mdb, 0x06, 0); // drLsBkUp
        put_be16(mdb, 0x0A, 0); // drAtrb
        put_be16(mdb, 0x0C, self.file_count as u16); // drNmFls
        put_be16(mdb, 0x0E, self.dir_start as u16); // drDirSt
        put_be16(mdb, 0x10, self.dir_blocks as u16); // drBlLen
        put_be16(mdb, 0x12, self.num_alloc_blks as u16); // drNmAlBlks
        put_be32(mdb, 0x14, self.alloc_blk_size); // drAlBlkSiz
        put_be32(mdb, 0x18, self.alloc_blk_size); // drClpSiz (default clump)
        put_be16(mdb, 0x1C, self.alloc_blk_start as u16); // drAlBlSt
        put_be32(mdb, 0x1E, self.next_file_num); // drNxtFNum
        put_be16(mdb, 0x22, free_blks); // drFreeBks

        // Volume name as a Pascal string starting at 0x24.
        let name_bytes = &self.volume_name;
        let n = name_bytes.len().min(27);
        mdb[0x24] = n as u8;
        mdb[0x25..0x25 + n].copy_from_slice(&name_bytes[..n]);
        // Trailing bytes within the 28-byte name field stay zero.

        self.pack_block_map();

        // Backup MDB at the end of the volume mirrors the full MDB+map.
        let total_blocks = self.image_size / BLOCK_SIZE as u32;
        let backup_block = total_blocks - self.mdb_blocks;
        let primary_off = 2 * BLOCK_SIZE;
        let backup_off = backup_block as usize * BLOCK_SIZE;
        let span = self.mdb_blocks as usize * BLOCK_SIZE;
        // Copy primary MDB area to the backup region.
        let (front, back) = self.image.split_at_mut(backup_off);
        back[..span].copy_from_slice(&front[primary_off..primary_off + span]);
    }

    fn pack_block_map(&mut self) {
        let map_base = 2 * BLOCK_SIZE + 0x40;
        let mut i = 0u32;
        while i < self.num_alloc_blks {
            let a = self.blk_map[i as usize];
            let b = if i + 1 < self.num_alloc_blks {
                self.blk_map[(i + 1) as usize]
            } else {
                0
            };
            let byte_off = map_base + (i as usize / 2) * 3;
            self.image[byte_off] = ((a >> 4) & 0xFF) as u8;
            self.image[byte_off + 1] = (((a & 0x0F) << 4) | ((b >> 8) & 0x0F)) as u8;
            self.image[byte_off + 2] = (b & 0xFF) as u8;
            i += 2;
        }
    }
}

/// Map a file extension to a Mac (Type, Creator) pair.
fn type_for_ext(name: &str) -> (&'static [u8; 4], &'static [u8; 4]) {
    let dot = match name.rfind('.') {
        Some(d) => d,
        None => return (b"????", b"????"),
    };
    let ext = &name[dot + 1..].to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "text" | "c" | "h" => (b"TEXT", b"ttxt"),
        "bin" => (b"BINA", b"hDmp"),
        _ => (b"????", b"????"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn read_be16(buf: &[u8], off: usize) -> u16 {
        u16::from_be_bytes(buf[off..off + 2].try_into().unwrap())
    }
    fn read_be32(buf: &[u8], off: usize) -> u32 {
        u32::from_be_bytes(buf[off..off + 4].try_into().unwrap())
    }

    #[test]
    fn oversized_inputs_are_bounded_and_rejected() {
        assert_eq!(read_bounded(&b"abc"[..], 3).unwrap(), b"abc");
        assert!(read_bounded(std::io::repeat(0), 3).is_err());
        assert!(read_bounded(&b"x"[..], 0).is_err());
        let src = TempDir::new().unwrap();
        File::create(src.path().join("huge"))
            .unwrap()
            .set_len(32 * 1024 * 1024)
            .unwrap();
        let mut mfs = Mfs::new(400).unwrap();
        let err = mfs.populate_root(src.path()).unwrap_err();
        assert!(err.to_string().contains("remaining image capacity"));
        assert_eq!(mfs.next_alloc_blk, 0);
    }

    #[test]
    fn names_use_mac_roman_and_encoded_length_limits() {
        let mut mfs = Mfs::new(400).unwrap();
        mfs.set_volume_name(&"é".repeat(27)).unwrap();
        assert!(mfs.set_volume_name(&"é".repeat(28)).is_err());
        mfs.add_file("café.txt", b"", 0, b"TEXT", b"ttxt").unwrap();
        mfs.write_mdb();
        assert_eq!(&mfs.image[1061..1088], &[0x8e; 27]);
        let ent = mfs.dir_start as usize * 512;
        assert_eq!(&mfs.image[ent + 50..ent + 59], b"\x08caf\x8e.txt");
        assert!(mfs.set_volume_name("😀").is_err());
        assert!(mfs.add_file("😀", b"", 0, b"????", b"????").is_err());
        let all: String = MAC_ROMAN_HIGH.iter().collect();
        assert_eq!(
            encode_mac_roman(&all).unwrap(),
            (128..=255).collect::<Vec<u8>>()
        );
    }

    #[test]
    fn truncation_collisions_are_rejected_without_allocating() {
        let mut mfs = Mfs::new(400).unwrap();
        let prefix = "a".repeat(255);
        mfs.add_file(&format!("{prefix}A"), b"a", 1, b"TEXT", b"ttxt")
            .unwrap();
        let before = mfs.next_alloc_blk;
        let err = mfs
            .add_file(&format!("{prefix}B"), b"b", 1, b"TEXT", b"ttxt")
            .unwrap_err();
        assert!(err.to_string().contains("duplicate filename"));
        assert_eq!(mfs.file_count, 1);
        assert_eq!(mfs.next_alloc_blk, before);
    }

    #[test]
    fn larger_images_keep_a_two_sector_mdb_and_round_trip_data() {
        for size in [400, 655, 656, 800, 1440, 4200, 8192] {
            let mut mfs = Mfs::new(size).unwrap();
            assert_eq!(mfs.dir_start, 4);
            assert!(mfs.num_alloc_blks <= 640);
            let data: Vec<u8> = (0..mfs.alloc_blk_size as usize * 3 + 1)
                .map(|i| (i % 251) as u8)
                .collect();
            mfs.add_file("payload", &data, data.len() as u32, b"BINA", b"hDmp")
                .unwrap();
            mfs.write_mdb();
            let start = read_be16(&mfs.image, 1024 + 0x1C) as usize * 512;
            assert_eq!(&mfs.image[start..start + data.len()], data);
            assert_eq!(read_be32(&mfs.image, 1024 + 0x14), mfs.alloc_blk_size);
            assert_eq!(&mfs.image[1088..1094], &[0, 0x30, 4, 0, 0x50, 1]);
            assert_eq!(&mfs.image[1024..2048], &mfs.image[mfs.image.len() - 1024..]);
        }
    }

    #[test]
    fn allocation_chain_never_uses_reserved_block_number() {
        let mut mfs = Mfs::new(4200).unwrap();
        let count = mfs.num_alloc_blks;
        mfs.alloc_blocks(count).unwrap();
        assert!(mfs.blk_map.iter().all(|&link| link != 0xFFF));
        assert_eq!(mfs.blk_map.last(), Some(&ABLK_LAST));
        assert!(mfs.alloc_blocks(1).is_err());
    }

    #[test]
    fn empty_400k_image_has_correct_mdb() {
        let dir = TempDir::new().unwrap();
        let img = dir.path().join("test.dsk");
        Mfs::new(400).unwrap().write_to(&img).unwrap();

        let bytes = fs::read(&img).unwrap();
        assert_eq!(bytes.len(), 400 * 1024);
        // MDB at sector 2 = byte 1024.
        assert_eq!(read_be16(&bytes, 1024), SIGNATURE);
        assert_eq!(read_be16(&bytes, 1024 + 0x10), 12); // drBlLen
                                                        // Volume name "Untitled" (8 bytes).
        assert_eq!(bytes[1024 + 0x24], 8);
        assert_eq!(&bytes[1024 + 0x25..1024 + 0x2D], b"Untitled");
    }

    #[test]
    fn populate_writes_file_dirent_and_data() {
        let src = TempDir::new().unwrap();
        let mut f = File::create(src.path().join("hello.txt")).unwrap();
        f.write_all(b"hi there").unwrap();
        drop(f);

        let dst = TempDir::new().unwrap();
        let img = dst.path().join("out.dsk");
        let mut mfs = Mfs::new(400).unwrap();
        mfs.populate_root(src.path()).unwrap();
        let stats = mfs.write_to(&img).unwrap();
        assert_eq!(stats.file_count, 1);

        let bytes = fs::read(&img).unwrap();
        // First dirent at start of directory area. dir_start is in the MDB
        // at offset 0x0E (BE u16).
        let dir_start = read_be16(&bytes, 1024 + 0x0E) as usize;
        let ent_off = dir_start * BLOCK_SIZE;
        assert_eq!(bytes[ent_off], 0x80); // in-use flag
        assert_eq!(&bytes[ent_off + 0x02..ent_off + 0x06], b"TEXT");
        assert_eq!(&bytes[ent_off + 0x06..ent_off + 0x0A], b"ttxt");
        assert_eq!(read_be32(&bytes, ent_off + 0x18), 8); // logical length
        assert_eq!(bytes[ent_off + 0x32], 9); // name length
        assert_eq!(&bytes[ent_off + 0x33..ent_off + 0x33 + 9], b"hello.txt");
    }

    #[test]
    fn colon_in_filename_is_rejected() {
        let src = TempDir::new().unwrap();
        File::create(src.path().join("has:colon")).unwrap();
        let mut mfs = Mfs::new(400).unwrap();
        let err = mfs.populate_root(src.path()).unwrap_err();
        assert!(err.to_string().contains(":"));
    }

    #[test]
    fn subdir_is_flattened_with_colon() {
        let src = TempDir::new().unwrap();
        fs::create_dir(src.path().join("sub")).unwrap();
        File::create(src.path().join("sub").join("a.txt")).unwrap();
        let dst = TempDir::new().unwrap();
        let img = dst.path().join("out.dsk");
        let mut mfs = Mfs::new(400).unwrap();
        mfs.populate_root(src.path()).unwrap();
        mfs.write_to(&img).unwrap();

        let bytes = fs::read(&img).unwrap();
        let dir_start = read_be16(&bytes, 1024 + 0x0E) as usize;
        let ent_off = dir_start * BLOCK_SIZE;
        assert_eq!(bytes[ent_off + 0x32], 9); // "sub:a.txt"
        assert_eq!(&bytes[ent_off + 0x33..ent_off + 0x3C], b"sub:a.txt");
    }
}
