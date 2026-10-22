use crate::folder::FolderImage;
use crate::god::ContentType;
use crate::iso::IsoReader;
use anyhow::{Context, Error, bail};
use byteorder::{BE, LE, ReadBytesExt};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

pub mod xbe;
pub mod xex;

#[derive(Clone, Debug)]
pub struct TitleExecutionInfo {
    pub media_id: u32,
    pub version: u32,
    pub base_version: u32,
    pub title_id: u32,
    pub platform: u8,
    pub executable_type: u8,
    pub disc_number: u8,
    pub disc_count: u8,
}

pub struct TitleInfo {
    pub content_type: ContentType,
    pub execution_info: TitleExecutionInfo,
}

impl TitleExecutionInfo {
    pub fn from_xex<R: Read>(mut reader: R) -> Result<TitleExecutionInfo, Error> {
        Ok(TitleExecutionInfo {
            media_id: reader.read_u32::<BE>()?,
            version: reader.read_u32::<BE>()?,
            base_version: reader.read_u32::<BE>()?,
            title_id: reader.read_u32::<BE>()?,
            platform: reader.read_u8()?,
            executable_type: reader.read_u8()?,
            disc_number: reader.read_u8()?,
            disc_count: reader.read_u8()?,
        })
    }

    pub fn from_xbe<R: Read + Seek>(mut reader: R) -> Result<TitleExecutionInfo, Error> {
        reader.seek(SeekFrom::Current(8))?;
        let title_id = reader.read_u32::<LE>()?;

        reader.seek(SeekFrom::Current(164))?;
        let version = reader.read_u32::<LE>()?;

        Ok(TitleExecutionInfo {
            media_id: 0,
            version,
            base_version: 0,
            title_id,
            platform: 0,
            executable_type: 0,
            disc_number: 1,
            disc_count: 1,
        })
    }
}

impl TitleInfo {
    pub fn from_image<R: Read + Seek>(iso_image: &mut IsoReader<R>) -> Result<TitleInfo, Error> {
        if let Some(mut executable) = iso_image.get_entry(&"\\default.xex".into())? {
            Self::from_xex_executable(&mut executable)
        } else if let Some(mut executable) = iso_image.get_entry(&"\\default.xbe".into())? {
            Self::from_xbe_executable(&mut executable)
        } else {
            bail!("no executable found in this image");
        }
    }

    /// Reads the title of a game that is a plain folder holding a
    /// `default.xex` (Xbox 360) or `default.xbe` (original Xbox).
    pub fn from_folder(folder: &FolderImage) -> Result<TitleInfo, Error> {
        if let Some(path) = folder.root_file("default.xex") {
            let mut executable =
                File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
            Self::from_xex_executable(&mut executable)
        } else if let Some(path) = folder.root_file("default.xbe") {
            let mut executable =
                File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
            Self::from_xbe_executable(&mut executable)
        } else {
            bail!("no default.xex or default.xbe found in this folder");
        }
    }

    fn from_xex_executable<R: Read + Seek>(executable: R) -> Result<TitleInfo, Error> {
        let header = xex::XexHeader::read(executable).context("error reading default.xex")?;
        let execution_info = header
            .fields
            .execution_info
            .context("no execution info in default.xex header")?;

        Ok(TitleInfo {
            content_type: ContentType::GamesOnDemand,
            execution_info,
        })
    }

    fn from_xbe_executable<R: Read + Seek>(executable: R) -> Result<TitleInfo, Error> {
        let header = xbe::XbeHeader::read(executable).context("error reading default.xbe")?;
        let execution_info = header
            .fields
            .execution_info
            .context("no execution info in default.xbe header")?;

        Ok(TitleInfo {
            content_type: ContentType::XboxOriginal,
            execution_info,
        })
    }
}
