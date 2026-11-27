//! Session tests: usage, prices, resume, and Claude, Codex and Qwen turns against fake CLIs.
//!
//! Python patched `asyncio.create_subprocess_exec` and the claude SDK; here the tests run fake provider
//! executables (shell scripts, so the process tests are `#[cfg(unix)]`) and, for the Codex request plumbing, a
//! test-only request hook.

use super::claude_sdk::{Block, ClaudeOptions, Message, UserContent, parse_message};
use super::*;

type Events = Arc<Mutex<Vec<(String, Value)>>>;

fn recorder() -> (Emit, Events) {
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    (
        Arc::new(move |kind: &str, data: Value| lock(&sink).push((kind.to_string(), data))),
        events,
    )
}

fn deny() -> Approve {
    Arc::new(|_: &str, _: &Value| Decision::No)
}

fn kinds(events: &Events, kind: &str) -> Vec<Value> {
    lock(events)
        .iter()
        .filter(|(k, _)| k == kind)
        .map(|(_, v)| v.clone())
        .collect()
}

fn assert_close(left: f64, right: f64) {
    assert!((left - right).abs() < 1e-9, "{left} != {right}");
}

// ---- usage parsing (UsageParsingTests) ----

#[test]
fn a_claude_response_is_priced_from_its_tokens_while_the_turn_runs() {
    let usage = json!({
        "input_tokens": 1_000_000,
        "output_tokens": 100_000,
        "cache_read_input_tokens": 1_000_000,
        "cache_creation_input_tokens": 100_000,
    });
    assert_close(
        claude_cost(Some("claude-opus-5-5"), &usage),
        4.0 + 2.0 + 0.2 + 0.5,
    );
    assert_close(
        claude_cost(Some("sonnet"), &json!({"output_tokens": 1_000_000})),
        10.0,
    );
    assert_eq!(claude_cost(Some("gpt-6.1-mini"), &usage), 0.0);
    let (emit, events) = recorder();
    let session = ClaudeSession::new(
        Path::new("."),
        json!({}),
        SessionOptions::default(),
        emit,
        deny(),
    );
    let message = Message::Assistant {
        content: vec![Block::Text("hi".into())],
        model: "claude-haiku-5-5".into(),
        usage: Some(usage),
        message_id: Some("m1".into()),
        uuid: None,
    };
    session
        .inner
        .handle(message, &mut HandleState::default())
        .unwrap();
    let first = lock(&events)[0].clone();
    assert_eq!(first.0, "spend");
    assert_eq!(first.1["id"], "m1");
    assert_close(
        first.1["dollars"].as_f64().unwrap(),
        0.1 + 0.05 + 0.01 + 0.0125,
    );
}

#[test]
fn test_claude_and_codex_windows() {
    let info = RateLimitInfo {
        raw: json!({"unifiedWindows": {"five_hour": {"utilization": 0.18, "resetsAt": 1}, "seven_day": {"utilization": 0.3}}}),
        utilization: None,
        rate_limit_type: Some("five_hour".into()),
        resets_at: Some(json!(1)),
    };
    let windows: Vec<_> = claude_usage(&info)
        .iter()
        .map(|w| (w.window.clone(), w.percent.round() as i64))
        .collect();
    assert_eq!(windows, [("5h".to_string(), 18), ("7d".to_string(), 30)]);
    let snapshot = json!({
        "primary": {"usedPercent": 12, "windowDurationMins": 300},
        "secondary": {"usedPercent": 40, "windowDurationMins": 10080},
    });
    let windows: Vec<_> = codex_usage(&snapshot)
        .iter()
        .map(|w| (w.window.clone(), w.percent))
        .collect();
    assert_eq!(
        windows,
        [("5h".to_string(), 12.0), ("7d".to_string(), 40.0)]
    );
    assert_eq!(codex_usage(&Value::Null), []);
}

#[test]
fn a_single_rate_limit_without_unified_windows_uses_its_own_fields() {
    let info = RateLimitInfo {
        raw: json!({}),
        utilization: Some(0.5),
        rate_limit_type: Some("seven_day_opus".into()),
        resets_at: Some(json!("1970-01-01T00:00:10Z")),
    };
    assert_eq!(
        claude_usage(&info),
        [UsageWindow {
            window: "7d opus".into(),
            percent: 50.0,
            resets_at: Some(10.0)
        }]
    );
    assert_eq!(window_label(Some(0.0)), "");
    assert_eq!(window_label(None), "");
    assert_eq!(window_label(Some(150.0)), "2h"); // Python rounds half to even.
}

#[test]
fn test_claude_get_usage_answer() {
    let answer = json!({
        "rate_limits_available": true,
        "rate_limits": {
            "five_hour": {"utilization": 18.0, "resets_at": "2026-11-09T03:40:00Z"},
            "seven_day": {"utilization": 30.0, "resets_at": null},
            "seven_day_opus": null,
        },
    });
    let windows = claude_plan_usage(&answer).unwrap();
    let shown: Vec<_> = windows
        .iter()
        .map(|w| (w.window.clone(), w.percent))
        .collect();
    assert_eq!(shown, [("5h".to_string(), 18.0), ("7d".to_string(), 30.0)]);
    assert_eq!(windows[0].resets_at, Some(1794195600.0));
    assert_eq!(windows[1].resets_at, None);
    assert_eq!(
        claude_plan_usage(&json!({"rate_limits_available": false, "rate_limits": null})),
        Some(Vec::new())
    );
    assert_eq!(
        claude_plan_usage(&json!({"rate_limits_available": true, "rate_limits": null})),
        None
    );
    assert_eq!(claude_plan_usage(&json!("unexpected")), None);
}

// ---- the Claude protocol pieces that run everywhere ----

