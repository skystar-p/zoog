#[path = "../output_file.rs"]
mod output_file;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufReader, BufWriter};
use std::ops::BitOrAssign;
use std::path::PathBuf;

use clap::Parser;
use output_file::OutputFile;
use thiserror::Error;
use zoog::comment_rewriter::{CommentHeaderRewrite, CommentRewriterAction, CommentRewriterConfig};
use zoog::header_rewriter::{rewrite_stream, SubmitResult};
use zoog::opus::{parse_comment, validate_comment_field_name, CommentList, DiscreteCommentList};
use zoog::{escaping, Error};

#[derive(Debug, Error)]
enum AppError {
    #[error("{0}")]
    LibraryError(#[from] Error),

    #[error("Silent exit because error was already printed")]
    SilentExit,
}

fn main() {
    if let Err(e) = main_impl() {
        match e {
            AppError::LibraryError(e) => eprintln!("Aborted due to error: {}", e),
            AppError::SilentExit => {}
        }
        std::process::exit(1);
    }
}

#[derive(Debug, Parser)]
#[clap(author, version, about = "List or edit comments in Ogg Opus files.")]
struct Cli {
    #[clap(short, long, action, conflicts_with = "write", conflicts_with = "append")]
    /// List comments in the Ogg Opus file
    list: bool,

    #[clap(short, long, action, conflicts_with = "write")]
    /// Append comments in the Ogg Opus file
    append: bool,

    #[clap(short, long, action)]
    /// Replace comments in the Ogg Opus file
    write: bool,

    #[clap(short = 't', long = "tag", value_name = "NAME")]
    /// Specify a tag
    tags: Vec<String>,

    #[clap(short = 'd', long = "rm", value_name = "NAME[=VALUE]", conflicts_with = "write")]
    /// Specify a tag name or name-value mapping to be deleted
    delete: Vec<String>,

    #[clap(short, long, action)]
    /// Use escapes \n, \r, \0 and \\ for tag-value input and output
    escapes: bool,

    /// Input file
    input_file: PathBuf,

    /// Output file
    output_file: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug)]
enum OperationMode {
    List,
    Append,
    Replace,
}

/// Match type for Opus comment values
#[derive(Clone, Debug)]
enum ValueMatch {
    All,
    ContainedIn(HashSet<String>),
}

impl ValueMatch {
    pub fn singleton(value: String) -> ValueMatch { ValueMatch::ContainedIn(HashSet::from([value])) }

    pub fn matches(&self, value: &str) -> bool {
        match self {
            ValueMatch::All => true,
            ValueMatch::ContainedIn(values) => values.contains(value),
        }
    }
}

impl Default for ValueMatch {
    fn default() -> ValueMatch { ValueMatch::ContainedIn(HashSet::new()) }
}

impl BitOrAssign for ValueMatch {
    fn bitor_assign(&mut self, rhs: ValueMatch) {
        let mut old_lhs = ValueMatch::All;
        std::mem::swap(self, &mut old_lhs);
        let new_value = match (old_lhs, rhs) {
            (ValueMatch::ContainedIn(mut lhs), ValueMatch::ContainedIn(mut rhs)) => {
                // Preserve the larger set when merging
                if rhs.len() > lhs.len() {
                    std::mem::swap(&mut rhs, &mut lhs)
                }
                lhs.extend(rhs.into_iter());
                ValueMatch::ContainedIn(lhs)
            }
            _ => ValueMatch::All,
        };
        *self = new_value;
    }
}

#[derive(Clone, Debug, Default)]
struct KeyValueMatch {
    keys: HashMap<String, ValueMatch>,
}

impl KeyValueMatch {
    pub fn add(&mut self, key: String, value: ValueMatch) { *self.keys.entry(key).or_default() |= value; }

    pub fn matches(&self, key: &str, value: &str) -> bool {
        match self.keys.get(key) {
            None => false,
            Some(value_match) => value_match.matches(value),
        }
    }
}

