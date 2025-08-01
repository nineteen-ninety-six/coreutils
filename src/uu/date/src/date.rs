// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

// spell-checker:ignore strtime ; (format) DATEFILE MMDDhhmm ; (vars) datetime datetimes

use clap::{Arg, ArgAction, Command};
use jiff::fmt::strtime;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp, Zoned};
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "redox")))]
use libc::{CLOCK_REALTIME, clock_settime, timespec};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use uucore::error::FromIo;
use uucore::error::{UResult, USimpleError};
use uucore::translate;
use uucore::{format_usage, show};
#[cfg(windows)]
use windows_sys::Win32::{Foundation::SYSTEMTIME, System::SystemInformation::SetSystemTime};

use uucore::parser::shortcut_value_parser::ShortcutValueParser;

// Options
const DATE: &str = "date";
const HOURS: &str = "hours";
const MINUTES: &str = "minutes";
const SECONDS: &str = "seconds";
const NS: &str = "ns";

const OPT_DATE: &str = "date";
const OPT_FORMAT: &str = "format";
const OPT_FILE: &str = "file";
const OPT_DEBUG: &str = "debug";
const OPT_ISO_8601: &str = "iso-8601";
const OPT_RFC_EMAIL: &str = "rfc-email";
const OPT_RFC_3339: &str = "rfc-3339";
const OPT_SET: &str = "set";
const OPT_REFERENCE: &str = "reference";
const OPT_UNIVERSAL: &str = "universal";
const OPT_UNIVERSAL_2: &str = "utc";

/// Settings for this program, parsed from the command line
struct Settings {
    utc: bool,
    format: Format,
    date_source: DateSource,
    set_to: Option<Zoned>,
}

/// Various ways of displaying the date
enum Format {
    Iso8601(Iso8601Format),
    Rfc5322,
    Rfc3339(Rfc3339Format),
    Custom(String),
    Default,
}

/// Various places that dates can come from
enum DateSource {
    Now,
    Custom(String),
    File(PathBuf),
    Stdin,
    Human(SignedDuration),
}

enum Iso8601Format {
    Date,
    Hours,
    Minutes,
    Seconds,
    Ns,
}

impl From<&str> for Iso8601Format {
    fn from(s: &str) -> Self {
        match s {
            HOURS => Self::Hours,
            MINUTES => Self::Minutes,
            SECONDS => Self::Seconds,
            NS => Self::Ns,
            DATE => Self::Date,
            // Note: This is caught by clap via `possible_values`
            _ => unreachable!(),
        }
    }
}

enum Rfc3339Format {
    Date,
    Seconds,
    Ns,
}

impl From<&str> for Rfc3339Format {
    fn from(s: &str) -> Self {
        match s {
            DATE => Self::Date,
            SECONDS => Self::Seconds,
            NS => Self::Ns,
            // Should be caught by clap
            _ => panic!("Invalid format: {s}"),
        }
    }
}

