//! Minimal read-only Compound File Binary (CFB / OLE2) reader.
//!
//! The container behind the legacy binary Office formats (`.doc`, `.xls`,
//! `.ppt`, issue #127): a FAT filesystem-in-a-file holding named streams.
//! This reader does exactly what the `doc`/`ppt` backends need — open the
//! container and extract a named stream's bytes — with the hostile-input
//! guards the rest of the crate applies to archive formats: chain walks are
//! bounded by the sector count (no cycle can loop forever) and stream sizes
//! are capped by the same per-part budget as OOXML parts.
//!
//! Layout ([MS-CFB]): a 512-byte header names the first sectors of the DIFAT
//! (which locates the FAT), the directory chain, and the mini FAT. Streams
//! ≥ `mini_stream_cutoff` (4096) chain through the FAT; smaller ones live in
//! the *mini stream* (the root entry's stream) and chain through the mini FAT
//! in 64-byte mini sectors.

use crate::backend::ooxml;

const HEADER_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
const FREESECT: u32 = 0xFFFF_FFFF;
/// Sector numbers ≥ this are special markers (DIFSECT/FATSECT/…), never data.
const MAXREGSECT: u32 = 0xFFFF_FFFA;

/// One directory entry we care about: a named stream (or storage).
struct DirEntry {
    name: String,
    /// 1 = storage, 2 = stream, 5 = root.
    object_type: u8,
    start_sector: u32,
    size: u64,
}

/// An opened compound file: parsed FAT/directory, ready to extract streams.
pub(crate) struct CompoundFile<'a> {
    data: &'a [u8],
    sector_size: usize,
    fat: Vec<u32>,
    mini_fat: Vec<u32>,
    /// The root entry's stream, read eagerly: it *is* the mini stream.
    mini_stream: Vec<u8>,
    entries: Vec<DirEntry>,
}

fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}

impl<'a> CompoundFile<'a> {
    /// `true` when `data` starts with the CFB signature.
    pub(crate) fn detect(data: &[u8]) -> bool {
        data.get(..8) == Some(&HEADER_MAGIC)
    }

    pub(crate) fn open(data: &'a [u8]) -> Option<Self> {
        if !Self::detect(data) {
            return None;
        }
        let sector_shift = u16_at(data, 30)?; // 9 (512) for v3, 12 (4096) for v4
        if !(7..=16).contains(&sector_shift) {
            return None;
        }
        let sector_size = 1usize << sector_shift;
        let sector_count = data.len() / sector_size; // bound for every chain walk

        // DIFAT: 109 entries in the header, then a chain of DIFAT sectors.
        let mut fat_sectors: Vec<u32> = Vec::new();
        for i in 0..109 {
            let s = u32_at(data, 76 + i * 4)?;
            if s < MAXREGSECT {
                fat_sectors.push(s);
            }
        }
        let mut difat_sector = u32_at(data, 68)?;
        let mut difat_walked = 0usize;
        while difat_sector < MAXREGSECT && difat_walked <= sector_count {
            difat_walked += 1;
            let base = sector_offset(difat_sector, sector_size);
            let per = sector_size / 4 - 1;
            for i in 0..per {
                let s = u32_at(data, base + i * 4)?;
                if s < MAXREGSECT {
                    fat_sectors.push(s);
                }
            }
            difat_sector = u32_at(data, base + per * 4)?;
        }

        // FAT: the concatenated entries of every FAT sector.
        let mut fat: Vec<u32> = Vec::with_capacity(fat_sectors.len() * (sector_size / 4));
        for s in fat_sectors {
            let base = sector_offset(s, sector_size);
            for i in 0..sector_size / 4 {
                fat.push(u32_at(data, base + i * 4).unwrap_or(FREESECT));
            }
        }

        // Directory: walk its FAT chain, parse 128-byte entries.
        let dir_start = u32_at(data, 48)?;
        let dir_bytes = read_chain(data, &fat, dir_start, sector_size, u64::MAX)?;
        let mut entries = Vec::new();
        for chunk in dir_bytes.chunks_exact(128) {
            let name_len = u16_at(chunk, 64)? as usize; // bytes incl. terminator
            if !(2..=64).contains(&name_len) {
                continue;
            }
            let name: String = chunk[..name_len - 2]
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .map(|u| char::from_u32(u as u32).unwrap_or('\u{FFFD}'))
                .collect();
            entries.push(DirEntry {
                name,
                object_type: chunk[66],
                start_sector: u32_at(chunk, 116)?,
                size: u32_at(chunk, 120)? as u64 | ((u32_at(chunk, 124)? as u64) << 32),
            });
        }

        // Mini FAT + mini stream (the root entry's chain).
        let mini_fat_start = u32_at(data, 60)?;
        let mini_fat_bytes =
            read_chain(data, &fat, mini_fat_start, sector_size, u64::MAX).unwrap_or_default();
        let mini_fat: Vec<u32> = mini_fat_bytes
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let mini_stream = entries
            .iter()
            .find(|e| e.object_type == 5)
            .and_then(|root| read_chain(data, &fat, root.start_sector, sector_size, root.size))
            .unwrap_or_default();

        Some(Self {
            data,
            sector_size,
            fat,
            mini_fat,
            mini_stream,
            entries,
        })
    }

