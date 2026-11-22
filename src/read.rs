use std::{
    fmt::Write as _,
    fs::{self, File},
    io::{BufRead, BufReader, ErrorKind},
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

pub const MAX_LINES: usize = 2_000;
const MAX_LINE_CHARS: usize = 2_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    pub path: String,
    #[serde(default = "first_line")]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn first_line() -> usize {
    1
}

fn default_limit() -> usize {
    MAX_LINES
}

pub fn parse_args(arguments: &str) -> Result<ReadArgs> {
    ensure!(
        arguments.trim_start().starts_with('{'),
        "expected an object with path, optional offset, and optional limit"
    );
    let args: ReadArgs = serde_json::from_str(arguments)
        .context("expected a string path and optional integer offset and limit")?;
    ensure!(!args.path.is_empty(), "path must not be empty");
    ensure!(args.offset >= 1, "offset must be at least 1");
    ensure!(
        (1..=MAX_LINES).contains(&args.limit),
        "limit must be between 1 and {MAX_LINES}"
    );
    Ok(args)
}

pub fn run(args: &ReadArgs, cwd: &Path) -> Result<String> {
    let path = cwd.join(&args.path);
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => bail!("file not found"),
        Err(error) => return Err(error).context("failed to inspect file"),
    };
    ensure!(!metadata.is_dir(), "path is a directory");
    ensure!(metadata.is_file(), "path is not a regular file");

    let file = File::open(path).context("failed to open file")?;
    ensure!(
        file.metadata()
            .context("failed to inspect opened file")?
            .is_file(),
        "path is not a regular file"
    );
    format_page(BufReader::new(file), args.offset, args.limit)
}

fn format_page(mut reader: impl BufRead, offset: usize, limit: usize) -> Result<String> {
    let mut bytes = Vec::new();
    let mut output = String::new();
    let mut total = 0;
    let mut shown = 0;
    while reader
        .read_until(b'\n', &mut bytes)
        .context("failed to read file")?
        != 0
    {
        total += 1;
        ensure!(!bytes.contains(&0), "binary file (contains NUL bytes)");
        let line = std::str::from_utf8(&bytes).context("binary file (invalid UTF-8)")?;
        if total >= offset && shown < limit {
            let line = line
                .strip_suffix("\r\n")
                .or_else(|| line.strip_suffix('\n'))
                .unwrap_or(line);
            let end = line
                .char_indices()
                .nth(MAX_LINE_CHARS)
                .map_or(line.len(), |(index, _)| index);
            writeln!(output, "{total:>6}\t{}", &line[..end]).unwrap();
            shown += 1;
        }
        bytes.clear();
    }

    if total == 0 {
        output.push_str("[empty file]\n");
    } else if shown == 0 {
        writeln!(
            output,
            "[no lines at offset {offset}; file has {total} lines]"
        )
        .unwrap();
    } else {
        let last = offset + (shown - 1);
        if last < total {
            writeln!(
                output,
                "[showing lines {offset}-{last} of {total}, use offset={} to read more]",
                last + 1
            )
            .unwrap();
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_require_a_bounded_object() {
        let args = parse_args(r#"{"path":"src/main.rs"}"#).unwrap();
        assert_eq!((args.offset, args.limit), (1, 2_000));
        assert!(parse_args(r#"{"path":"a","offset":2,"limit":2000}"#).is_ok());
        for invalid in [
            "{",
            "[]",
            "{}",
            r#"{"path":""}"#,
            r#"{"path":"a","offset":0}"#,
            r#"{"path":"a","offset":-1}"#,
            r#"{"path":"a","limit":0}"#,
            r#"{"path":"a","limit":2001}"#,
            r#"{"path":"a","limit":1.5}"#,
            r#"{"path":"a","limit":null}"#,
            r#"{"path":"a","other":1}"#,
        ] {
            assert!(parse_args(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn page_numbers_and_counts_lf_crlf_and_unterminated_lines() {
        let bytes = &b"one\r\n\nthree"[..];
        assert_eq!(
            format_page(bytes, 1, 2).unwrap(),
            "     1\tone\n     2\t\n[showing lines 1-2 of 3, use offset=3 to read more]\n"
        );
        assert_eq!(format_page(bytes, 3, 2).unwrap(), "     3\tthree\n");
        assert_eq!(
            format_page(bytes, 4, 2).unwrap(),
            "[no lines at offset 4; file has 3 lines]\n"
        );
        assert_eq!(format_page(&b""[..], 1, 2).unwrap(), "[empty file]\n");
    }

    #[test]
    fn default_page_has_exactly_two_thousand_lines() {
        let text = "line\n".repeat(2_001);
        let output = format_page(text.as_bytes(), 1, default_limit()).unwrap();
        assert_eq!(output.lines().count(), 2_001);
        assert!(output.ends_with(
            "  2000\tline\n[showing lines 1-2000 of 2001, use offset=2001 to read more]\n"
        ));
    }

    #[test]
    fn line_cap_respects_utf8_and_binary_after_the_page_is_rejected() {
        let prefix = "🙂".repeat(MAX_LINE_CHARS);
        let input = format!("{prefix}é\n");
        assert_eq!(
            format_page(input.as_bytes(), 1, 1).unwrap(),
            format!("     1\t{prefix}\n")
        );
        assert!(format_page(&b"visible\n\xff"[..], 1, 1).is_err());
        assert!(format_page(&b"visible\n\0"[..], 1, 1).is_err());
    }
}
