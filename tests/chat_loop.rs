mod support;

use serde_json::{Value, json};
use std::fs;
use support::*;
use tokio::sync::oneshot;

#[tokio::test]
async fn two_turns_replay_exact_history_and_cwd_with_configured_wire_fields() {
    let (output, requests, cwd) = run(
        b"  hello \t\nremember me?\n/exit\nignored\n",
        vec![
            completion(&["Hello!", "\nHow can ", "I help?"]),
            answer("Yes."),
        ],
        |command| {
            command.env("OPENAI_MODEL", "mock-model");
        },
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Hello!\nHow can I help?\nYes.\n");
    assert_eq!(stderr(&output), "> > > ");
    assert_eq!(requests.len(), 2);
    let system = json!({"role": "system", "content": format!(
        "You are a coding assistant.\nBe concise and direct.\nHelp the user with programming tasks.\nWork in the current working directory: {}\nBe clear about what you know and what you have actually done.",
        cwd.display()
    )});
    let first_user = json!({"role": "user", "content": "  hello \t"});
    assert_eq!(requests[0].body["messages"], json!([system, first_user]));
    assert_eq!(
        requests[1].body["messages"],
        json!([
            system,
            first_user,
            {"role": "assistant", "content": "Hello!\nHow can I help?"},
            {"role": "user", "content": "remember me?"}
        ])
    );
    for request in requests {
        assert_eq!(request.start_line, "POST /v1/chat/completions HTTP/1.1\r\n");
        assert!(
            request
                .headers
                .contains(&("authorization".to_owned(), "Bearer dummy-key".to_owned()))
        );
        assert_eq!(request.body["model"], "mock-model");
        assert_eq!(request.body["stream"], true);
        assert_eq!(
            request.body["tools"],
            json!([{
                "type": "function",
                "function": {
                    "name": "bash",
                    "description": "Run a bash command in the current directory.",
                    "parameters": {
                        "type": "object",
                        "properties": {"command": {"type": "string"}},
                        "required": ["command"]
                    }
                }
            }])
        );
        assert_eq!(
            request.body["stream_options"],
            json!({"include_usage": true})
        );
    }
}

#[tokio::test]
async fn blank_lines_crlf_slash_text_and_final_unterminated_line() {
    let (output, requests, _) = run(
        b"\n \t\r\n/help\r\n final \t",
        vec![answer("help"), answer("final")],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "help\nfinal\n");
    assert_eq!(stderr(&output), "> > > > > ");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body["model"], "gpt-4.1");
    assert_eq!(requests[0].body["messages"][1]["content"], "/help");
    assert_eq!(requests[1].body["messages"][3]["content"], " final \t");
}

#[tokio::test]
async fn exit_and_eof_are_successful_without_requests() {
    for input in ["", "/exit\nignored\n", " \t/exit \t\r\nignored\n", "/exit"] {
        let (output, requests, _) = run(input.as_bytes(), vec![], |_| {}).await;
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(stdout(&output), "");
        assert_eq!(stderr(&output), "> ");
        assert!(requests.is_empty());
    }
}

#[tokio::test]
async fn positional_arguments_are_rejected_before_prompt_or_request() {
    let (output, requests, _) = run(b"", vec![], |command| {
        command.arg("say hi");
    })
    .await;
    assert!(!output.status.success());
    assert_eq!(stdout(&output), "");
    assert!(stderr(&output).contains("Positional arguments are not supported"));
    assert!(!stderr(&output).contains("> "));
    assert!(requests.is_empty());
}

#[tokio::test]
async fn missing_and_blank_keys_fail_before_prompt_or_request() {
    for key in [None, Some(""), Some(" \t\n")] {
        let (output, requests, _) = run(b"", vec![], |command| {
            if let Some(key) = key {
                command.env("OPENAI_API_KEY", key);
            } else {
                command.env_remove("OPENAI_API_KEY");
            }
        })
        .await;
        assert!(!output.status.success());
        assert_eq!(stdout(&output), "");
        assert!(stderr(&output).contains(if key.is_none() {
            "OPENAI_API_KEY is required"
        } else {
            "OPENAI_API_KEY must not be empty"
        }));
        assert!(!stderr(&output).contains("> "));
        assert!(requests.is_empty());
    }
}

