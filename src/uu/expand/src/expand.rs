// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

// spell-checker:ignore (ToDO) ctype cwidth iflag nbytes nspaces nums tspaces uflag Preprocess

use clap::{Arg, ArgAction, ArgMatches, Command};
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write, stdin, stdout};
use std::num::IntErrorKind;
use std::path::Path;
use std::str::from_utf8;
use thiserror::Error;
use unicode_width::UnicodeWidthChar;
use uucore::display::Quotable;
use uucore::error::{FromIo, UError, UResult, set_exit_code};
use uucore::translate;
use uucore::{format_usage, show_error};

pub mod options {
    pub static TABS: &str = "tabs";
    pub static INITIAL: &str = "initial";
    pub static NO_UTF8: &str = "no-utf8";
    pub static FILES: &str = "FILES";
}

static LONG_HELP: &str = "";

static DEFAULT_TABSTOP: usize = 8;

/// The mode to use when replacing tabs beyond the last one specified in
/// the `--tabs` argument.
#[derive(PartialEq)]
enum RemainingMode {
    None,
    Slash,
    Plus,
}

/// Decide whether the character is either a space or a comma.
///
/// # Examples
///
/// ```rust,ignore
/// assert!(is_space_or_comma(' '))
/// assert!(is_space_or_comma(','))
/// assert!(!is_space_or_comma('a'))
/// ```
fn is_space_or_comma(c: char) -> bool {
    c == ' ' || c == ','
}

/// Decide whether the character is either a digit or a comma.
fn is_digit_or_comma(c: char) -> bool {
    c.is_ascii_digit() || c == ','
}

/// Errors that can occur when parsing a `--tabs` argument.
#[derive(Debug, Error)]
enum ParseError {
    #[error("{}", translate!("expand-error-invalid-character", "char" => .0.quote()))]
    InvalidCharacter(String),
    #[error("{}", translate!("expand-error-specifier-not-at-start", "specifier" => .0.quote(), "number" => .1.quote()))]
    SpecifierNotAtStartOfNumber(String, String),
    #[error("{}", translate!("expand-error-specifier-only-allowed-with-last", "specifier" => .0.quote()))]
    SpecifierOnlyAllowedWithLastValue(String),
    #[error("{}", translate!("expand-error-tab-size-cannot-be-zero"))]
    TabSizeCannotBeZero,
    #[error("{}", translate!("expand-error-tab-size-too-large", "size" => .0.quote()))]
    TabSizeTooLarge(String),
    #[error("{}", translate!("expand-error-tab-sizes-must-be-ascending"))]
    TabSizesMustBeAscending,
}

impl UError for ParseError {}

