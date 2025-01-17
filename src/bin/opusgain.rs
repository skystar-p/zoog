#![warn(clippy::pedantic)]
#![allow(clippy::uninlined_format_args)]

#[path = "../console_output.rs"]
mod console_output;

#[path = "../ctrlc_handling.rs"]
mod ctrlc_handling;

#[path = "../output_file.rs"]
mod output_file;

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use clap::{Parser, ValueEnum};
use console_output::{ConsoleOutput, Delayed as DelayedConsoleOutput, Standard};
use ctrlc_handling::CtrlCChecker;
use ogg::reading::PacketReader;
use output_file::OutputFile;
use parking_lot::Mutex;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use rayon::ThreadPoolBuilder;
use thiserror::Error;
use zoog::header_rewriter::{rewrite_stream_with_interrupt, SubmitResult};
use zoog::opus::{VolumeAnalyzer, TAG_ALBUM_GAIN, TAG_TRACK_GAIN};
use zoog::volume_rewrite::{
    GainsSummary, OpusGains, OutputGainMode, VolumeHeaderRewrite, VolumeRewriterConfig, VolumeTarget,
};
use zoog::{Decibels, Error, R128_LUFS, REPLAY_GAIN_LUFS};

#[derive(Debug, Error)]
enum AppError {
    #[error("{0}")]
    Library(#[from] Error),

    #[error("Unable to register Ctrl-C handler: `{0}`")]
    CtrlCRegistration(#[from] ctrlc_handling::CtrlCRegistrationError),
}

fn main() {
    match main_impl() {
        Ok(()) => {}
        Err(e) => {
            eprintln!("Aborted due to error: {}", e);
            std::process::exit(1);
        }
    }
}

fn check_running(checker: &CtrlCChecker) -> Result<(), Error> {
    if checker.is_running() {
        Ok(())
    } else {
        Err(Error::Interrupted)
    }
}

fn apply_volume_analysis<P, C>(
    analyzer: &mut VolumeAnalyzer, path: P, console_output: &C, report_error: bool, interrupt_checker: &CtrlCChecker,
) -> Result<(), Error>
where
    P: AsRef<Path>,
    C: ConsoleOutput,
{
    let mut body = || -> Result<(), Error> {
        let input_path = path.as_ref();
        let input_file = File::open(input_path).map_err(|e| Error::FileOpenError(input_path.to_path_buf(), e))?;
        let input_file = BufReader::new(input_file);
        let mut ogg_reader = PacketReader::new(input_file);
        loop {
            check_running(interrupt_checker)?;
            match ogg_reader.read_packet() {
                Err(e) => break Err(Error::OggDecode(e)),
                Ok(None) => {
                    analyzer.file_complete();
                    writeln!(
                        console_output.out(),
                        "Computed loudness of {} as {:.2} LUFS (ignoring output gain)",
                        input_path.display(),
                        analyzer.last_track_lufs().expect("Last track volume unexpectedly missing").as_f64()
                    )
                    .map_err(Error::ConsoleIoError)?;
                    break Ok(());
                }
                Ok(Some(packet)) => analyzer.submit(packet)?,
            }
        }
    };
    let result = body();
    if report_error {
        if let Err(ref e) = result {
            writeln!(console_output.err(), "Failed to analyze volume of {}: {}", path.as_ref().display(), e)
                .map_err(Error::ConsoleIoError)?;
        }
    }
    result
}

fn print_gains<C: ConsoleOutput>(gains: &OpusGains, console: &C) -> Result<(), Error> {
    let do_io = || {
        writeln!(console.out(), "\tOutput Gain: {}", gains.output)?;
        if let Some(gain) = gains.track_r128 {
            writeln!(console.out(), "\t{}: {}", TAG_TRACK_GAIN, gain)?;
        }
        if let Some(gain) = gains.album_r128 {
            writeln!(console.out(), "\t{}: {}", TAG_ALBUM_GAIN, gain)?;
        }
        Ok(())
    };
    do_io().map_err(Error::ConsoleIoError)
}

#[derive(Debug)]
struct AlbumVolume {
    mean: Decibels,
    tracks: HashMap<PathBuf, Decibels>,
}

impl AlbumVolume {
    pub fn get_album_mean(&self) -> Decibels { self.mean }

    pub fn get_track_mean(&self, path: &Path) -> Option<Decibels> { self.tracks.get(path).copied() }
}

fn compute_album_volume<I, P, C>(
    paths: I, console_output: &C, interrupt_checker: &CtrlCChecker,
) -> Result<AlbumVolume, Error>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path> + Sync,
    C: ConsoleOutput + Sync,
{
    let paths: Vec<_> = paths.into_iter().enumerate().collect();
    let tracks = Mutex::new(HashMap::new());

    // This is a BTreeMap so we process the analyzers in the supplied order
    let analyzers = Mutex::new(BTreeMap::new());

    paths.into_par_iter().panic_fuse().try_for_each(|(idx, input_path)| -> Result<(), Error> {
        let mut analyzer = VolumeAnalyzer::default();
        apply_volume_analysis(
            &mut analyzer,
            input_path.as_ref(),
            &DelayedConsoleOutput::new(console_output),
            true,
            interrupt_checker,
        )?;
        tracks.lock().insert(
            input_path.as_ref().to_path_buf(),
            analyzer.last_track_lufs().expect("Track volume unexpectedly missing"),
        );
        analyzers.lock().insert(idx, analyzer);
        Ok(())
    })?;

    let analyzers = analyzers.into_inner();
    let analyzers: Vec<_> = analyzers.into_values().collect();
    let tracks = tracks.into_inner();
    let mean = VolumeAnalyzer::mean_lufs_across_multiple(analyzers.iter());
    let album_volume = AlbumVolume { mean, tracks };
    Ok(album_volume)
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Preset {
    /// ReplayGain (normalize to -18 LUFS)
    #[clap(name = "rg")]
    ReplayGain,

    /// EBU R 128 (normalize -23 LUFS)
    #[clap(name = "r128")]
    R128,

    /// original source volume (set output gain to 0dB)
    #[clap(name = "original")]
    ZeroGain,

    /// leave the output gain unchanged
    #[clap(name = "no-change")]
    NoChange,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum OutputGainSetting {
    /// Use album volume in album mode and track volume otherwise
    Auto,

    /// Use track volume even in album mode
    Track,
}

#[derive(Debug, Parser)]
#[clap(author, version, about = "Modifies Ogg Opus output gain values and R128 tags")]
struct Cli {
    #[clap(short, long, action)]
    /// Enable album mode
    album: bool,

    #[clap(value_enum, short, long, default_value_t = Preset::ReplayGain)]
    /// Choices for modifying the output gain value
    preset: Preset,

    #[clap(value_enum, short, long, default_value_t = OutputGainSetting::Auto)]
    /// When modifying the output gain to target a particular LUFS, what volume
    /// should be used
    output_gain_mode: OutputGainSetting,

    #[clap(required(true))]
    /// The Opus files to process
    input_files: Vec<PathBuf>,

    #[clap(short = 'n', long = "dry-run", action)]
    /// Display output without performing any file modification.
    dry_run: bool,

    #[clap(short='j', long, default_value_t = num_cpus::get())]
    /// Number of threads to use for processing. Default is the number of cores
    /// on the system.
    num_threads: usize,

    #[clap(short, long, action)]
    /// Clear all R128 tags from the specified files. Output gain will remain
    /// unchanged regardless of the specified preset.
    clear: bool,
}

#[allow(clippy::too_many_lines)]
fn main_impl() -> Result<(), AppError> {
    let interrupt_checker = CtrlCChecker::new()?;
    let cli = Cli::parse_from(wild::args_os());
    let album_mode = cli.album;
    let num_threads = if cli.num_threads == 0 {
        eprintln!("The number of thread specified must be greater than 0.");
        Err(Error::InvalidThreadCount)
    } else {
        let num_cores = num_cpus::get();
        let rounded = std::cmp::min(cli.num_threads, num_cores);
        if rounded != cli.num_threads {
            eprintln!("Rounding down number of threads from {} to {}.", cli.num_threads, num_cores);
        }
        Ok(rounded)
    }?;
    ThreadPoolBuilder::new().num_threads(num_threads).build_global().expect("Failed to initialize thread pool");

    let output_gain_mode = match cli.output_gain_mode {
        OutputGainSetting::Auto => {
            if album_mode {
                OutputGainMode::Album
            } else {
                OutputGainMode::Track
            }
        }
        OutputGainSetting::Track => OutputGainMode::Track,
    };
    let volume_target = match cli.preset {
        Preset::ReplayGain => VolumeTarget::LUFS(REPLAY_GAIN_LUFS),
        Preset::R128 => VolumeTarget::LUFS(R128_LUFS),
        Preset::ZeroGain => VolumeTarget::ZeroGain,
        Preset::NoChange => VolumeTarget::NoChange,
    };

    let dry_run = cli.dry_run;
    let clear = cli.clear;
    let (album_mode, volume_target) = if clear {
        // We do not compute album loudness or change output gain when clearing tags
        (false, VolumeTarget::NoChange)
    } else {
        (album_mode, volume_target)
    };

    let num_processed = AtomicUsize::new(0);
    let num_already_normalized = AtomicUsize::new(0);

    if dry_run {
        println!("Display-only mode is enabled so no files will actually be modified.\n");
    }

    let console_output = Standard::default();
    let input_files = cli.input_files;
    let album_volume =
        if album_mode { Some(compute_album_volume(&input_files, &console_output, &interrupt_checker)?) } else { None };

    // Prevent us from rewriting more than one file at once. This is to stop us
    // consuming too much disk space or leaving lots of temporary files around
    // if we encounter an error.
    let rewrite_mutex = Mutex::new(());

    input_files.into_par_iter().panic_fuse().try_for_each(|input_path| -> Result<(), AppError> {
        let console = &DelayedConsoleOutput::new(&console_output);
        let body = || -> Result<(), AppError> {
            writeln!(
                console.out(),
                "Processing file {} with target loudness of {}...",
                &input_path.display(),
                volume_target.to_friendly_string()
            )
            .map_err(Error::ConsoleIoError)?;
            let track_volume = if clear {
                None
            } else {
                Some(match &album_volume {
                    None => {
                        let mut analyzer = VolumeAnalyzer::default();
                        apply_volume_analysis(&mut analyzer, &input_path, console, false, &interrupt_checker)?;
                        analyzer.last_track_lufs().expect("Last track volume unexpectedly missing")
                    }
                    Some(album_volume) => album_volume
                        .get_track_mean(&input_path)
                        .expect("Could not find previously computed track volume"),
                })
            };
            let rewriter_config = VolumeRewriterConfig {
                output_gain: volume_target,
                output_gain_mode,
                track_volume,
                album_volume: album_volume.as_ref().map(AlbumVolume::get_album_mean),
            };

            let input_file = File::open(&input_path).map_err(|e| Error::FileOpenError(input_path.clone(), e))?;
            let mut input_file = BufReader::new(input_file);

            {
                let rewrite_guard = rewrite_mutex.lock();
                check_running(&interrupt_checker)?;
                let mut output_file = OutputFile::new_target_or_discard(&input_path, dry_run)?;
                let rewrite_result = {
                    let mut output_file = BufWriter::new(&mut output_file);
                    let rewrite = VolumeHeaderRewrite::new(rewriter_config);
                    let summarize = GainsSummary::default();
                    let abort_on_unchanged = true;
                    rewrite_stream_with_interrupt(
                        rewrite,
                        summarize,
                        &mut input_file,
                        &mut output_file,
                        abort_on_unchanged,
                        &interrupt_checker,
                    )
                };
                drop(input_file); // Important for Windows
                num_processed.fetch_add(1, Ordering::Relaxed);

                match rewrite_result {
                    Err(e) => {
                        writeln!(console.err(), "Failure during processing of {}.", input_path.display())
                            .map_err(Error::ConsoleIoError)?;
                        return Err(e.into());
                    }
                    Ok(SubmitResult::Good) => {
                        // Either we should already be normalized or get back a result which
                        // indicated we changed the gains in the input file. If we get neither
                        // then something weird happened.
                        writeln!(
                            console.err(),
                            "File {} appeared to be oddly truncated. Doing nothing.",
                            input_path.display(),
                        )
                        .map_err(Error::ConsoleIoError)?;
                    }
                    Ok(SubmitResult::HeadersChanged { from: old_gains, to: new_gains }) => {
                        output_file.commit()?;
                        writeln!(console.out(), "Old gain values:").map_err(Error::ConsoleIoError)?;
                        print_gains(&old_gains, console)?;
                        writeln!(console.out(), "New gain values:").map_err(Error::ConsoleIoError)?;
                        print_gains(&new_gains, console)?;
                    }
                    Ok(SubmitResult::HeadersUnchanged(gains)) => {
                        writeln!(console.out(), "All gains are already correct so doing nothing. Existing gains were:")
                            .map_err(Error::ConsoleIoError)?;
                        print_gains(&gains, console)?;
                        num_already_normalized.fetch_add(1, Ordering::Relaxed);
                    }
                }
                drop(rewrite_guard);
            }
            Ok(())
        };
        let result = body();
        if let Err(ref e) = result {
            writeln!(console.err(), "Failed to rewrite {}: {}", input_path.display(), e)
                .map_err(Error::ConsoleIoError)?;
        }
        writeln!(console.out()).map_err(Error::ConsoleIoError)?;
        result
    })?;

    let num_processed = num_processed.into_inner();
    let num_already_normalized = num_already_normalized.into_inner();
    println!("Processing complete.");
    println!("Total files processed: {}", num_processed);
    println!("Files processed but already normalized: {}", num_already_normalized);
    Ok(())
}