#[tokio::test]
async fn default_configuration_starts_without_network_and_bad_url_fails_before_prompt() {
    let (output, requests, _) = run(b"", vec![], |command| {
        command.env_remove("OPENAI_BASE_URL");
    })
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "> ");
    assert!(requests.is_empty());

    let (output, requests, _) = run(b"", vec![], |command| {
        command.env("OPENAI_BASE_URL", "invalid-url");
    })
    .await;
    assert!(!output.status.success());
    assert!(stderr(&output).contains("Invalid OPENAI_BASE_URL"));
    assert!(!stderr(&output).contains("> "));
    assert!(requests.is_empty());
}

#[tokio::test]
async fn empty_reply_is_printed_and_retained_and_only_first_choice_is_used() {
    let reply = Reply::Http(
        200,
        format!(
            "{}data: [DONE]\n\n",
            event(json!({"choices": [
                {"delta": {"role": "assistant", "content": ""}, "finish_reason": "stop"},
                {"delta": {"content": "ignore this"}, "finish_reason": "length"}
            ]}))
        ),
    );
    let (output, requests, _) = run(b"first\nsecond\n", vec![reply, answer("ok")], |_| {}).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "\nok\n");
    assert_eq!(stderr(&output), "> > > ");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].body["messages"][2],
        json!({"role": "assistant", "content": ""})
    );
}

#[tokio::test]
async fn second_turn_failures_preserve_first_reply_and_do_not_submit_third_turn() {
    let cases = [
        (
            Reply::Http(429, "mock rate limit body".to_owned()),
            vec!["429", "mock rate limit body"],
        ),
        (
            Reply::Http(200, "data: not json\n\n".to_owned()),
            vec!["Failed to parse Chat Completions stream chunk"],
        ),
        (
            Reply::Http(200, event(json!({"choices": []}))),
            vec!["API stream chunk contained no choices or usage"],
        ),
        (
            Reply::Http(200, event(json!({"choices": [], "usage": null}))),
            vec!["API stream chunk contained no choices or usage"],
        ),
        (
            Reply::Http(
                200,
                format!(
                    "{}data: [DONE]\n\n",
                    event(json!({"choices": [
                        {"delta": {"role": "assistant"}, "finish_reason": "length"}
                    ]}))
                ),
            ),
            vec!["no message content", "finish_reason: length"],
        ),
        (
            Reply::Http(
                200,
                format!(
                    "{}data: [DONE]\n\n",
                    event(json!({"choices": [
                        {"delta": {"content": null}}
                    ]}))
                ),
            ),
            vec!["no message content", "finish_reason: unknown"],
        ),
        (
            Reply::Disconnect,
            vec!["Failed to send Chat Completions request"],
        ),
    ];
    for (reply, expected_errors) in cases {
        let (output, requests, _) = run(
            b"first\nsecond\nthird\n",
            vec![answer("first reply"), reply],
            |_| {},
        )
        .await;
        assert!(!output.status.success());
        assert_eq!(stdout(&output), "first reply\n");
        assert!(stderr(&output).starts_with("> > Error: "));
        for expected in expected_errors {
            assert!(stderr(&output).contains(expected), "{}", stderr(&output));
        }
        assert!(!stderr(&output).contains("dummy-key"));
        assert_eq!(requests.len(), 2);
    }
}

