//! A one-sentence recap of the conversation so far, shown under each chat reply.
//!
//! Donut asks the model to end its reply with one `DONUT-RECAP: <sentence>` line. The model already holds the
//! whole context window, so no second request is needed. Donut takes that line out of the reply (so /copy, the
//! saved conversation and the history other models get do not include it) and shows it on its own, dim and
//! labelled, so it is clearly not part of the answer.

use std::sync::LazyLock;

use regex::Regex;

static RECAP_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^[ \t]*`?DONUT-RECAP:[ \t]*(.*?)`?[ \t]*$").unwrap());

/// The start of a recap line still being streamed, from the line start to the end of the text.
static RECAP_TAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^[ \t]*`?DONUT-R(?:E(?:C(?:A(?:P(?::.*)?)?)?)?)?$").unwrap());

/// What the label on the recap line says.
pub const LABEL: &str = "⟲ context recap";

pub const NOTE: &str = "\n\n(Donut: end your reply with one last line `DONUT-RECAP: <one sentence>` that sums up \
the whole conversation so far: the goal and where it stands now, not only this reply. Donut shows that line \
apart from your reply.)";

/// The prompt with the recap note added, unless the message is a slash command, whose arguments it would change.
pub fn with_note(text: &str, prompt: String) -> String {
    if text.trim_start().starts_with('/') {
        prompt
    } else {
        prompt + NOTE
    }
}

/// Take the last recap line out of `answer`, if it starts at or after byte `from` (text before `from` is already
/// on the screen). Returns the sentence; empty recaps count as none but are still removed.
pub fn take(answer: &mut String, from: usize) -> Option<String> {
    let found = RECAP_LINE.captures_iter(answer).last()?;
    let whole = found.get(0)?;
    if whole.start() < from {
        return None;
    }
    let sentence = found[1].trim().trim_matches('`').trim().to_string();
    let (start, end) = (whole.start(), whole.end());
    answer.replace_range(start..end, "");
    let kept = answer.trim_end().len();
    answer.truncate(kept.max(from.min(answer.len())));
    (!sentence.is_empty()).then_some(sentence)
}

/// Streaming text without a recap line, finished or partly written, so it never flashes in the reply area.
pub fn hide(pending: &str) -> String {
    let text = RECAP_LINE.replace_all(pending, "");
    let text = RECAP_TAIL.replace_all(&text, "");
    text.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_take_removes_the_last_recap_line_and_returns_its_sentence() {
        let mut answer =
            "Fixed the parser.\n\nDONUT-RECAP: We are fixing the parser; the bug is fixed.\n"
                .to_string();
        assert_eq!(
            take(&mut answer, 0).as_deref(),
            Some("We are fixing the parser; the bug is fixed.")
        );
        assert_eq!(answer, "Fixed the parser.");
    }

    #[test]
    fn test_take_accepts_backticks_and_keeps_an_answer_without_a_recap() {
        let mut answer = "Done.\n`DONUT-RECAP: All set.`".to_string();
        assert_eq!(take(&mut answer, 0).as_deref(), Some("All set."));
        assert_eq!(answer, "Done.");
        let mut plain = "No recap here. DONUT-RECAP: inside a sentence".to_string();
        assert_eq!(take(&mut plain, 0), None);
        assert_eq!(plain, "No recap here. DONUT-RECAP: inside a sentence");
    }

    #[test]
    fn test_take_leaves_a_recap_that_is_already_on_screen() {
        let mut answer = "DONUT-RECAP: early.\n\nMore text.".to_string();
        assert_eq!(take(&mut answer, 5), None);
        assert!(answer.starts_with("DONUT-RECAP"));
    }

    #[test]
    fn test_hide_drops_whole_and_partial_recap_lines_while_streaming() {
        assert_eq!(hide("Answer.\n\nDONUT-RECAP: We are"), "Answer.");
        assert_eq!(hide("Answer.\n\nDONUT-RE"), "Answer.");
        assert_eq!(hide("Answer.\nDONUT is great"), "Answer.\nDONUT is great");
    }

    #[test]
    fn test_slash_commands_get_no_note() {
        assert_eq!(with_note("/compact", "/compact".into()), "/compact");
        assert!(with_note("fix it", "fix it".into()).ends_with(NOTE));
    }
}
