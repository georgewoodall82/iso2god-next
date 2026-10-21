use std::io::{Read, Seek, SeekFrom};

use anyhow::Error;

pub mod directory_table;
pub mod iso_type;
pub mod volume_descriptor;

pub use directory_table::*;
pub use volume_descriptor::*;

pub const SECTOR_SIZE: u64 = 0x800;

pub struct IsoReader<R: Read + Seek> {
    pub volume_descriptor: VolumeDescriptor,
    pub directory_table: DirectoryTable,
    reader: R,
}

impl<R: Read + Seek> IsoReader<R> {
    pub fn read(mut reader: R) -> Result<IsoReader<R>, Error> {
        let volume_descriptor = VolumeDescriptor::read(&mut reader)?;
        let directory_table = DirectoryTable::read_root(&mut reader, &volume_descriptor)?;

        Ok(IsoReader {
            volume_descriptor,
            directory_table,
            reader,
        })
    }

    pub fn get_root(&mut self) -> Result<&mut R, Error> {
        self.reader
            .seek(SeekFrom::Start(self.volume_descriptor.root_offset))?;
        Ok(&mut self.reader)
    }

    pub fn get_entry(&mut self, path: &WindowsPath) -> Result<Option<&mut R>, Error> {
        let mut entry: Option<&DirectoryEntry> = None;
        let mut dir = Some(&self.directory_table);

        for name in path.components.iter() {
            entry = dir.and_then(|dir| dir.get_entry(name));
            dir = entry.and_then(|entry| entry.subdirectory.as_ref());
        }

        if let Some(entry) = entry {
            let position = self.volume_descriptor.root_offset
                + (entry.sector as u64) * self.volume_descriptor.sector_size;

            self.reader.seek(SeekFrom::Start(position))?;

            Ok(Some(&mut self.reader))
        } else {
            Ok(None)
        }
    }

    pub fn get_max_used_prefix_size(&self) -> u64 {
        // The data volume must contain the volume descriptor and the root
        // directory table, and neither appears as a directory entry - so
        // account for their extents explicitly. Subdirectory tables need no
        // special handling: their extents are their parent directories'
        // entries.
        let volume_descriptor_end = 0x21 * SECTOR_SIZE;
        let root_table_end = (self.volume_descriptor.root_directory_sector as u64) * SECTOR_SIZE
            + (self.volume_descriptor.root_directory_size as u64);

        return volume_descriptor_end
            .max(root_table_end)
            .max(rec(&self.directory_table));
        fn rec(dir: &DirectoryTable) -> u64 {
            dir.entries
                .iter()
                .filter(|entry| entry.size > 0)
                .map(|entry| {
                    let mut v = (entry.sector as u64) * SECTOR_SIZE + (entry.size as u64);
                    if let Some(subdir) = &entry.subdirectory {
                        v = v.max(rec(subdir));
                    }
                    v
                })
                .max()
                .unwrap_or(0)
        }
    }
}

#[derive(Clone, Debug)]
pub struct WindowsPath {
    pub components: Vec<String>,
}