#[test]
fn claude_cli_arguments_match_what_the_sdk_builds_for_donut() {
    let options = ClaudeOptions {
        model: Some("claude-opus-5-5".into()),
        setting_sources: Some(vec!["user".into(), "project".into(), "local".into()]),
        permission_mode: Some("acceptEdits".into()),
        include_partial_messages: true,
        resume: Some("claude-sid".into()),
        effort: Some("high".into()),
        extra_args: vec![
            ("allow-dangerously-skip-permissions".into(), None),
            ("chrome".into(), None),
        ],
        ..ClaudeOptions::default()
    };
    let expected = [
        "--output-format",
        "stream-json",
        "--verbose",
        "--system-prompt",
        "",
        "--model",
        "claude-opus-5-5",
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
        "acceptEdits",
        "--resume=claude-sid",
        "--include-partial-messages",
        "--setting-sources=user,project,local",
        "--allow-dangerously-skip-permissions",
        "--chrome",
        "--effort",
        "high",
        "--input-format",
        "stream-json",
    ];
    assert_eq!(options.build_args(true).unwrap(), expected);
    let bare = ClaudeOptions {
        setting_sources: Some(Vec::new()),
        extra_args: vec![
            ("strict-mcp-config".into(), None),
            ("tools".into(), Some(String::new())),
            ("x".into(), Some("-y".into())),
        ],
        ..ClaudeOptions::default()
    };
    assert_eq!(
        bare.build_args(true).unwrap(),
        [
            "--output-format",
            "stream-json",
            "--verbose",
            "--system-prompt",
            "",
            "--permission-prompt-tool",
            "stdio",
            "--setting-sources=",
            "--strict-mcp-config",
            "--tools",
            "",
            "--x=-y",
            "--input-format",
            "stream-json",
        ]
    );
}

#[test]
fn claude_messages_parse_into_the_kinds_donut_handles() {
    let assistant = json!({
        "type": "assistant",
        "uuid": "u1",
        "message": {
            "model": "claude-haiku-5-5",
            "id": "m1",
            "usage": {"output_tokens": 3},
            "content": [
                {"type": "text", "text": "hi"},
                {"type": "thinking", "thinking": "hm", "signature": "s"},
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}},
                {"type": "server_tool_use", "id": "t2", "name": "x", "input": {}},
            ],
        },
    });
    let Some(Message::Assistant {
        content,
        model,
        message_id,
        uuid,
        ..
    }) = parse_message(&assistant).unwrap()
    else {
        panic!("not an assistant message");
    };
    assert_eq!(
        (model.as_str(), message_id.as_deref(), uuid.as_deref()),
        ("claude-haiku-5-5", Some("m1"), Some("u1"))
    );
    assert_eq!(content.len(), 4);
    assert_eq!(
        content[2],
        Block::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: json!({"command": "ls"})
        }
    );
    assert_eq!(content[3], Block::Other);

    let echo = json!({"type": "user", "message": {"role": "user", "content": "Set model to x"}});
    assert!(matches!(
        parse_message(&echo).unwrap(),
        Some(Message::User {
            content: UserContent::Text(_),
            ..
        })
    ));
    let results = json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok", "is_error": true}]}});
    let Some(Message::User {
        content: UserContent::Blocks(blocks),
        ..
    }) = parse_message(&results).unwrap()
    else {
        panic!("not a user message");
    };
    assert_eq!(
        blocks,
        [Block::ToolResult {
            tool_use_id: "t1".into(),
            content: json!("ok"),
            is_error: Some(true)
        }]
    );

    let started = json!({"type": "system", "subtype": "task_started", "task_id": "k", "description": "d", "uuid": "u", "session_id": "s", "task_type": "local_bash"});
    assert!(
        matches!(parse_message(&started).unwrap(), Some(Message::TaskStarted { task_type: Some(t), .. }) if t == "local_bash")
    );
    let updated = json!({"type": "system", "subtype": "task_updated", "task_id": "k", "patch": {"status": "completed"}});
    assert!(
        matches!(parse_message(&updated).unwrap(), Some(Message::TaskUpdated { status: Some(s), .. }) if s == "completed")
    );
    let note = json!({"type": "system", "subtype": "task_notification", "task_id": "k", "status": "completed", "output_file": "f", "summary": "done", "uuid": "u", "session_id": "s"});
    assert!(
        matches!(parse_message(&note).unwrap(), Some(Message::TaskNotification { summary, .. }) if summary == "done")
    );
    let progress = json!({"type": "system", "subtype": "task_progress", "task_id": "k"});
    assert!(matches!(
        parse_message(&progress).unwrap(),
        Some(Message::System { .. })
    ));
    let result = json!({"type": "result", "subtype": "success", "duration_ms": 1, "duration_api_ms": 1, "is_error": false, "num_turns": 1, "session_id": "s", "total_cost_usd": 0.5});
    assert!(
        matches!(parse_message(&result).unwrap(), Some(Message::Result(r)) if r.session_id == "s" && r.total_cost_usd == Some(0.5))
    );
    let rate = json!({"type": "rate_limit_event", "uuid": "u", "session_id": "s", "rate_limit_info": {"status": "allowed", "utilization": 0.2, "rateLimitType": "five_hour", "resetsAt": 5}});
    let Some(Message::RateLimit(info)) = parse_message(&rate).unwrap() else {
        panic!("not a rate limit")
    };
    assert_eq!(
        (info.utilization, info.rate_limit_type.as_deref()),
        (Some(0.2), Some("five_hour"))
    );
    let stream = json!({"type": "stream_event", "uuid": "u", "session_id": "s", "event": {}});
    assert!(matches!(
        parse_message(&stream).unwrap(),
        Some(Message::Other)
    ));
    assert_eq!(
        parse_message(&json!({"type": "something_new"})).unwrap(),
        None
    );
    assert!(
        parse_message(&json!({"type": "result"}))
            .unwrap_err()
            .contains("Missing required field in result message")
    );
    assert!(parse_message(&json!({"no": "type"})).is_err());
}

#[test]
fn test_qwen_and_claude_resume_ids() {
    let (emit, _) = recorder();
    let options = SessionOptions {
        resume_id: Some("qwen-sid".into()),
        ..SessionOptions::default()
    };
    let qwen = QwenSession::new(
        Path::new("."),
        json!({"engine": "ollama", "model": "m"}),
        options,
        emit,
    );
    qwen.connect().unwrap();
    assert_eq!(qwen.provider_id().as_deref(), Some("qwen-sid"));

    let (emit, _) = recorder();
    let options = SessionOptions {
        resume_id: Some("claude-sid".into()),
        ..SessionOptions::default()
    };
    let claude = ClaudeSession::new(Path::new("."), json!({}), options, emit, deny());
    assert_eq!(claude.inner.options().resume.as_deref(), Some("claude-sid"));
}

