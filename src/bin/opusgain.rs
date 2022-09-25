use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use clap::{Parser, ValueEnum};
use ogg::reading::PacketReader;
use ogg::writing::PacketWriter;
use zoog::rewriter::{OutputGainMode, RewriteResult, Rewriter, RewriterConfig, VolumeTarget};
use zoog::volume_analyzer::VolumeAnalyzer;
use zoog::{Decibels, Error, R128_LUFS, REPLAY_GAIN_LUFS};

fn main() {
    match main_impl() {
        Ok(()) => {}
        Err(e) => {
            eprintln!("Error was: {}", e);
            std::process::exit(1);
        }
    }
}

fn remove_file_verbose<P: AsRef<Path>>(path: P) {
    let path = path.as_ref();
    if let Err(e) = std::fs::remove_file(path) {
        eprintln!("Unable to delete {} due to error {}", path.to_string_lossy(), e);
    }
}

fn rename_file<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> Result<(), Error> {
    std::fs::rename(from.as_ref(), to.as_ref())
        .map_err(|e| Error::FileMove(PathBuf::from(from.as_ref()), PathBuf::from(to.as_ref()), e))
}

fn apply_volume_analysis<P: AsRef<Path>>(analyzer: &mut VolumeAnalyzer, path: P) -> Result<(), Error> {
    let input_path = path.as_ref();
    print!("Computing loudness of {}... ", input_path.to_string_lossy());
    io::stdout().flush().map_err(Error::GenericIoError)?;
    let input_file = File::open(input_path).map_err(|e| Error::FileOpenError(input_path.to_path_buf(), e))?;
    let input_file = BufReader::new(input_file);
    let mut ogg_reader = PacketReader::new(input_file);
    loop {
        match ogg_reader.read_packet() {
            Err(e) => {
                println!();
                break Err(Error::OggDecode(e));
            }
            Ok(None) => {
                analyzer.file_complete();
                println!(
                    "{:.2} LUFS (ignoring output gain)",
                    analyzer.last_track_lufs().expect("Last track volume unexpectedly missing").as_f64()
                );
                break Ok(());
            }
            Ok(Some(packet)) => analyzer.submit(packet)?,
        }
    }
}

#[derive(Debug)]
struct AlbumVolume {
    mean: Decibels,
    tracks: HashMap<PathBuf, Decibels>,
}

impl AlbumVolume {
    pub fn get_album_mean(&self) -> Decibels { self.mean }

    pub fn get_track_mean(&self, path: &Path) -> Option<Decibels> { self.tracks.get(path).cloned() }
}

fn compute_album_volume<I: IntoIterator<Item = P>, P: AsRef<Path>>(paths: I) -> Result<AlbumVolume, Error> {
    let mut analyzer = VolumeAnalyzer::default();
    let mut tracks = HashMap::new();
    for input_path in paths.into_iter() {
        apply_volume_analysis(&mut analyzer, input_path.as_ref())?;
        tracks.insert(
            input_path.as_ref().to_path_buf(),
            analyzer.last_track_lufs().expect("Track volume unexpectedly missing"),
        );
    }
    let album_volume = AlbumVolume { tracks, mean: analyzer.mean_lufs() };
    Ok(album_volume)
}