    /// Extract a stream's bytes by name (exact match, any storage level —
    /// the Office streams we need live in the root storage and carry unique
    /// names). `None` for a missing stream or one over the per-part budget.
    pub(crate) fn stream(&self, name: &str) -> Option<Vec<u8>> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.object_type == 2 && e.name == name)?;
        if entry.size > ooxml::max_part_bytes() {
            return None;
        }
        if entry.size < 4096 {
            // Mini stream: 64-byte sectors chained through the mini FAT.
            read_mini_chain(
                &self.mini_stream,
                &self.mini_fat,
                entry.start_sector,
                entry.size,
            )
        } else {
            read_chain(
                self.data,
                &self.fat,
                entry.start_sector,
                self.sector_size,
                entry.size,
            )
        }
    }

    /// Names of all stream entries.
    #[cfg(test)]
    pub(crate) fn stream_names(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(|e| e.object_type == 2)
            .map(|e| e.name.as_str())
    }
}

/// Byte offset of sector `n` (sector 0 starts right after the 512-byte header).
fn sector_offset(n: u32, sector_size: usize) -> usize {
    512 + n as usize * sector_size
}

/// Follow a FAT chain from `start`, concatenating sectors, truncated to `size`.
/// Bounded by the FAT length — a cyclic chain terminates instead of spinning.
fn read_chain(
    data: &[u8],
    fat: &[u32],
    start: u32,
    sector_size: usize,
    size: u64,
) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut sector = start;
    let mut walked = 0usize;
    while sector < MAXREGSECT {
        walked += 1;
        if walked > fat.len() + 1 {
            return None; // cycle
        }
        let base = sector_offset(sector, sector_size);
        out.extend_from_slice(data.get(base..base + sector_size)?);
        if out.len() as u64 >= size {
            break;
        }
        sector = *fat.get(sector as usize)?;
    }
    if sector == ENDOFCHAIN || out.len() as u64 >= size {
        out.truncate(out.len().min(size.try_into().unwrap_or(usize::MAX)));
        Some(out)
    } else {
        None
    }
}

/// Follow a mini-FAT chain through the mini stream (64-byte sectors).
fn read_mini_chain(mini_stream: &[u8], mini_fat: &[u32], start: u32, size: u64) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut sector = start;
    let mut walked = 0usize;
    while sector < MAXREGSECT {
        walked += 1;
        if walked > mini_fat.len() + 1 {
            return None; // cycle
        }
        let base = sector as usize * 64;
        out.extend_from_slice(mini_stream.get(base..(base + 64).min(mini_stream.len()))?);
        if out.len() as u64 >= size {
            break;
        }
        sector = *mini_fat.get(sector as usize)?;
    }
    out.truncate(out.len().min(size.try_into().unwrap_or(usize::MAX)));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_cfb_data() {
        assert!(!CompoundFile::detect(b"PK\x03\x04"));
        assert!(CompoundFile::open(b"not a compound file").is_none());
        assert!(CompoundFile::open(&[]).is_none());
    }

    #[test]
    fn opens_real_word_file_and_reads_streams() {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/doc/sources/docx_lists.doc"
        ))
        .unwrap();
        let cfb = CompoundFile::open(&data).expect("valid CFB");
        let names: Vec<&str> = cfb.stream_names().collect();
        assert!(names.contains(&"WordDocument"), "streams: {names:?}");
        let word = cfb.stream("WordDocument").expect("WordDocument stream");
        assert_eq!(&word[..2], &[0xEC, 0xA5], "FIB wIdent magic");
        assert!(cfb.stream("NoSuchStream").is_none());
    }

    #[test]
    fn truncated_header_is_rejected_not_panicked() {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/doc/sources/docx_lists.doc"
        ))
        .unwrap();
        // Every truncation point must fail cleanly, never panic.
        for cut in [8, 76, 512, 700] {
            let _ = CompoundFile::open(&data[..cut.min(data.len())]);
        }
    }
}
