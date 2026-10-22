//! High-level conversion pipeline: everything a frontend (CLI or GUI) needs to
//! inspect an ISO image (or a folder holding a game's files) and turn it into a
//! Games-On-Demand package, without having to know about the on-disk formats.
//!
//! The pipeline is frontend-agnostic: it reports progress through a callback
//! and can be cancelled cooperatively at any point, so it can be driven from a
//! terminal, a GTK worker thread or anything else.

use std::fmt;
use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Error, bail};
use rayon::prelude::*;

use crate::executable::{TitleExecutionInfo, TitleInfo};
use crate::folder::FolderImage;
use crate::god::{self, ContentType, FileLayout};
use crate::{game_list, iso};

/// Marks an error caused by the user cancelling a conversion.
///
/// Frontends can tell it apart from real failures with
/// [`anyhow::Error::is`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("conversion cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// What kind of source a package is made from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// An ISO image of a disc.
    Iso,
    /// A folder with the game's files, including a `default.xex` (or
    /// `default.xbe`) at the top level.
    Folder,
}

/// How much of the source image should be copied into the GOD package.
///
/// Folders have no unused space, so the mode only affects ISO images.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrimMode {
    /// (default) Copy everything up to the last used byte of the image,
    /// dropping unallocated space at the end.
    #[default]
    FromEnd,
    /// Copy the whole data volume.
    None,
}

impl TrimMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TrimMode::FromEnd => "from-end",
            TrimMode::None => "none",
        }
    }
}

/// Options for a single ISO (or folder) -> GOD conversion.
#[derive(Clone, Debug)]
pub struct ConvertOptions {
    /// An ISO file or a game folder.
    pub source: PathBuf,
    pub dest_dir: PathBuf,
    /// Overrides the game title stored in the CON header.
    pub game_title: Option<String>,
    pub trim: TrimMode,
    /// Worker threads used while writing part files.
    pub num_threads: usize,
}

impl ConvertOptions {
    pub fn new(source: impl Into<PathBuf>, dest_dir: impl Into<PathBuf>) -> Self {
        ConvertOptions {
            source: source.into(),
            dest_dir: dest_dir.into(),
            game_title: None,
            trim: TrimMode::default(),
            num_threads: default_num_threads(),
        }
    }
}

/// A sensible default for frontends that don't expose thread tuning.
pub fn default_num_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 8)
}

/// Everything that can be learned about an image or folder without converting
/// it.
#[derive(Clone, Debug)]
pub struct IsoInfo {
    pub kind: SourceKind,
    pub title_id: u32,
    pub title_name: Option<String>,
    pub content_type: ContentType,
    pub execution_info: TitleExecutionInfo,
    /// Offset of the data volume inside the ISO file (zero for folders).
    pub root_offset: u64,
    /// Size of the ISO file on disk; for a folder, the size of the volume
    /// built from it.
    pub source_size: u64,
    /// Size of the data volume that would be converted.
    pub data_size: u64,
    pub block_count: u64,
    pub part_count: u64,
}

impl IsoInfo {
    pub fn title_id_string(&self) -> String {
        format!("{:08X}", self.title_id)
    }

    /// The known game title, falling back to the name of the ISO or folder.
    pub fn display_name(&self, source: &Path) -> String {
        self.title_name
            .clone()
            .or_else(|| source_name(source))
            .unwrap_or_else(|| "Unknown title".to_owned())
    }

    pub fn layout<'a>(&'a self, dest_dir: &'a Path) -> FileLayout<'a> {
        FileLayout::new(dest_dir, &self.execution_info, self.content_type)
    }

    /// Folder that receives the `Data####` files, including the stale data that
    /// a conversion removes before writing.
    pub fn data_dir_path(&self, dest_dir: &Path) -> PathBuf {
        self.layout(dest_dir).data_dir_path()
    }

    /// Folder holding everything for this title (`<TitleID>/<ContentType>`),
    /// only present after a conversion.
    pub fn package_dir_path(&self, dest_dir: &Path) -> PathBuf {
        self.layout(dest_dir).package_dir_path()
    }

    pub fn package_relative_path(&self) -> PathBuf {
        self.layout(Path::new("")).package_relative_path()
    }

