//! a per-user log of terminal conversations, so /resume can reopen one with its
//! mode and model.
//!
//! Each record keeps the mode, role, alias and model, the provider's own session or thread id (what actually
//! resumes the conversation), a title from the first message, and the last few messages for a recap. It lives
//! with the run logs under `$XDG_STATE_HOME/donut`, readable only by the user, because it holds project content.
//!
//! Records are `serde_json::Value` objects, so keys Donut does not know survive a load and save.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::config::state_dir;

pub const KEEP_MESSAGES: usize = 12;
pub const MESSAGE_CHARS: usize = 2000;
pub const KEEP_RECORDS: usize = 200;

static UNNAMED_SESSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{8}-\d{4}(-\d+)?$").unwrap());

/// Serializes tests that change environment variables.
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Seconds since the epoch, as Python's time.time().
pub fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Python's `Path.resolve()`: an absolute path with symlinks followed, even when the path does not exist.
pub fn resolve(path: &Path) -> PathBuf {
    if let Ok(found) = dunce::canonicalize(path) {
        return found;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut rest = Vec::new();
    let mut base = absolute.clone();
    while !base.exists() {
        match (
            base.file_name().map(|n| n.to_os_string()),
            base.parent().map(Path::to_path_buf),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                base = parent;
            }
            _ => return absolute,
        }
    }
    let mut out = dunce::canonicalize(&base).unwrap_or(base);
    for part in rest.into_iter().rev() {
        if part == ".." {
            out.pop();
        } else if part != "." {
            out.push(part);
        }
    }
    out
}

fn escape_into(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn dump_into(value: &Value, indent: Option<usize>, level: usize, out: &mut String) {
    let newline = |out: &mut String, level: usize| {
        if let Some(width) = indent {
            out.push('\n');
            out.push_str(&" ".repeat(width * level));
        }
    };
    match value {
        Value::String(text) => escape_into(out, text),
        Value::Array(items) if !items.is_empty() => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                    if indent.is_none() {
                        out.push(' ');
                    }
                }
                newline(out, level + 1);
                dump_into(item, indent, level + 1, out);
            }
            newline(out, level);
            out.push(']');
        }
        Value::Object(map) if !map.is_empty() => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                    if indent.is_none() {
                        out.push(' ');
                    }
                }
                newline(out, level + 1);
                escape_into(out, key);
                out.push_str(": ");
                dump_into(item, indent, level + 1, out);
            }
            newline(out, level);
            out.push('}');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// Python's `json.dumps(value, indent=indent)`: ASCII only, ", " and ": " separators when not indented.
pub fn json_dumps(value: &Value, indent: Option<usize>) -> String {
    let mut out = String::new();
    dump_into(value, indent, 0, &mut out);
    out
}

fn private_dir(path: &Path) {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    let _ = builder.create(path);
}

/// Replace a file with text through a temporary file in the same folder, readable only by the user.
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let folder = path.parent().unwrap_or_else(|| Path::new("."));
    private_dir(folder);
    let mut file = tempfile::NamedTempFile::new_in(folder)?;
    file.write_all(text.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600))?;
    }
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn text_of<'a>(record: &'a Value, key: &str) -> &'a str {
    record.get(key).and_then(Value::as_str).unwrap_or("")
}

fn updated(record: &Value) -> f64 {
    record.get("updated").and_then(Value::as_f64).unwrap_or(0.0)
}

