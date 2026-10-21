use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};

use bitflags::bitflags;

use anyhow::{Context, Error, bail};

use super::*;

/// Directories deeper than this are assumed to be corrupt (or a loop).
const MAX_DEPTH: usize = 64;

/// Link value that padding bytes decode to.
const EMPTY_LINK: u16 = 0xffff;

pub struct DirectoryTable {
    pub sector: u32,
    pub size: u32,
    pub entries: Vec<DirectoryEntry>,
}

pub struct DirectoryEntry {
    pub attributes: DirectoryEntryAttributes,
    pub name: String,
    pub name_length: u8,
    pub sector: u32,
    pub size: u32,
    pub subtree_left: u16,
    pub subtree_right: u16,
    pub subdirectory: Option<DirectoryTable>,
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct DirectoryEntryAttributes: u8 {
        const ARCHIVE = 0x20;
        const DIRECTORY = 0x10;
        const HIDDEN = 0x02;
        const NORMAL = 0x80;
        const READ_ONLY = 0x01;
        const SYSTEM = 0x04;
    }
}

impl DirectoryTable {
    pub fn read_root<R: Read + Seek>(
        mut reader: R,
        volume: &VolumeDescriptor,
    ) -> Result<DirectoryTable, Error> {
        Self::read(
            &mut reader,
            volume,
            volume.root_directory_sector,
            volume.root_directory_size,
            0,
        )
    }

    /// Reads the table at `sector` by walking its binary tree.
    ///
    /// Entries are linked through 16-bit left/right offsets (in dwords from
    /// the start of the table), which is the only layout guarantee the format
    /// makes. Reading them one after another instead breaks on tables that
    /// fill their last sector exactly, hold zero-sized files, or are padded
    /// differently by the mastering tool, and can run on into file data.
    fn read<R: Read + Seek>(
        reader: &mut R,
        volume: &VolumeDescriptor,
        sector: u32,
        size: u32,
        depth: usize,
    ) -> Result<DirectoryTable, Error> {
        if depth > MAX_DEPTH {
            bail!("directories are nested too deeply");
        }

        let start = (sector as u64) * volume.sector_size;
        if start + size as u64 > volume.volume_size {
            bail!("directory table at sector {sector} extends past the end of the image");
        }

        let mut buffer = vec![0_u8; size as usize];
        reader.seek(SeekFrom::Start(volume.root_offset + start))?;
        reader.read_exact(&mut buffer)?;

        let mut entries = Vec::new();
        for raw in walk_tree(&buffer)? {
            let subdirectory = if raw.is_directory() && raw.size > 0 {
                Some(Self::read(reader, volume, raw.sector, raw.size, depth + 1)?)
            } else {
                None
            };
            entries.push(raw.into_entry(subdirectory));
        }

        Ok(DirectoryTable {
            sector,
            size,
            entries,
        })
    }

    pub fn get_entry(&self, name: &str) -> Option<&DirectoryEntry> {
        self.entries
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(name))
    }
}

impl DirectoryEntry {
    pub fn is_directory(&self) -> bool {
        self.attributes
            .contains(DirectoryEntryAttributes::DIRECTORY)
    }
}

/// An entry as stored on disk, before its subdirectory is read.
struct RawEntry {
    attributes: DirectoryEntryAttributes,
    name: String,
    name_length: u8,
    sector: u32,
    size: u32,
    subtree_left: u16,
    subtree_right: u16,
}

impl RawEntry {
    /// Size of the fixed part: two subtree offsets, sector, size, attributes
    /// and name length.
    const HEADER_SIZE: usize = 14;

    fn parse(table: &[u8], offset: usize) -> Result<RawEntry, Error> {
        let header = table
            .get(offset..offset + Self::HEADER_SIZE)
            .context("directory entry lies outside its table")?;
        let name_length = header[13];
        let name = table
            .get(offset + Self::HEADER_SIZE..offset + Self::HEADER_SIZE + name_length as usize)
            .context("directory entry name lies outside its table")?;

        Ok(RawEntry {
            subtree_left: u16::from_le_bytes([header[0], header[1]]),
            subtree_right: u16::from_le_bytes([header[2], header[3]]),
            sector: u32::from_le_bytes([header[4], header[5], header[6], header[7]]),
            size: u32::from_le_bytes([header[8], header[9], header[10], header[11]]),
            attributes: DirectoryEntryAttributes::from_bits_truncate(header[12]),
            name_length,
            name: String::from_utf8_lossy(name).into_owned(),
        })
    }

    fn is_directory(&self) -> bool {
        self.attributes
            .contains(DirectoryEntryAttributes::DIRECTORY)
    }

    fn into_entry(self, subdirectory: Option<DirectoryTable>) -> DirectoryEntry {
        DirectoryEntry {
            attributes: self.attributes,
            name: self.name,
            name_length: self.name_length,
            sector: self.sector,
            size: self.size,
            subtree_left: self.subtree_left,
            subtree_right: self.subtree_right,
            subdirectory,
        }
    }
}

/// Collects the entries of one table in sorted (in-order) sequence.
fn walk_tree(table: &[u8]) -> Result<Vec<RawEntry>, Error> {
    let mut entries = Vec::new();
    if table.is_empty() {
        return Ok(entries);
    }

    let mut visited = HashSet::new();
    let mut pending = Vec::new();
    let mut next = Some(0_usize);

    loop {
        while let Some(offset) = next {
            if !visited.insert(offset) {
                bail!("directory tree revisits the entry at offset {offset:#x}");
            }
            let entry = RawEntry::parse(table, offset)?;

            // An all-ones entry is how some tools mark an empty table.
            if entry.subtree_left == EMPTY_LINK && entry.subtree_right == EMPTY_LINK {
                break;
            }

            next = child_offset(entry.subtree_left);
            pending.push(entry);
        }

        let Some(entry) = pending.pop() else {
            return Ok(entries);
        };
        next = child_offset(entry.subtree_right);
        entries.push(entry);
    }
}

/// Subtree links count dwords from the start of the table; zero means "none"
/// because the first entry can never be anyone's child.
fn child_offset(link: u16) -> Option<usize> {
    (link != 0).then(|| link as usize * 4)
}
