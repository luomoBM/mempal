//! Claude Code session reader.

use crate::cowork::peek::{
    PeekError, PeekMessage, PeekPlan, PeekPlanFinalizeStatus, PeekPlanWriteStatus, parse_rfc3339,
};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct ClaudeSessionParse {
    pub messages: Vec<PeekMessage>,
    pub plan: Option<PeekPlan>,
    pub truncated: bool,
}

/// Build the legacy Claude Code project encoding from a cwd: `/` → `-`.
///
/// Claude Code stores session files under
/// `~/.claude/projects/<encoded>/<session-uuid>.jsonl` where `<encoded>`
/// was originally the cwd with every `/` replaced by `-`.
pub fn encode_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('/', "-")
}

/// Build candidate Claude Code project directories for a given cwd and HOME.
///
/// Claude Code versions observed in the wild differ in how they encode cwd
/// punctuation: older fixtures only replace `/`, while current local sessions
/// also collapse `_` to `-` (for example `android_proj` becomes
/// `android-proj`). Keep both candidates so old session layouts still work.
pub fn claude_project_dirs(home: &Path, cwd: &Path) -> Vec<PathBuf> {
    let projects = home.join(".claude/projects");
    let legacy = encode_cwd(cwd);
    let normalized = legacy.replace('_', "-");

    let mut dirs = Vec::with_capacity(2);
    dirs.push(projects.join(&legacy));
    if normalized != legacy {
        dirs.push(projects.join(normalized));
    }
    dirs
}

/// Find the latest (by mtime) `.jsonl` in Claude project directories.
/// Returns `None` if no candidate directory exists or has jsonl files.
pub fn latest_session_file(project_dirs: &[PathBuf]) -> Option<(PathBuf, SystemTime)> {
    project_dirs
        .iter()
        .filter_map(|project_dir| fs::read_dir(project_dir).ok())
        .flat_map(|entries| entries.filter_map(|e| e.ok()))
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), mtime))
        })
        .max_by_key(|(_, m)| *m)
}

/// Parse a Claude Code session jsonl. Returns text messages plus a narrow
/// plan summary when Claude plan-mode artifacts are present.
pub fn parse_jsonl_messages(
    path: &Path,
    since: Option<&str>,
    limit: usize,
) -> Result<ClaudeSessionParse, PeekError> {
    // Pre-parse the `since` cutoff once into epoch seconds. Compare in
    // instant semantics, not lexicographic string order — string compare is
    // broken across timezone offsets (e.g. "10:00+08:00" vs "02:00Z" look
    // unequal but are the same instant).
    let since_cutoff: Option<i64> = match since {
        Some(raw) => Some(parse_rfc3339(raw).ok_or_else(|| {
            PeekError::Parse(format!("invalid `since` RFC3339 timestamp: {raw}"))
        })?),
        None => None,
    };

    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut all: Vec<PeekMessage> = Vec::new();
    let mut plan_tracker = PlanTracker::default();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let val: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        plan_tracker.observe(&val);
        if let Some(msg) = extract_message(&val) {
            if let Some(cutoff) = since_cutoff {
                // Messages whose timestamp can't be parsed are kept (safer
                // default than silently dropping them).
                if let Some(msg_ts) = parse_rfc3339(&msg.at) {
                    if msg_ts <= cutoff {
                        continue;
                    }
                }
            }
            all.push(msg);
        }
    }

    let total = all.len();
    let truncated = total > limit;
    let start = total.saturating_sub(limit);
    let tail = all.split_off(start);
    Ok(ClaudeSessionParse {
        messages: tail,
        plan: plan_tracker.finish(),
        truncated,
    })
}