#[tokio::test]
async fn invalid_utf8_input_exits_with_context_without_request() {
    let (output, requests, _) = run(b"\xff\n", vec![], |_| {}).await;
    assert!(!output.status.success());
    assert_eq!(stdout(&output), "");
    assert!(stderr(&output).contains("Failed to read stdin"));
    assert!(requests.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn stdout_failure_exits_before_submitting_next_turn() {
    for (reply, error) in [
        ("reply".to_owned(), "Failed to flush assistant reply"),
        ("x".repeat(65536), "Failed to write assistant reply"),
        (String::new(), "Failed to write assistant reply"),
    ] {
        let (output, requests, _) = run(b"first\nsecond\n", vec![answer(&reply)], |command| {
            command.stdout(fs::File::options().write(true).open("/dev/full").unwrap());
        })
        .await;
        assert!(!output.status.success());
        assert!(stderr(&output).contains(error), "{}", stderr(&output));
        assert_eq!(requests.len(), 1);
    }
}

#[tokio::test]
async fn fragments_are_flushed_before_remainder_and_sse_boundaries_preserve_exact_history() {
    let prefix = " \tHello";
    let (proceed, after_first) = oneshot::channel();
    let first = format!(
        ": comment\n\n{}{}{}{}{}",
        event(json!({"choices": [{"delta": {"role": "assistant"}}]})),
        event(json!({"choices": [{"delta": {"content": null}}]})),
        event(json!({"choices": [{"delta": {}}]})),
        // No event name is required; unrelated fields are ignored.
        event(json!({"id": "ignored", "choices": [{"delta": {"refusal": null}}]})),
        text_event(prefix)
    )
    .replace('\n', "\r\n");
    let unicode_event = text_event(" 🌍").replacen("\"choices\":", "\"choices\":\ndata: ", 1);
    let tail = format!(
        ": keepalive\r\n\r\n{}{}{}{}data: [DONE]\r\n\r\n",
        unicode_event,
        text_event("\nend "),
        finish_event(),
        event(json!({"choices": [], "usage": {"total_tokens": 42}}))
    );
    let bytes = tail.into_bytes();
    let unicode = bytes
        .windows(4)
        .position(|window| window == "🌍".as_bytes())
        .unwrap();
    let cr = bytes.iter().position(|byte| *byte == b'\r').unwrap();
    // Separate HTTP chunks and network writes inside JSON, a UTF-8 character,
    // and CRLF. Later chunks also contain multiple complete events.
    let parts = vec![
        first.into_bytes(),
        bytes[..cr + 1].to_vec(),
        bytes[cr + 1..unicode - 1].to_vec(),
        bytes[unicode - 1..unicode + 1].to_vec(),
        bytes[unicode + 1..unicode + 3].to_vec(),
        bytes[unicode + 3..].to_vec(),
    ];
    let (output, requests, _) = run_with_handshake(
        b"first\nsecond\n/exit\n",
        vec![
            Reply::Stream {
                parts,
                after_first: Some(after_first),
                truncated: false,
            },
            answer("ok"),
        ],
        |_| {},
        Some((prefix.as_bytes(), proceed)),
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), " \tHello 🌍\nend \nok\n");
    assert_eq!(stderr(&output), "> > > ");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].body["messages"][2],
        json!({"role": "assistant", "content": " \tHello 🌍\nend "})
    );
}