    /// The extension-less CON header file that marks a finished conversion.
    pub fn con_header_file_path(&self, dest_dir: &Path) -> PathBuf {
        self.layout(dest_dir).con_header_file_path()
    }
}

/// Stage of the conversion pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    WritingParts,
    Hashing,
    WritingHeader,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::WritingParts => "Writing part files",
            Stage::Hashing => "Calculating MHT hash chain",
            Stage::WritingHeader => "Writing CON header",
        }
    }
}

/// A progress sample reported during a conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConversionProgress {
    pub stage: Stage,
    /// Work done in the current stage, in bytes or items depending on `stage`.
    pub done: u64,
    pub total: u64,
}

impl ConversionProgress {
    /// Completion of the current stage, clamped to `0.0..=1.0`.
    pub fn fraction(self) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        (self.done as f64 / self.total as f64).clamp(0.0, 1.0)
    }

    /// Completion of the whole conversion, weighted by how long each stage
    /// takes. Useful for a single per-file progress bar.
    pub fn overall_fraction(self) -> f64 {
        let fraction = self.fraction();
        match self.stage {
            Stage::WritingParts => fraction * 0.90,
            Stage::Hashing => 0.90 + fraction * 0.09,
            Stage::WritingHeader => 0.99 + fraction * 0.01,
        }
    }
}

/// Shared, thread-safe cancellation flag.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// The file stem of an ISO, or the whole name of a folder (whose dots are
/// usually part of the name, as in "Game v1.2").
fn source_name(source: &Path) -> Option<String> {
    let name = if source.is_dir() {
        source.file_name()
    } else {
        source.file_stem()
    };
    name.map(|name| name.to_string_lossy().into_owned())
}

/// Where the data volume of a source comes from.
enum Volume {
    Iso { path: PathBuf, root_offset: u64 },
    Folder(FolderImage),
}

/// Reads the metadata needed to display and convert an image or folder.
pub fn inspect(source: &Path) -> Result<IsoInfo, Error> {
    inspect_with_trim(source, TrimMode::default())
}

/// Reads the metadata needed to display and convert an image or folder, using
/// the given trimming mode to decide how large the converted data volume would
/// be.
pub fn inspect_with_trim(source: &Path, trim: TrimMode) -> Result<IsoInfo, Error> {
    open_volume(source, trim).map(|(info, _)| info)
}

fn open_volume(source: &Path, trim: TrimMode) -> Result<(IsoInfo, Volume), Error> {
    if source.is_dir() {
        open_folder(source)
    } else {
        open_iso(source, trim)
    }
}

fn open_iso(source_iso: &Path, trim: TrimMode) -> Result<(IsoInfo, Volume), Error> {
    let file =
        File::open(source_iso).with_context(|| format!("cannot open {}", source_iso.display()))?;
    let source_size = fs::metadata(source_iso)
        .with_context(|| format!("cannot read metadata of {}", source_iso.display()))?
        .len();

    let mut reader = iso::IsoReader::read(file)
        .with_context(|| format!("cannot read {} as an Xbox ISO", source_iso.display()))?;
    let title_info = TitleInfo::from_image(&mut reader)
        .with_context(|| format!("cannot read the executable of {}", source_iso.display()))?;

    let root_offset = reader.volume_descriptor.root_offset;
    let data_size = match trim {
        TrimMode::FromEnd => reader.get_max_used_prefix_size(),
        TrimMode::None => source_size.saturating_sub(root_offset),
    };
    let data_size = data_size.min(source_size.saturating_sub(root_offset));

    let info = describe_source(
        SourceKind::Iso,
        title_info,
        root_offset,
        source_size,
        data_size,
    );
    let volume = Volume::Iso {
        path: source_iso.to_owned(),
        root_offset,
    };
    Ok((info, volume))
}

fn open_folder(folder: &Path) -> Result<(IsoInfo, Volume), Error> {
    let image = FolderImage::scan(folder)
        .with_context(|| format!("cannot read the game files in {}", folder.display()))?;
    let title_info = TitleInfo::from_folder(&image)
        .with_context(|| format!("cannot read the executable in {}", folder.display()))?;

    let size = image.volume_size();
    let info = describe_source(SourceKind::Folder, title_info, 0, size, size);
    Ok((info, Volume::Folder(image)))
}