fn parse_new_comment_args<S, I>(comments: I, escaped: bool) -> Result<DiscreteCommentList, Error>
where
    S: AsRef<str>,
    I: IntoIterator<Item = S>,
{
    let comments = comments.into_iter();
    let mut result = DiscreteCommentList::with_capacity(comments.size_hint().0);
    for comment in comments {
        let comment = comment.as_ref();
        let (key, value) = parse_comment(comment)?;
        let value = if escaped { escaping::unescape_str(value)? } else { Cow::from(value) };
        result.append(&key, &value)?;
    }
    Ok(result)
}

fn parse_delete_comment_args<S, I>(patterns: I, escaped: bool) -> Result<KeyValueMatch, Error>
where
    S: AsRef<str>,
    I: IntoIterator<Item = S>,
{
    let patterns = patterns.into_iter();
    let mut result = KeyValueMatch::default();
    for pattern_string in patterns {
        let pattern_string = pattern_string.as_ref();
        let (key, value) = match parse_comment(pattern_string) {
            Ok((key, value)) => {
                let value = if escaped { escaping::unescape_str(value)? } else { Cow::from(value) };
                (key, Some(value))
            }
            Err(_) => match validate_comment_field_name(pattern_string) {
                Ok(()) => (pattern_string, None),
                Err(e) => return Err(e),
            },
        };
        let rhs = match value {
            None => ValueMatch::All,
            Some(value) => ValueMatch::singleton(value.to_string()),
        };
        result.add(key.to_string(), rhs);
    }
    Ok(result)
}

fn main_impl() -> Result<(), AppError> {
    let cli = Cli::parse_from(wild::args_os());
    let operation_mode = match (cli.list, cli.append, cli.write) {
        (_, false, false) => OperationMode::List,
        (false, true, false) => OperationMode::Append,
        (false, false, true) => OperationMode::Replace,
        _ => {
            eprintln!("Invalid combination of modes passed");
            return Err(AppError::SilentExit);
        }
    };

    let escape = cli.escapes;
    let append = parse_new_comment_args(cli.tags, escape)?;
    let delete_tags = parse_delete_comment_args(cli.delete, escape)?;
    println!("Operating in mode: {:?}", operation_mode);
    println!("tags={:?}", append);
    println!("delete_tags={:?}", delete_tags);

    let action = match operation_mode {
        OperationMode::List => CommentRewriterAction::NoChange,
        OperationMode::Append => {
            let retain: Box<dyn Fn(&str, &str) -> bool> = Box::new(|k, v| !delete_tags.matches(k, v));
            CommentRewriterAction::Modify { retain, append }
        }
        OperationMode::Replace => CommentRewriterAction::Replace(append),
    };

    let rewriter_config = CommentRewriterConfig { action };
    let input_path = cli.input_file;
    let input_file = File::open(&input_path).map_err(|e| Error::FileOpenError(input_path.to_path_buf(), e))?;
    let mut input_file = BufReader::new(input_file);

    let mut output_file = match operation_mode {
        OperationMode::List => OutputFile::new_sink(),
        OperationMode::Append | OperationMode::Replace => {
            let output_path = cli.output_file.unwrap_or(input_path.to_path_buf());
            OutputFile::new_target(&output_path)?
        }
    };

    let rewrite_result = {
        let output_file = output_file.as_write();
        let mut output_file = BufWriter::new(output_file);
        let rewrite = CommentHeaderRewrite::new(rewriter_config);
        let abort_on_unchanged = true;
        rewrite_stream(rewrite, &mut input_file, &mut output_file, abort_on_unchanged)
    };
    drop(input_file);

    match rewrite_result {
        Err(e) => {
            eprintln!("Failure during processing of {}.", input_path.display());
            return Err(e.into());
        }
        Ok(SubmitResult::Good) => {
            // We finished processing the file but never got the headers
            eprintln!("File {} appeared to be oddly truncated. Doing nothing.", input_path.display());
        }
        Ok(SubmitResult::HeadersUnchanged(comments)) => {
            if let OperationMode::List = operation_mode {
                comments.write_as_text(io::stdout(), escape).map_err(Error::ConsoleIoError)?;
            }
        }
        Ok(SubmitResult::HeadersChanged { .. }) => {
            output_file.commit()?;
        }
    };
    Ok(())
}
