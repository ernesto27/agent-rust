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
    let mut output = String::new();
    // Bytes awaiting UTF-8 validation: at most one buffered chunk plus an
    // incomplete multi-byte sequence carried over from the previous chunk.
    let mut pending = Vec::new();
    // First MAX_LINE_CHARS + 1 characters of the current page line; the extra
    // character lets a CR before LF be stripped before truncating.
    let mut line = String::new();
    let mut chars = 0;
    let mut ends_with_cr = false;
    let mut in_line = false;
    let mut on_page = false;
    let mut total = 0;
    let mut shown = 0;
    loop {
        let chunk = match reader.fill_buf() {
            Ok(chunk) => chunk,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("failed to read file"),
        };
        if chunk.is_empty() {
            break;
        }
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let len = newline.map_or(chunk.len(), |index| index + 1);
        ensure!(
            !chunk[..len].contains(&0),
            "binary file (contains NUL bytes)"
        );
        pending.extend_from_slice(&chunk[..len]);
        reader.consume(len);

        let text = match std::str::from_utf8(&pending) {
            Ok(text) => text,
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&pending[..error.valid_up_to()]).unwrap()
            }
            Err(_) => bail!("binary file (invalid UTF-8)"),
        };
        let validated = text.len();
        if !in_line {
            in_line = true;
            total += 1;
            on_page = total >= offset && shown < limit;
        }
        let text = text.strip_suffix('\n').unwrap_or(text);
        if !text.is_empty() {
            ends_with_cr = text.ends_with('\r');
        }
        if on_page {
            let room = (MAX_LINE_CHARS + 1).saturating_sub(chars);
            line.extend(text.chars().take(room));
            chars += text.chars().count();
        }
        pending.drain(..validated);

        if newline.is_some() {
            if on_page {
                push_line(&mut output, total, &line, chars - usize::from(ends_with_cr));
                shown += 1;
            }
            line.clear();
            chars = 0;
            ends_with_cr = false;
            in_line = false;
        }
    }
    ensure!(pending.is_empty(), "binary file (invalid UTF-8)");
    if in_line && on_page {
        push_line(&mut output, total, &line, chars);
        shown += 1;
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

/// Writes a numbered line using the first `chars` characters of `line`,
/// capped at MAX_LINE_CHARS.
fn push_line(output: &mut String, number: usize, line: &str, chars: usize) {
    let end = line
        .char_indices()
        .nth(chars.min(MAX_LINE_CHARS))
        .map_or(line.len(), |(index, _)| index);
    writeln!(output, "{number:>6}\t{}", &line[..end]).unwrap();
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

    /// The pre-chunking implementation, used as an oracle for valid input.
    fn whole_line_reference(input: &str, offset: usize, limit: usize) -> String {
        let mut output = String::new();
        let lines: Vec<&str> = input.split_inclusive('\n').collect();
        for (index, line) in lines.iter().enumerate().skip(offset - 1).take(limit) {
            let line = line
                .strip_suffix("\r\n")
                .or_else(|| line.strip_suffix('\n'))
                .unwrap_or(line);
            let text: String = line.chars().take(MAX_LINE_CHARS).collect();
            writeln!(output, "{:>6}\t{text}", index + 1).unwrap();
        }
        let total = lines.len();
        let shown = total.saturating_sub(offset - 1).min(limit);
        if total == 0 {
            output.push_str("[empty file]\n");
        } else if shown == 0 {
            writeln!(
                output,
                "[no lines at offset {offset}; file has {total} lines]"
            )
            .unwrap();
        } else if offset + shown - 1 < total {
            let last = offset + shown - 1;
            writeln!(
                output,
                "[showing lines {offset}-{last} of {total}, use offset={} to read more]",
                last + 1
            )
            .unwrap();
        }
        output
    }

    fn chunked(input: &[u8], capacity: usize, offset: usize, limit: usize) -> Result<String> {
        format_page(BufReader::with_capacity(capacity, input), offset, limit)
    }

    #[test]
    fn small_chunks_match_whole_line_semantics() {
        let cap = MAX_LINE_CHARS;
        let inputs = [
            String::new(),
            "\n".into(),
            "\r\n".into(),
            "\r".into(),
            "a\rb\r\n\r\rc\r".into(),
            "one\r\n\nthree".into(),
            "é🙂ü\r\nx\n€".into(),
            format!("{}\r\n", "a".repeat(cap)),
            format!("{}\r\n", "a".repeat(cap - 1)),
            format!("{}\r\r\n", "a".repeat(cap - 1)),
            format!("{}\rb\n", "a".repeat(cap)),
            format!("{}\r", "a".repeat(cap)),
            format!("{}\n{}", "🙂".repeat(cap + 3), "é".repeat(cap + 1)),
        ];
        for input in &inputs {
            for (offset, limit) in [(1, 2_000), (1, 1), (2, 1), (2, 5), (9, 1)] {
                let expected = whole_line_reference(input, offset, limit);
                for capacity in 1..=7 {
                    let actual = chunked(input.as_bytes(), capacity, offset, limit).unwrap();
                    assert_eq!(actual, expected, "{input:?} {offset} {limit} {capacity}");
                }
            }
        }
    }

    #[test]
    fn small_chunks_accept_split_utf8_and_crlf() {
        // Four-byte characters straddle every boundary of a 3-byte buffer.
        let input = "🙂🙂\r\n🙂";
        assert_eq!(
            chunked(input.as_bytes(), 3, 1, 2_000).unwrap(),
            "     1\t🙂🙂\n     2\t🙂\n"
        );
        // CR ends one chunk and LF starts the next.
        assert_eq!(
            chunked(b"abc\r\nd", 4, 1, 1).unwrap(),
            "     1\tabc\n[showing lines 1-1 of 2, use offset=2 to read more]\n"
        );
    }

    #[test]
    fn long_line_is_truncated_to_exactly_the_character_cap() {
        let input = format!("{}tail\n", "é".repeat(MAX_LINE_CHARS));
        let output = chunked(input.as_bytes(), 4, 1, 1).unwrap();
        let text = output
            .strip_prefix("     1\t")
            .unwrap()
            .strip_suffix('\n')
            .unwrap();
        assert_eq!(text.chars().count(), MAX_LINE_CHARS);
        assert!(text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn binary_bytes_far_past_the_line_cap_are_rejected() {
        let mut input = "a".repeat(MAX_LINE_CHARS * 3).into_bytes();
        input.push(0xff);
        let error = chunked(&input, 4, 1, 1).unwrap_err();
        assert_eq!(error.to_string(), "binary file (invalid UTF-8)");

        input.pop();
        input.extend_from_slice(b"\0rest\n");
        let error = chunked(&input, 4, 1, 1).unwrap_err();
        assert_eq!(error.to_string(), "binary file (contains NUL bytes)");

        // A multi-byte sequence truncated by EOF or by a newline is invalid.
        for input in [&b"ok\xf0\x9f\x99"[..], b"ok\xf0\x9f\n"] {
            let error = chunked(input, 4, 1, 1).unwrap_err();
            assert_eq!(error.to_string(), "binary file (invalid UTF-8)");
        }
    }

    #[test]
    fn newline_free_binary_input_is_rejected_without_buffering_it() {
        use std::io::Read as _;
        // 64 GiB of NUL bytes would abort the process if buffered as one line.
        let zeros = std::io::repeat(0).take(64 << 30);
        let error = format_page(BufReader::new(zeros), 1, 1).unwrap_err();
        assert_eq!(error.to_string(), "binary file (contains NUL bytes)");
    }
}