#[uucore::main]
#[allow(clippy::cognitive_complexity)]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let matches = uu_app().try_get_matches_from(args)?;

    let format = if let Some(form) = matches.get_one::<String>(OPT_FORMAT) {
        if !form.starts_with('+') {
            return Err(USimpleError::new(
                1,
                translate!("date-error-invalid-date", "date" => form),
            ));
        }
        let form = form[1..].to_string();
        Format::Custom(form)
    } else if let Some(fmt) = matches
        .get_many::<String>(OPT_ISO_8601)
        .map(|mut iter| iter.next().unwrap_or(&DATE.to_string()).as_str().into())
    {
        Format::Iso8601(fmt)
    } else if matches.get_flag(OPT_RFC_EMAIL) {
        Format::Rfc5322
    } else if let Some(fmt) = matches
        .get_one::<String>(OPT_RFC_3339)
        .map(|s| s.as_str().into())
    {
        Format::Rfc3339(fmt)
    } else {
        Format::Default
    };

    let date_source = if let Some(date) = matches.get_one::<String>(OPT_DATE) {
        if let Ok(duration) = parse_offset(date.as_str()) {
            DateSource::Human(duration)
        } else {
            DateSource::Custom(date.into())
        }
    } else if let Some(file) = matches.get_one::<String>(OPT_FILE) {
        match file.as_ref() {
            "-" => DateSource::Stdin,
            _ => DateSource::File(file.into()),
        }
    } else {
        DateSource::Now
    };

    let set_to = match matches.get_one::<String>(OPT_SET).map(parse_date) {
        None => None,
        Some(Err((input, _err))) => {
            return Err(USimpleError::new(
                1,
                translate!("date-error-invalid-date", "date" => input),
            ));
        }
        Some(Ok(date)) => Some(date),
    };

    let settings = Settings {
        utc: matches.get_flag(OPT_UNIVERSAL),
        format,
        date_source,
        set_to,
    };

    if let Some(date) = settings.set_to {
        // All set time functions expect UTC datetimes.
        let date = if settings.utc {
            date.with_time_zone(TimeZone::UTC)
        } else {
            date
        };

        return set_system_datetime(date);
    }

    // Get the current time, either in the local time zone or UTC.
    let now = if settings.utc {
        Timestamp::now().to_zoned(TimeZone::UTC)
    } else {
        Zoned::now()
    };

    // Iterate over all dates - whether it's a single date or a file.
    let dates: Box<dyn Iterator<Item = _>> = match settings.date_source {
        DateSource::Custom(ref input) => {
            let date = parse_date(input);
            let iter = std::iter::once(date);
            Box::new(iter)
        }
        DateSource::Human(relative_time) => {
            // Double check the result is overflow or not of the current_time + relative_time
            // it may cause a panic of chrono::datetime::DateTime add
            match now.checked_add(relative_time) {
                Ok(date) => {
                    let iter = std::iter::once(Ok(date));
                    Box::new(iter)
                }
                Err(_) => {
                    return Err(USimpleError::new(
                        1,
                        translate!("date-error-date-overflow", "date" => relative_time),
                    ));
                }
            }
        }
        DateSource::Stdin => {
            let lines = BufReader::new(std::io::stdin()).lines();
            let iter = lines.map_while(Result::ok).map(parse_date);
            Box::new(iter)
        }
        DateSource::File(ref path) => {
            if path.is_dir() {
                return Err(USimpleError::new(
                    2,
                    translate!("date-error-expected-file-got-directory", "path" => path.to_string_lossy()),
                ));
            }
            let file = File::open(path)
                .map_err_context(|| path.as_os_str().to_string_lossy().to_string())?;
            let lines = BufReader::new(file).lines();
            let iter = lines.map_while(Result::ok).map(parse_date);
            Box::new(iter)
        }
        DateSource::Now => {
            let iter = std::iter::once(Ok(now));
            Box::new(iter)
        }
    };

    let format_string = make_format_string(&settings);

    // Format all the dates
    for date in dates {
        match date {
            // TODO: Switch to lenient formatting.
            Ok(date) => match strtime::format(format_string, &date) {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    return Err(USimpleError::new(
                        1,
                        translate!("date-error-invalid-format", "format" => format_string, "error" => e),
                    ));
                }
            },
            Err((input, _err)) => show!(USimpleError::new(
                1,
                translate!("date-error-invalid-date", "date" => input)
            )),
        }
    }

    Ok(())
}

