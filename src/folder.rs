//! Presents a folder of game files as an Xbox (XDVDFS/GDF) volume.
//!
//! Games on Demand packages hold a copy of the disc's data volume, so a folder
//! that contains a `default.xex` can be converted directly: [`FolderImage`]
//! lays the folder out the way a mastering tool would (volume descriptor,
//! directory tables, then the files) and [`FolderReader`] serves that volume
//! on the fly. Nothing is written to disk and the files are read once per pass
//! like an ISO would be, so no temporary multi-gigabyte image is needed.

use std::cmp::min;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Error, bail};

use crate::god::BLOCK_SIZE;
use crate::iso::SECTOR_SIZE;

const SECTOR: usize = SECTOR_SIZE as usize;

/// The volume descriptor lives in this sector; the root table follows it.
const DESCRIPTOR_SECTOR: u32 = 0x20;
const IDENTIFIER: &[u8; 20] = b"MICROSOFT*XBOX*MEDIA";
/// The identifier is repeated at the end of the descriptor sector.
const TRAILING_IDENTIFIER_OFFSET: usize = 0x7ec;

const ATTRIBUTE_DIRECTORY: u8 = 0x10;
const ATTRIBUTE_ARCHIVE: u8 = 0x20;

/// Fixed part of a directory entry: two links, sector, size, attributes and
/// name length.
const ENTRY_HEADER_SIZE: usize = 14;
const MAX_NAME_LENGTH: usize = 255;
/// Links count dwords from the start of the table and are 16 bits wide.
const MAX_TABLE_SIZE: usize = (u16::MAX as usize + 1) * 4;
/// Guards against symbolic link loops.
const MAX_DEPTH: usize = 64;

/// Files that file managers litter folders with; they don't belong on a disc.
fn is_junk(name: &str) -> bool {
    name.eq_ignore_ascii_case(".DS_Store")
        || name.eq_ignore_ascii_case("Thumbs.db")
        || name.eq_ignore_ascii_case("desktop.ini")
        || name.starts_with("._")
}

/// A folder laid out as an XDVDFS volume starting at offset 0.
#[derive(Clone, Debug)]
pub struct FolderImage {
    layout: Arc<Layout>,
}

#[derive(Debug)]
struct Layout {
    /// Everything before the first file: the zeroed system area, the volume
    /// descriptor and all directory tables.
    metadata: Vec<u8>,
    /// Non-empty files in ascending, non-overlapping order.
    files: Vec<FileExtent>,
    /// Files in the top-level folder, for finding the executable.
    root_files: Vec<(String, PathBuf)>,
    /// Size of the volume, a whole number of GOD blocks.
    size: u64,
}

#[derive(Debug)]
struct FileExtent {
    start: u64,
    len: u64,
    path: PathBuf,
}

impl FileExtent {
    fn end(&self) -> u64 {
        self.start + self.len
    }
}

