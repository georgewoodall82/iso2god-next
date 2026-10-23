//! End-to-end tests for the conversion pipeline using a tiny synthetic Xbox
//! image, so the whole ISO -> GOD path runs without a retail disc image.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use iso2god_next::convert::{self, ConvertOptions, SourceKind, Stage};
use iso2god_next::folder::FolderImage;
use iso2god_next::god::ContentType;
use iso2god_next::iso::IsoReader;
use iso2god_next::{CancelToken, Cancelled};

const SECTOR: usize = 0x800;
const TITLE_ID: u32 = 0x4156_07F7;
const MEDIA_ID: u32 = 0x1122_3344;

#[test]
fn converts_a_synthetic_image_into_the_god_layout() {
    let dir = temp_dir("convert");
    let iso_path = dir.join("Test Game.iso");
    let payload = vec![0x5a_u8; 0x4000];
    fs::write(&iso_path, build_xsf_iso(TITLE_ID, MEDIA_ID, &payload)).unwrap();

    let info = convert::inspect(&iso_path).unwrap();
    assert_eq!(info.title_id, TITLE_ID);
    assert_eq!(info.content_type, ContentType::GamesOnDemand);
    assert_eq!(info.execution_info.media_id, MEDIA_ID);
    assert_eq!(info.part_count, 1);
    assert_eq!(info.block_count, info.data_size.div_ceil(0x1000));

    let dest_dir = dir.join("out");
    let stages = Mutex::new(Vec::new());
    let progress = Mutex::new(Vec::new());

    convert::convert(&ConvertOptions::new(&iso_path, &dest_dir), |sample| {
        stages.lock().unwrap().push(sample.stage);
        progress.lock().unwrap().push(sample.overall_fraction());
    })
    .unwrap();

    let con_header = info.con_header_file_path(&dest_dir);
    let data_dir = info.data_dir_path(&dest_dir);
    let part_file = data_dir.join("Data0000");

    assert_eq!(
        con_header,
        dest_dir.join("415607F7").join("00007000").join("11223344")
    );
    assert!(con_header.is_file(), "missing {}", con_header.display());
    assert!(part_file.is_file(), "missing {}", part_file.display());

    // Master hash list, one sub hash list, then the data volume itself.
    let part = fs::read(&part_file).unwrap();
    assert!(part.len() >= 2 * 0x1000);
    let data = iso_bytes_at(&iso_path, 0, info.data_size as usize);
    assert_eq!(&part[0x2000..0x2000 + data.len()], data.as_slice());

    // The CON header holds the LIVE container followed by the metadata block.
    let header = fs::read(&con_header).unwrap();
    assert!(header.len() > 0xacbc);
    assert_eq!(
        &header[0x0392..0x0395],
        &(info.block_count as u32).to_be_bytes()[1..]
    );

    let seen: Vec<Stage> = stages.into_inner().unwrap();
    assert!(seen.contains(&Stage::WritingParts));
    assert!(seen.contains(&Stage::Hashing));
    assert_eq!(seen.last(), Some(&Stage::WritingHeader));

    let fractions = progress.into_inner().unwrap();
    assert!(
        fractions
            .windows(2)
            .all(|pair| pair[1] + f64::EPSILON >= pair[0]),
        "progress must not go backwards: {fractions:?}"
    );
    assert_eq!(fractions.last().copied(), Some(1.0));

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cancellation_is_reported_and_leaves_no_partial_folder() {
    let dir = temp_dir("cancel");
    let iso_path = dir.join("Test Game.iso");
    fs::write(
        &iso_path,
        build_xsf_iso(TITLE_ID, MEDIA_ID, &[0x5a_u8; 0x4000]),
    )
    .unwrap();

    let dest_dir = dir.join("out");
    let cancel = CancelToken::new();
    cancel.cancel();

    let error =
        convert::convert_cancellable(&ConvertOptions::new(&iso_path, &dest_dir), &cancel, |_| {})
            .unwrap_err();

    assert!(error.is::<Cancelled>(), "unexpected error: {error:#}");
    assert!(!dest_dir.exists(), "a cancelled conversion wrote files");

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cancelling_halfway_removes_the_half_converted_package() {
    let dir = temp_dir("cancel-halfway");
    let iso_path = dir.join("Test Game.iso");
    fs::write(
        &iso_path,
        build_xsf_iso(TITLE_ID, MEDIA_ID, &[0x5a_u8; 0x4000]),
    )
    .unwrap();
    let info = convert::inspect(&iso_path).unwrap();

    let dest_dir = dir.join("out");
    let other_game = dest_dir.join("DEADBEEF").join("00007000");
    fs::create_dir_all(&other_game).unwrap();
    fs::write(other_game.join("keep"), b"another game").unwrap();

    // An earlier, finished conversion of the same game is being redone.
    let options = ConvertOptions::new(&iso_path, &dest_dir);
    convert::convert(&options, |_| {}).unwrap();
    assert!(info.con_header_file_path(&dest_dir).is_file());

    let cancel = CancelToken::new();
    let error = convert::convert_cancellable(&options, &cancel, |sample| {
        if sample.stage == Stage::WritingParts && sample.done > 0 {
            cancel.cancel();
        }
    })
    .unwrap_err();

    assert!(error.is::<Cancelled>(), "unexpected error: {error:#}");
    assert!(!info.data_dir_path(&dest_dir).exists(), "data folder left");
    assert!(
        !info.con_header_file_path(&dest_dir).exists(),
        "stale header left"
    );
    assert!(
        !info.package_dir_path(&dest_dir).exists(),
        "package folder left"
    );
    assert!(
        !dest_dir.join("415607F7").exists(),
        "title folder left behind"
    );
    assert!(
        other_game.join("keep").is_file(),
        "another game was touched"
    );

    fs::remove_dir_all(&dir).unwrap();
}

fn iso_bytes_at(path: &Path, offset: usize, len: usize) -> Vec<u8> {
    let image = fs::read(path).unwrap();
    image[offset..offset + len].to_vec()
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("iso2god-next-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Minimal base-0 (XSF) Xbox image: volume descriptor, root table with a XEX and
/// a payload file, and a `default.xex` carrying a title ID.
fn build_xsf_iso(title_id: u32, media_id: u32, payload: &[u8]) -> Vec<u8> {
    let root_sector = 0x21_usize;
    let xex_sector = 0x22_usize;
    let payload_sector = 0x24_usize;
    let xex_size = 0x200_usize;

    let payload_end = payload_sector * SECTOR + payload.len();
    let mut image = vec![0_u8; payload_end.next_multiple_of(SECTOR)];

    let descriptor = 0x20 * SECTOR;
    image[descriptor..descriptor + 20].copy_from_slice(b"MICROSOFT*XBOX*MEDIA");
    image[descriptor + 20..descriptor + 24].copy_from_slice(&(root_sector as u32).to_le_bytes());
    image[descriptor + 24..descriptor + 28].copy_from_slice(&(SECTOR as u32).to_le_bytes());

    let mut cursor = root_sector * SECTOR;
    for (name, sector, size) in [
        ("default.xex", xex_sector, xex_size),
        ("payload.bin", payload_sector, payload.len()),
    ] {
        image[cursor..cursor + 2].copy_from_slice(&0_u16.to_le_bytes());
        image[cursor + 2..cursor + 4].copy_from_slice(&0_u16.to_le_bytes());
        image[cursor + 4..cursor + 8].copy_from_slice(&(sector as u32).to_le_bytes());
        image[cursor + 8..cursor + 12].copy_from_slice(&(size as u32).to_le_bytes());
        image[cursor + 12] = 0x20; // ARCHIVE
        image[cursor + 13] = name.len() as u8;
        image[cursor + 14..cursor + 14 + name.len()].copy_from_slice(name.as_bytes());
        cursor += 14 + name.len();
        cursor += (4 - cursor % 4) % 4;
    }
    image[cursor..cursor + 2].copy_from_slice(&0xffff_u16.to_le_bytes());
    image[cursor + 2..cursor + 4].copy_from_slice(&0xffff_u16.to_le_bytes());

    let xex = xex_sector * SECTOR;
    image[xex..xex + xex_size].copy_from_slice(&build_xex(title_id, media_id, xex_size));

    let payload_offset = payload_sector * SECTOR;
    image[payload_offset..payload_offset + payload.len()].copy_from_slice(payload);

    image
}

/// A `default.xex` of `size` bytes: an XEX2 header with a single ExecutionId
/// field carrying the title and media IDs.
fn build_xex(title_id: u32, media_id: u32, size: usize) -> Vec<u8> {
    let mut xex = vec![0_u8; size];
    xex[..4].copy_from_slice(b"XEX2");
    xex[20..24].copy_from_slice(&1_u32.to_be_bytes()); // field count
    xex[24..28].copy_from_slice(&0x0004_0006_u32.to_be_bytes()); // ExecutionId
    xex[28..32].copy_from_slice(&0x100_u32.to_be_bytes());

    let execution_info = 0x100;
    xex[execution_info..execution_info + 4].copy_from_slice(&media_id.to_be_bytes());
    xex[execution_info + 4..execution_info + 8].copy_from_slice(&1_u32.to_be_bytes());
    xex[execution_info + 8..execution_info + 12].copy_from_slice(&0_u32.to_be_bytes());
    xex[execution_info + 12..execution_info + 16].copy_from_slice(&title_id.to_be_bytes());
    xex[execution_info + 16] = 0; // platform
    xex[execution_info + 17] = 0; // executable type
    xex[execution_info + 18] = 1; // disc number
    xex[execution_info + 19] = 1; // disc count
    xex
}

/// A game folder: `default.xex`, a payload and a nested file.
fn build_game_folder(dir: &Path, payload: &[u8]) -> PathBuf {
    let game = dir.join("Test Game v1.2");
    fs::create_dir_all(game.join("Media")).unwrap();
    fs::write(
        game.join("default.xex"),
        build_xex(TITLE_ID, MEDIA_ID, 0x200),
    )
    .unwrap();
    fs::write(game.join("payload.bin"), payload).unwrap();
    fs::write(game.join("Media").join("intro.bik"), vec![0x11_u8; 5000]).unwrap();
    game
}

#[test]
fn converts_a_game_folder_without_building_an_iso() {
    let dir = temp_dir("folder");
    let payload = vec![0x5a_u8; 0x9000];
    let game = build_game_folder(&dir, &payload);

    let info = convert::inspect(&game).unwrap();
    assert_eq!(info.kind, SourceKind::Folder);
    assert_eq!(info.title_id, TITLE_ID);
    assert_eq!(info.content_type, ContentType::GamesOnDemand);
    assert_eq!(info.execution_info.media_id, MEDIA_ID);
    assert_eq!(info.part_count, 1);
    // an unknown title is named after the folder, dots included
    assert_eq!(info.display_name(&game), {
        info.title_name
            .clone()
            .unwrap_or_else(|| "Test Game v1.2".to_owned())
    });

    let dest_dir = dir.join("out");
    convert::convert(&ConvertOptions::new(&game, &dest_dir), |_| {}).unwrap();

    let con_header = info.con_header_file_path(&dest_dir);
    let part_file = info.data_dir_path(&dest_dir).join("Data0000");
    assert!(con_header.is_file(), "missing {}", con_header.display());
    assert!(part_file.is_file(), "missing {}", part_file.display());

    // The package holds exactly the volume the folder presents ...
    let mut volume = Vec::new();
    FolderImage::scan(&game)
        .unwrap()
        .open()
        .read_to_end(&mut volume)
        .unwrap();
    assert_eq!(volume.len() as u64, info.data_size);
    let part = fs::read(&part_file).unwrap();
    assert_eq!(&part[0x2000..0x2000 + volume.len()], volume.as_slice());

    // ... which is a valid Xbox volume containing the folder's files.
    let mut iso = IsoReader::read(std::io::Cursor::new(&volume)).unwrap();
    let payload_entry = iso.directory_table.get_entry("payload.bin").unwrap();
    let start = payload_entry.sector as usize * SECTOR;
    assert_eq!(&volume[start..start + payload.len()], payload.as_slice());
    assert!(
        iso.get_entry(&"\\Media\\intro.bik".into())
            .unwrap()
            .is_some()
    );

    let header = fs::read(&con_header).unwrap();
    assert_eq!(
        &header[0x0392..0x0395],
        &(info.block_count as u32).to_be_bytes()[1..]
    );

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_folder_and_the_matching_iso_give_the_same_package() {
    let dir = temp_dir("folder-vs-iso");
    let game = build_game_folder(&dir, &[0x5a_u8; 0x3000]);

    // Write the folder's volume out as an ISO image and convert that too.
    let iso_path = dir.join("Test Game.iso");
    let mut volume = Vec::new();
    FolderImage::scan(&game)
        .unwrap()
        .open()
        .read_to_end(&mut volume)
        .unwrap();
    fs::write(&iso_path, &volume).unwrap();

    let from_folder = dir.join("from-folder");
    let from_iso = dir.join("from-iso");
    convert::convert(&ConvertOptions::new(&game, &from_folder), |_| {}).unwrap();
    convert::convert(&ConvertOptions::new(&iso_path, &from_iso), |_| {}).unwrap();

    let info = convert::inspect(&game).unwrap();
    for file in [
        info.con_header_file_path(Path::new("")),
        info.data_dir_path(Path::new("")).join("Data0000"),
    ] {
        assert_eq!(
            fs::read(from_folder.join(&file)).unwrap(),
            fs::read(from_iso.join(&file)).unwrap(),
            "{} differs",
            file.display()
        );
    }

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_folder_without_an_executable_is_rejected() {
    let dir = temp_dir("folder-no-exe");
    let game = dir.join("game");
    fs::create_dir_all(game.join("nested")).unwrap();
    // an executable below the top level does not make it a game folder
    fs::write(
        game.join("nested").join("default.xex"),
        build_xex(1, 2, 0x200),
    )
    .unwrap();

    let error = convert::inspect(&game).unwrap_err();
    assert!(
        format!("{error:#}").contains("no default.xex"),
        "unexpected error: {error:#}"
    );

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_output_folder_may_not_be_inside_the_game_folder() {
    let dir = temp_dir("folder-dest-inside");
    let game = build_game_folder(&dir, &[1_u8; 100]);
    let dest_dir = game.join("out");

    let error = convert::convert(&ConvertOptions::new(&game, &dest_dir), |_| {}).unwrap_err();
    assert!(
        format!("{error:#}").contains("inside the game folder"),
        "unexpected error: {error:#}"
    );
    assert!(!dest_dir.exists());

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cancelling_a_folder_conversion_leaves_no_partial_folder() {
    let dir = temp_dir("folder-cancel");
    let game = build_game_folder(&dir, &[1_u8; 100]);
    let dest_dir = dir.join("out");
    let cancel = CancelToken::new();
    cancel.cancel();

    let error =
        convert::convert_cancellable(&ConvertOptions::new(&game, &dest_dir), &cancel, |_| {})
            .unwrap_err();
    assert!(error.is::<Cancelled>(), "unexpected error: {error:#}");
    assert!(!dest_dir.exists());

    fs::remove_dir_all(&dir).unwrap();
}
