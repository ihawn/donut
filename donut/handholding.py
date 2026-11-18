"""/handholding: how small and how spelled-out the planner makes each ticket, for the worker it will get.

Low suits a capable worker: bigger tickets that state the goal and leave the approach to it. High suits a small or
weak worker model: tiny tickets that leave it nothing to infer. Medium is the planner skill's own guidance.
"""

DEFAULT = "medium"

# level: (label, what the picker says, what the planner is told)
LEVELS = {
    "low": (
        "Low",
        "larger tickets for a capable worker: the goal and checks, the approach left to it",
        "Ticket scope can be larger. One ticket may cover a whole feature or a few closely related behaviors, up "
        "to about 5 implementation files and 200 changed lines. Give the goal, the code anchors, the requirements "
        "and the Verify command and tests, and leave the implementation approach to the worker; do not write its "
        "code for it. Still split work that cannot be verified as one unit.",
    ),
    "medium": (
        "Medium",
        "broad but specific: one behavior per ticket, named files and steps (default)",
        "Ticket scope is somewhat broad but specific: one behavior per ticket, preferably at most 2 implementation "
        "files and about 40 changed lines. Name the exact files, functions and code anchors and give numbered "
        "implementation steps, but describe each change rather than writing every line of it.",
    ),
    "high": (
        "High",
        "very specific tickets for a small or weak worker: one tiny change, exact code given",
        "Ticket scope is very specific: the worker is a small, weak model that cannot infer anything or make "
        "design decisions. One tiny change per ticket, ideally one file and at most about 20 changed lines; split "
        "anything bigger into more tickets. For every edit, quote the exact existing lines to find and give the "
        "exact code to insert or replace them with. Give exact commands with their expected output. Spell out "
        "imports, names, types and edge cases; never write 'and so on', 'similarly' or 'as needed'. Leave no "
        "choice to the worker.",
    ),
}


def level(config):
    """The saved level, or the default when none (or an unknown one) is saved."""
    chosen = (config or {}).get("handholding")
    return chosen if chosen in LEVELS else DEFAULT


def instruction(config):
    """The sentence added to every planning prompt, telling the planner how to size and detail its tickets.
    It takes precedence over the planner skill's own size guideline."""
    chosen = level(config)
    return f"Handholding is {chosen} (this overrides the planner skill's ticket-size guideline): " + LEVELS[chosen][2]