/// Case-insensitive (ascii case, for simplicity). Uses `\` as separator.
impl<'a, S: Into<&'a str>> From<S> for WindowsPath {
    fn from(path: S) -> WindowsPath {
        let path: &'a str = path.into();

        WindowsPath {
            components: path
                .split('\\')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Minimal base-0 image: volume descriptor at sector 0x20, one file
    /// entry in the root table. Table and file placement are parameters
    /// so tests can lay them out in either order.
    fn build_image(root_table_sector: u32, file_sector: u32, file_size: u32) -> Vec<u8> {
        let end_sector = root_table_sector.max(file_sector) as usize + 2;
        let mut img = vec![0_u8; end_sector * SECTOR_SIZE as usize];

        let desc = (0x20 * SECTOR_SIZE) as usize;
        img[desc..desc + 20].copy_from_slice(b"MICROSOFT*XBOX*MEDIA");
        img[desc + 20..desc + 24].copy_from_slice(&root_table_sector.to_le_bytes());
        img[desc + 24..desc + 28].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());

        let table = (root_table_sector as u64 * SECTOR_SIZE) as usize;
        img[table..table + 2].copy_from_slice(&0_u16.to_le_bytes());
        img[table + 2..table + 4].copy_from_slice(&0_u16.to_le_bytes());
        img[table + 4..table + 8].copy_from_slice(&file_sector.to_le_bytes());
        img[table + 8..table + 12].copy_from_slice(&file_size.to_le_bytes());
        img[table + 12] = 0x20; // ARCHIVE
        img[table + 13] = 1;
        img[table + 14] = b'a';
        for b in &mut img[table + 15..table + SECTOR_SIZE as usize] {
            *b = 0xff;
        }

        img
    }

    /// Writes one directory entry at `at` and returns the offset just past it.
    fn put_entry(
        img: &mut [u8],
        at: usize,
        (left, right): (u16, u16),
        (sector, size): (u32, u32),
        name: &str,
    ) -> usize {
        img[at..at + 2].copy_from_slice(&left.to_le_bytes());
        img[at + 2..at + 4].copy_from_slice(&right.to_le_bytes());
        img[at + 4..at + 8].copy_from_slice(&sector.to_le_bytes());
        img[at + 8..at + 12].copy_from_slice(&size.to_le_bytes());
        img[at + 12] = 0x20; // ARCHIVE
        img[at + 13] = name.len() as u8;
        img[at + 14..at + 14 + name.len()].copy_from_slice(name.as_bytes());
        (at + 14 + name.len()).next_multiple_of(4)
    }

    fn empty_image(sectors: usize, root_table_sector: u32, root_table_size: u32) -> Vec<u8> {
        let mut img = vec![0_u8; sectors * SECTOR_SIZE as usize];
        let desc = (0x20 * SECTOR_SIZE) as usize;
        img[desc..desc + 20].copy_from_slice(b"MICROSOFT*XBOX*MEDIA");
        img[desc + 20..desc + 24].copy_from_slice(&root_table_sector.to_le_bytes());
        img[desc + 24..desc + 28].copy_from_slice(&root_table_size.to_le_bytes());
        img
    }

    #[test]
    fn full_directory_table_does_not_read_into_the_data_behind_it() {
        // 128 sixteen-byte entries fill the single table sector exactly, so
        // there is no padding to stop a sequential reader, and the file data
        // that follows would be parsed as more entries.
        let mut img = empty_image(40, 33, SECTOR_SIZE as u32);
        let table = (33 * SECTOR_SIZE) as usize;
        for i in 0..128_usize {
            let name = format!(
                "{}{}",
                (b'a' + (i / 16) as u8) as char,
                (b'a' + (i % 16) as u8) as char
            );
            let next = if i == 127 {
                0
            } else {
                ((i + 1) * 16 / 4) as u16
            };
            put_entry(&mut img, table + i * 16, (0, next), (36, 100), &name);
        }
        for b in &mut img[34 * SECTOR_SIZE as usize..38 * SECTOR_SIZE as usize] {
            *b = 0x5a;
        }

        let iso = IsoReader::read(Cursor::new(img)).unwrap();
        assert_eq!(iso.directory_table.entries.len(), 128);
        assert!(iso.directory_table.get_entry("hp").is_some());
    }

    #[test]
    fn empty_files_do_not_hide_the_entries_after_them() {
        let mut img = empty_image(40, 33, SECTOR_SIZE as u32);
        let table = (33 * SECTOR_SIZE) as usize;
        // a chain: empty.txt -> default.xex
        let next = put_entry(&mut img, table, (0, 6), (0, 0), "empty.txt");
        assert_eq!(next, table + 24);
        put_entry(&mut img, table + 24, (0, 0), (36, 100), "default.xex");

        let iso = IsoReader::read(Cursor::new(img)).unwrap();
        assert!(iso.directory_table.get_entry("empty.txt").is_some());
        assert!(iso.directory_table.get_entry("default.xex").is_some());
        // the empty file occupies no space, wherever it claims to live
        assert_eq!(iso.get_max_used_prefix_size(), 36 * SECTOR_SIZE + 100);
    }

    #[test]
    fn entries_may_be_laid_out_in_any_order() {
        // the tree, not the byte order, defines the directory: here the root
        // of the tree ("m") comes first and its children follow.
        let mut img = empty_image(40, 33, SECTOR_SIZE as u32);
        let table = (33 * SECTOR_SIZE) as usize;
        put_entry(&mut img, table, (6, 12), (36, 10), "m");
        put_entry(&mut img, table + 24, (0, 0), (36, 10), "z");
        put_entry(&mut img, table + 48, (0, 0), (36, 10), "a");
        // "m" at 0, "z" at 24 -> offset 6; "a" at 48 -> offset 12
        let iso = IsoReader::read(Cursor::new(img)).unwrap();
        let names: Vec<_> = iso
            .directory_table
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, ["z", "m", "a"]);
    }

    #[test]
    fn corrupt_directory_trees_are_rejected() {
        let table = (33 * SECTOR_SIZE) as usize;

        // an entry that is its own right child
        let mut img = empty_image(40, 33, SECTOR_SIZE as u32);
        put_entry(&mut img, table, (0, 6), (36, 10), "a");
        put_entry(&mut img, table + 24, (0, 6), (36, 10), "b");
        assert!(IsoReader::read(Cursor::new(img)).is_err());

        // a child that lies beyond the end of the table
        let mut img = empty_image(40, 33, SECTOR_SIZE as u32);
        put_entry(&mut img, table, (0, 0x7fff), (36, 10), "a");
        assert!(IsoReader::read(Cursor::new(img)).is_err());
    }

    #[test]
    fn trim_includes_root_directory_table_placed_after_file_data() {
        // root table at sector 40, file data ending inside sector 33:
        // the trimmed volume must still reach the end of the root table.
        let iso = IsoReader::read(Cursor::new(build_image(40, 33, 100))).unwrap();
        assert!(iso.get_max_used_prefix_size() >= 41 * SECTOR_SIZE);
    }

    #[test]
    fn trim_is_governed_by_file_extents_when_tables_come_first() {
        // the common retail layout: table before the data. The last file
        // extent decides the size, exactly as before.
        let iso = IsoReader::read(Cursor::new(build_image(33, 40, 100))).unwrap();
        assert_eq!(iso.get_max_used_prefix_size(), 40 * SECTOR_SIZE + 100);
    }
}
