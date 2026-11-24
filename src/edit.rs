use std::{fs, io::Write, path::Path};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::write::{persist, render_preview};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    old_text: String,
    new_text: String,
}

fn parse_args(raw: &str) -> Result<Args> {
    let args: Args = serde_json::from_str(raw)?;
    ensure!(!args.path.is_empty(), "path must not be empty");
    ensure!(!args.old_text.is_empty(), "old_text must not be empty");
    Ok(args)
}

pub fn execute(raw: &str, cwd: &Path, stderr: &mut impl Write) -> Result<String> {
    let args = match parse_args(raw) {
        Ok(args) => args,
        Err(error) => {
            writeln!(stderr, "edit: invalid arguments")?;
            stderr.flush()?;
            return Ok(format!("Error: edit arguments: {error:#}"));
        }
    };
    writeln!(stderr, "edit {:?}", args.path)?;
    stderr.flush()?;
    let diff = match replace(&cwd.join(&args.path), &args.old_text, &args.new_text) {
        Ok(diff) => diff,
        Err(error) => return Ok(format!("Error: edit {:?}: {error:#}", args.path)),
    };
    stderr.write_all(diff.as_bytes())?;
    stderr.flush()?;
    Ok(format!("Edited {:?}", args.path))
}

fn replace(path: &Path, old_text: &str, new_text: &str) -> Result<String> {
    let metadata = fs::symlink_metadata(path).context("failed to inspect destination")?;
    ensure!(metadata.is_file(), "destination must be a regular file");
    let original = fs::read_to_string(path).context("failed to read destination as UTF-8")?;
    let (old_crlf, new_crlf);
    let (old_text, new_text) = if uses_crlf(&original) {
        old_crlf = to_crlf(old_text);
        new_crlf = to_crlf(new_text);
        (old_crlf.as_str(), new_crlf.as_str())
    } else {
        (old_text, new_text)
    };
    let start = unique_match(&original, old_text)?;
    if old_text == new_text {
        return Ok(String::new());
    }
    let updated = [
        &original[..start],
        new_text,
        &original[start + old_text.len()..],
    ]
    .concat();
    let diff = render_preview(&original, &updated);
    let parent = path.parent().context("destination has no parent")?;
    persist(path, parent, &updated, Some(metadata.permissions()))?;
    Ok(diff)
}

fn uses_crlf(text: &str) -> bool {
    text.contains('\n')
        && text
            .split_inclusive('\n')
            .all(|line| !line.ends_with('\n') || line.ends_with("\r\n"))
}

fn to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn unique_match(body: &str, needle: &str) -> Result<usize> {
    let Some(first) = body.find(needle) else {
        bail!("not found, re-read the file");
    };
    // Advance by one character, not the whole match, to count overlaps too.
    let step = needle.chars().next().expect("nonempty needle").len_utf8();
    let mut rest = &body[first + step..];
    let mut count = 1;
    while let Some(next) = rest.find(needle) {
        count += 1;
        rest = &rest[next + step..];
    }
    ensure!(
        count == 1,
        "found {count} times, add more surrounding context"
    );
    Ok(first)
}
