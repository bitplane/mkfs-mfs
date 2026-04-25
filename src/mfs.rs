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
//! sectors dir_end..(end - mdb_blocks):  allocation blocks (1 KB each)
//! sectors (end - mdb_blocks)..end:      backup MDB (mirror of primary)
//! ```
//!
//! All on-disk values are big-endian. The 12-bit allocation map packs two
//! entries into three bytes (`AB CD EF` -> `0xABC, 0xDEF`).

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const BLOCK_SIZE: usize = 512;
const ALLOC_BLK_SIZE: usize = 1024;
pub const SIGNATURE: u16 = 0xD2D7;
const MAX_FILES: u32 = 4094;
const MAX_ALLOC_BLKS: u32 = 4094;
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

/// Stats reported after writing.
pub struct Stats {
    pub num_alloc_blks: u32,
    pub file_count: u32,
}

pub struct Mfs {
    image: Vec<u8>,
    image_size: u32,
    num_alloc_blks: u32,
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
    /// Volume name (1..=27 bytes).
    volume_name: String,
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

        // MDB+map sizing is circular: more alloc blocks → bigger map →
        // bigger MDB → fewer alloc blocks. Iterate until it converges.
        let mut mdb_blocks: u32 = 2;
        let mut num_alloc_blks: u32 = 0;
        let mut alloc_blk_start: u32 = 0;
        let mut dir_start: u32 = 0;
        for _ in 0..8 {
            if total_blocks <= 2 + mdb_blocks + dir_blocks + mdb_blocks + 1 {
                anyhow::bail!("volume too small for layout");
            }
            let alloc_start = 2 + mdb_blocks + dir_blocks;
            let alloc_area = (total_blocks - alloc_start - mdb_blocks) * BLOCK_SIZE as u32;
            let mut nab = alloc_area / ALLOC_BLK_SIZE as u32;
            if nab > MAX_ALLOC_BLKS {
                nab = MAX_ALLOC_BLKS;
            }
            let map_bytes = (nab * 12).div_ceil(8);
            let needed = (0x40 + map_bytes).div_ceil(BLOCK_SIZE as u32);
            if needed <= mdb_blocks {
                num_alloc_blks = nab;
                alloc_blk_start = alloc_start;
                dir_start = 2 + mdb_blocks;
                break;
            }
            mdb_blocks = needed;
        }
        if alloc_blk_start == 0 {
            anyhow::bail!("MDB sizing did not converge");
        }

        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);
        let now_mac = now_unix.wrapping_add(MAC_EPOCH_OFFSET);

        Ok(Self {
            image: vec![0u8; image_size as usize],
            image_size,
            num_alloc_blks,
            alloc_blk_start,
            dir_start,
            dir_blocks,
            mdb_blocks,
            blk_map: vec![ABLK_FREE; num_alloc_blks as usize],
            next_alloc_blk: 0,
            dir_used: 0,
            next_file_num: 1,
            file_count: 0,
            volume_name: DEFAULT_VOLUME_NAME.to_string(),
            now_mac,
        })
    }

    pub fn set_volume_name(&mut self, name: &str) -> anyhow::Result<()> {
        if name.is_empty() {
            anyhow::bail!("volume name must not be empty");
        }
        if name.len() > 27 {
            anyhow::bail!("volume name too long (max 27 bytes)");
        }
        self.volume_name = name.to_string();
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
                let mut data = Vec::new();
                File::open(&path)?.read_to_end(&mut data)?;
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

        let raw_len = name.len();
        let name_len = if raw_len > 255 {
            eprintln!("mkfs-mfs: warning: name truncated to 255 bytes: {name}");
            255usize
        } else {
            raw_len
        };

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
            let num_blocks = size.div_ceil(ALLOC_BLK_SIZE as u32);
            first_blk = self.alloc_blocks(num_blocks)?;
            phys_len = num_blocks * ALLOC_BLK_SIZE as u32;

            let mut remaining = size as usize;
            let mut src_off = 0usize;
            for i in 0..num_blocks {
                let off = (self.alloc_blk_start as usize) * BLOCK_SIZE
                    + (first_blk as usize + i as usize) * ALLOC_BLK_SIZE;
                let chunk = remaining.min(ALLOC_BLK_SIZE);
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
        ent[0x33..0x33 + name_len].copy_from_slice(&name.as_bytes()[..name_len]);

        self.dir_used += entry_size;
        self.file_count += 1;
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
        put_be32(mdb, 0x14, ALLOC_BLK_SIZE as u32); // drAlBlkSiz
        put_be32(mdb, 0x18, ALLOC_BLK_SIZE as u32); // drClpSiz (default clump)
        put_be16(mdb, 0x1C, self.alloc_blk_start as u16); // drAlBlSt
        put_be32(mdb, 0x1E, self.next_file_num); // drNxtFNum
        put_be16(mdb, 0x22, free_blks); // drFreeBks

        // Volume name as a Pascal string starting at 0x24.
        let name_bytes = self.volume_name.as_bytes();
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
