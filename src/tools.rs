use std::{io::Write, path::Path};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    bash, read,
    types::{ToolCall, parse_bash_args},
    write,
};

#[derive(Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionDef,
}

#[derive(Serialize)]
struct FunctionDef {
    name: &'static str,
    description: &'static str,
    parameters: Value,
}

#[derive(Clone, Copy)]
pub enum Tool {
    Bash,
    Read,
    Write,
}

impl Tool {
    const ALL: [Self; 3] = [Self::Bash, Self::Read, Self::Write];

    fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }

    fn definition(self) -> ToolDef {
        let (description, parameters) = match self {
            Self::Bash => (
                "Run a bash command in the current directory.",
                json!({
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"],
                    "additionalProperties": false
                }),
            ),
            Self::Read => (
                "Read a UTF-8 text file by line number. Prefer this over cat for reading files.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "minLength": 1},
                        "offset": {"type": "integer", "minimum": 1, "default": 1},
                        "limit": {
                            "type": "integer", "minimum": 1,
                            "maximum": read::MAX_LINES, "default": read::MAX_LINES
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            ),
            Self::Write => (
                "Create or overwrite a file with the supplied text. Creates missing parent directories.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "minLength": 1},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                }),
            ),
        };
        ToolDef {
            kind: "function",
            function: FunctionDef {
                name: self.name(),
                description,
                parameters,
            },
        }
    }
}

pub fn definitions() -> Vec<ToolDef> {
    Tool::ALL.into_iter().map(Tool::definition).collect()
}

pub async fn execute(call: &ToolCall, cwd: &Path, stderr: &mut impl Write) -> Result<String> {
    match Tool::from_name(&call.function.name).context("Unknown tool in completed call")? {
        Tool::Bash => {
            let args = parse_bash_args(&call.function.arguments)?;
            bash::run(&args.command, cwd, stderr).await
        }
        Tool::Write => write::execute(&call.function.arguments, cwd, stderr),
        Tool::Read => {
            let args = match read::parse_args(&call.function.arguments) {
                Ok(args) => args,
                Err(error) => {
                    writeln!(stderr, "read: invalid arguments")
                        .context("Failed to write read diagnostic")?;
                    stderr.flush().context("Failed to flush read diagnostic")?;
                    return Ok(format!("Error: read arguments: {error:#}"));
                }
            };
            writeln!(stderr, "read {:?}", args.path).context("Failed to write read diagnostic")?;
            stderr.flush().context("Failed to flush read diagnostic")?;
            Ok(read::run(&args, cwd)
                .unwrap_or_else(|error| format!("Error: read {:?}: {error:#}", args.path)))
        }
    }
}