/// Extract a PeekMessage from one Claude jsonl entry if it's a user/assistant
/// text message. Returns `None` for:
///   * top-level `type` other than "user" / "assistant"
///   * entries with `isMeta: true` (command caveats, shell echoes)
///   * messages whose content has no extractable text (only tool_use / only tool_result)
fn extract_message(val: &Value) -> Option<PeekMessage> {
    let top_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if top_type != "user" && top_type != "assistant" {
        return None;
    }

    if val.get("isMeta").and_then(|v| v.as_bool()).unwrap_or(false) {
        return None;
    }

    let message = val.get("message")?;
    let role = message.get("role").and_then(|v| v.as_str())?;
    if role != "user" && role != "assistant" {
        return None;
    }

    // `message.content` can be either a plain string or an array of content blocks.
    let content = message.get("content")?;
    let text = match content {
        Value::String(s) => s.trim().to_string(),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|b| {
                    let block_type = b.get("type").and_then(|v| v.as_str())?;
                    if block_type == "text" {
                        b.get("text")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    } else {
                        None
                    }
                })
                .collect();
            parts.join("\n")
        }
        _ => return None,
    };

    if text.is_empty() {
        return None;
    }

    let at = val
        .get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Some(PeekMessage {
        role: role.to_string(),
        at,
        text,
    })
}

#[derive(Debug, Default)]
struct PlanTracker {
    plan: Option<PeekPlan>,
    pending_write_tool_use_id: Option<String>,
    pending_finalize_tool_use_id: Option<String>,
}

impl PlanTracker {
    fn observe(&mut self, val: &Value) {
        self.observe_plan_mode(val);
        self.observe_assistant_tool_use(val);
        self.observe_user_tool_results(val);
    }

    fn finish(mut self) -> Option<PeekPlan> {
        if let Some(plan) = &mut self.plan {
            plan.interrupted_after_write = plan.write_status == PeekPlanWriteStatus::Succeeded
                && plan.finalize_status == PeekPlanFinalizeStatus::Rejected;
        }
        self.plan
    }

    fn observe_plan_mode(&mut self, val: &Value) {
        let Some(attachment) = val.get("attachment") else {
            return;
        };
        if attachment.get("type").and_then(|v| v.as_str()) != Some("plan_mode") {
            return;
        }
        let Some(path) = attachment.get("planFilePath").and_then(|v| v.as_str()) else {
            return;
        };
        let declared_at = timestamp_of(val);
        let write_status = if attachment
            .get("planExists")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            PeekPlanWriteStatus::Succeeded
        } else {
            PeekPlanWriteStatus::Declared
        };
        self.plan = Some(PeekPlan {
            path: path.to_string(),
            declared_at,
            written_at: None,
            write_status,
            finalize_status: PeekPlanFinalizeStatus::NotAttempted,
            interrupted_after_write: false,
        });
        self.pending_write_tool_use_id = None;
        self.pending_finalize_tool_use_id = None;
    }

    fn observe_assistant_tool_use(&mut self, val: &Value) {
        if val.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            return;
        }
        let Some(message) = val.get("message") else {
            return;
        };
        if message.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            return;
        }
        let Some(Value::Array(blocks)) = message.get("content") else {
            return;
        };
        for block in blocks {
            if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                continue;
            }
            let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
            match name {
                "Write" => {
                    let Some(path) = block
                        .get("input")
                        .and_then(|v| v.get("file_path"))
                        .and_then(|v| v.as_str())
                    else {
                        continue;
                    };
                    if !path.contains("/.claude/plans/") {
                        continue;
                    }
                    let plan = self.ensure_plan(path);
                    plan.write_status = PeekPlanWriteStatus::Attempted;
                    plan.written_at = None;
                    self.pending_write_tool_use_id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                }
                "ExitPlanMode" => {
                    let Some(plan) = self.plan.as_mut() else {
                        continue;
                    };
                    plan.finalize_status = PeekPlanFinalizeStatus::Attempted;
                    self.pending_finalize_tool_use_id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                }
                _ => {}
            }
        }
    }

    fn observe_user_tool_results(&mut self, val: &Value) {
        if val.get("type").and_then(|v| v.as_str()) != Some("user") {
            return;
        }

        if let Some(tool_use_result) = val.get("toolUseResult") {
            self.observe_top_level_tool_use_result(tool_use_result, val);
        }

        let Some(message) = val.get("message") else {
            return;
        };
        if message.get("role").and_then(|v| v.as_str()) != Some("user") {
            return;
        }
        let Some(Value::Array(blocks)) = message.get("content") else {
            return;
        };
        for block in blocks {
            if block.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
                continue;
            }
            let tool_use_id = block.get("tool_use_id").and_then(|v| v.as_str());
            let is_error = block
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let content_text = value_text(block.get("content"));

            if tool_use_id == self.pending_write_tool_use_id.as_deref() {
                if is_error {
                    if let Some(plan) = self.plan.as_mut() {
                        plan.write_status = PeekPlanWriteStatus::Failed;
                    }
                } else if let Some(plan) = self.plan.as_mut() {
                    if let Some(text) = content_text.as_deref() {
                        if text.contains(&plan.path) {
                            plan.write_status = PeekPlanWriteStatus::Succeeded;
                            plan.written_at = timestamp_of(val);
                        }
                    }
                }
            }

            if tool_use_id == self.pending_finalize_tool_use_id.as_deref() {
                if let Some(plan) = self.plan.as_mut() {
                    plan.finalize_status = if is_error {
                        if is_user_rejection(content_text.as_deref(), val.get("toolUseResult")) {
                            PeekPlanFinalizeStatus::Rejected
                        } else {
                            PeekPlanFinalizeStatus::Failed
                        }
                    } else {
                        PeekPlanFinalizeStatus::Succeeded
                    };
                }
            }
        }
    }

    fn observe_top_level_tool_use_result(&mut self, tool_use_result: &Value, val: &Value) {
        let Some(file_path) = tool_use_result.get("filePath").and_then(|v| v.as_str()) else {
            return;
        };
        if !file_path.contains("/.claude/plans/") {
            return;
        }
        let plan = self.ensure_plan(file_path);
        let is_error = tool_use_result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_error {
            plan.write_status = PeekPlanWriteStatus::Failed;
            return;
        }
        plan.write_status = PeekPlanWriteStatus::Succeeded;
        plan.written_at = timestamp_of(val);
    }

    fn ensure_plan(&mut self, path: &str) -> &mut PeekPlan {
        let replace = self
            .plan
            .as_ref()
            .map(|plan| plan.path != path)
            .unwrap_or(true);
        if replace {
            self.plan = Some(PeekPlan {
                path: path.to_string(),
                declared_at: None,
                written_at: None,
                write_status: PeekPlanWriteStatus::Unknown,
                finalize_status: PeekPlanFinalizeStatus::NotAttempted,
                interrupted_after_write: false,
            });
            self.pending_write_tool_use_id = None;
            self.pending_finalize_tool_use_id = None;
        }
        self.plan
            .as_mut()
            .expect("ensure_plan must create or reuse a plan")
    }
}