#[test]
fn claude_options_follow_the_profile_and_the_session_flags() {
    let (emit, _) = recorder();
    let profile =
        json!({"model": "claude-opus-5-5", "effort": "low", "claude_config_dir": "/tmp/cfg"});
    let session = ClaudeSession::new(
        Path::new("/proj"),
        profile.clone(),
        SessionOptions::default(),
        Arc::clone(&emit),
        deny(),
    );
    let options = session.inner.options();
    assert_eq!(options.permission_mode.as_deref(), Some("default"));
    assert_eq!(
        options.env,
        [("CLAUDE_CONFIG_DIR".to_string(), "/tmp/cfg".to_string())]
    );
    assert_eq!(
        options.setting_sources,
        Some(vec![
            "user".to_string(),
            "project".to_string(),
            "local".to_string()
        ])
    );
    assert_eq!(
        options.extra_args,
        [("allow-dangerously-skip-permissions".to_string(), None)]
    );
    assert_eq!(
        (options.model.as_deref(), options.effort.as_deref()),
        (Some("claude-opus-5-5"), Some("low"))
    );
    assert_eq!(options.max_buffer_size, MAX_MESSAGE_BYTES);

    let review = SessionOptions {
        permission: "full".into(),
        read_only: true,
        bare: true,
        ..SessionOptions::default()
    };
    let session = ClaudeSession::new(Path::new("/proj"), profile, review, emit, deny());
    let options = session.inner.options();
    assert_eq!(options.permission_mode.as_deref(), Some("plan")); // Review mode stays read-only.
    assert_eq!(options.setting_sources, Some(Vec::new()));
    assert_eq!(
        options.extra_args,
        [
            ("strict-mcp-config".to_string(), None),
            ("tools".to_string(), Some(String::new()))
        ]
    );
}

// ---- Codex through a fake app-server ----

#[cfg(unix)]
mod fake {
    use std::os::unix::fs::PermissionsExt;

    pub fn script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/bash\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}

/// Spawning a script that was written a moment ago can meet ETXTBSY when another test forks at that time.
#[cfg(unix)]
fn connect_retry(session: &dyn Session) {
    for attempt in 0..20 {
        match session.connect() {
            Ok(()) => return,
            Err(error) if attempt < 19 && error.to_string().contains("busy") => {
                thread::sleep(Duration::from_millis(50))
            }
            Err(error) => panic!("connect failed: {error}"),
        }
    }
}

#[cfg(unix)]
const FAKE_CODEX: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -E 's/^\{"id":([0-9]+),.*$/\1/')
  case "$line" in
    *'"method":"initialize"'*) echo "{\"id\":$id,\"result\":{}}" ;;
    *'"method":"model/list"'*)
      desc=$(head -c 100000 /dev/zero | tr '\0' x)
      echo "{\"id\":$id,\"result\":{\"data\":[{\"model\":\"test\",\"displayName\":\"Test model\",\"description\":\"$desc\"}]}}" ;;
    *'"method":"account/rateLimits/read"'*) echo "{\"id\":$id,\"result\":{\"rateLimits\":{\"primary\":{\"usedPercent\":25}}}}" ;;
    *'"method":"thread/start"'*) echo "{\"id\":$id,\"result\":{\"thread\":{\"id\":\"persistent\"}}}" ;;
    *'"method":"turn/start"'*)
      case "$line" in *'"threadId":"persistent"'*) ;; *) exit 3 ;; esac
      echo "{\"id\":$id,\"result\":{}}"
      echo '{"id":999,"method":"item/commandExecution/requestApproval","params":{"command":"test"}}' ;;
    *'"decision":"decline"'*)
      echo '{"method":"item/agentMessage/delta","params":{"delta":"READY"}}'
      echo '{"method":"turn/completed","params":{"turn":{"status":"completed"}}}' ;;
  esac
done
"#;

#[cfg(unix)]
#[test]
fn test_codex_protocol_models_usage_approval_and_persistent_thread() {
    let temp = tempfile::tempdir().unwrap();
    let program = fake::script(temp.path(), "fake_codex", FAKE_CODEX);
    let (emit, events) = recorder();
    let approvals: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&approvals);
    let approve: Approve = Arc::new(move |name: &str, _: &Value| {
        lock(&seen).push(name.to_string());
        Decision::No
    });
    let session = CodexSession::new(
        temp.path(),
        json!({}),
        SessionOptions::default(),
        emit,
        approve,
    );
    session.set_program(&program);
    connect_retry(&session);
    let outcome = (|| -> Result<()> {
        assert_eq!(session.models()?[0].id, "test");
        assert_eq!(
            session.account_usage()?["rateLimits"]["primary"]["usedPercent"],
            25
        );
        session.query("first")?;
        session.query("second")?;
        Ok(())
    })();
    // Reported at connect for the usage bar.
    let end = Instant::now() + Duration::from_secs(5);
    while kinds(&events, "usage").is_empty() && Instant::now() < end {
        thread::sleep(Duration::from_millis(10));
    }
    let usage = kinds(&events, "usage");
    session.close().unwrap();
    outcome.unwrap();
    assert_eq!(
        *lock(&approvals),
        ["item/commandExecution/requestApproval"; 2]
    );
    assert_eq!(kinds(&events, "text"), [json!("READY"), json!("READY")]);
    assert_eq!(usage[0][0]["percent"], 25.0);
}

