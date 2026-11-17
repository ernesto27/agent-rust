use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    io::{Read, Write},
    path::Path,
    process::Stdio,
};
use tokio::process::Command;

#[derive(Serialize)]
struct BashResult<'a> {
    output: &'a str,
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    termination: Option<String>,
}

pub async fn run(command: &str, cwd: &Path, stderr: &mut impl Write) -> Result<String> {
    writeln!(stderr, "$ {command}").context("Failed to write bash command")?;
    stderr.flush().context("Failed to flush bash command")?;

    let (mut reader, writer) = std::io::pipe().context("Failed to create bash output pipe")?;
    let error_writer = writer
        .try_clone()
        .context("Failed to clone bash output pipe")?;
    let mut process = Command::new("bash");
    process
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(error_writer);
    let mut child = process.spawn().context("Failed to spawn bash")?;
    // Command retains the configured handles after spawn; close them for EOF.
    drop(process);
    // Drain while the child runs: waiting first would deadlock on a full pipe.
    let capture = tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .context("Failed to capture bash output")?;
        Ok::<_, anyhow::Error>(bytes)
    });
    let (status, captured) = tokio::join!(child.wait(), capture);
    let status = status.context("Failed to wait for bash")?;
    let bytes = captured.context("Failed to join bash output capture")??;
    let output = String::from_utf8_lossy(&bytes);
    let result = BashResult {
        output: &output,
        exit_code: status.code(),
        termination: status.code().is_none().then(|| status.to_string()),
    };
    let content = serde_json::to_string(&result).context("Failed to serialize bash result")?;

    if let Some(code) = result.exit_code {
        writeln!(stderr, "exit code: {code}").context("Failed to write bash status")?;
    } else {
        writeln!(stderr, "termination: {status}").context("Failed to write bash status")?;
    }
    let (preview, omitted) = preview(&output);
    write!(stderr, "{preview}").context("Failed to write bash output preview")?;
    if !preview.is_empty() && !preview.ends_with('\n') {
        writeln!(stderr).context("Failed to write bash output preview")?;
    }
    if omitted {
        writeln!(stderr, "[output omitted from preview]")
            .context("Failed to write bash output omission note")?;
    }
    stderr
        .flush()
        .context("Failed to flush bash output preview")?;
    Ok(content)
}

fn preview(output: &str) -> (String, bool) {
    let mut lines = 0;
    for (characters, (offset, character)) in output.char_indices().enumerate() {
        if characters == 2000 || lines == 10 {
            return (output[..offset].to_owned(), true);
        }
        if character == '\n' {
            lines += 1;
        }
    }
    (output.to_owned(), false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{
        fs, io,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-bash-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[tokio::test]
    async fn merged_output_and_nonzero_status() {
        let cwd = TestDir::new();
        let mut diagnostics = Vec::new();
        let result = run(
            "printf out; printf err >&2; exit 7",
            &cwd.0,
            &mut diagnostics,
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&result).unwrap(),
            json!({"output":"outerr","exit_code":7})
        );
        let diagnostics = String::from_utf8(diagnostics).unwrap();
        assert!(diagnostics.starts_with("$ printf out; printf err >&2; exit 7\n"));
        assert!(diagnostics.contains("exit code: 7\nouterr\n"));
    }

    #[tokio::test]
    async fn empty_command_cwd_and_null_stdin() {
        let cwd = TestDir::new();
        for (command, expected) in [
            ("", "".to_owned()),
            ("pwd", format!("{}\n", cwd.0.display())),
            ("cat", "".to_owned()),
        ] {
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                run(command, &cwd.0, &mut Vec::new()),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&result).unwrap(),
                json!({"output":expected,"exit_code":0})
            );
        }
    }

    #[tokio::test]
    async fn shell_state_does_not_persist() {
        let cwd = TestDir::new();
        run(
            "mkdir sub; cd sub; local_bash_value=secret",
            &cwd.0,
            &mut Vec::new(),
        )
        .await
        .unwrap();
        let result = run(
            "printf '%s|' \"${local_bash_value-unset}\"; pwd",
            &cwd.0,
            &mut Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&result).unwrap()["output"],
            format!("unset|{}\n", cwd.0.display())
        );
    }

    #[tokio::test]
    async fn lossy_utf8_and_signal_termination() {
        let cwd = TestDir::new();
        let result = run("printf '\\377'", &cwd.0, &mut Vec::new())
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&result).unwrap(),
            json!({"output":"�","exit_code":0})
        );
        let result = run("kill -TERM $$", &cwd.0, &mut Vec::new()).await.unwrap();
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["exit_code"], Value::Null);
        assert!(!result["termination"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pipe_sized_output_drains_while_running() {
        let cwd = TestDir::new();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run("head -c 262144 /dev/zero", &cwd.0, &mut Vec::new()),
        )
        .await
        .expect("pipe capture deadlocked")
        .unwrap();
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["output"].as_str().unwrap().len(), 262144);
        assert_eq!(result["exit_code"], 0);
    }

    struct FailingWriter {
        fail_flush: bool,
    }
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail_flush {
                Ok(bytes.len())
            } else {
                Err(io::Error::other("diagnostic write failed"))
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("diagnostic flush failed"))
        }
    }

    #[tokio::test]
    async fn diagnostic_failures_prevent_execution_and_spawn_errors_have_context() {
        let cwd = TestDir::new();
        for fail_flush in [false, true] {
            let error = run("touch marker", &cwd.0, &mut FailingWriter { fail_flush })
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains(if fail_flush {
                    "flush bash command"
                } else {
                    "write bash command"
                }),
                "{error:#}"
            );
            assert!(!cwd.0.join("marker").exists());
        }
        let error = run("true", &cwd.0.join("absent"), &mut Vec::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("spawn bash"), "{error:#}");
    }

    #[test]
    fn preview_respects_line_and_unicode_character_limits() {
        for (input, expected, omitted) in [
            ("".to_owned(), "".to_owned(), false),
            ("🌍".repeat(2000), "🌍".repeat(2000), false),
            ("🌍".repeat(2001), "🌍".repeat(2000), true),
            ("a\n".repeat(10), "a\n".repeat(10), false),
            ("a\n".repeat(11), "a\n".repeat(10), true),
            (
                format!("{}\nx", "a\n".repeat(9)),
                format!("{}\n", "a\n".repeat(9)),
                true,
            ),
            (
                format!("{}\nx", "🌍".repeat(1999)),
                format!("{}\n", "🌍".repeat(1999)),
                true,
            ),
        ] {
            let actual = preview(&input);
            assert_eq!(actual, (expected, omitted));
            assert!(actual.0.chars().count() <= 2000);
            assert!(actual.0.lines().count() <= 10);
        }
    }
}
