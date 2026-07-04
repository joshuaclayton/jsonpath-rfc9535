//! `jp` — apply an RFC 9535 JSONPath query to a JSON document.
//!
//! Reads JSON from a file argument, or from stdin when no file is given, applies the
//! query, and prints the selected values as a JSON array. With `--paths`, prints their
//! normalized paths (as a JSON array of strings) instead.

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use jsonpath_rfc9535::JsonPath;
use serde_json::Value;

const USAGE: &str = "usage: jp [-p|--paths] <query> [file]

Apply an RFC 9535 JSONPath query to JSON read from <file> (or stdin if omitted).
Prints the selected values as a JSON array.

  -p, --paths    print each match's normalized path instead of its value
  -h, --help     print this help and exit
  -V, --version  print version and exit";

/// A command-line failure: the message to report and the exit code to return.
struct CliError {
    message: String,
    code: u8,
}

impl CliError {
    fn new(message: impl Into<String>, code: u8) -> Self {
        Self {
            message: message.into(),
            code,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Report to stderr; a failure to write there leaves nothing else to do.
            writeln!(io::stderr(), "jp: {error}").ok();
            ExitCode::from(error.code)
        }
    }
}

fn run() -> Result<(), CliError> {
    let mut paths = false;
    let mut help = false;
    let mut version = false;
    let mut positionals: Vec<String> = Vec::new();

    for arg in std::env::args().skip(1) {
        if arg == "-h" || arg == "--help" {
            help = true;
        } else if arg == "-V" || arg == "--version" {
            version = true;
        } else if arg == "-p" || arg == "--paths" {
            paths = true;
        } else if arg.starts_with('-') && arg != "-" {
            return Err(CliError::new(
                format!("unknown option `{arg}`\n\n{USAGE}"),
                2,
            ));
        } else {
            positionals.push(arg);
        }
    }

    if help {
        return write_line(USAGE);
    }
    if version {
        return write_line(concat!("jp ", env!("CARGO_PKG_VERSION")));
    }

    let mut positionals = positionals.into_iter();
    let query = positionals
        .next()
        .ok_or_else(|| CliError::new(format!("missing <query>\n\n{USAGE}"), 2))?;
    let file = positionals.next();
    if positionals.next().is_some() {
        return Err(CliError::new(format!("too many arguments\n\n{USAGE}"), 2));
    }

    let input = read_input(file.as_deref())?;
    let document: Value = serde_json::from_str(&input)
        .map_err(|error| CliError::new(format!("invalid JSON input: {error}"), 1))?;
    let compiled = JsonPath::parse(&query)
        .map_err(|error| CliError::new(format!("invalid query: {error}"), 2))?;

    let rendered = if paths {
        let located: Vec<String> = compiled
            .query(&document)
            .paths()
            .map(ToString::to_string)
            .collect();
        serde_json::to_string_pretty(&located)
    } else {
        serde_json::to_string_pretty(&compiled.query_values(&document))
    }
    .map_err(|error| CliError::new(format!("cannot render output: {error}"), 1))?;

    writeln!(io::stdout().lock(), "{rendered}")
        .map_err(|error| CliError::new(format!("write failed: {error}"), 1))
}

/// Reads the input document from `file`, or from stdin when `file` is `None`.
fn read_input(file: Option<&str>) -> Result<String, CliError> {
    if let Some(path) = file {
        return fs::read_to_string(path)
            .map_err(|error| CliError::new(format!("cannot read {path}: {error}"), 1));
    }
    let mut buffer = String::new();
    io::stdin()
        .read_to_string(&mut buffer)
        .map_err(|error| CliError::new(format!("cannot read stdin: {error}"), 1))?;
    Ok(buffer)
}

/// Writes a single line to stdout, mapping any I/O failure to a [`CliError`].
fn write_line(text: &str) -> Result<(), CliError> {
    writeln!(io::stdout().lock(), "{text}")
        .map_err(|error| CliError::new(format!("write failed: {error}"), 1))
}