#[cfg(unix)]
#[test]
fn codex_answers_approvals_with_the_users_decision_and_refuses_unknown_requests() {
    let temp = tempfile::tempdir().unwrap();
    let body = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -E 's/^\{"id":([0-9]+),.*$/\1/')
  case "$line" in
    *'"method":"initialize"'*) echo "{\"id\":$id,\"result\":{}}" ;;
    *'"method":"account/rateLimits/read"'*) echo "{\"id\":$id,\"error\":{\"message\":\"nope\"}}" ;;
    *'"method":"thread/start"'*) echo "{\"id\":$id,\"result\":{\"thread\":{\"id\":\"t\"}}}" ;;
    *'"method":"turn/start"'*)
      echo "{\"id\":$id,\"result\":{}}"
      echo '{"id":7,"method":"item/fileChange/requestApproval","params":{}}' ;;
    *'"id":7,'*)
      printf '%s\n' "$line" > "$0.answer"
      echo '{"id":8,"method":"item/tool/call","params":{}}' ;;
    *'"id":8,'*)
      printf '%s\n' "$line" > "$0.refusal"
      echo '{"method":"turn/completed","params":{"turn":{"status":"failed","error":"it broke"}}}' ;;
  esac
done
"#;
    let program = fake::script(temp.path(), "fake_codex", body);
    let (emit, events) = recorder();
    let approve: Approve = Arc::new(|_: &str, _: &Value| Decision::Always);
    let session = CodexSession::new(
        temp.path(),
        json!({}),
        SessionOptions::default(),
        emit,
        approve,
    );
    session.set_program(&program);
    connect_retry(&session);
    let error = session.query("go").unwrap_err().to_string();
    assert_eq!(error, "it broke");
    session.close().unwrap();
    let answer = std::fs::read_to_string(format!("{}.answer", program.display())).unwrap();
    assert!(
        answer.contains(r#""decision":"acceptForSession""#),
        "{answer}"
    );
    let refusal = std::fs::read_to_string(format!("{}.refusal", program.display())).unwrap();
    assert!(refusal.contains("-32601"), "{refusal}");
    assert_eq!(
        kinds(&events, "error"),
        [json!(
            "Unsupported provider interaction: item/tool/call; use /native."
        )]
    );
    assert!(kinds(&events, "usage").is_empty()); // The rate limit read failed: the bar stays empty.
}

// ---- Codex request plumbing (ResumePlumbingTests, CodexSwitchTests, RealCodexThroughDonutTests) ----

type Sent = Arc<Mutex<Vec<(String, Value)>>>;

/// Make `request` answer from the test: `thread/start` and `thread/resume` get a thread, anything else ends the turn.
fn plumb(session: &CodexSession, thread_id: &'static str) -> Sent {
    let sent: Sent = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&sent);
    *lock(&session.inner.hook) = Some(Box::new(
        move |inner: &CodexInner, method: &str, params: &Value| {
            lock(&log).push((method.to_string(), params.clone()));
            match method {
                "thread/start" => Ok(json!({"thread": {"id": thread_id}})),
                "thread/resume" => Ok(json!({"thread": {"id": params["threadId"]}})),
                _ => {
                    inner.complete_turn(Ok(()));
                    Ok(json!({}))
                }
            }
        },
    ));
    sent
}

fn turns(sent: &Sent) -> Vec<Value> {
    lock(sent)
        .iter()
        .filter(|(m, _)| m == "turn/start")
        .map(|(_, p)| p.clone())
        .collect()
}

#[test]
fn test_codex_resumes_its_thread() {
    let (emit, _) = recorder();
    let options = SessionOptions {
        resume_id: Some("thread-9".into()),
        ..SessionOptions::default()
    };
    let session = CodexSession::new(
        Path::new("."),
        json!({"model": "gpt-6.1"}),
        options,
        emit,
        deny(),
    );
    let sent = plumb(&session, "unused");
    session.query("continue").unwrap();
    let sent_now = lock(&sent).clone();
    assert_eq!(sent_now[0].0, "thread/resume");
    assert_eq!(sent_now[0].1["threadId"], "thread-9");
    assert_eq!(sent_now[1].1["threadId"], "thread-9");
    assert_eq!(session.provider_id().as_deref(), Some("thread-9"));
}

#[test]
fn test_a_codex_reviewer_starts_its_own_thread() {
    // Donut's own bookkeeping must never touch the Codex thread id (it once set .thread to "review").
    let (emit, _) = recorder();
    let options = SessionOptions {
        read_only: true,
        ..SessionOptions::default()
    };
    let session = CodexSession::new(Path::new("."), json!({}), options, emit, deny());
    let sent = plumb(&session, "0b6f5c1e-1d4e-4c62-9a8e-2f1f6f6d7a10");
    session.query("https://github.com/x/y/pull/7").unwrap();
    session.query("and again").unwrap();
    let starts = lock(&sent)
        .iter()
        .filter(|(m, _)| m == "thread/start")
        .count();
    assert_eq!(starts, 1);
    let thread_ids: Vec<_> = turns(&sent).iter().map(|t| t["threadId"].clone()).collect();
    let id = json!("0b6f5c1e-1d4e-4c62-9a8e-2f1f6f6d7a10");
    assert_eq!(thread_ids, [id.clone(), id]);
    // A review session starts read-only whatever the permission mode says.
    let start = lock(&sent)
        .iter()
        .find(|(m, _)| m == "thread/start")
        .unwrap()
        .1
        .clone();
    assert_eq!(
        (start["approvalPolicy"].as_str(), start["sandbox"].as_str()),
        (Some("on-request"), Some("read-only"))
    );
}

#[test]
fn test_codex_switches_model_on_the_next_turn_of_the_same_thread() {
    let (emit, _) = recorder();
    let session = CodexSession::new(
        Path::new("."),
        json!({"model": "gpt-6.1"}),
        SessionOptions::default(),
        emit,
        deny(),
    );
    let sent = plumb(&session, "t");
    session.query("one").unwrap();
    session.switch_model(Some("gpt-6.1-mini")).unwrap();
    session.query("two").unwrap();
    session.query("three").unwrap();
    let turns = turns(&sent);
    assert!(turns[0].get("model").is_none());
    assert_eq!(turns[1]["model"], "gpt-6.1-mini");
    assert_eq!(turns[1]["threadId"], "t"); // The same thread, so its context stays.
    assert!(turns[2].get("model").is_none()); // Codex keeps an override for later turns.
}