fn timestamp_of(val: &Value) -> Option<String> {
    val.get("timestamp")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

fn value_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.to_string()),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(|v| v.as_str()))
                .map(ToString::to_string)
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

fn is_user_rejection(content_text: Option<&str>, tool_use_result: Option<&Value>) -> bool {
    let content = content_text.unwrap_or("").to_ascii_lowercase();
    if content.contains("doesn't want to proceed")
        || content.contains("rejected tool use")
        || content.contains("request interrupted by user for tool use")
    {
        return true;
    }

    let top_level = match tool_use_result {
        Some(Value::String(s)) => s.to_ascii_lowercase(),
        Some(other) => other.to_string().to_ascii_lowercase(),
        None => String::new(),
    };
    top_level.contains("rejected tool use") || top_level.contains("doesn't want to proceed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn reads_plain_text_and_structured_content() {
        let fixture = Path::new("tests/fixtures/cowork/claude/session.jsonl");
        let parsed = parse_jsonl_messages(fixture, None, 30).expect("parse");
        assert_eq!(parsed.messages.len(), 4);
        assert_eq!(parsed.messages[0].role, "user");
        assert_eq!(parsed.messages[0].text, "Hello from user turn 1");
        assert_eq!(parsed.messages[1].role, "assistant");
        assert_eq!(parsed.messages[1].text, "Hello from assistant turn 1");
        assert_eq!(parsed.messages[3].text, "Second assistant reply");
        assert!(parsed.messages[0].at <= parsed.messages[3].at);
        assert!(!parsed.truncated);
        assert!(parsed.plan.is_none());
    }

    #[test]
    fn filters_tool_use_blocks_and_is_meta_entries() {
        let fixture = Path::new("tests/fixtures/cowork/claude/session_with_tools.jsonl");
        let parsed = parse_jsonl_messages(fixture, None, 30).expect("parse");
        // Expected: u1 ("User turn with tool"), a1 ("Let me check"),
        //           a2 ("Here is the listing"), u4 ("Follow-up question")
        // Skipped:  u2 (only tool_result — no text block), u3 (isMeta:true)
        assert_eq!(parsed.messages.len(), 4);
        for m in &parsed.messages {
            assert!(m.role == "user" || m.role == "assistant");
            assert!(!m.text.is_empty());
            assert!(!m.text.contains("tool_use"));
            assert!(!m.text.contains("tool_result"));
        }
        assert_eq!(parsed.messages[0].text, "User turn with tool");
        assert_eq!(parsed.messages[1].text, "Let me check");
        assert_eq!(parsed.messages[2].text, "Here is the listing");
        assert_eq!(parsed.messages[3].text, "Follow-up question");
        assert!(parsed.plan.is_none());
    }

    #[test]
    fn honors_limit_by_taking_tail_and_sets_truncated() {
        let fixture = Path::new("tests/fixtures/cowork/claude/session.jsonl");
        let parsed = parse_jsonl_messages(fixture, None, 2).expect("parse");
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[0].text, "Second user message");
        assert_eq!(parsed.messages[1].text, "Second assistant reply");
        assert!(parsed.truncated);
    }

    #[test]
    fn since_filter_compares_instants_not_strings() {
        // Cross-timezone regression: `since` with a +08:00 offset represents
        // UTC 02:00:00. Messages at UTC 02:30:00Z and UTC 05:00:00Z are both
        // strictly newer than that instant, so they MUST be returned. The
        // message at UTC 01:00:00Z is older and must be dropped.
        //
        // With a naive lexicographic string compare, `"...01:00:00Z"`,
        // `"...02:30:00Z"`, `"...05:00:00Z"` are all textually less than
        // `"...10:00:00+08:00"`, so the broken implementation drops all 3.
        let fixture = Path::new("tests/fixtures/cowork/claude_since/session.jsonl");
        let since = Some("2026-04-13T10:00:00+08:00");
        let parsed = parse_jsonl_messages(fixture, since, 30).expect("parse");
        assert_eq!(
            parsed.messages.len(),
            2,
            "expected 2 messages strictly newer than the +08:00 cutoff \
             (UTC 02:00), got {}: {:?}",
            parsed.messages.len(),
            parsed.messages.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
        assert!(parsed.messages.iter().any(|m| m.text.contains("02:30")));
        assert!(parsed.messages.iter().any(|m| m.text.contains("05:00")));
        assert!(!parsed.messages.iter().any(|m| m.text.contains("01:00")));
    }

    #[test]
    fn encoded_cwd_replaces_slashes_with_dashes() {
        assert_eq!(encode_cwd(Path::new("/Users/foo/bar")), "-Users-foo-bar");
        assert_eq!(encode_cwd(Path::new("/a")), "-a");
    }

    #[test]
    fn extracts_plan_summary_from_claude_plan_mode_turn() {
        let fixture = Path::new("tests/fixtures/cowork/claude/session_plan.jsonl");
        let parsed = parse_jsonl_messages(fixture, None, 30).expect("parse");
        let plan = parsed.plan.expect("plan summary");

        assert_eq!(plan.path, "/Users/example/.claude/plans/example-plan.md");
        assert_eq!(plan.declared_at.as_deref(), Some("2026-04-13T10:00:00Z"));
        assert_eq!(plan.written_at.as_deref(), Some("2026-04-13T10:00:08Z"));
        assert_eq!(plan.write_status, PeekPlanWriteStatus::Succeeded);
        assert_eq!(plan.finalize_status, PeekPlanFinalizeStatus::Rejected);
        assert!(plan.interrupted_after_write);
    }

    #[test]
    fn keeps_messages_clean_while_extracting_plan_summary() {
        let fixture = Path::new("tests/fixtures/cowork/claude/session_plan.jsonl");
        let parsed = parse_jsonl_messages(fixture, None, 30).expect("parse");

        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(
            parsed.messages[0].text,
            "I have enough context. I am writing the plan file now."
        );
        assert_eq!(
            parsed.messages[1].text,
            "[Request interrupted by user for tool use]"
        );
        for message in &parsed.messages {
            assert!(!message.text.contains("tool_use"));
            assert!(!message.text.contains("tool_result"));
            assert!(!message.text.contains(".claude/plans/example-plan.md"));
        }
    }
}
