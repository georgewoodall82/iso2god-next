use std::io::{Read, Seek, SeekFrom, Write};

use anyhow::Error;

use crate::convert::Cancelled;

mod con_header;
pub use con_header::*;

mod file_layout;
pub use file_layout::*;

mod gdf_sector;
pub use gdf_sector::*;

mod hash_list;
pub use hash_list::*;

pub const BLOCKS_PER_PART: u64 = 0xa1c4;
pub const BLOCKS_PER_SUBPART: u64 = 0xcc;
pub const BLOCK_SIZE: u64 = 0x1000;
pub const SUBPARTS_PER_PART: u32 = 0xcb;
pub const SUBPART_SIZE: u64 = BLOCK_SIZE * BLOCKS_PER_SUBPART;

pub fn write_part<R: Read + Seek, W: Write + Seek>(
    data_volume: R,
    part_index: u64,
    part_file: W,
) -> Result<(), Error> {
    write_part_observed(data_volume, part_index, part_file, |_| {}, || false)
}

/// Writes one part file, reporting copied bytes through `on_bytes` and checking
/// `cancelled` between subparts so long conversions can be stopped promptly.
pub fn write_part_observed<R, W, P, C>(
    mut data_volume: R,
    part_index: u64,
    mut part_file: W,
    mut on_bytes: P,
    mut cancelled: C,
) -> Result<(), Error>
where
    R: Read + Seek,
    W: Write + Seek,
    P: FnMut(u64),
    C: FnMut() -> bool,
{
    data_volume.seek_relative((part_index * BLOCKS_PER_PART * BLOCK_SIZE) as i64)?;

    let mut master_hash_list = HashList::new();

    let master_hash_list_position = part_file.stream_position()?;
    master_hash_list.write(&mut part_file)?;

    let mut subpart_buf = Vec::with_capacity(SUBPART_SIZE as usize);

    for _subpart_index in 0..SUBPARTS_PER_PART {
        if cancelled() {
            return Err(Cancelled.into());
        }

        data_volume
            .by_ref()
            .take(SUBPART_SIZE)
            .read_to_end(&mut subpart_buf)?;

        if subpart_buf.is_empty() {
            break;
        }

        let mut sub_hash_list = HashList::new();

        for block in subpart_buf.chunks(BLOCK_SIZE as usize) {
            sub_hash_list.add_block_hash(block);
        }

        sub_hash_list.write(&mut part_file)?;
        master_hash_list.add_block_hash(sub_hash_list.bytes());

        // using io::copy here to benefit from potential reflink optimizations
        // https://doc.rust-lang.org/std/io/fn.copy.html#platform-specific-behavior
        data_volume.seek_relative(0 - subpart_buf.len() as i64)?;
        std::io::copy(&mut data_volume.by_ref().take(SUBPART_SIZE), &mut part_file)?;
        on_bytes(subpart_buf.len() as u64);

        if subpart_buf.len() < SUBPART_SIZE as usize {
            break;
        }
        subpart_buf.clear();
    }

    part_file.seek(SeekFrom::Start(master_hash_list_position))?;
    master_hash_list.write(&mut part_file)?;

    Ok(())
}
