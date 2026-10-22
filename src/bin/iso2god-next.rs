use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Error};

use clap::{Parser, ValueEnum};

use iso2god_next::convert::{
    self, ConversionProgress, ConvertOptions, SourceKind, Stage, TrimMode,
};
use iso2god_next::god::ContentType;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(color = clap::ColorChoice::Never)]
struct Cli {
    /// ISO file, or folder holding a game's files (with a default.xex), to convert
    source: PathBuf,

    /// A folder to write resulting GOD files to
    dest_dir: PathBuf,

    /// Do not convert anything, just print the title info
    #[arg(long)]
    dry_run: bool,

    /// Set game title
    #[arg(long, value_name = "TITLE")]
    game_title: Option<String>,

    /// Whether to trim off unused space from the ISO image (folders have none);
    /// passing no --trim flag at all is equivalent to "from-end"
    #[arg(
        verbatim_doc_comment,
        long,
        value_enum,
        require_equals = true,
        num_args = 0..=1,
        default_missing_value = "from-end"
    )]
    trim: Option<TrimModeArg>,

    /// Number of worker threads to use
    #[arg(long, short = 'j', value_name = "N", default_value_t = 1)]
    num_threads: usize,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Default, ValueEnum)]
enum TrimModeArg {
    /// (default) Trim unallocated space from the end
    #[default]
    FromEnd,

    /// Trim nothing
    None,
    // TODO
    // FullRebuild,
}

impl TrimModeArg {
    fn core(self) -> TrimMode {
        match self {
            TrimModeArg::FromEnd => TrimMode::FromEnd,
            TrimModeArg::None => TrimMode::None,
        }
    }
}

fn main() -> Result<(), Error> {
    let args = Cli::parse();

    if args.num_threads == 1 {
        eprintln!(
            "The default number of threads was changed to 1 because of the problems witn Windows and/or hard drives."
        );
        eprintln!(
            "If you don't use Windows or use and SSD, might be worth increasing it with the -j <N> flag!"
        );
    }

    let trim = args.trim.unwrap_or_default().core();

    let is_folder = args.source.is_dir();
    println!(
        "extracting {} metadata",
        if is_folder { "folder" } else { "ISO" }
    );

    let info = convert::inspect_with_trim(&args.source, trim).with_context(|| {
        if is_folder {
            "error reading source folder"
        } else {
            "error reading source ISO"
        }
    })?;

    println!("Title ID: {}", info.title_id_string());
    println!(
        "    Name: {}",
        info.title_name.as_deref().unwrap_or("(unknown)")
    );
    println!(
        "    Type: {}",
        match info.content_type {
            ContentType::GamesOnDemand => "Games on Demand",
            ContentType::XboxOriginal => "Xbox Original",
        }
    );
    match info.kind {
        SourceKind::Iso => println!(
            "    Size: {} -> {}",
            format_size(info.source_size),
            format_size(info.data_size)
        ),
        SourceKind::Folder => println!("    Size: {}", format_size(info.data_size)),
    }

    if args.dry_run {
        return Ok(());
    }

    println!("clearing data directory");

    let options = ConvertOptions {
        source: args.source,
        dest_dir: args.dest_dir,
        game_title: args.game_title,
        trim,
        num_threads: args.num_threads,
    };

    let printer = Mutex::new(CliPrinter::default());

    convert::convert(&options, move |progress| {
        if let Ok(mut printer) = printer.lock() {
            printer.report(progress);
        }
    })?;

    println!("\ndone");

    Ok(())
}

#[derive(Default)]
struct CliPrinter {
    stage: Option<Stage>,
    last_percent: u8,
}

impl CliPrinter {
    fn report(&mut self, progress: ConversionProgress) {
        if self.stage != Some(progress.stage) {
            if self.stage.is_some() {
                println!();
            }
            self.stage = Some(progress.stage);
            self.last_percent = u8::MAX;
        }

        let percent = (progress.fraction() * 100.0).round() as u8;
        if percent != self.last_percent {
            self.last_percent = percent;
            print!("\r{}: {percent:3}%", progress.stage.label());
            let _ = std::io::stdout().flush();
        }
    }
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}