fn describe_source(
    kind: SourceKind,
    title_info: TitleInfo,
    root_offset: u64,
    source_size: u64,
    data_size: u64,
) -> IsoInfo {
    let block_count = data_size.div_ceil(god::BLOCK_SIZE).max(1);
    let part_count = block_count.div_ceil(god::BLOCKS_PER_PART).max(1);

    IsoInfo {
        kind,
        title_id: title_info.execution_info.title_id,
        title_name: game_list::find_title_by_id(title_info.execution_info.title_id),
        content_type: title_info.content_type,
        execution_info: title_info.execution_info,
        root_offset,
        source_size,
        data_size,
        block_count,
        part_count,
    }
}

/// A package written into the folder it was made from would be picked up by
/// the next conversion, so refuse that up front.
fn ensure_dest_outside_source(source: &Path, dest_dir: &Path) -> Result<(), Error> {
    let source =
        fs::canonicalize(source).with_context(|| format!("cannot resolve {}", source.display()))?;
    let dest = fs::canonicalize(dest_dir)
        .or_else(|_| std::path::absolute(dest_dir))
        .with_context(|| format!("cannot resolve {}", dest_dir.display()))?;

    if dest.starts_with(&source) {
        bail!(
            "the output folder {} is inside the game folder {}; choose a different output folder",
            dest_dir.display(),
            source.display()
        );
    }
    Ok(())
}

/// Converts one ISO into the GOD layout inside `options.dest_dir`.
///
/// `on_progress` is called from worker threads and should be cheap and
/// thread-safe; frontends usually throttle it before touching the UI.
pub fn convert<F>(options: &ConvertOptions, on_progress: F) -> Result<(), Error>
where
    F: Fn(ConversionProgress) + Send + Sync,
{
    convert_cancellable(options, &CancelToken::new(), on_progress)
}

/// Same as [`convert`], but can be stopped with `cancel`.
///
/// Cancelling removes the partially written data folder, but leaves anything
/// that was already in the destination untouched.
pub fn convert_cancellable<F>(
    options: &ConvertOptions,
    cancel: &CancelToken,
    on_progress: F,
) -> Result<(), Error>
where
    F: Fn(ConversionProgress) + Send + Sync,
{
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.num_threads.max(1))
        .build()
        .context("error creating the worker thread pool")?;

    pool.install(|| convert_in_pool(options, cancel, on_progress))
}