impl FolderImage {
    /// Reads the folder's structure (but not the contents of its files) and
    /// plans the volume.
    pub fn scan(root: &Path) -> Result<FolderImage, Error> {
        let mut root_dir = Dir::new(scan_dir(root, 0)?);

        let root_files = root_dir
            .entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                Kind::File { path, .. } => Some((entry.name.clone(), path.clone())),
                Kind::Dir(_) => None,
            })
            .collect();

        // The root table directly follows the volume descriptor.
        let mut next_sector = DESCRIPTOR_SECTOR + 1;
        root_dir.place_tables(&mut next_sector)?;

        // The files follow the tables.
        let mut files = Vec::new();
        let mut next_file_sector = u64::from(next_sector);
        root_dir.place_files(&mut next_file_sector, &mut files)?;

        let mut metadata = vec![0_u8; next_sector as usize * SECTOR];
        write_descriptor(&mut metadata, &root_dir);
        root_dir.write_tables(&mut metadata);

        // Files start on sector boundaries; pad the volume to whole GOD blocks.
        let size = (next_file_sector * SECTOR_SIZE).next_multiple_of(BLOCK_SIZE);
        if size / SECTOR_SIZE > u64::from(u32::MAX) {
            bail!("this folder is too large for an Xbox volume");
        }

        Ok(FolderImage {
            layout: Arc::new(Layout {
                metadata,
                files,
                root_files,
                size,
            }),
        })
    }

    /// Size of the volume in bytes (what a GOD package would hold).
    pub fn volume_size(&self) -> u64 {
        self.layout.size
    }

    /// Looks a file up in the top-level folder, ignoring case.
    pub fn root_file(&self, name: &str) -> Option<&Path> {
        self.layout
            .root_files
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, path)| path.as_path())
    }

    /// Opens an independent reader positioned at the start of the volume.
    pub fn open(&self) -> FolderReader {
        FolderReader {
            layout: Arc::clone(&self.layout),
            position: 0,
            open_file: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

struct Dir {
    entries: Vec<Entry>,
    /// Sector and size of the table; both zero for an empty directory.
    sector: u32,
    size: u32,
}

struct Entry {
    name: String,
    kind: Kind,
    /// Where the entry sits in its table and how it links to its children.
    slot: Slot,
}

enum Kind {
    File {
        path: PathBuf,
        size: u32,
        sector: u32,
    },
    Dir(Dir),
}

#[derive(Clone, Copy, Default)]
struct Slot {
    /// Byte offset in the table.
    offset: usize,
    left: u16,
    right: u16,
}

impl Dir {
    fn new(mut entries: Vec<Entry>) -> Dir {
        entries.sort_by_cached_key(|entry| sort_key(&entry.name));
        Dir {
            entries,
            sector: 0,
            size: 0,
        }
    }

    /// Gives this directory and everything below it a table, in pre-order.
    fn place_tables(&mut self, next_sector: &mut u32) -> Result<(), Error> {
        if self.entries.is_empty() {
            return Ok(());
        }

        let size = self.plan_table()?;
        self.sector = *next_sector;
        self.size = size as u32;
        *next_sector = next_sector
            .checked_add((size / SECTOR) as u32)
            .context("this folder has too many directories")?;

        for entry in &mut self.entries {
            if let Kind::Dir(dir) = &mut entry.kind {
                dir.place_tables(next_sector)?;
            }
        }
        Ok(())
    }

    /// Arranges the entries as a balanced binary search tree and returns the
    /// size of the table in bytes.
    ///
    /// The console starts every lookup at offset 0, so the root of the tree
    /// is written first and its descendants after it. Entries never straddle
    /// a sector boundary.
    fn plan_table(&mut self) -> Result<usize, Error> {
        fn place(
            entries: &mut [Entry],
            range: std::ops::Range<usize>,
            cursor: &mut usize,
        ) -> Result<u16, Error> {
            if range.is_empty() {
                return Ok(0);
            }

            let middle = range.start + range.len() / 2;
            let length = (ENTRY_HEADER_SIZE + entries[middle].name.len()).next_multiple_of(4);
            if *cursor % SECTOR + length > SECTOR {
                *cursor = cursor.next_multiple_of(SECTOR);
            }

            let offset = *cursor;
            *cursor += length;
            if *cursor > MAX_TABLE_SIZE {
                bail!("a directory holds too many entries for an Xbox volume");
            }

            let left = place(entries, range.start..middle, cursor)?;
            let right = place(entries, middle + 1..range.end, cursor)?;
            entries[middle].slot = Slot {
                offset,
                left,
                right,
            };
            Ok((offset / 4) as u16)
        }

        let mut cursor = 0;
        let count = self.entries.len();
        place(&mut self.entries, 0..count, &mut cursor)?;
        Ok(cursor.next_multiple_of(SECTOR))
    }

    /// Gives every file its sectors, directory by directory.
    fn place_files(
        &mut self,
        next_sector: &mut u64,
        extents: &mut Vec<FileExtent>,
    ) -> Result<(), Error> {
        for entry in &mut self.entries {
            match &mut entry.kind {
                Kind::File { path, size, sector } => {
                    if *size == 0 {
                        continue;
                    }
                    *sector = u32::try_from(*next_sector).context("this folder is too large")?;
                    extents.push(FileExtent {
                        start: *next_sector * SECTOR_SIZE,
                        len: u64::from(*size),
                        path: path.clone(),
                    });
                    *next_sector += u64::from(*size).div_ceil(SECTOR_SIZE);
                }
                Kind::Dir(dir) => dir.place_files(next_sector, extents)?,
            }
        }
        Ok(())
    }

    /// Serialises this table and those below it into `image`.
    fn write_tables(&self, image: &mut [u8]) {
        if self.entries.is_empty() {
            return;
        }

        let start = self.sector as usize * SECTOR;
        let table = &mut image[start..start + self.size as usize];
        table.fill(0xff);

        for entry in &self.entries {
            let (sector, size, attributes) = match &entry.kind {
                Kind::File { size, sector, .. } => (*sector, *size, ATTRIBUTE_ARCHIVE),
                Kind::Dir(dir) => (dir.sector, dir.size, ATTRIBUTE_DIRECTORY),
            };

            let at = entry.slot.offset;
            table[at..at + 2].copy_from_slice(&entry.slot.left.to_le_bytes());
            table[at + 2..at + 4].copy_from_slice(&entry.slot.right.to_le_bytes());
            table[at + 4..at + 8].copy_from_slice(&sector.to_le_bytes());
            table[at + 8..at + 12].copy_from_slice(&size.to_le_bytes());
            table[at + 12] = attributes;
            table[at + 13] = entry.name.len() as u8;
            table[at + 14..at + 14 + entry.name.len()].copy_from_slice(entry.name.as_bytes());
        }

        for entry in &self.entries {
            if let Kind::Dir(dir) = &entry.kind {
                dir.write_tables(image);
            }
        }
    }
}

/// The console compares names case-insensitively by upper-casing them, so the
/// trees have to be ordered the same way.
fn sort_key(name: &str) -> String {
    name.to_ascii_uppercase()
}

fn write_descriptor(metadata: &mut [u8], root: &Dir) {
    let start = DESCRIPTOR_SECTOR as usize * SECTOR;
    let descriptor = &mut metadata[start..start + SECTOR];

    descriptor[..20].copy_from_slice(IDENTIFIER);
    descriptor[20..24].copy_from_slice(&root.sector.to_le_bytes());
    descriptor[24..28].copy_from_slice(&root.size.to_le_bytes());
    // The creation time is left at zero so that converting the same folder
    // twice gives byte-identical packages.
    descriptor[TRAILING_IDENTIFIER_OFFSET..TRAILING_IDENTIFIER_OFFSET + 20]
        .copy_from_slice(IDENTIFIER);
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

fn scan_dir(path: &Path, depth: usize) -> Result<Vec<Entry>, Error> {
    if depth > MAX_DEPTH {
        bail!(
            "{} is nested too deeply (is there a symbolic link loop?)",
            path.display()
        );
    }

    let mut entries: Vec<Entry> = Vec::new();
    let listing = fs::read_dir(path).with_context(|| format!("cannot read {}", path.display()))?;

    for item in listing {
        let item = item.with_context(|| format!("cannot read {}", path.display()))?;
        let item_path = item.path();

        let name = item
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("{} is not valid UTF-8", item_path.display()))?;
        if is_junk(&name) {
            continue;
        }

        // Follows symbolic links.
        let metadata = fs::metadata(&item_path)
            .with_context(|| format!("cannot read {}", item_path.display()))?;
        let kind = if metadata.is_dir() {
            Kind::Dir(Dir::new(scan_dir(&item_path, depth + 1)?))
        } else if metadata.is_file() {
            let size = u32::try_from(metadata.len()).map_err(|_| {
                anyhow::anyhow!(
                    "{} is 4 GiB or larger, which an Xbox volume cannot hold",
                    item_path.display()
                )
            })?;
            Kind::File {
                path: item_path.clone(),
                size,
                sector: 0,
            }
        } else {
            continue;
        };

        check_name(&name, &item_path)?;
        if let Some(existing) = entries
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(&name))
        {
            bail!(
                "{} and {} only differ in letter case, which an Xbox volume cannot tell apart",
                item_path.display(),
                path.join(&existing.name).display()
            );
        }

        entries.push(Entry {
            name,
            kind,
            slot: Slot::default(),
        });
    }

    Ok(entries)
}

fn check_name(name: &str, path: &Path) -> Result<(), Error> {
    if !name.is_ascii() || name.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        bail!(
            "{} cannot be stored on an Xbox volume: names must be plain ASCII",
            path.display()
        );
    }
    if name.len() > MAX_NAME_LENGTH {
        bail!(
            "{} cannot be stored on an Xbox volume: the name is longer than {MAX_NAME_LENGTH} characters",
            path.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// A seekable reader over a [`FolderImage`]. Each reader has its own file
/// handle, so one can be used per worker thread.
pub struct FolderReader {
    layout: Arc<Layout>,
    position: u64,
    open_file: Option<OpenFile>,
}

struct OpenFile {
    index: usize,
    file: File,
    /// Where the handle's cursor currently is, relative to the file start.
    cursor: u64,
}

impl FolderReader {
    fn file_for(&mut self, index: usize) -> io::Result<&mut OpenFile> {
        if self
            .open_file
            .as_ref()
            .is_none_or(|open| open.index != index)
        {
            let extent = &self.layout.files[index];
            let file = File::open(&extent.path).map_err(|error| {
                io::Error::new(error.kind(), format!("{}: {error}", extent.path.display()))
            })?;
            let size = file.metadata()?.len();
            if size != extent.len {
                return Err(io::Error::other(format!(
                    "{} changed while it was being converted",
                    extent.path.display()
                )));
            }
            self.open_file = Some(OpenFile {
                index,
                file,
                cursor: 0,
            });
        }
        Ok(self.open_file.as_mut().expect("file was just opened"))
    }
}

impl Read for FolderReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let layout = Arc::clone(&self.layout);
        let position = self.position;
        if buf.is_empty() || position >= layout.size {
            return Ok(0);
        }

        let read = if position < layout.metadata.len() as u64 {
            let start = position as usize;
            let count = min(buf.len(), layout.metadata.len() - start);
            buf[..count].copy_from_slice(&layout.metadata[start..start + count]);
            count
        } else {
            let index = layout.files.partition_point(|file| file.end() <= position);
            match layout.files.get(index) {
                Some(extent) if extent.start <= position => {
                    let offset = position - extent.start;
                    let count = min(buf.len() as u64, extent.len - offset) as usize;

                    let open = self.file_for(index)?;
                    if open.cursor != offset {
                        open.file.seek(SeekFrom::Start(offset))?;
                        open.cursor = offset;
                    }
                    let count = match open.file.read(&mut buf[..count]) {
                        Ok(0) => {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                format!("{} became shorter", extent.path.display()),
                            ));
                        }
                        Ok(count) => count,
                        Err(error) => {
                            // The handle's position is unknown now.
                            self.open_file = None;
                            return Err(error);
                        }
                    };
                    open.cursor += count as u64;
                    count
                }
                // Padding between files, or after the last one.
                next => {
                    let gap_end = next.map_or(layout.size, |extent| extent.start);
                    let count = min(buf.len() as u64, gap_end - position) as usize;
                    buf[..count].fill(0);
                    count
                }
            }
        };

        self.position += read as u64;
        Ok(read)
    }
}