/// Parse a list of tabstops from a `--tabs` argument.
///
/// This function returns both the vector of numbers appearing in the
/// comma- or space-separated list, and also an optional mode, specified
/// by either a "/" or a "+" character appearing before the final number
/// in the list. This mode defines the strategy to use for computing the
/// number of spaces to use for columns beyond the end of the tab stop
/// list specified here.
fn tabstops_parse(s: &str) -> Result<(RemainingMode, Vec<usize>), ParseError> {
    // Leading commas and spaces are ignored.
    let s = s.trim_start_matches(is_space_or_comma);

    // If there were only commas and spaces in the string, just use the
    // default tabstops.
    if s.is_empty() {
        return Ok((RemainingMode::None, vec![DEFAULT_TABSTOP]));
    }

    let mut nums = vec![];
    let mut remaining_mode = RemainingMode::None;
    let mut is_specifier_already_used = false;
    for word in s.split(is_space_or_comma) {
        let bytes = word.as_bytes();
        for i in 0..bytes.len() {
            match bytes[i] {
                b'+' => remaining_mode = RemainingMode::Plus,
                b'/' => remaining_mode = RemainingMode::Slash,
                _ => {
                    // Parse a number from the byte sequence.
                    let s = from_utf8(&bytes[i..]).unwrap();
                    match s.parse::<usize>() {
                        Ok(num) => {
                            // Tab size must be positive.
                            if num == 0 {
                                return Err(ParseError::TabSizeCannotBeZero);
                            }

                            // Tab sizes must be ascending.
                            if let Some(last_stop) = nums.last() {
                                if *last_stop >= num {
                                    return Err(ParseError::TabSizesMustBeAscending);
                                }
                            }

                            if is_specifier_already_used {
                                let specifier = if remaining_mode == RemainingMode::Slash {
                                    "/".to_string()
                                } else {
                                    "+".to_string()
                                };
                                return Err(ParseError::SpecifierOnlyAllowedWithLastValue(
                                    specifier,
                                ));
                            } else if remaining_mode != RemainingMode::None {
                                is_specifier_already_used = true;
                            }

                            // Append this tab stop to the list of all tabstops.
                            nums.push(num);
                            break;
                        }
                        Err(e) => {
                            if *e.kind() == IntErrorKind::PosOverflow {
                                return Err(ParseError::TabSizeTooLarge(s.to_string()));
                            }

                            let s = s.trim_start_matches(char::is_numeric);
                            return if s.starts_with('/') || s.starts_with('+') {
                                Err(ParseError::SpecifierNotAtStartOfNumber(
                                    s[0..1].to_string(),
                                    s.to_string(),
                                ))
                            } else {
                                Err(ParseError::InvalidCharacter(s.to_string()))
                            };
                        }
                    }
                }
            }
        }
    }
    // If no numbers could be parsed (for example, if `s` were "+,+,+"),
    // then just use the default tabstops.
    if nums.is_empty() {
        nums = vec![DEFAULT_TABSTOP];
    }

    if nums.len() < 2 {
        remaining_mode = RemainingMode::None;
    }
    Ok((remaining_mode, nums))
}

struct Options {
    files: Vec<String>,
    tabstops: Vec<usize>,
    tspaces: String,
    iflag: bool,
    uflag: bool,

    /// Strategy for expanding tabs for columns beyond those specified
    /// in `tabstops`.
    remaining_mode: RemainingMode,
}

impl Options {
    fn new(matches: &ArgMatches) -> Result<Self, ParseError> {
        let (remaining_mode, tabstops) = match matches.get_many::<String>(options::TABS) {
            Some(s) => tabstops_parse(&s.map(|s| s.as_str()).collect::<Vec<_>>().join(","))?,
            None => (RemainingMode::None, vec![DEFAULT_TABSTOP]),
        };

        let iflag = matches.get_flag(options::INITIAL);
        let uflag = !matches.get_flag(options::NO_UTF8);

        // avoid allocations when dumping out long sequences of spaces
        // by precomputing the longest string of spaces we will ever need
        let nspaces = tabstops
            .iter()
            .scan(0, |pr, &it| {
                let ret = Some(it - *pr);
                *pr = it;
                ret
            })
            .max()
            .unwrap(); // length of tabstops is guaranteed >= 1
        let tspaces = " ".repeat(nspaces);

        let files: Vec<String> = match matches.get_many::<String>(options::FILES) {
            Some(s) => s.map(|v| v.to_string()).collect(),
            None => vec!["-".to_owned()],
        };

        Ok(Self {
            files,
            tabstops,
            tspaces,
            iflag,
            uflag,
            remaining_mode,
        })
    }
}

/// Preprocess command line arguments and expand shortcuts. For example, "-7" is expanded to
/// "--tabs=7" and "-1,3" to "--tabs=1 --tabs=3".
fn expand_shortcuts(args: Vec<OsString>) -> Vec<OsString> {
    let mut processed_args = Vec::with_capacity(args.len());

    for arg in args {
        if let Some(arg) = arg.to_str() {
            if arg.starts_with('-') && arg[1..].chars().all(is_digit_or_comma) {
                arg[1..]
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .for_each(|s| processed_args.push(OsString::from(format!("--tabs={s}"))));
                continue;
            }
        }
        processed_args.push(arg);
    }

    processed_args
}