#[test]
fn codex_sends_effort_once_and_permission_changes_on_the_next_turn() {
    let (emit, _) = recorder();
    let session = CodexSession::new(
        Path::new("."),
        json!({"effort": "high"}),
        SessionOptions::default(),
        emit,
        deny(),
    );
    let sent = plumb(&session, "t");
    session.set_permission("full").unwrap(); // No thread yet: the thread starts with it.
    session.query("one").unwrap();
    session.set_permission("accept_edits").unwrap();
    session.query("two").unwrap();
    session.set_effort("low").unwrap();
    session.query("three").unwrap();
    let start = lock(&sent)
        .iter()
        .find(|(m, _)| m == "thread/start")
        .unwrap()
        .1
        .clone();
    assert_eq!(
        (start["approvalPolicy"].as_str(), start["sandbox"].as_str()),
        (Some("never"), Some("danger-full-access"))
    );
    let turns = turns(&sent);
    assert_eq!(turns[0]["effort"], "high");
    assert!(turns[0].get("approvalPolicy").is_none());
    assert!(turns[1].get("effort").is_none());
    assert_eq!(turns[1]["approvalPolicy"], "on-request");
    assert_eq!(turns[1]["sandboxPolicy"], json!({"type": "workspaceWrite"}));
    assert_eq!(turns[2]["effort"], "low");
    assert_eq!(session.profile()["effort"], "low");
}

// ---- Claude through a fake CLI ----

#[cfg(unix)]
const FAKE_CLAUDE: &str = r#"
dir=$(dirname "$0")
printf '%s\n' "$@" > "$dir/args.txt"
env | grep -E '^(CLAUDE_CODE_ENTRYPOINT|CLAUDE_AGENT_SDK_VERSION|CLAUDE_CONFIG_DIR|CLAUDE_CODE_SDK_READS_SESSION_STATE|CLAUDECODE)=' | sort > "$dir/env.txt"
respond() { echo "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$1\",\"response\":$2}}"; }
while IFS= read -r line; do
  rid=$(printf '%s' "$line" | sed -E 's/.*"request_id":"([^"]*)".*/\1/')
  case "$line" in
    *'"subtype":"initialize"'*)
      respond "$rid" '{"commands":[{"name":"compact"}],"models":[{"value":"m1","displayName":"Model One"},{"value":"m2"}]}' ;;
    *'"subtype":"get_usage"'*)
      respond "$rid" '{"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":42.0,"resets_at":null}}}' ;;
    *'"subtype":"mcp_status"'*)
      printf '%s\n' "$line" >> "$dir/requests.log"
      respond "$rid" '{"mcpServers":[{"name":"notes","status":"connected"}]}' ;;
    *'"subtype":"get_context_usage"'*)
      printf '%s\n' "$line" >> "$dir/requests.log"
      respond "$rid" '{"totalTokens":10,"maxTokens":100,"categories":[]}' ;;
    *'"type":"control_request"'*)
      printf '%s\n' "$line" >> "$dir/requests.log"
      respond "$rid" '{}' ;;
    *'"type":"user"'*)
      echo '{"type":"system","subtype":"init","apiKeySource":"none","session_id":"sid-1"}'
      echo '{"type":"stream_event","uuid":"s","session_id":"sid-1","event":{}}'
      case "$line" in
        *fail*)
          echo '{"type":"result","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":true,"num_turns":1,"session_id":"sid-1","result":"boom"}'
          continue ;;
        *question*)
          echo '{"type":"control_request","request_id":"q1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Which color?","header":"Color","multiSelect":false,"options":[{"label":"Red","description":"warm"},{"label":"Blue","description":"cool"}]}]}}}'
          read -r answer
          printf '%s\n' "$answer" >> "$dir/answers.log" ;;
        *ask*)
          echo '{"type":"control_request","request_id":"p1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}'
          read -r answer
          printf '%s\n' "$answer" >> "$dir/answers.log" ;;
        *background*)
          echo '{"type":"system","subtype":"task_started","task_id":"k1","description":"sleep","uuid":"u","session_id":"sid-1","task_type":"local_bash"}'
          echo '{"type":"system","subtype":"task_updated","task_id":"k1","patch":{"status":"completed"}}' ;;
      esac
      echo '{"type":"rate_limit_event","uuid":"r","session_id":"sid-1","rate_limit_info":{"status":"allowed","utilization":0.25,"rateLimitType":"five_hour"}}'
      echo '{"type":"assistant","uuid":"u1","message":{"id":"m1","model":"claude-haiku-5-5","usage":{"output_tokens":1000000},"content":[{"type":"text","text":"hello"},{"type":"thinking","thinking":"hm","signature":"x"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}},{"type":"text","text":"again"}]}}'
      echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"a.txt"},{"type":"text","text":"b.txt"}],"is_error":false}]}}'
      echo '{"type":"result","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"sid-1","total_cost_usd":0.5,"usage":{"output_tokens":5}}' ;;
  esac
done
"#;

#[cfg(unix)]
fn claude(temp: &Path, options: SessionOptions, approve: Approve) -> (ClaudeSession, Events) {
    let program = fake::script(temp, "claude", FAKE_CLAUDE);
    let (emit, events) = recorder();
    let session = ClaudeSession::new(
        temp,
        json!({"model": "claude-opus-5-5", "claude_config_dir": "/tmp/donut-cfg"}),
        options,
        emit,
        approve,
    );
    session.set_program(program);
    (session, events)
}

