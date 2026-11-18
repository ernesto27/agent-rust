use std::{
    io::{self, Read, Write},
    path::Path,
    process::Stdio,
};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::process::Command;

#[derive(Serialize)]
struct BashResult {
    output: String,
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    termination: Option<String>,
}

pub async fn run(command: &str, cwd: &Path, stderr: &mut impl Write) -> Result<String> {
    writeln!(stderr, "$ {command}").context("Failed to write bash command")?;
    stderr.flush().context("Failed to flush bash command")?;

    let (mut reader, writer) = io::pipe().context("Failed to create bash output pipe")?;
    let error_writer = writer
        .try_clone()
        .context("Failed to clone bash output pipe")?;
    let mut process = Command::new("bash");
    process
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(error_writer);
    let mut child = process.spawn().context("Failed to spawn bash command")?;
    // Command retains its configured pipe handles; release them for capture EOF.
    drop(process);
    let capture = tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .context("Failed to capture bash output")?;
        Ok::<_, anyhow::Error>(bytes)
    });
    // Drain concurrently: waiting before reading would deadlock on a full pipe.
    let (status, bytes) = tokio::join!(child.wait(), capture);
    let status = status.context("Failed to wait for bash command")?;
    let bytes = bytes.context("Failed to join bash output capture")??;
    let result = BashResult {
        output: String::from_utf8_lossy(&bytes).into_owned(),
        exit_code: status.code(),
        termination: status.code().is_none().then(|| status.to_string()),
    };
    if let Some(code) = result.exit_code {
        writeln!(stderr, "Exit code: {code}")
    } else {
        writeln!(stderr, "Terminated: {status}")
    }
    .context("Failed to write bash status")?;
    let (text, omitted) = preview(&result.output);
    write!(stderr, "{text}").context("Failed to write bash output preview")?;
    if !text.is_empty() && !text.ends_with('\n') {
        writeln!(stderr).context("Failed to terminate bash output preview")?;
    }
    if omitted {
        writeln!(stderr, "[output preview truncated]")
            .context("Failed to write bash preview omission note")?;
    }
    stderr
        .flush()
        .context("Failed to flush bash output preview")?;
    serde_json::to_string(&result).context("Failed to serialize bash result")
}

fn preview(output: &str) -> (String, bool) {
    let mut lines = 0;
    for (characters, (offset, character)) in output.char_indices().enumerate() {
        if lines == 10 || characters == 2000 {
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
    use tokio::time::timeout;

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

    async fn result(command: &str, cwd: &Path) -> (Value, String) {
        let mut diagnostics = Vec::new();
        let content = timeout(Duration::from_secs(5), run(command, cwd, &mut diagnostics))
            .await
            .expect("bash or pipe capture deadlocked")
            .unwrap();
        (
            serde_json::from_str(&content).unwrap(),
            String::from_utf8(diagnostics).unwrap(),
        )
    }

    #[tokio::test]
    async fn merged_output_and_nonzero_status_are_completed_results() {
        let cwd = TestDir::new();
        let command = "printf out; printf err >&2; printf again; printf last >&2; exit 7";
        let (value, diagnostics) = result(command, &cwd.0).await;
        assert_eq!(value, json!({"output": "outerragainlast", "exit_code": 7}));
        assert!(diagnostics.starts_with(&format!("$ {command}\n")));
        assert!(diagnostics.contains("Exit code: 7\nouterragainlast\n"));
    }

    #[tokio::test]
    async fn empty_command_cwd_and_fresh_shell_state() {
        let cwd = TestDir::new();
        assert_eq!(
            result("", &cwd.0).await.0,
            json!({"output": "", "exit_code": 0})
        );
        let (value, _) = result("pwd", &cwd.0).await;
        assert_eq!(value["output"], format!("{}\n", cwd.0.display()));
        result("export STEP5_TEST_VAR=secret; cd /", &cwd.0).await;
        let (value, _) = result("printf '%s' \"${STEP5_TEST_VAR-unset}\"; pwd", &cwd.0).await;
        assert_eq!(value["output"], format!("unset{}\n", cwd.0.display()));
    }

    #[tokio::test]
    async fn stdin_is_null_and_utf8_is_lossy_and_signals_have_a_description() {
        let cwd = TestDir::new();
        assert_eq!(
            result("cat", &cwd.0).await.0,
            json!({"output": "", "exit_code": 0})
        );
        assert_eq!(
            result("printf '\\377'", &cwd.0).await.0,
            json!({"output": "�", "exit_code": 0})
        );
        let (value, diagnostics) = result("kill -TERM $$", &cwd.0).await;
        assert!(value["exit_code"].is_null());
        assert!(!value["termination"].as_str().unwrap().is_empty());
        assert!(diagnostics.contains("Terminated:"));
    }

    #[tokio::test]
    async fn pipe_sized_output_drains_while_child_runs() {
        let cwd = TestDir::new();
        let (value, diagnostics) = result("printf '%1000000s' x", &cwd.0).await;
        let output = value["output"].as_str().unwrap();
        assert_eq!(output.len(), 1_000_000);
        assert!(output.ends_with('x'));
        assert_eq!(value["exit_code"], 0);
        assert!(diagnostics.contains("[output preview truncated]"));
        assert!(diagnostics.len() < 2200);
    }

    #[test]
    fn preview_obeys_both_caps_on_unicode_boundaries() {
        for exact in [String::new(), "雪".repeat(2000), "雪\n".repeat(10)] {
            assert_eq!(preview(&exact), (exact, false));
        }
        assert_eq!(preview(&"雪".repeat(2001)), ("雪".repeat(2000), true));
        assert_eq!(preview(&"雪\n".repeat(11)), ("雪\n".repeat(10), true));
        let (text, omitted) = preview(&format!("{}\n", "雪".repeat(2000)).repeat(11));
        assert_eq!(text.chars().count(), 2000);
        assert!(text.lines().count() <= 10);
        assert!(omitted);
    }

    struct FailingWriter {
        on_flush: bool,
    }
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.on_flush {
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
    async fn failed_command_diagnostics_prevent_execution() {
        let cwd = TestDir::new();
        for on_flush in [false, true] {
            let error = run("touch marker", &cwd.0, &mut FailingWriter { on_flush })
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains(if on_flush {
                    "flush bash command"
                } else {
                    "write bash command"
                }),
                "{error:#}"
            );
            assert!(!cwd.0.join("marker").exists());
        }
    }
}