fn rewrite_stream<R: Read + Seek, W: Write>(
    input: R, mut output: W, config: &RewriterConfig,
) -> Result<RewriteResult, Error> {
    let mut ogg_reader = PacketReader::new(input);
    let ogg_writer = PacketWriter::new(&mut output);
    let mut rewriter = Rewriter::new(config, ogg_writer, true);
    loop {
        match ogg_reader.read_packet() {
            Err(e) => break Err(Error::OggDecode(e)),
            Ok(None) => {
                // Make sure to flush any buffered data
                break output.flush().map(|_| RewriteResult::Ready).map_err(Error::WriteError);
            }
            Ok(Some(packet)) => {
                let submit_result = rewriter.submit(packet);
                match submit_result {
                    Ok(RewriteResult::Ready) => {}
                    _ => break submit_result,
                }
            }
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Preset {
    #[clap(name = "rg")]
    ReplayGain,
    #[clap(name = "r128")]
    R128,
    #[clap(name = "original")]
    ZeroGain,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum OutputGainSetting {
    Auto,
    Track,
}

#[derive(Debug, Parser)]
#[clap(author, version, about)]
struct Cli {
    #[clap(short, long, action)]
    /// Enable album mode
    album: bool,

    #[clap(arg_enum, value_parser, short, long, default_value_t = Preset::ReplayGain)]
    /// Normalizes to loudness used by ReplayGain (rg), EBU R 128 (r128) or
    /// original (none)
    preset: Preset,

    #[clap(arg_enum, value_parser, short, long, default_value_t = OutputGainSetting::Auto)]
    /// When "auto" is specified, each track's output gain is chosen to be
    /// per-track or per-album dependent on whether album mode is enabled.
    /// When "track" is specified, each file's output gain will be
    /// track-specific, even in album mode.
    output_gain_mode: OutputGainSetting,

    #[clap(value_parser, required(true))]
    /// The Opus files to process
    input_files: Vec<PathBuf>,

    #[clap(short, long, action)]
    /// Display output without performing any file modification.
    display_only: bool,
}

#[derive(Debug)]
enum OutputFile {
    Temp(tempfile::NamedTempFile),
    Sink(io::Sink),
}

impl OutputFile {
    fn as_write(&mut self) -> &mut dyn Write {
        match self {
            OutputFile::Temp(ref mut temp) => temp,
            OutputFile::Sink(ref mut sink) => sink,
        }
    }
}

fn main_impl() -> Result<(), Error> {
    let cli = Cli::parse();
    let album_mode = cli.album;

    let output_gain_mode = match cli.output_gain_mode {
        OutputGainSetting::Auto => match album_mode {
            true => OutputGainMode::Album,
            false => OutputGainMode::Track,
        },
        OutputGainSetting::Track => OutputGainMode::Track,
    };
    let volume_target = match cli.preset {
        Preset::ReplayGain => VolumeTarget::LUFS(REPLAY_GAIN_LUFS),
        Preset::R128 => VolumeTarget::LUFS(R128_LUFS),
        Preset::ZeroGain => VolumeTarget::ZeroGain,
    };

    let mut num_processed: usize = 0;
    let mut num_already_normalized: usize = 0;

    let display_only = cli.display_only;
    if display_only {
        println!("Display-only mode is enabled so no files will actually be modified.\n");
    }

    let input_files = cli.input_files;
    let album_volume = if album_mode { Some(compute_album_volume(&input_files)?) } else { None };

    for input_path in input_files {
        println!(
            "Processing file {} with target loudness of {}...",
            &input_path.to_string_lossy(),
            volume_target.to_friendly_string()
        );
        let track_volume = match &album_volume {
            None => {
                let mut analyzer = VolumeAnalyzer::default();
                apply_volume_analysis(&mut analyzer, &input_path)?;
                analyzer.last_track_lufs().expect("Last track volume unexpectedly missing")
            }
            Some(album_volume) => {
                album_volume.get_track_mean(&input_path).expect("Could not find previously computed track volume")
            }
        };
        let rewriter_config = RewriterConfig::new(
            volume_target,
            output_gain_mode,
            track_volume,
            album_volume.as_ref().map(|a| a.get_album_mean()),
        );

        let input_dir = input_path.parent().expect("Unable to find parent folder of input file");
        let input_base = input_path.file_name().expect("Unable to find name of input file");
        let input_file = File::open(&input_path).map_err(|e| Error::FileOpenError(input_path.to_path_buf(), e))?;
        let mut input_file = BufReader::new(input_file);

        let mut output_file = if display_only {
            OutputFile::Sink(io::sink())
        } else {
            let temp = tempfile::Builder::new()
                .prefix(input_base)
                .suffix("zoog")
                .tempfile_in(input_dir)
                .map_err(Error::TempFileOpenError)?;
            OutputFile::Temp(temp)
        };
        let rewrite_result = {
            let output_file = output_file.as_write();
            let mut output_file = BufWriter::new(output_file);
            rewrite_stream(&mut input_file, &mut output_file, &rewriter_config)
        };
        num_processed += 1;

        match rewrite_result {
            Err(e) => {
                println!("Failure during processing of {:#?}.", input_path);
                return Err(e);
            }
            Ok(RewriteResult::Ready) => match output_file {
                OutputFile::Temp(output_file) => {
                    let mut backup_path = input_path.clone();
                    backup_path.set_extension("zoog-orig");
                    rename_file(&input_path, &backup_path)?;
                    output_file
                        .persist_noclobber(&input_path)
                        .map_err(Error::PersistError)
                        .and_then(|f| f.sync_all().map_err(Error::WriteError))?;
                    remove_file_verbose(&backup_path);
                }
                OutputFile::Sink(_) => {}
            },
            Ok(RewriteResult::AlreadyNormalized) => {
                println!("All gains are already correct so doing nothing.");
                num_already_normalized += 1;
            }
        }
        println!();
    }
    println!("Processing complete.");
    println!("Total files processed: {}", num_processed);
    println!("Files processed but already normalized: {}", num_already_normalized);
    Ok(())
}