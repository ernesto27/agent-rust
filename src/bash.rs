use std::{
    io::{Read, Write, pipe},
    path::Path,
    process::Stdio,
};

use anyhow::{Context, Result};
use tokio::process::Command;

pub fn preview(output: &str) -> (&str, bool) {
    let mut end = 0;
    for line in output.split_inclusive('\n').take(20) {
        end += line.len();
    }
    end = end.min(4096);
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    (&output[..end], end < output.len())
}

fn show_preview(stderr: &mut impl Write, status: &str, output: &str) -> Result<()> {
    writeln!(stderr, "{status}").context("Failed to write tool status")?;
    let (text, truncated) = preview(output);
    write!(stderr, "{text}").context("Failed to write tool preview")?;
    if !text.is_empty() && !text.ends_with('\n') {
        writeln!(stderr).context("Failed to terminate tool preview")?;
    }
    if truncated {
        writeln!(stderr, "[preview truncated; full output sent to model]")
            .context("Failed to write preview truncation notice")?;
    }
    stderr.flush().context("Failed to flush tool preview")
}

pub async fn run(command: &str, cwd: &Path, stderr: &mut impl Write) -> Result<String> {
    writeln!(stderr, "$ {command}").context("Failed to write tool command")?;
    stderr.flush().context("Failed to flush tool command")?;
    let (mut reader, writer) = pipe().context("Failed to create tool output pipe")?;
    let error_writer = writer
        .try_clone()
        .context("Failed to clone tool output pipe")?;
    let mut builder = Command::new("bash");
    builder
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer))
        .stderr(Stdio::from(error_writer));
    let mut child = builder.spawn().context("Failed to start bash")?;
    // The builder owns parent pipe writers; drop it so EOF is possible.
    drop(builder);
    let capture = tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).map(|_| bytes)
    });
    let (status, bytes) = tokio::try_join!(
        async { child.wait().await.context("Failed to wait for bash") },
        async {
            capture
                .await
                .context("Tool output reader failed")?
                .context("Failed to capture tool output")
        }
    )?;
    let output = String::from_utf8_lossy(&bytes);
    let status = match status.code() {
        Some(code) => format!("Exit code: {code}"),
        None => "Termination: signal".to_owned(),
    };
    show_preview(stderr, &status, &output)?;
    Ok(format!("{status}\n{output}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_caps_lines_and_utf8_bytes() {
        assert_eq!(preview(""), ("", false));
        assert_eq!(preview("one\ntwo"), ("one\ntwo", false));
        let twenty = "line\n".repeat(20);
        assert_eq!(preview(&twenty), (twenty.as_str(), false));
        let more = format!("{twenty}last");
        assert_eq!(preview(&more), (twenty.as_str(), true));
        let exact = "x".repeat(4096);
        assert_eq!(preview(&exact), (exact.as_str(), false));
        let multibyte = format!("{}🙂", "x".repeat(4095));
        let (text, cut) = preview(&multibyte);
        assert_eq!(text.len(), 4095);
        assert!(cut);
    }

    #[test]
    fn previews_report_write_errors() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("broken"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(show_preview(&mut Broken, "Exit code: 0", "ok").is_err());
    }
}