#[cfg(unix)]
#[test]
fn claude_runs_a_turn_through_the_control_protocol() {
    let temp = tempfile::tempdir().unwrap();
    let options = SessionOptions {
        resume_id: Some("claude-sid".into()),
        chrome: true,
        ..SessionOptions::default()
    };
    let (session, events) = claude(temp.path(), options, deny());
    connect_retry(&session);

    let args = std::fs::read_to_string(temp.path().join("args.txt")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    for expected in [
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-prompt-tool",
        "stdio",
        "--resume=claude-sid",
        "--include-partial-messages",
        "--setting-sources=user,project,local",
        "--allow-dangerously-skip-permissions",
        "--chrome",
        "--input-format",
    ] {
        assert!(args.contains(&expected), "{expected} missing from {args:?}");
    }
    let env = std::fs::read_to_string(temp.path().join("env.txt")).unwrap();
    assert!(env.contains("CLAUDE_CODE_ENTRYPOINT=sdk-py"), "{env}");
    assert!(
        env.contains(&format!("CLAUDE_AGENT_SDK_VERSION={CLAUDE_SDK_VERSION}")),
        "{env}"
    );
    assert!(env.contains("CLAUDE_CONFIG_DIR=/tmp/donut-cfg"), "{env}");
    assert!(
        env.contains("CLAUDE_CODE_SDK_READS_SESSION_STATE=1"),
        "{env}"
    );

    assert_eq!(session.info()["commands"][0]["name"], "compact");
    let models = session.models().unwrap();
    assert_eq!(
        models,
        [
            ModelInfo {
                id: "m1".into(),
                name: "Model One".into()
            },
            ModelInfo {
                id: "m2".into(),
                name: "m2".into()
            }
        ]
    );

    session.query("hello there").unwrap();
    let seen: Vec<_> = lock(&events).iter().map(|(k, _)| k.clone()).collect();
    assert_eq!(
        seen,
        [
            "usage",
            "spend",
            "text",
            "thinking",
            "activity",
            "tool",
            "command",
            "break",
            "text",
            "command_output",
            "tool_result"
        ]
    );
    assert_eq!(kinds(&events, "text"), [json!("hello"), json!("again")]);
    assert_eq!(
        kinds(&events, "command"),
        [json!({"id": "t1", "command": "ls"})]
    );
    assert_eq!(
        kinds(&events, "tool_result"),
        [json!({"id": "t1", "output": "a.txt\nb.txt", "error": false})]
    );
    assert_close(kinds(&events, "spend")[0]["dollars"].as_f64().unwrap(), 0.5);
    assert_close(
        kinds(&events, "usage")[0][0]["percent"].as_f64().unwrap(),
        25.0,
    );
    assert_eq!(session.provider_id().as_deref(), Some("sid-1"));
    assert_eq!(session.account().as_deref(), Some("none"));
    assert_eq!(
        session.usage(),
        json!({"tokens": {"output_tokens": 5}, "reported_cost_usd": 0.5})
    );
    assert_eq!(
        session.account_usage().unwrap()["session"]["reported_cost_usd"],
        0.5
    );

    let windows = session.quota().unwrap();
    assert_eq!(
        (windows[0].window.as_str(), windows[0].percent),
        ("5h", 42.0)
    );

    session.set_permission("accept_edits").unwrap();
    session.switch_model(Some("claude-sonnet-5-5")).unwrap();
    session.set_effort("low").unwrap();
    session.interrupt().unwrap();
    assert_eq!(session.profile()["model"], "claude-sonnet-5-5");
    assert_eq!(session.profile()["effort"], "low");
    assert_eq!(session.permission(), "accept_edits");
    session.close().unwrap();
    session.close().unwrap();
    let requests = std::fs::read_to_string(temp.path().join("requests.log")).unwrap();
    for expected in [
        r#""subtype":"set_permission_mode","mode":"acceptEdits""#,
        r#""subtype":"set_model","model":"claude-sonnet-5-5""#,
        r#""subtype":"apply_flag_settings","settings":{"effortLevel":"low"}"#,
        r#""subtype":"interrupt""#,
    ] {
        assert!(
            requests.contains(expected),
            "{expected} missing from {requests}"
        );
    }
    // The session is closed now: no more operations reach the CLI.
    assert!(session.quota().is_none());
}

#[cfg(unix)]
#[test]
fn claude_control_sends_a_raw_control_request_and_returns_the_answer() {
    let temp = tempfile::tempdir().unwrap();
    let (session, _) = claude(temp.path(), SessionOptions::default(), deny());
    assert!(session.control("mcp_status", json!({})).is_err()); // Not connected yet.
    connect_retry(&session);
    assert_eq!(
        session.control("mcp_status", json!({})).unwrap(),
        json!({"mcpServers": [{"name": "notes", "status": "connected"}]})
    );
    assert_eq!(
        session.control("get_context_usage", Value::Null).unwrap()["totalTokens"],
        10
    );
    // Other requests still work, and params are merged next to the subtype.
    session
        .control("set_model", json!({"model": "claude-sonnet-5-5"}))
        .unwrap();
    assert!(session.control("mcp_status", json!("bad")).is_err());
    session.close().unwrap();
    let requests = std::fs::read_to_string(temp.path().join("requests.log")).unwrap();
    for expected in [
        r#""subtype":"mcp_status""#,
        r#""subtype":"get_context_usage""#,
        r#""subtype":"set_model","model":"claude-sonnet-5-5""#,
    ] {
        assert!(
            requests.contains(expected),
            "{expected} missing from {requests}"
        );
    }
}

#[cfg(unix)]
#[test]
fn claude_answers_tool_permission_requests_from_the_approve_callback() {
    for (decision, behavior) in [
        (Decision::Yes, "allow"),
        (Decision::Always, "allow"),
        (Decision::No, "deny"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let asked: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&asked);
        let approve: Approve = Arc::new(move |name: &str, input: &Value| {
            lock(&log).push((name.to_string(), input.clone()));
            decision
        });
        let (session, _) = claude(temp.path(), SessionOptions::default(), approve);
        connect_retry(&session);
        session.query("please ask first").unwrap();
        session.close().unwrap();
        assert_eq!(
            *lock(&asked),
            [("Bash".to_string(), json!({"command": "ls"}))]
        );
        let answer = std::fs::read_to_string(temp.path().join("answers.log")).unwrap();
        assert!(answer.contains(r#""request_id":"p1""#), "{answer}");
        assert!(
            answer.contains(&format!(r#""behavior":"{behavior}""#)),
            "{answer}"
        );
        if behavior == "allow" {
            assert!(
                answer.contains(r#""updatedInput":{"command":"ls"}"#),
                "{answer}"
            );
        } else {
            assert!(answer.contains(r#""message":"User declined""#), "{answer}");
        }
    }
}

#[cfg(unix)]
#[test]
fn claude_reports_background_tasks_and_a_failed_turn() {
    let temp = tempfile::tempdir().unwrap();
    let (session, events) = claude(temp.path(), SessionOptions::default(), deny());
    connect_retry(&session);
    session.query("run it in the background").unwrap();
    assert_eq!(
        kinds(&events, "task"),
        [
            json!({"id": "k1", "state": "running", "kind": "shell", "description": "sleep"}),
            json!({"id": "k1", "state": "completed"}),
        ]
    );
    assert_eq!(
        session.query("this will fail").unwrap_err().to_string(),
        "boom"
    );
    // A failed turn does not end the session.
    session.query("fine again").unwrap();
    session.close().unwrap();
}

#[cfg(unix)]
#[test]
fn test_claude_session_operations_run_one_at_a_time() {
    // Python kept every operation on one owner task. Here concurrent callers on several threads queue instead.
    let temp = tempfile::tempdir().unwrap();
    let (session, events) = claude(temp.path(), SessionOptions::default(), deny());
    connect_retry(&session);
    let session = Arc::new(session);
    let workers: Vec<_> = (0..3)
        .map(|n| {
            let session = Arc::clone(&session);
            thread::spawn(move || session.query(&format!("turn {n}")))
        })
        .collect();
    for worker in workers {
        worker.join().unwrap().unwrap();
    }
    // Each turn finished before the next began: text and its break marker never interleave.
    let order: Vec<_> = lock(&events)
        .iter()
        .map(|(k, _)| k.clone())
        .filter(|k| k == "text" || k == "break")
        .collect();
    assert_eq!(order, ["text", "break", "text"].repeat(3));
    session.close().unwrap();
}

#[cfg(unix)]
#[test]
fn claude_that_cannot_start_reports_why() {
    let temp = tempfile::tempdir().unwrap();
    let (emit, _) = recorder();
    let session = ClaudeSession::new(
        temp.path(),
        json!({}),
        SessionOptions::default(),
        emit,
        deny(),
    );
    session.set_program(temp.path().join("no-such-claude"));
    let error = session.connect().unwrap_err().to_string();
    assert!(error.contains("Claude Code not found at"), "{error}");
    // Nothing connected, so a query is refused instead of hanging.
    assert!(session.query("hi").is_err());
    assert!(session.close().is_ok());
}

#[cfg(unix)]
#[test]
fn claude_that_exits_during_a_turn_fails_the_turn() {
    let temp = tempfile::tempdir().unwrap();
    let body = r#"
while IFS= read -r line; do
  rid=$(printf '%s' "$line" | sed -E 's/.*"request_id":"([^"]*)".*/\1/')
  case "$line" in
    *'"subtype":"initialize"'*) echo "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$rid\",\"response\":{}}}" ;;
    *'"type":"user"'*)
      echo '{"type":"result","subtype":"error_max_turns","duration_ms":1,"duration_api_ms":1,"is_error":true,"num_turns":1,"session_id":"s","errors":["too many turns"]}'
      exit 1 ;;
  esac
done
"#;
    let program = fake::script(temp.path(), "claude", body);
    let (emit, _) = recorder();
    let session = ClaudeSession::new(
        temp.path(),
        json!({}),
        SessionOptions::default(),
        emit,
        deny(),
    );
    session.set_program(program);
    connect_retry(&session);
    let error = session.query("go").unwrap_err().to_string();
    // The error result arrives first and ends the turn; the exit that follows is reported by the next call.
    assert_eq!(error, "Claude turn failed");
    let next = session.query("again").unwrap_err().to_string();
    assert!(
        next.contains("too many turns")
            || next.contains("exit code")
            || next.contains("terminated")
            || next.contains("not ready for writing")
            || next.contains("Failed to write to process stdin"),
        "{next}"
    );
    session.close().unwrap();
}

// ---- Qwen ----

#[test]
fn qwen_reports_events_and_keeps_its_own_permission_rule() {
    let (emit, events) = recorder();
    let session = QwenSession::new(
        Path::new("."),
        json!({"engine": "ollama", "model": "m"}),
        SessionOptions::default(),
        emit,
    );
    assert_eq!(session.effective_permission(), "accept_edits");
    session.set_permission("full").unwrap();
    assert_eq!(session.effective_permission(), "full");
    assert!(session.quota().is_none());
    let mut wrote = false;
    let inner = &session.inner;
    for event in [
        json!({"type": "system", "session_id": "q1"}),
        json!({"type": "stream_event", "event": {"content_block": {"type": "text"}, "delta": {"type": "text_delta", "text": "a"}}}),
        json!({"type": "stream_event", "event": {"content_block": {"type": "text"}}}),
        json!({"type": "stream_event", "event": {"delta": {"type": "thinking_delta", "thinking": "t"}}}),
        json!({"type": "stream_event", "event": {"content_block": {"type": "tool_use", "id": "i", "name": "Read"}}}),
        json!({"type": "result", "usage": {"tokens": 3}}),
    ] {
        inner.handle_event(&event, &mut wrote).unwrap();
    }
    let seen: Vec<_> = lock(&events).iter().map(|(k, _)| k.clone()).collect();
    assert_eq!(seen, ["text", "break", "thinking", "activity", "tool"]);
    assert_eq!(session.provider_id().as_deref(), Some("q1"));
    assert_eq!(
        session.account_usage().unwrap(),
        json!({"session": {"tokens": 3}})
    );
    let failed = inner.handle_event(&json!({"type": "result", "is_error": true}), &mut wrote);
    assert_eq!(
        failed.unwrap_err().to_string(),
        "Qwen reported a failed turn"
    );
}

#[test]
fn new_session_picks_the_class_for_an_engine() {
    let (emit, _) = recorder();
    let make = |engine: &str| {
        new_session(
            engine,
            Path::new("."),
            json!({}),
            SessionOptions::default(),
            Arc::clone(&emit),
            deny(),
        )
    };
    assert_eq!(make("claude").unwrap().family(), "claude");
    assert_eq!(make("codex").unwrap().family(), "codex");
    assert_eq!(make("ollama").unwrap().family(), "qwen");
    assert_eq!(make("openai").unwrap().family(), "qwen");
    assert!(make("gemini").is_none());
}

#[test]
fn epoch_seconds_reads_numbers_and_iso_strings() {
    assert_eq!(epoch_seconds(&json!(5)), Some(5.0));
    assert_eq!(
        epoch_seconds(&json!("2026-11-09T03:40:00Z")),
        Some(1794195600.0)
    );
    assert_eq!(
        epoch_seconds(&json!("2026-11-09T03:40:00+01:00")),
        Some(1794192000.0)
    );
    assert_eq!(epoch_seconds(&json!("not a time")), None);
    assert_eq!(epoch_seconds(&json!("")), None);
    assert_eq!(epoch_seconds(&Value::Null), None);
}

#[cfg(unix)]
#[test]
fn claude_answers_ask_user_question_from_the_ask_callback_not_the_approval() {
    for (typed, behavior) in [(Some("Green"), "allow"), (None, "deny")] {
        let temp = tempfile::tempdir().unwrap();
        let seen: Arc<Mutex<Vec<Question>>> = Arc::default();
        let log = Arc::clone(&seen);
        let options = SessionOptions {
            ask: Some(Ask(Arc::new(move |questions: &[Question]| {
                lock(&log).extend(questions.iter().cloned());
                typed.map(|text| vec![(questions[0].id.clone(), vec![text.to_string()])])
            }))),
            ..SessionOptions::default()
        };
        let (session, _) = claude(temp.path(), options, deny());
        connect_retry(&session);
        session.query("a question please").unwrap();
        session.close().unwrap();
        let asked = lock(&seen).clone();
        assert_eq!(asked.len(), 1);
        assert_eq!(
            (
                asked[0].id.as_str(),
                asked[0].header.as_str(),
                asked[0].other
            ),
            ("Which color?", "Color", true)
        );
        assert_eq!(
            asked[0].options,
            [
                ("Red".to_string(), "warm".to_string()),
                ("Blue".to_string(), "cool".to_string())
            ]
        );
        let answer = std::fs::read_to_string(temp.path().join("answers.log")).unwrap();
        assert!(
            answer.contains(&format!(r#""behavior":"{behavior}""#)),
            "{answer}"
        );
        if behavior == "allow" {
            assert!(
                answer.contains(r#""answers":{"Which color?":"Green"}"#),
                "{answer}"
            );
            assert!(answer.contains(r#""questions":[{"#), "{answer}");
        }
    }
}

#[cfg(unix)]
#[test]
fn codex_answers_request_user_input_and_turns_the_feature_on() {
    let temp = tempfile::tempdir().unwrap();
    let body = r#"
printf '%s\n' "$*" > "$0.args"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -E 's/^\{"id":([0-9]+),.*$/\1/')
  case "$line" in
    *'"method":"initialize"'*) echo "{\"id\":$id,\"result\":{}}" ;;
    *'"method":"account/rateLimits/read"'*) echo "{\"id\":$id,\"error\":{\"message\":\"nope\"}}" ;;
    *'"method":"thread/start"'*) echo "{\"id\":$id,\"result\":{\"thread\":{\"id\":\"t\"}}}" ;;
    *'"method":"turn/start"'*)
      echo "{\"id\":$id,\"result\":{}}"
      echo '{"id":7,"method":"item/tool/requestUserInput","params":{"threadId":"t","turnId":"u","itemId":"i","isBlocking":false,"questions":[{"id":"color","header":"Color","question":"Which color?","isOther":true,"isSecret":false,"options":[{"label":"Red","description":"warm"}]}]}}' ;;
    *'"id":7,'*)
      printf '%s\n' "$line" > "$0.answer"
      echo '{"method":"turn/completed","params":{"turn":{"status":"completed"}}}' ;;
  esac
done
"#;
    let program = fake::script(temp.path(), "fake_codex", body);
    let (emit, _) = recorder();
    let seen: Arc<Mutex<Vec<Question>>> = Arc::default();
    let log = Arc::clone(&seen);
    let options = SessionOptions {
        ask: Some(Ask(Arc::new(move |questions: &[Question]| {
            lock(&log).extend(questions.iter().cloned());
            Some(vec![("color".to_string(), vec!["Green".to_string()])])
        }))),
        ..SessionOptions::default()
    };
    let session = CodexSession::new(temp.path(), json!({}), options, emit, deny());
    session.set_program(&program);
    connect_retry(&session);
    session.query("go").unwrap();
    session.close().unwrap();
    let asked = lock(&seen).clone();
    assert_eq!(
        (
            asked[0].id.as_str(),
            asked[0].question.as_str(),
            asked[0].other,
            asked[0].multi
        ),
        ("color", "Which color?", true, false)
    );
    let answer = std::fs::read_to_string(format!("{}.answer", program.display())).unwrap();
    assert!(
        answer.contains(r#""result":{"answers":{"color":{"answers":["Green"]}}}"#),
        "{answer}"
    );
    let args = std::fs::read_to_string(format!("{}.args", program.display())).unwrap();
    assert!(
        args.contains("-c features.default_mode_request_user_input=true"),
        "{args}"
    );
}

#[test]
fn question_parsing_covers_both_providers() {
    let codex = codex_questions(&json!({"questions": [
        {"id": "name", "header": "Name", "question": "Your name?", "isSecret": true},
    ]}));
    assert_eq!(
        codex,
        [Question {
            id: "name".into(),
            header: "Name".into(),
            question: "Your name?".into(),
            other: true, // No options: only a typed answer fits.
            secret: true,
            ..Question::default()
        }]
    );
    let claude = claude_questions(&json!({"questions": [
        {"question": "Which?", "multiSelect": true, "options": [{"label": "A"}, "B"]},
        {"header": "no question text"},
    ]}));
    assert_eq!(claude.len(), 1);
    assert!(claude[0].multi);
    assert_eq!(
        claude[0].options,
        [
            ("A".to_string(), String::new()),
            ("B".to_string(), String::new())
        ]
    );
    match claude_answer(&json!({"questions": []}), None) {
        PermissionResult::Deny { message } => assert!(message.contains("dismissed")),
        other => panic!("{other:?}"),
    }
}
