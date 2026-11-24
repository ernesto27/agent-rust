use std::{
    fs::{self, File, Permissions},
    io::{ErrorKind, Read, Write},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use similar::TextDiff;
use tempfile::NamedTempFile;

const DIFF_INPUT_LIMIT: usize = 64 * 1024;
const TOO_LARGE: &str = "[diff omitted: file exceeds 64 KiB]\n";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    content: String,
}

fn parse_args(raw: &str) -> Result<Args> {
    let args: Args = serde_json::from_str(raw)?;
    ensure!(!args.path.is_empty(), "path must not be empty");
    Ok(args)
}

pub fn execute(raw: &str, cwd: &Path, stderr: &mut impl Write) -> Result<String> {
    let args = match parse_args(raw) {
        Ok(args) => args,
        Err(error) => {
            writeln!(stderr, "write: invalid arguments")?;
            stderr.flush()?;
            return Ok(format!("Error: write arguments: {error:#}"));
        }
    };
    writeln!(stderr, "write {:?}", args.path)?;
    stderr.flush()?;

    let diff = match replace(&cwd.join(&args.path), &args.content) {
        Ok(diff) => diff,
        Err(error) => return Ok(format!("Error: write {:?}: {error:#}", args.path)),
    };
    stderr.write_all(diff.as_bytes())?;
    stderr.flush()?;
    Ok(format!(
        "Wrote {} bytes to {:?}",
        args.content.len(),
        args.path
    ))
}

fn replace(path: &Path, content: &str) -> Result<String> {
    let parent = path.parent().context("destination has no parent")?;
    let permissions = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(metadata.is_file(), "destination must be a regular file");
            Some(metadata.permissions())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("failed to inspect destination"),
    };
    let diff = build_preview(path, permissions.is_some(), content)?;

    fs::create_dir_all(parent).context("failed to create parent directories")?;
    persist(path, parent, content, permissions)?;
    Ok(diff)
}

pub(crate) fn persist(
    path: &Path,
    parent: &Path,
    content: &str,
    permissions: Option<Permissions>,
) -> Result<()> {
    // Stage beside the destination so a failed write never truncates the original.
    let mut staged = NamedTempFile::new_in(parent).context("failed to stage file")?;
    staged
        .write_all(content.as_bytes())
        .context("failed to write staged file")?;
    if let Some(permissions) = permissions {
        staged
            .as_file()
            .set_permissions(ordinary_permissions(permissions))
            .context("failed to set staged file permissions")?;
        staged
            .persist(path)
            .context("failed to replace destination")?;
    } else {
        staged
            .persist_noclobber(path)
            .context("failed to create destination")?;
    }
    Ok(())
}

fn ordinary_permissions(permissions: Permissions) -> Permissions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Permissions::from_mode(permissions.mode() & 0o777)
    }
    #[cfg(not(unix))]
    permissions
}

fn build_preview(path: &Path, exists: bool, content: &str) -> Result<String> {
    if content.len() > DIFF_INPUT_LIMIT {
        return Ok(TOO_LARGE.into());
    }
    let mut old = Vec::new();
    if exists {
        File::open(path)
            .context("failed to open destination for diff")?
            .take(DIFF_INPUT_LIMIT as u64 + 1)
            .read_to_end(&mut old)
            .context("failed to read destination for diff")?;
    }
    if old.len() > DIFF_INPUT_LIMIT {
        return Ok(TOO_LARGE.into());
    }
    let Ok(old) = std::str::from_utf8(&old) else {
        return Ok("[diff omitted: destination is not UTF-8]\n".into());
    };
    Ok(render_preview(old, content))
}

pub(crate) fn render_preview(old: &str, content: &str) -> String {
    if old.len().max(content.len()) > DIFF_INPUT_LIMIT {
        return TOO_LARGE.into();
    }
    let diff = TextDiff::configure()
        .timeout(Duration::from_millis(100))
        .diff_lines(old, content);
    let mut diff = diff.unified_diff().context_radius(1).to_string();
    let mut end = diff
        .split_inclusive('\n')
        .take(20)
        .map(str::len)
        .sum::<usize>()
        .min(4096);
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = end < diff.len();
    diff.truncate(end);
    let mut shown = String::new();
    for c in diff.chars() {
        if c.is_control() && c != '\n' && c != '\t' {
            shown.extend(c.escape_default());
        } else {
            shown.push(c);
        }
    }
    if truncated {
        if !shown.ends_with('\n') {
            shown.push('\n');
        }
        shown.push_str("[diff preview truncated]\n");
    }
    shown
}