fn truncate_chars(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

pub fn store_path() -> PathBuf {
    state_dir().join("conversations.json")
}

pub fn load_all() -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(store_path()) else {
        return Vec::new();
    };
    let Ok(Value::Array(records)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    records
        .into_iter()
        .filter(|r| {
            r.is_object()
                && r.get("id")
                    .is_some_and(|id| id.as_str().is_none_or(|s| !s.is_empty()) && !id.is_null())
        })
        .collect()
}

fn id_of(record: &Value) -> String {
    match record.get("id") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

pub fn transcript_path(record_id: &str) -> PathBuf {
    store_path()
        .with_file_name("transcripts")
        .join(format!("{record_id}.jsonl"))
}

fn full_transcript(record: &Value) -> Option<Vec<(String, String)>> {
    let text = std::fs::read_to_string(transcript_path(&id_of(record))).ok()?;
    let mut turns = Vec::new();
    for line in text.lines() {
        let turn: Value = serde_json::from_str(line).ok()?;
        turn.as_object()?;
        turns.push((
            text_of(&turn, "you").to_string(),
            text_of(&turn, "answer").to_string(),
        ));
    }
    Some(turns)
}

/// Every exchange of a conversation as (you, answer), in full, for /resume to show.
///
/// A conversation saved before transcripts were kept has only its last few exchanges, each cut short.
pub fn transcript(record: &Value) -> Vec<(String, String)> {
    if let Some(turns) = full_transcript(record) {
        return turns;
    }
    let messages = record
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    messages
        .chunks_exact(2)
        .map(|pair| {
            (
                text_of(&pair[0], "text").to_string(),
                text_of(&pair[1], "text").to_string(),
            )
        })
        .collect()
}

/// The name /resume lists a conversation under: the name it was given, else its renamed session, else None.
pub fn display_name(record: &Value) -> Option<String> {
    let session = text_of(record, "session");
    // A session still named after its start time (20261109-1712) was never renamed.
    let name = text_of(record, "name");
    if !name.is_empty() {
        return Some(name.to_string());
    }
    (!session.is_empty() && !UNNAMED_SESSION.is_match(session)).then(|| session.to_string())
}

fn write(records: &[Value]) {
    let path = store_path();
    private_dir(path.parent().unwrap_or_else(|| Path::new(".")));
    let split = records.len().saturating_sub(KEEP_RECORDS);
    for record in &records[..split] {
        let _ = std::fs::remove_file(transcript_path(&id_of(record))); // A conversation that leaves the list takes its text.
    }
    let text = json_dumps(&Value::Array(records[split..].to_vec()), Some(1));
    let _ = write_private(&path, &text);
}

/// This project's conversations, most recently used first.
pub fn for_project(project: &Path) -> Vec<Value> {
    let project = resolve(project).to_string_lossy().into_owned();
    let mut records: Vec<Value> = load_all()
        .into_iter()
        .filter(|r| text_of(r, "project") == project)
        .collect();
    records.sort_by(|a, b| updated(b).total_cmp(&updated(a)));
    records
}

/// Records grouped into sessions, newest session first, each with its newest record first.
///
/// Each mode (plan, solo, review) keeps a conversation of its own, so one session holds several records.
/// /resume lists the session once. A record from before sessions stands alone.
pub fn by_session(records: &[Value]) -> Vec<Vec<Value>> {
    let mut sorted: Vec<&Value> = records.iter().collect();
    sorted.sort_by(|a, b| updated(b).total_cmp(&updated(a)));
    let mut keys: Vec<String> = Vec::new();
    let mut groups: Vec<Vec<Value>> = Vec::new();
    for record in sorted {
        let session = text_of(record, "session");
        let key = if session.is_empty() {
            id_of(record)
        } else {
            session.to_string()
        };
        match keys.iter().position(|k| *k == key) {
            Some(index) => groups[index].push(record.clone()),
            None => {
                keys.push(key);
                groups.push(vec![record.clone()]);
            }
        }
    }
    groups
}

pub fn get(record_id: &str) -> Option<Value> {
    load_all()
        .into_iter()
        .find(|record| id_of(record) == record_id)
}

/// Add one exchange to a conversation, creating it on its first turn. Returns the record's id.
#[allow(clippy::too_many_arguments)]
pub fn record_turn(
    record_id: Option<&str>,
    project: &Path,
    mode: &str,
    role: &str,
    alias: &str,
    model: Option<&str>,
    provider_id: Option<&str>,
    user_text: &str,
    answer: Option<&str>,
    session: Option<&str>,
    dollars: Option<f64>,
) -> String {
    let mut records = load_all();
    let record_id = record_id.filter(|id| !id.is_empty());
    let index = record_id.and_then(|id| records.iter().position(|r| id_of(r) == id));
    // Strictly later than any saved turn, so "most recent first" holds even when the clock is coarse (Windows).
    let now = records
        .iter()
        .map(|r| updated(r) + 1e-6)
        .fold(now_seconds(), f64::max);
    let model = model.unwrap_or("");
    let answer = answer.unwrap_or("");
    let found = match index {
        Some(found) => found,
        None => {
            let title = truncate_chars(
                &user_text.split_whitespace().collect::<Vec<_>>().join(" "),
                80,
            );
            let mut record = Map::new();
            record.insert(
                "id".into(),
                json!(
                    record_id
                        .map(str::to_string)
                        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string())
                ),
            );
            record.insert("project".into(), json!(resolve(project).to_string_lossy()));
            record.insert(
                "title".into(),
                json!(if title.is_empty() {
                    "(untitled)".to_string()
                } else {
                    title
                }),
            );
            record.insert("created".into(), json!(now));
            record.insert("turns".into(), json!(0));
            record.insert("messages".into(), json!([]));
            records.push(Value::Object(record));
            records.len() - 1
        }
    };
    let project_key = text_of(&records[found], "project").to_string();
    let named = session.filter(|s| !s.is_empty()).and_then(|session| {
        records
            .iter()
            .find(|r| {
                text_of(r, "project") == project_key
                    && text_of(r, "session") == session
                    && !text_of(r, "name").is_empty()
            })
            .map(|r| text_of(r, "name").to_string())
    });
    let record = records[found].as_object_mut().expect("record is an object");
    record.insert("mode".into(), json!(mode));
    record.insert("role".into(), json!(role));
    record.insert("alias".into(), json!(alias));
    record.insert("model".into(), json!(model));
    record.insert("updated".into(), json!(now));
    if let Some(session) = session.filter(|s| !s.is_empty()) {
        record.insert("session".into(), json!(session)); // The tickets folder, .donut/<session>/, that /resume switches back to.
        if !record.contains_key("name") {
            // A conversation started in a renamed session is listed under that session's name too.
            if let Some(named) = named {
                record.insert("name".into(), json!(named));
            }
        }
    }
    if let Some(provider_id) = provider_id.filter(|p| !p.is_empty()) {
        record.insert("provider_id".into(), json!(provider_id));
    }
    let turns = record.get("turns").and_then(Value::as_i64).unwrap_or(0) + 1;
    record.insert("turns".into(), json!(turns));
    let messages = record.entry("messages").or_insert_with(|| json!([]));
    if !messages.is_array() {
        *messages = json!([]);
    }
    let messages = messages.as_array_mut().expect("messages is an array");
    messages.push(json!({"from": "you", "text": truncate_chars(user_text, MESSAGE_CHARS)}));
    messages.push(json!({"from": "model", "text": truncate_chars(answer, MESSAGE_CHARS)}));
    if messages.len() > KEEP_MESSAGES {
        let extra = messages.len() - KEEP_MESSAGES;
        messages.drain(..extra);
    }
    let id = id_of(&records[found]);
    records.sort_by(|a, b| updated(a).total_cmp(&updated(b)));
    write(&records);
    let path = transcript_path(&id);
    private_dir(path.parent().unwrap_or_else(|| Path::new(".")));
    // The model that answered and what it cost, for donut stats.
    let line = json!({
        "you": user_text,
        "answer": answer,
        "model": if model.is_empty() { alias } else { model },
        "dollars": dollars,
    });
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Ok(mut file) = options.open(&path) {
        let _ = writeln!(file, "{}", json_dumps(&line, None));
    }
    id
}

/// 1700000000 -> "5m ago", "3h ago", "2d ago".
pub fn ago(timestamp: f64, now: Option<f64>) -> String {
    let seconds = ((now.unwrap_or_else(now_seconds) - timestamp) as i64).max(0);
    for (size, unit) in [(86400, "d"), (3600, "h"), (60, "m")] {
        if seconds >= size {
            return format!("{}{unit} ago", seconds / size);
        }
    }
    "just now".to_string()
}

pub fn projects_path() -> PathBuf {
    store_path().with_file_name("projects.json")
}

fn known_projects() -> Vec<Value> {
    std::fs::read_to_string(projects_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| match value {
            Value::Array(items) => Some(items),
            _ => None,
        })
        .unwrap_or_default()
}

/// Note that Donut opened this project, newest first, for /project.
pub fn remember_project(project: &Path) {
    let project = resolve(project).to_string_lossy().into_owned();
    let mut known = vec![Value::String(project.clone())];
    known.extend(
        known_projects()
            .into_iter()
            .filter(|p| p.as_str().is_some_and(|s| s != project)),
    );
    known.truncate(KEEP_RECORDS);
    let _ = write_private(&projects_path(), &json_dumps(&Value::Array(known), Some(1)));
}

/// Projects Donut has opened or talked about, most recent first, that still exist.
pub fn recent_projects() -> Vec<String> {
    let known = known_projects();
    let mut by_use = load_all();
    by_use.sort_by(|a, b| updated(b).total_cmp(&updated(a)));
    let mut ordered: Vec<String> = Vec::new();
    let candidates = known
        .iter()
        .filter_map(|p| p.as_str().map(str::to_string))
        .chain(by_use.iter().map(|r| text_of(r, "project").to_string()));
    for project in candidates {
        if !project.is_empty() && !ordered.contains(&project) && Path::new(&project).is_dir() {
            ordered.push(project);
        }
    }
    ordered
}

/// Point this project's saved conversations at a renamed session: its tickets folder and the name /resume
/// shows for them (the name as typed, such as "fix login bug", where the folder is fix-login-bug).
pub fn rename_session(project: &Path, old: Option<&str>, new: &str, name: Option<&str>) {
    let Some(old) = old.filter(|o| !o.is_empty()) else {
        return; // Conversations from before sessions have none to follow.
    };
    let project = resolve(project).to_string_lossy().into_owned();
    let mut records = load_all();
    let mut changed = false;
    for record in &mut records {
        if text_of(record, "project") == project && text_of(record, "session") == old {
            if let Some(map) = record.as_object_mut() {
                map.insert("session".into(), json!(new));
                if let Some(name) = name.filter(|n| !n.is_empty()) {
                    map.insert("name".into(), json!(name));
                }
                changed = true;
            }
        }
    }
    if changed {
        write(&records);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env {
        _guard: std::sync::MutexGuard<'static, ()>,
        temp: tempfile::TempDir,
    }

    fn env() -> Env {
        let guard = env_lock();
        let temp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temp.path().join("state"));
            std::env::remove_var("DONUT_PROFILE");
        }
        Env {
            _guard: guard,
            temp,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn turn(
        env: &Env,
        mode: &str,
        alias: &str,
        model: &str,
        provider: &str,
        text: &str,
        answer: &str,
        session: Option<&str>,
    ) -> String {
        record_turn(
            None,
            env.temp.path(),
            mode,
            mode,
            alias,
            Some(model),
            Some(provider),
            text,
            Some(answer),
            session,
            None,
        )
    }

    #[test]
    fn test_conversations_are_saved_with_their_mode_and_model() {
        let env = env();
        turn(
            &env,
            "plan",
            "claude",
            "claude-opus-5-5",
            "provider-1",
            "plan the wall jump",
            "ok",
            None,
        );
        let solo = turn(
            &env,
            "solo",
            "codex",
            "gpt-6.1",
            "provider-2",
            "fix the typo in README",
            "ok",
            None,
        );
        record_turn(
            Some(&solo),
            env.temp.path(),
            "solo",
            "solo",
            "codex",
            Some("gpt-6.1"),
            Some("provider-2"),
            "and the other one",
            Some("ok"),
            None,
            None,
        );
        let records = for_project(env.temp.path());
        let summary: Vec<(String, String, String, i64)> = records
            .iter()
            .map(|r| {
                (
                    text_of(r, "mode").into(),
                    text_of(r, "model").into(),
                    text_of(r, "title").into(),
                    r["turns"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "solo".into(),
                    "gpt-6.1".into(),
                    "fix the typo in README".into(),
                    2
                ),
                (
                    "plan".into(),
                    "claude-opus-5-5".into(),
                    "plan the wall jump".into(),
                    1
                ),
            ]
        );
        assert_eq!(records[0]["provider_id"], "provider-2");
        assert_eq!(
            records[0]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .nth(1)
                .unwrap()["text"],
            "and the other one"
        );
    }

    #[test]
    fn test_a_resumed_conversation_keeps_growing_and_is_saved_privately() {
        let env = env();
        let first = turn(
            &env,
            "solo",
            "claude",
            "claude-sonnet-5-5",
            "claude-sid",
            "rename jump",
            "Renamed it.",
            None,
        );
        turn(
            &env,
            "plan",
            "codex",
            "gpt-6.1",
            "thread-9",
            "plan sounds",
            "Wrote 2 tickets.",
            None,
        );
        let titles: Vec<String> = for_project(env.temp.path())
            .iter()
            .map(|r| text_of(r, "title").to_string())
            .collect();
        assert_eq!(titles, ["plan sounds", "rename jump"]);
        record_turn(
            Some(&first),
            env.temp.path(),
            "solo",
            "solo",
            "claude",
            Some("claude-sonnet-5-5"),
            Some("claude-sid"),
            "now rename land too",
            Some("ok"),
            None,
            Some(0.5),
        );
        let record = get(&first).unwrap();
        assert_eq!(record["turns"], 2);
        let exchanges = transcript(&record);
        assert_eq!(
            exchanges,
            [
                ("rename jump".to_string(), "Renamed it.".to_string()),
                ("now rename land too".into(), "ok".into())
            ]
        );
        let line = std::fs::read_to_string(transcript_path(&first)).unwrap();
        assert!(line.contains("\"dollars\": 0.5"), "{line}");
        assert!(line.contains("\"model\": \"claude-sonnet-5-5\""));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(get("missing").is_none());
    }

    #[test]
    fn test_a_session_is_one_resume_entry_and_each_mode_continues_its_own_conversation() {
        let env = env();
        turn(
            &env,
            "solo",
            "claude",
            "claude-sonnet-5-5",
            "old-sid",
            "an earlier solo chat",
            "ok",
            Some("feature"),
        );
        let plan = turn(
            &env,
            "plan",
            "codex",
            "gpt-6.1",
            "thread-1",
            "plan the feature",
            "ok",
            Some("feature"),
        );
        let solo = turn(
            &env,
            "solo",
            "claude",
            "claude-sonnet-5-5",
            "sid-1",
            "do the feature",
            "ok",
            Some("feature"),
        );
        turn(
            &env,
            "plan",
            "claude",
            "m",
            "p",
            "something else",
            "ok",
            Some("other"),
        );
        turn(
            &env,
            "solo",
            "claude",
            "m",
            "p",
            "before sessions",
            "ok",
            None,
        );
        let groups = by_session(&for_project(env.temp.path()));
        assert_eq!(groups.len(), 3);
        let feature = groups
            .iter()
            .find(|g| text_of(&g[0], "session") == "feature")
            .unwrap();
        let ids: Vec<String> = feature.iter().map(id_of).collect();
        assert_eq!(ids[0], solo); // Newest record first.
        assert!(ids.contains(&plan));
        assert_eq!(feature.len(), 3);
    }

    #[test]
    fn test_sessions_can_be_renamed_and_name_their_conversations() {
        let env = env();
        let first = turn(
            &env,
            "plan",
            "claude",
            "m",
            "sid",
            "jump",
            "ok",
            Some("20261109-1712"),
        );
        assert_eq!(display_name(&get(&first).unwrap()), None);
        rename_session(env.temp.path(), None, "ignored", None);
        rename_session(
            env.temp.path(),
            Some("20261109-1712"),
            "fix-login-bug",
            Some("fix login bug"),
        );
        let record = get(&first).unwrap();
        assert_eq!(
            (text_of(&record, "session"), text_of(&record, "name")),
            ("fix-login-bug", "fix login bug")
        );
        assert_eq!(display_name(&record).as_deref(), Some("fix login bug"));
        // A conversation started later in the renamed session is listed under its name too.
        let second = turn(
            &env,
            "solo",
            "claude",
            "m",
            "sid2",
            "more",
            "ok",
            Some("fix-login-bug"),
        );
        assert_eq!(text_of(&get(&second).unwrap(), "name"), "fix login bug");
        let renamed = json!({"session": "fix-login-bug"});
        assert_eq!(display_name(&renamed).as_deref(), Some("fix-login-bug"));
        assert_eq!(display_name(&json!({"session": "20261109-1712-2"})), None);
    }

    #[test]
    fn test_projects_are_remembered_newest_first_and_missing_ones_are_dropped() {
        let env = env();
        let other = env.temp.path().join("other-app");
        let third = env.temp.path().join("third-app");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(&third).unwrap();
        remember_project(&other);
        remember_project(&third);
        remember_project(&other);
        let other = resolve(&other).to_string_lossy().into_owned();
        let third = resolve(&third).to_string_lossy().into_owned();
        assert_eq!(recent_projects(), [other.clone(), third.clone()]);
        remember_project(&env.temp.path().join("gone"));
        assert_eq!(recent_projects(), [other, third]);
    }

    #[test]
    fn test_old_conversations_fall_back_to_their_saved_messages() {
        let record = json!({"id": "no-such-transcript", "messages": [
            {"from": "you", "text": "a"}, {"from": "model", "text": "b"}, {"from": "you", "text": "c"}]});
        let _guard = env_lock();
        assert_eq!(transcript(&record), [("a".to_string(), "b".to_string())]);
    }

    #[test]
    fn test_ago_and_python_style_json() {
        assert_eq!(ago(1000.0, Some(1000.0 + 5.0 * 60.0)), "5m ago");
        assert_eq!(ago(1000.0, Some(1000.0 + 3.0 * 3600.0)), "3h ago");
        assert_eq!(ago(1000.0, Some(1000.0 + 2.0 * 86400.0)), "2d ago");
        assert_eq!(ago(1000.0, Some(1010.0)), "just now");
        assert_eq!(ago(2000.0, Some(1000.0)), "just now");
        let value = json!({"a": [1, "é"], "b": {}});
        assert_eq!(json_dumps(&value, None), r#"{"a": [1, "\u00e9"], "b": {}}"#);
        assert_eq!(
            json_dumps(&value, Some(1)),
            "{\n \"a\": [\n  1,\n  \"\\u00e9\"\n ],\n \"b\": {}\n}"
        );
        assert_eq!(json_dumps(&json!("😀"), None), "\"\\ud83d\\ude00\"");
    }
}