#[tokio::test]
async fn malformed_chunk_shapes_are_rejected_before_next_turn() {
    for chunk in [
        json!({}),
        json!({"choices": null}),
        json!({"choices": {}}),
        json!({"choices": [{}]}),
        json!({"choices": [{"delta": null}]}),
        json!({"choices": [{"delta": {"content": 7}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": false}]}),
    ] {
        let (output, requests, _) = run(
            b"first\nsecond\n",
            vec![Reply::Http(200, event(chunk))],
            |_| {},
        )
        .await;
        assert!(!output.status.success());
        assert_eq!(stdout(&output), "");
        assert!(
            stderr(&output).contains("Failed to parse Chat Completions stream chunk"),
            "{}",
            stderr(&output)
        );
        assert_eq!(requests.len(), 1);
    }
}

#[tokio::test]
async fn partial_second_turn_failures_keep_flushed_text_without_newline_or_third_request() {
    let cases = [
        (
            Reply::Http(200, format!("{}data: bad json\n\n", text_event("partial"))),
            "Failed to parse Chat Completions stream chunk",
        ),
        // An in-stream provider error after HTTP 200 surfaces its message.
        (
            Reply::Http(
                200,
                format!(
                    "{}{}",
                    text_event("partial"),
                    event(json!({"error": {
                        "message": "The server had an error while processing your request.",
                        "type": "server_error"
                    }}))
                ),
            ),
            "The server had an error while processing your request.",
        ),
        (
            Reply::Http(200, format!("{}data: [DONE]\n\n", text_event("partial"))),
            "contained no finish_reason",
        ),
        (
            Reply::Http(200, format!("{}{}", text_event("partial"), finish_event())),
            "ended before [DONE]",
        ),
        (
            Reply::Http(200, text_event("partial")),
            "ended before [DONE]",
        ),
        // An unterminated event is never a completion marker.
        (
            Reply::Http(
                200,
                format!("{}{}data: [DONE]\n", text_event("partial"), finish_event()),
            ),
            "ended before [DONE]",
        ),
        (
            Reply::Stream {
                parts: vec![text_event("partial").into_bytes()],
                after_first: None,
                truncated: true,
            },
            "Failed to decode Chat Completions SSE stream",
        ),
        (
            Reply::Stream {
                parts: vec![
                    text_event("partial").into_bytes(),
                    b"data: \xff\n\n".to_vec(),
                ],
                after_first: None,
                truncated: false,
            },
            "Failed to decode Chat Completions SSE stream",
        ),
    ];
    for (reply, error) in cases {
        let (output, requests, _) = run(
            b"first\nsecond\nthird\n",
            vec![answer("complete"), reply],
            |_| {},
        )
        .await;
        assert!(!output.status.success());
        assert_eq!(stdout(&output), "complete\npartial");
        assert!(
            stderr(&output).starts_with("> > Error: "),
            "{}",
            stderr(&output)
        );
        assert!(stderr(&output).contains(error), "{}", stderr(&output));
        assert_eq!(requests.len(), 2);
    }
}

#[tokio::test]
async fn second_choice_cannot_supply_missing_first_choice_content_or_finish_reason() {
    for (choices, expected_output, error) in [
        (
            json!([
                {"delta": {"content": "first"}},
                {"delta": {"content": "ignored"}, "finish_reason": "stop"}
            ]),
            "first",
            "contained no finish_reason",
        ),
        (
            json!([
                {"delta": {}, "finish_reason": "length"},
                {"delta": {"content": "ignored"}, "finish_reason": "stop"}
            ]),
            "",
            "no message content (finish_reason: length)",
        ),
    ] {
        let body = format!("{}data: [DONE]\n\n", event(json!({"choices": choices})));
        let (output, requests, _) =
            run(b"first\nnext\n", vec![Reply::Http(200, body)], |_| {}).await;
        assert!(!output.status.success());
        assert_eq!(stdout(&output), expected_output);
        assert!(stderr(&output).contains(error), "{}", stderr(&output));
        assert_eq!(requests.len(), 1);
    }
}

#[tokio::test]
async fn first_non_null_finish_reason_is_retained_until_done() {
    let body = format!(
        "{}{}{}data: [DONE]\n\n",
        event(json!({"choices": [{"delta": {}, "finish_reason": "length"}]})),
        finish_event(),
        event(json!({"choices": [], "usage": {}}))
    );
    let (output, requests, _) = run(b"first\nnext\n", vec![Reply::Http(200, body)], |_| {}).await;
    assert!(!output.status.success());
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("no message content (finish_reason: length)"),
        "{}",
        stderr(&output)
    );
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn failure_after_observed_second_turn_fragment_never_submits_queued_third_turn() {
    let (proceed, after_first) = oneshot::channel();
    let (output, requests, _) = run_with_handshake(
        b"first\nsecond\nthird\n",
        vec![
            answer("complete"),
            Reply::Stream {
                parts: vec![
                    text_event("partial").into_bytes(),
                    b"data: malformed\n\n".to_vec(),
                ],
                after_first: Some(after_first),
                truncated: false,
            },
        ],
        |_| {},
        Some((b"complete\npartial", proceed)),
    )
    .await;
    assert!(!output.status.success());
    assert_eq!(stdout(&output), "complete\npartial");
    assert!(stderr(&output).starts_with("> > Error: "));
    assert!(stderr(&output).contains("Failed to parse Chat Completions stream chunk"));
    assert_eq!(requests.len(), 2);
}

#[tokio::test]
async fn bash_round_replays_exact_history() {
    // Whitespace in JSON arguments must survive decoding and history replay.
    let arguments = "{ \"command\" : \"printf hi\" }";
    let (output, requests, _) = run(
        b"say hi\nnext\n/exit\n",
        vec![
            tool_completion(&[tool_delta(0, Some("a"), Some("bash"), arguments)]),
            answer("Hi."),
            answer("Next."),
        ],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Hi.\nNext.\n");
    assert_eq!(stderr(&output), "> $ printf hi\nExit code: 0\nhi\n> > ");
    assert_eq!(requests.len(), 3);
    let mut expected = requests[0].body["messages"].as_array().unwrap().clone();
    assert_eq!(expected.len(), 2);
    expected.push(
        json!({"role": "assistant", "content": null, "tool_calls": [{
            "id": "a", "type": "function", "function": {"name": "bash", "arguments": arguments}
        }]}),
    );
    expected.push(json!({"role": "tool", "tool_call_id": "a", "content": "{\"output\":\"hi\",\"exit_code\":0}"}));
    assert_eq!(requests[1].body["messages"], json!(expected));
    expected.push(json!({"role": "assistant", "content": "Hi."}));
    expected.push(json!({"role": "user", "content": "next"}));
    assert_eq!(requests[2].body["messages"], json!(expected));
    for request in &requests {
        assert_eq!(request.body["tools"], requests[0].body["tools"]);
        assert_eq!(request.body["stream"], true);
        assert_eq!(
            request.body["stream_options"],
            json!({"include_usage": true})
        );
    }
}

#[tokio::test]
async fn multiple_calls_and_rounds_execute_in_numeric_order() {
    let (output, requests, _) = run_inspected(
        b"do work\n/exit\n",
        vec![
            tool_completion(&[
                tool_delta(7, Some("read"), Some("bash"), "{\"command\":\"cat "),
                bash_call(0, "write", "printf first > marker"),
                tool_delta(7, None, None, "marker\"}"),
            ]),
            tool_completion(&[bash_call(
                0,
                "append",
                "printf second >> marker; cat marker",
            )]),
            answer("Done."),
        ],
        |_| {},
        None,
        |cwd| {
            assert_eq!(
                fs::read_to_string(cwd.join("marker")).unwrap(),
                "firstsecond"
            )
        },
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Done.\n");
    assert_eq!(requests.len(), 3);
    let first = &requests[1].body["messages"];
    assert_eq!(first[2]["tool_calls"][0]["id"], "write");
    assert_eq!(first[2]["tool_calls"][1]["id"], "read");
    assert_eq!(first[3]["tool_call_id"], "write");
    assert_eq!(first[4]["tool_call_id"], "read");
    assert_eq!(
        tool_result(&requests[1], 3),
        json!({"output": "", "exit_code": 0})
    );
    assert_eq!(
        tool_result(&requests[1], 4),
        json!({"output": "first", "exit_code": 0})
    );
    let last = requests[2].body["messages"].as_array().unwrap();
    assert_eq!(&last[..5], first.as_array().unwrap());
    assert_eq!(last[5]["tool_calls"][0]["id"], "append");
    assert_eq!(last[6]["tool_call_id"], "append");
    assert_eq!(
        tool_result(&requests[2], 6),
        json!({"output": "firstsecond", "exit_code": 0})
    );
    assert_eq!(last.len(), 7);
    assert_eq!(
        stderr(&output),
        "> $ printf first > marker\nExit code: 0\n$ cat marker\nExit code: 0\nfirst\n$ printf second >> marker; cat marker\nExit code: 0\nfirstsecond\n> "
    );
}

#[tokio::test]
async fn mixed_text_and_tool_only_responses() {
    let body = format!(
        "{}{}{}{}{}data: [DONE]\n\n",
        text_event("Checking 雪…"),
        tool_delta(
            0,
            Some("escaped"),
            Some("bash"),
            "{\"command\":\"printf '%s' "
        ),
        tool_delta(0, None, None, "\\\"雪\\\"\"}"),
        tool_finish(),
        event(json!({"choices": [], "usage": {"total_tokens": 8}}))
    );
    let parts = body.as_bytes().chunks(1).map(|b| b.to_vec()).collect();
    let (output, requests, _) = run(
        b"look\n/exit\n",
        vec![
            Reply::Stream {
                parts,
                after_first: None,
                truncated: false,
            },
            tool_completion(&[text_event(""), bash_call(0, "empty", "")]),
            tool_completion(&[bash_call(0, "only", "true")]),
            answer("雪 found"),
        ],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Checking 雪…\n\n雪 found\n");
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[1].body["messages"][2]["content"], "Checking 雪…");
    assert_eq!(
        tool_result(&requests[1], 3),
        json!({"output": "雪", "exit_code": 0})
    );
    assert_eq!(requests[2].body["messages"][4]["content"], "");
    assert!(requests[3].body["messages"][6]["content"].is_null());
}

#[tokio::test]
async fn invalid_batch_never_executes() {
    let first = bash_call(0, "valid", "touch marker");
    let valid_second = json!({"index": 1, "id": "second", "type": "function", "function": {
        "name": "bash", "arguments": "{\"command\":\"touch other\"}"
    }});
    let mut invalid = Vec::new();
    for field in ["index", "id", "type", "function"] {
        let mut bad = valid_second.clone();
        bad.as_object_mut().unwrap().remove(field);
        invalid.push(bad);
    }
    for (field, value) in [
        ("index", json!(-1)),
        ("id", json!("valid")),
        ("id", json!(" ")),
        ("type", json!("other")),
    ] {
        let mut bad = valid_second.clone();
        bad[field] = value;
        invalid.push(bad);
    }
    for name in [Value::Null, json!(""), json!("other")] {
        let mut bad = valid_second.clone();
        bad["function"]["name"] = name;
        invalid.push(bad);
    }
    for args in ["{", "{}", "{\"command\":7}", "[\"true\"]"] {
        let mut bad = valid_second.clone();
        bad["function"]["arguments"] = json!(args);
        invalid.push(bad);
    }
    let mut bodies: Vec<Vec<u8>> = invalid
        .into_iter()
        .map(|bad| {
            format!(
                "{}{}{}data: [DONE]\n\n",
                first,
                event(json!({"choices": [{"delta": {"tool_calls": [bad]}}]})),
                tool_finish()
            )
            .into_bytes()
        })
        .collect();
    for metadata in [
        json!({"index": 0, "id": "conflict"}),
        json!({"index": 0, "type": "other"}),
        json!({"index": 0, "function": {"name": "other"}}),
    ] {
        bodies.push(
            format!(
                "{}{}{}data: [DONE]\n\n",
                first,
                event(json!({"choices": [{"delta": {"tool_calls": [metadata]}}]})),
                tool_finish()
            )
            .into_bytes(),
        );
    }
    for tail in [
        format!("{}data: [DONE]\n\n", finish_event()),
        "data: [DONE]\n\n".to_owned(),
        tool_finish(),
        format!("{}data: malformed\n\ndata: [DONE]\n\n", tool_finish()),
        format!("{}{}data: [DONE]\n\n", finish_event(), tool_finish()),
    ] {
        bodies.push(format!("{first}{tail}").into_bytes());
    }
    let mut invalid_sse = format!("{first}{}", tool_finish()).into_bytes();
    invalid_sse.extend_from_slice(b"data: \xff\n\ndata: [DONE]\n\n");
    bodies.push(invalid_sse);
    // The finish reason alone cannot manufacture a batch, even with text present.
    bodies.push(format!("{}data: [DONE]\n\n", tool_finish()).into_bytes());
    for (case, body) in bodies.into_iter().enumerate() {
        let (output, requests, _) = run_inspected(
            b"run\nqueued\n",
            vec![Reply::Stream {
                parts: vec![text_event("before").into_bytes(), body],
                after_first: None,
                truncated: false,
            }],
            |_| {},
            None,
            |cwd| {
                assert!(!cwd.join("marker").exists(), "case {case}");
                assert!(!cwd.join("other").exists(), "case {case}");
            },
        )
        .await;
        assert!(!output.status.success(), "case {case}: {}", stderr(&output));
        assert_eq!(stdout(&output), "before", "case {case}");
        assert!(!stderr(&output).contains("$ "), "case {case}");
        assert!(stderr(&output).contains("Error:"), "case {case}");
        assert_eq!(requests.len(), 1, "case {case}");
    }
}

#[tokio::test]
async fn nonzero_exit_returns_to_model() {
    let (output, requests, _) = run(
        b"fail\n/exit\n",
        vec![
            tool_completion(&[bash_call(0, "a", "printf out; printf err >&2; exit 7")]),
            answer("Command failed with 7."),
        ],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Command failed with 7.\n");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        tool_result(&requests[1], 3),
        json!({"output": "outerr", "exit_code": 7})
    );
}

#[tokio::test]
async fn tool_stdin_does_not_consume_next_turn() {
    // Exceed BufRead's prefetch so inherited child stdin would steal pending input.
    let queued = format!("queued {}", "x".repeat(32768));
    let input = format!("cat\n{queued}\n/exit\n");
    let (output, requests, _) = run(
        input.as_bytes(),
        vec![
            tool_completion(&[bash_call(0, "stdin", "cat")]),
            answer("EOF."),
            answer("Queued."),
        ],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "EOF.\nQueued.\n");
    assert_eq!(requests.len(), 3);
    assert_eq!(
        tool_result(&requests[1], 3),
        json!({"output": "", "exit_code": 0})
    );
    assert_eq!(
        requests[2].body["messages"][5],
        json!({"role": "user", "content": queued})
    );
}

#[tokio::test]
async fn preview_is_bounded_but_result_is_complete() {
    for (command, full, preview) in [
        (
            "for ((i=0;i<12;i++)); do printf '雪\\n'; done",
            "雪\n".repeat(12),
            "雪\n".repeat(10),
        ),
        (
            "for ((i=0;i<12;i++)); do printf '雪%.0s' {1..300}; printf '\\n'; done",
            format!("{}\n", "雪".repeat(300)).repeat(12),
            format!(
                "{}{}",
                format!("{}\n", "雪".repeat(300)).repeat(6),
                "雪".repeat(194)
            ),
        ),
    ] {
        let (output, requests, _) = run(
            b"large\n/exit\n",
            vec![
                tool_completion(&[bash_call(0, "large", command)]),
                answer("Complete."),
            ],
            |_| {},
        )
        .await;
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(stdout(&output), "Complete.\n");
        assert_eq!(requests.len(), 2);
        assert_eq!(
            tool_result(&requests[1], 3),
            json!({"output": full, "exit_code": 0})
        );
        let stderr = stderr(&output);
        let body = stderr.split_once("Exit code: 0\n").unwrap().1;
        let actual = body.split_once("[output preview truncated]\n").unwrap().0;
        let expected = if preview.ends_with('\n') {
            preview
        } else {
            format!("{preview}\n")
        };
        assert_eq!(actual, expected);
        assert!(actual.trim_end_matches('\n').chars().count() <= 2000);
        assert!(actual.lines().count() <= 10);
    }
}

#[tokio::test]
async fn failure_after_a_tool_round_stops_queued_input() {
    for (reply, expected) in [
        (
            Reply::Http(503, "provider unavailable".to_owned()),
            "Checking.\n",
        ),
        (
            Reply::Http(200, format!("{}data: malformed\n\n", text_event("partial"))),
            "Checking.\npartial",
        ),
    ] {
        let (output, requests, _) = run(
            b"work\nqueued\n",
            vec![
                tool_completion(&[text_event("Checking."), bash_call(0, "a", "printf hi")]),
                reply,
            ],
            |_| {},
        )
        .await;
        assert!(!output.status.success());
        assert_eq!(stdout(&output), expected);
        assert!(stderr(&output).contains("hi\nError:"));
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].body["messages"].as_array().unwrap().len(), 4);
    }
}

#[tokio::test]
async fn bash_does_not_inherit_api_key_but_http_remains_authorized() {
    let (output, requests, _) = run(
        b"check environment\n/exit\n",
        vec![
            tool_completion(&[bash_call(
                0,
                "env",
                "if [ \"${OPENAI_API_KEY+x}\" = x ]; then printf present; else printf absent; fi",
            )]),
            answer("Checked."),
        ],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(requests.len(), 2);
    assert_eq!(
        tool_result(&requests[1], 3),
        json!({"output": "absent", "exit_code": 0})
    );
    assert_eq!(stdout(&output), "Checked.\n");
    assert!(!stderr(&output).contains("dummy-key"));
    for request in requests {
        assert!(
            request
                .headers
                .contains(&("authorization".to_owned(), "Bearer dummy-key".to_owned()))
        );
    }
}