#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let matches = uu_app().try_get_matches_from(expand_shortcuts(args.collect()))?;

    expand(&Options::new(&matches)?)
}

pub fn uu_app() -> Command {
    Command::new(uucore::util_name())
        .version(uucore::crate_version!())
        .about(translate!("expand-about"))
        .after_help(LONG_HELP)
        .override_usage(format_usage(&translate!("expand-usage")))
        .infer_long_args(true)
        .args_override_self(true)
        .arg(
            Arg::new(options::INITIAL)
                .long(options::INITIAL)
                .short('i')
                .help(translate!("expand-help-initial"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::TABS)
                .long(options::TABS)
                .short('t')
                .value_name("N, LIST")
                .action(ArgAction::Append)
                .help(translate!("expand-help-tabs")),
        )
        .arg(
            Arg::new(options::NO_UTF8)
                .long(options::NO_UTF8)
                .short('U')
                .help(translate!("expand-help-no-utf8"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::FILES)
                .action(ArgAction::Append)
                .hide(true)
                .value_hint(clap::ValueHint::FilePath),
        )
}

fn open(path: &str) -> UResult<BufReader<Box<dyn Read + 'static>>> {
    let file_buf;
    if path == "-" {
        Ok(BufReader::new(Box::new(stdin()) as Box<dyn Read>))
    } else {
        file_buf = File::open(path).map_err_context(|| path.to_string())?;
        Ok(BufReader::new(Box::new(file_buf) as Box<dyn Read>))
    }
}

/// Compute the number of spaces to the next tabstop.
///
/// `tabstops` is the sequence of tabstop locations.
///
/// `col` is the index of the current cursor in the line being written.
///
/// If `remaining_mode` is [`RemainingMode::Plus`], then the last entry
/// in the `tabstops` slice is interpreted as a relative number of
/// spaces, which this function will return for every input value of
/// `col` beyond the end of the second-to-last element of `tabstops`.
fn next_tabstop(tabstops: &[usize], col: usize, remaining_mode: &RemainingMode) -> usize {
    let num_tabstops = tabstops.len();
    match remaining_mode {
        RemainingMode::Plus => match tabstops[0..num_tabstops - 1].iter().find(|&&t| t > col) {
            Some(t) => t - col,
            None => {
                let step_size = tabstops[num_tabstops - 1];
                let last_fixed_tabstop = tabstops[num_tabstops - 2];
                let characters_since_last_tabstop = col - last_fixed_tabstop;

                let steps_required = 1 + characters_since_last_tabstop / step_size;
                steps_required * step_size - characters_since_last_tabstop
            }
        },
        RemainingMode::Slash => match tabstops[0..num_tabstops - 1].iter().find(|&&t| t > col) {
            Some(t) => t - col,
            None => tabstops[num_tabstops - 1] - col % tabstops[num_tabstops - 1],
        },
        RemainingMode::None => {
            if num_tabstops == 1 {
                tabstops[0] - col % tabstops[0]
            } else {
                match tabstops.iter().find(|&&t| t > col) {
                    Some(t) => t - col,
                    None => 1,
                }
            }
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
enum CharType {
    Backspace,
    Tab,
    Other,
}

#[allow(clippy::cognitive_complexity)]
fn expand_line(
    buf: &mut Vec<u8>,
    output: &mut BufWriter<std::io::Stdout>,
    tabstops: &[usize],
    options: &Options,
) -> std::io::Result<()> {
    use self::CharType::{Backspace, Other, Tab};

    let mut col = 0;
    let mut byte = 0;
    let mut init = true;

    while byte < buf.len() {
        let (ctype, cwidth, nbytes) = if options.uflag {
            let nbytes = char::from(buf[byte]).len_utf8();

            if byte + nbytes > buf.len() {
                // don't overrun buffer because of invalid UTF-8
                (Other, 1, 1)
            } else if let Ok(t) = from_utf8(&buf[byte..byte + nbytes]) {
                match t.chars().next() {
                    Some('\t') => (Tab, 0, nbytes),
                    Some('\x08') => (Backspace, 0, nbytes),
                    Some(c) => (Other, UnicodeWidthChar::width(c).unwrap_or(0), nbytes),
                    None => {
                        // no valid char at start of t, so take 1 byte
                        (Other, 1, 1)
                    }
                }
            } else {
                (Other, 1, 1) // implicit assumption: non-UTF-8 char is 1 col wide
            }
        } else {
            (
                match buf.get(byte) {
                    // always take exactly 1 byte in strict ASCII mode
                    Some(0x09) => Tab,
                    Some(0x08) => Backspace,
                    _ => Other,
                },
                1,
                1,
            )
        };

        // figure out how many columns this char takes up
        match ctype {
            Tab => {
                // figure out how many spaces to the next tabstop
                let nts = next_tabstop(tabstops, col, &options.remaining_mode);
                col += nts;

                // now dump out either spaces if we're expanding, or a literal tab if we're not
                if init || !options.iflag {
                    if nts <= options.tspaces.len() {
                        output.write_all(&options.tspaces.as_bytes()[..nts])?;
                    } else {
                        output.write_all(" ".repeat(nts).as_bytes())?;
                    }
                } else {
                    output.write_all(&buf[byte..byte + nbytes])?;
                }
            }
            _ => {
                col = if ctype == Other {
                    col + cwidth
                } else if col > 0 {
                    col - 1
                } else {
                    0
                };

                // if we're writing anything other than a space, then we're
                // done with the line's leading spaces
                if buf[byte] != 0x20 {
                    init = false;
                }

                output.write_all(&buf[byte..byte + nbytes])?;
            }
        }

        byte += nbytes; // advance the pointer
    }

    output.flush()?;
    buf.truncate(0); // clear the buffer

    Ok(())
}

fn expand(options: &Options) -> UResult<()> {
    let mut output = BufWriter::new(stdout());
    let ts = options.tabstops.as_ref();
    let mut buf = Vec::new();

    for file in &options.files {
        if Path::new(file).is_dir() {
            show_error!(
                "{}",
                translate!("expand-error-is-directory", "file" => file)
            );
            set_exit_code(1);
            continue;
        }
        match open(file) {
            Ok(mut fh) => {
                while match fh.read_until(b'\n', &mut buf) {
                    Ok(s) => s > 0,
                    Err(_) => buf.is_empty(),
                } {
                    expand_line(&mut buf, &mut output, ts, options)
                        .map_err_context(|| translate!("expand-error-failed-to-write-output"))?;
                }
            }
            Err(e) => {
                show_error!("{e}");
                set_exit_code(1);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::is_digit_or_comma;

    use super::RemainingMode;
    use super::next_tabstop;

    #[test]
    fn test_next_tabstop_remaining_mode_none() {
        assert_eq!(next_tabstop(&[1, 5], 0, &RemainingMode::None), 1);
        assert_eq!(next_tabstop(&[1, 5], 3, &RemainingMode::None), 2);
        assert_eq!(next_tabstop(&[1, 5], 6, &RemainingMode::None), 1);
    }

    #[test]
    fn test_next_tabstop_remaining_mode_plus() {
        assert_eq!(next_tabstop(&[1, 5], 0, &RemainingMode::Plus), 1);
        assert_eq!(next_tabstop(&[1, 5], 3, &RemainingMode::Plus), 3);
        assert_eq!(next_tabstop(&[1, 5], 6, &RemainingMode::Plus), 5);
    }

    #[test]
    fn test_next_tabstop_remaining_mode_slash() {
        assert_eq!(next_tabstop(&[1, 5], 0, &RemainingMode::Slash), 1);
        assert_eq!(next_tabstop(&[1, 5], 3, &RemainingMode::Slash), 2);
        assert_eq!(next_tabstop(&[1, 5], 6, &RemainingMode::Slash), 4);
    }

    #[test]
    fn test_is_digit_or_comma() {
        assert!(is_digit_or_comma('1'));
        assert!(is_digit_or_comma(','));
        assert!(!is_digit_or_comma('a'));
    }
}
