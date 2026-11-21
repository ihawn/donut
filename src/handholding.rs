//! /handholding: how small and how spelled-out the planner makes each ticket, for the
//! worker it will get.
//!
//! Low suits a capable worker: bigger tickets that state the goal and leave the approach to it. High suits a small or
//! weak worker model: tiny tickets that leave it nothing to infer. Medium is the planner skill's own guidance.

use serde_json::Value;

pub const DEFAULT: &str = "medium";

/// level: (label, what the picker says, what the planner is told)
pub const LEVELS: [(&str, (&str, &str, &str)); 3] = [
    (
        "low",
        (
            "Low",
            "larger tickets for a capable worker: the goal and checks, the approach left to it",
            "Ticket scope can be larger. One ticket may cover a whole feature or a few closely related behaviors, up \
             to about 5 implementation files and 200 changed lines. Give the goal, the code anchors, the requirements \
             and the Verify command and tests, and leave the implementation approach to the worker; do not write its \
             code for it. Still split work that cannot be verified as one unit.",
        ),
    ),
    (
        "medium",
        (
            "Medium",
            "broad but specific: one behavior per ticket, named files and steps (default)",
            "Ticket scope is somewhat broad but specific: one behavior per ticket, preferably at most 2 implementation \
             files and about 40 changed lines. Name the exact files, functions and code anchors and give numbered \
             implementation steps, but describe each change rather than writing every line of it.",
        ),
    ),
    (
        "high",
        (
            "High",
            "very specific tickets for a small or weak worker: one tiny change, exact code given",
            "Ticket scope is very specific: the worker is a small, weak model that cannot infer anything or make \
             design decisions. One tiny change per ticket, ideally one file and at most about 20 changed lines; split \
             anything bigger into more tickets. For every edit, quote the exact existing lines to find and give the \
             exact code to insert or replace them with. Give exact commands with their expected output. Spell out \
             imports, names, types and edge cases; never write 'and so on', 'similarly' or 'as needed'. Leave no \
             choice to the worker.",
        ),
    ),
];

/// The saved level, or the default when none (or an unknown one) is saved. Pass Value::Null for no config.
pub fn level(config: &Value) -> &'static str {
    let chosen = config.get("handholding").and_then(Value::as_str);
    LEVELS
        .iter()
        .find(|(key, _)| Some(*key) == chosen)
        .map_or(DEFAULT, |(key, _)| key)
}

/// The sentence added to every planning prompt, telling the planner how to size and detail its tickets.
/// It takes precedence over the planner skill's own size guideline.
pub fn instruction(config: &Value) -> String {
    let chosen = level(config);
    let told = LEVELS
        .iter()
        .find(|(key, _)| *key == chosen)
        .map_or("", |(_, (_, _, told))| told);
    format!(
        "Handholding is {chosen} (this overrides the planner skill's ticket-size guideline): {told}"
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn test_each_level_tells_the_planner_how_big_and_exact_tickets_are() {
        assert_eq!(level(&json!({})), "medium");
        assert_eq!(level(&Value::Null), "medium");
        assert_eq!(level(&json!({"handholding": "bogus"})), "medium");
        let low = instruction(&json!({"handholding": "low"}));
        let medium = instruction(&json!({}));
        let high = instruction(&json!({"handholding": "high"}));
        assert!(low.contains("Ticket scope can be larger"));
        assert!(low.contains("leave the implementation approach to the worker"));
        assert!(medium.contains("one behavior per ticket"));
        assert!(high.contains("weak model"));
        assert!(high.contains("exact code to insert or replace"));
        for text in [low, medium, high] {
            assert!(text.contains("overrides the planner skill's ticket-size guideline"));
        }
    }
}