impl Seek for FolderReader {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        let (base, delta) = match target {
            SeekFrom::Start(position) => {
                self.position = position;
                return Ok(position);
            }
            SeekFrom::Current(delta) => (self.position, delta),
            SeekFrom::End(delta) => (self.layout.size, delta),
        };

        self.position = base.checked_add_signed(delta).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to a negative or overflowing position",
            )
        })?;
        Ok(self.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iso::IsoReader;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("iso2god-next-folder-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read_all(image: &FolderImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        image.open().read_to_end(&mut bytes).unwrap();
        bytes
    }

    /// Parses the volume with the ISO reader and checks that every file in
    /// `expected` (path relative to the volume root, forward slashes) is
    /// present with the right contents.
    fn assert_volume_contains(image: &FolderImage, expected: &[(&str, Vec<u8>)]) {
        let bytes = read_all(image);
        let mut iso = IsoReader::read(io::Cursor::new(&bytes)).unwrap();

        for (path, contents) in expected {
            let windows_path = format!("\\{}", path.replace('/', "\\"));
            let entry_sector = {
                let mut dir = Some(&iso.directory_table);
                let mut found = None;
                for part in path.split('/') {
                    let entry = dir.and_then(|dir| dir.get_entry(part));
                    dir = entry.and_then(|entry| entry.subdirectory.as_ref());
                    found = entry.map(|entry| (entry.sector, entry.size));
                }
                found.unwrap_or_else(|| panic!("{windows_path} is missing from the volume"))
            };

            assert_eq!(entry_sector.1 as usize, contents.len(), "{windows_path}");
            let start = entry_sector.0 as usize * SECTOR;
            if !contents.is_empty() {
                assert_eq!(
                    &bytes[start..start + contents.len()],
                    contents.as_slice(),
                    "{windows_path}"
                );
            }
            // the generic accessor agrees with the manual walk
            if !contents.is_empty() {
                assert!(
                    iso.get_entry(&windows_path.as_str().into())
                        .unwrap()
                        .is_some()
                );
            }
        }
    }

    #[test]
    fn a_small_folder_becomes_a_readable_volume() {
        let dir = temp_dir("small");
        fs::write(dir.join("default.xex"), b"xex").unwrap();
        fs::create_dir_all(dir.join("Media").join("Video")).unwrap();
        fs::write(dir.join("Media").join("intro.bik"), vec![7_u8; 5000]).unwrap();
        fs::write(
            dir.join("Media").join("Video").join("logo.wmv"),
            [9_u8; 2048],
        )
        .unwrap();
        fs::write(dir.join("empty.dat"), b"").unwrap();

        let image = FolderImage::scan(&dir).unwrap();
        assert_eq!(image.volume_size() % BLOCK_SIZE, 0);
        assert_eq!(read_all(&image).len() as u64, image.volume_size());

        assert_volume_contains(
            &image,
            &[
                ("default.xex", b"xex".to_vec()),
                ("Media/intro.bik", vec![7_u8; 5000]),
                ("media/video/LOGO.WMV", vec![9_u8; 2048]),
                ("empty.dat", Vec::new()),
            ],
        );

        // identical output on a second scan
        assert_eq!(
            read_all(&image),
            read_all(&FolderImage::scan(&dir).unwrap())
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn big_directories_span_several_sectors_and_stay_searchable() {
        let dir = temp_dir("big");
        let mut expected = Vec::new();
        for index in 0..700_usize {
            let name = format!("File_{index:04}_{}.bin", "x".repeat(index % 40));
            let contents = vec![(index % 251) as u8; index % 3000];
            fs::write(dir.join(&name), &contents).unwrap();
            expected.push((name, contents));
        }

        let image = FolderImage::scan(&dir).unwrap();
        let expected: Vec<(&str, Vec<u8>)> = expected
            .iter()
            .map(|(name, contents)| (name.as_str(), contents.clone()))
            .collect();
        assert_volume_contains(&image, &expected);

        // The root table is several sectors long.
        let bytes = read_all(&image);
        let iso = IsoReader::read(io::Cursor::new(&bytes)).unwrap();
        assert!(iso.volume_descriptor.root_directory_size > 2 * SECTOR_SIZE as u32);
        assert_eq!(iso.directory_table.entries.len(), 700);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_are_sorted_the_way_the_console_compares_them() {
        let dir = temp_dir("sorted");
        for name in ["b.txt", "A.txt", "_x.txt", "z.txt", "Zed.txt", "a_b.txt"] {
            fs::write(dir.join(name), b"1").unwrap();
        }

        let image = FolderImage::scan(&dir).unwrap();
        let bytes = read_all(&image);
        let iso = IsoReader::read(io::Cursor::new(&bytes)).unwrap();
        let names: Vec<_> = iso
            .directory_table
            .entries
            .iter()
            .map(|entry| entry.name.to_ascii_uppercase())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_volume_descriptor_carries_both_identifiers() {
        let dir = temp_dir("descriptor");
        fs::write(dir.join("default.xex"), b"xex").unwrap();

        let bytes = read_all(&FolderImage::scan(&dir).unwrap());
        let start = DESCRIPTOR_SECTOR as usize * SECTOR;
        assert_eq!(&bytes[start..start + 20], IDENTIFIER);
        assert_eq!(&bytes[start + 0x7ec..start + 0x7ec + 20], IDENTIFIER);
        assert!(bytes[..start].iter().all(|byte| *byte == 0));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn root_files_are_found_ignoring_case() {
        let dir = temp_dir("root-files");
        fs::write(dir.join("Default.XEX"), b"xex").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub").join("default.xbe"), b"xbe").unwrap();

        let image = FolderImage::scan(&dir).unwrap();
        assert!(image.root_file("default.xex").is_some());
        assert!(image.root_file("default.xbe").is_none());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_manager_litter_is_left_out() {
        let dir = temp_dir("junk");
        fs::write(dir.join("default.xex"), b"xex").unwrap();
        for name in [".DS_Store", "Thumbs.db", "desktop.ini", "._default.xex"] {
            fs::write(dir.join(name), b"junk").unwrap();
        }

        let bytes = read_all(&FolderImage::scan(&dir).unwrap());
        let iso = IsoReader::read(io::Cursor::new(&bytes)).unwrap();
        assert_eq!(iso.directory_table.entries.len(), 1);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_that_cannot_be_stored_are_rejected() {
        let dir = temp_dir("names");
        fs::write(dir.join("caf\u{e9}.txt"), b"1").unwrap();
        let error = FolderImage::scan(&dir).unwrap_err();
        assert!(format!("{error:#}").contains("ASCII"), "{error:#}");
        fs::remove_dir_all(&dir).unwrap();

        assert!(check_name(&"a".repeat(255), Path::new("x")).is_ok());
        let error = check_name(&"a".repeat(256), Path::new("x")).unwrap_err();
        assert!(format!("{error:#}").contains("longer"), "{error:#}");
    }

    #[test]
    fn names_differing_only_in_case_are_rejected() {
        let dir = temp_dir("case");
        fs::write(dir.join("Movie.bik"), b"1").unwrap();
        // case-insensitive file systems cannot hold both; nothing to test there
        if fs::write(dir.join("movie.BIK"), b"2").is_ok()
            && fs::read_dir(&dir).unwrap().count() == 2
        {
            let error = FolderImage::scan(&dir).unwrap_err();
            assert!(format!("{error:#}").contains("letter case"), "{error:#}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_that_changes_size_is_reported() {
        let dir = temp_dir("changed");
        fs::write(dir.join("default.xex"), vec![1_u8; 100]).unwrap();
        let image = FolderImage::scan(&dir).unwrap();

        fs::write(dir.join("default.xex"), vec![1_u8; 50]).unwrap();
        let mut bytes = Vec::new();
        let error = image.open().read_to_end(&mut bytes).unwrap_err();
        assert!(error.to_string().contains("changed"), "{error}");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn seeking_reads_the_same_bytes_as_streaming() {
        let dir = temp_dir("seek");
        fs::write(
            dir.join("a.bin"),
            (0..10_000).map(|i| i as u8).collect::<Vec<_>>(),
        )
        .unwrap();
        fs::write(dir.join("b.bin"), vec![3_u8; 6000]).unwrap();

        let image = FolderImage::scan(&dir).unwrap();
        let all = read_all(&image);

        let mut reader = image.open();
        for (offset, length) in [
            (0_u64, 100_usize),
            (70_000, 5000),
            (4096, 4096),
            (123, 9000),
        ] {
            let end = (offset as usize + length).min(all.len());
            let mut buffer = vec![0_u8; end - offset as usize];
            reader.seek(SeekFrom::Start(offset)).unwrap();
            reader.read_exact(&mut buffer).unwrap();
            assert_eq!(buffer, &all[offset as usize..end], "offset {offset}");
        }

        let mut tail = Vec::new();
        reader.seek(SeekFrom::End(-10)).unwrap();
        reader.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, &all[all.len() - 10..]);

        assert!(reader.seek(SeekFrom::Current(-1_000_000_000)).is_err());

        fs::remove_dir_all(&dir).unwrap();
    }
}
