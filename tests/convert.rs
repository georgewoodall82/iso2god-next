//! End-to-end tests for the conversion pipeline using a tiny synthetic Xbox
//! image, so the whole ISO -> GOD path runs without a retail disc image.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use iso2god_next::convert::{self, ConvertOptions, Stage};
use iso2god_next::god::ContentType;
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

    // default.xex: XEX2 header with a single ExecutionId field.
    let xex = xex_sector * SECTOR;
    image[xex..xex + 4].copy_from_slice(b"XEX2");
    image[xex + 20..xex + 24].copy_from_slice(&1_u32.to_be_bytes()); // field count
    image[xex + 24..xex + 28].copy_from_slice(&0x0004_0006_u32.to_be_bytes()); // ExecutionId
    image[xex + 28..xex + 32].copy_from_slice(&0x100_u32.to_be_bytes());

    let execution_info = xex + 0x100;
    image[execution_info..execution_info + 4].copy_from_slice(&media_id.to_be_bytes());
    image[execution_info + 4..execution_info + 8].copy_from_slice(&1_u32.to_be_bytes());
    image[execution_info + 8..execution_info + 12].copy_from_slice(&0_u32.to_be_bytes());
    image[execution_info + 12..execution_info + 16].copy_from_slice(&title_id.to_be_bytes());
    image[execution_info + 16] = 0; // platform
    image[execution_info + 17] = 0; // executable type
    image[execution_info + 18] = 1; // disc number
    image[execution_info + 19] = 1; // disc count

    let payload_offset = payload_sector * SECTOR;
    image[payload_offset..payload_offset + payload.len()].copy_from_slice(payload);

    image
}