fn convert_in_pool<F>(
    options: &ConvertOptions,
    cancel: &CancelToken,
    on_progress: F,
) -> Result<(), Error>
where
    F: Fn(ConversionProgress) + Send + Sync,
{
    if cancel.is_cancelled() {
        bail!(Cancelled);
    }

    let (info, volume) = open_volume(&options.source, options.trim)?;
    if info.kind == SourceKind::Folder {
        ensure_dest_outside_source(&options.source, &options.dest_dir)?;
    }

    let file_layout = info.layout(&options.dest_dir);
    let data_dir = file_layout.data_dir_path();
    let part_count = info.part_count;

    ensure_empty_dir(&data_dir).context("error clearing the data directory")?;

    let total_bytes = info.block_count * god::BLOCK_SIZE;
    let done_bytes = AtomicU64::new(0);

    on_progress(ConversionProgress {
        stage: Stage::WritingParts,
        done: 0,
        total: total_bytes,
    });

    let write_result = (0..part_count).into_par_iter().try_for_each(|part_index| {
        if cancel.is_cancelled() {
            bail!(Cancelled);
        }

        let part_file = file_layout.part_file_path(part_index);
        let part_file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&part_file)
            .with_context(|| format!("cannot create {}", part_file.display()))?;

        let on_bytes = |bytes| {
            let done = done_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
            on_progress(ConversionProgress {
                stage: Stage::WritingParts,
                done,
                total: total_bytes,
            });
        };
        let is_cancelled = || cancel.is_cancelled();

        match &volume {
            Volume::Iso { path, root_offset } => {
                let mut data_volume =
                    File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
                data_volume
                    .seek(SeekFrom::Start(*root_offset))
                    .context("cannot seek to the start of the data volume")?;
                god::write_part_observed(data_volume, part_index, part_file, on_bytes, is_cancelled)
            }
            Volume::Folder(image) => god::write_part_observed(
                image.open(),
                part_index,
                part_file,
                on_bytes,
                is_cancelled,
            ),
        }
        .with_context(|| format!("error writing part {part_index}"))?;

        Ok::<_, Error>(())
    });

    if let Err(error) = write_result {
        discard_if_cancelled(cancel, &data_dir);
        return Err(error);
    }

    if discard_if_cancelled(cancel, &data_dir) {
        bail!(Cancelled);
    }

    on_progress(ConversionProgress {
        stage: Stage::Hashing,
        done: 0,
        total: part_count,
    });

    let mut mht =
        read_part_mht(&file_layout, part_count - 1).context("error reading the part file MHT")?;

    for (index, prev_part_index) in (0..part_count - 1).rev().enumerate() {
        if discard_if_cancelled(cancel, &data_dir) {
            bail!(Cancelled);
        }

        let mut prev_mht = read_part_mht(&file_layout, prev_part_index)
            .context("error reading the part file MHT")?;
        prev_mht.add_hash(&mht.digest());
        write_part_mht(&file_layout, prev_part_index, &prev_mht)
            .context("error writing the part file MHT")?;
        mht = prev_mht;

        on_progress(ConversionProgress {
            stage: Stage::Hashing,
            done: index as u64 + 1,
            total: part_count,
        });
    }

    let last_part_size = fs::metadata(file_layout.part_file_path(part_count - 1))
        .map(|metadata| metadata.len())
        .context("error reading the last part file")?;

    on_progress(ConversionProgress {
        stage: Stage::WritingHeader,
        done: 0,
        total: 1,
    });

    let mut con_header = god::ConHeaderBuilder::new()
        .with_execution_info(&info.execution_info)
        .with_block_counts(info.block_count as u32, 0)
        .with_data_parts_info(
            part_count as u32,
            last_part_size + (part_count - 1) * god::BLOCK_SIZE * 0xa290,
        )
        .with_content_type(info.content_type)
        .with_mht_hash(&mht.digest());

    let game_title = options
        .game_title
        .clone()
        .or_else(|| info.title_name.clone());
    if let Some(game_title) = game_title {
        con_header = con_header.with_game_title(&game_title);
    }

    let con_header = con_header.finalize();
    let con_header_file_path = file_layout.con_header_file_path();
    if let Some(parent) = con_header_file_path.parent() {
        fs::create_dir_all(parent).context("error creating the destination folder")?;
    }

    let mut con_header_file = File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&con_header_file_path)
        .with_context(|| format!("cannot open {}", con_header_file_path.display()))?;
    con_header_file
        .write_all(&con_header)
        .context("error writing the CON header file")?;

    on_progress(ConversionProgress {
        stage: Stage::WritingHeader,
        done: 1,
        total: 1,
    });

    Ok(())
}

/// Removes the half-converted data folder if the conversion was cancelled, so
/// a multi-gigabyte partial result isn't left behind. Returns whether it was.
fn discard_if_cancelled(cancel: &CancelToken, data_dir: &Path) -> bool {
    let cancelled = cancel.is_cancelled();
    if cancelled {
        let _ = fs::remove_dir_all(data_dir);
    }
    cancelled
}

fn ensure_empty_dir(path: &Path) -> Result<(), Error> {
    if fs::exists(path)? {
        fs::remove_dir_all(path)?;
    }
    fs::create_dir_all(path)?;
    Ok(())
}

fn read_part_mht(file_layout: &FileLayout, part_index: u64) -> Result<god::HashList, Error> {
    let mut part_file = File::options()
        .read(true)
        .open(file_layout.part_file_path(part_index))?;
    god::HashList::read(&mut part_file)
}

fn write_part_mht(
    file_layout: &FileLayout,
    part_index: u64,
    mht: &god::HashList,
) -> Result<(), Error> {
    let mut part_file = File::options()
        .write(true)
        .open(file_layout.part_file_path(part_index))?;
    mht.write(&mut part_file)?;
    Ok(())
}