pub fn uu_app() -> Command {
    Command::new(uucore::util_name())
        .version(uucore::crate_version!())
        .about(translate!("date-about"))
        .override_usage(format_usage(&translate!("date-usage")))
        .infer_long_args(true)
        .arg(
            Arg::new(OPT_DATE)
                .short('d')
                .long(OPT_DATE)
                .value_name("STRING")
                .allow_hyphen_values(true)
                .help(translate!("date-help-date")),
        )
        .arg(
            Arg::new(OPT_FILE)
                .short('f')
                .long(OPT_FILE)
                .value_name("DATEFILE")
                .value_hint(clap::ValueHint::FilePath)
                .help(translate!("date-help-file")),
        )
        .arg(
            Arg::new(OPT_ISO_8601)
                .short('I')
                .long(OPT_ISO_8601)
                .value_name("FMT")
                .value_parser(ShortcutValueParser::new([
                    DATE, HOURS, MINUTES, SECONDS, NS,
                ]))
                .num_args(0..=1)
                .default_missing_value(OPT_DATE)
                .help(translate!("date-help-iso-8601")),
        )
        .arg(
            Arg::new(OPT_RFC_EMAIL)
                .short('R')
                .long(OPT_RFC_EMAIL)
                .help(translate!("date-help-rfc-email"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_RFC_3339)
                .long(OPT_RFC_3339)
                .value_name("FMT")
                .value_parser(ShortcutValueParser::new([DATE, SECONDS, NS]))
                .help(translate!("date-help-rfc-3339")),
        )
        .arg(
            Arg::new(OPT_DEBUG)
                .long(OPT_DEBUG)
                .help(translate!("date-help-debug"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_REFERENCE)
                .short('r')
                .long(OPT_REFERENCE)
                .value_name("FILE")
                .value_hint(clap::ValueHint::AnyPath)
                .help(translate!("date-help-reference")),
        )
        .arg(
            Arg::new(OPT_SET)
                .short('s')
                .long(OPT_SET)
                .value_name("STRING")
                .help({
                    #[cfg(not(any(target_os = "macos", target_os = "redox")))]
                    {
                        translate!("date-help-set")
                    }
                    #[cfg(target_os = "macos")]
                    {
                        translate!("date-help-set-macos")
                    }
                    #[cfg(target_os = "redox")]
                    {
                        translate!("date-help-set-redox")
                    }
                }),
        )
        .arg(
            Arg::new(OPT_UNIVERSAL)
                .short('u')
                .long(OPT_UNIVERSAL)
                .alias(OPT_UNIVERSAL_2)
                .help(translate!("date-help-universal"))
                .action(ArgAction::SetTrue),
        )
        .arg(Arg::new(OPT_FORMAT))
}

/// Return the appropriate format string for the given settings.
fn make_format_string(settings: &Settings) -> &str {
    match settings.format {
        Format::Iso8601(ref fmt) => match *fmt {
            Iso8601Format::Date => "%F",
            Iso8601Format::Hours => "%FT%H%:z",
            Iso8601Format::Minutes => "%FT%H:%M%:z",
            Iso8601Format::Seconds => "%FT%T%:z",
            Iso8601Format::Ns => "%FT%T,%N%:z",
        },
        Format::Rfc5322 => "%a, %d %h %Y %T %z",
        Format::Rfc3339(ref fmt) => match *fmt {
            Rfc3339Format::Date => "%F",
            Rfc3339Format::Seconds => "%F %T%:z",
            Rfc3339Format::Ns => "%F %T.%N%:z",
        },
        Format::Custom(ref fmt) => fmt,
        Format::Default => "%a %b %e %X %Z %Y",
    }
}

/// Parse a `String` into a `DateTime`.
/// If it fails, return a tuple of the `String` along with its `ParseError`.
// TODO: Convert `parse_datetime` to jiff and remove wrapper from chrono to jiff structures.
fn parse_date<S: AsRef<str> + Clone>(
    s: S,
) -> Result<Zoned, (String, parse_datetime::ParseDateTimeError)> {
    match parse_datetime::parse_datetime(s.as_ref()) {
        Ok(date) => {
            let timestamp =
                Timestamp::new(date.timestamp(), date.timestamp_subsec_nanos() as i32).unwrap();
            Ok(Zoned::new(timestamp, TimeZone::UTC))
        }
        Err(e) => Err((s.as_ref().into(), e)),
    }
}

// TODO: Convert `parse_datetime` to jiff and remove wrapper from chrono to jiff structures.
// Also, consider whether parse_datetime::parse_datetime_at_date can be renamed to something
// like parse_datetime::parse_offset, instead of doing some addition/subtraction.
fn parse_offset(date: &str) -> Result<SignedDuration, ()> {
    let ref_time = chrono::Local::now();
    if let Ok(new_time) = parse_datetime::parse_datetime_at_date(ref_time, date) {
        let duration = new_time.signed_duration_since(ref_time);
        Ok(SignedDuration::new(
            duration.num_seconds(),
            duration.subsec_nanos(),
        ))
    } else {
        Err(())
    }
}

#[cfg(not(any(unix, windows)))]
fn set_system_datetime(_date: Zoned) -> UResult<()> {
    unimplemented!("setting date not implemented (unsupported target)");
}

#[cfg(target_os = "macos")]
fn set_system_datetime(_date: Zoned) -> UResult<()> {
    Err(USimpleError::new(
        1,
        translate!("date-error-setting-date-not-supported-macos"),
    ))
}

#[cfg(target_os = "redox")]
fn set_system_datetime(_date: Zoned) -> UResult<()> {
    Err(USimpleError::new(
        1,
        translate!("date-error-setting-date-not-supported-redox"),
    ))
}

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "redox")))]
/// System call to set date (unix).
/// See here for more:
/// `<https://doc.rust-lang.org/libc/i686-unknown-linux-gnu/libc/fn.clock_settime.html>`
/// `<https://linux.die.net/man/3/clock_settime>`
/// `<https://www.gnu.org/software/libc/manual/html_node/Time-Types.html>`
fn set_system_datetime(date: Zoned) -> UResult<()> {
    let ts = date.timestamp();
    let timespec = timespec {
        tv_sec: ts.as_second() as _,
        tv_nsec: ts.subsec_nanosecond() as _,
    };

    let result = unsafe { clock_settime(CLOCK_REALTIME, &raw const timespec) };

    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()
            .map_err_context(|| translate!("date-error-cannot-set-date")))
    }
}

#[cfg(windows)]
/// System call to set date (Windows).
/// See here for more:
/// * <https://docs.microsoft.com/en-us/windows/win32/api/sysinfoapi/nf-sysinfoapi-setsystemtime>
/// * <https://docs.microsoft.com/en-us/windows/win32/api/minwinbase/ns-minwinbase-systemtime>
fn set_system_datetime(date: Zoned) -> UResult<()> {
    let system_time = SYSTEMTIME {
        wYear: date.year() as u16,
        wMonth: date.month() as u16,
        // Ignored
        wDayOfWeek: 0,
        wDay: date.day() as u16,
        wHour: date.hour() as u16,
        wMinute: date.minute() as u16,
        wSecond: date.second() as u16,
        // TODO: be careful of leap seconds - valid range is [0, 999] - how to handle?
        wMilliseconds: ((date.subsec_nanosecond() / 1_000_000) % 1000) as u16,
    };

    let result = unsafe { SetSystemTime(&raw const system_time) };

    if result == 0 {
        Err(std::io::Error::last_os_error()
            .map_err_context(|| translate!("date-error-cannot-set-date")))
    } else {
        Ok(())
    }
}
