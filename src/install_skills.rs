//! Install project-local planner and worker skills without replacing existing files.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::worktrees::copy_tree;

pub const UNITY_SKILLS: [&str; 11] = [
    "unity-cli",
    "unity-pipeline",
    "sprite-editor",
    "create-2d-physics",
    "ui-ugui",
    "optimize-text-mesh-pro",
    "audio-setup-mixers",
    "optimize-audio",
    "manage-sprite-atlas",
    "urp-postprocessing",
    "unity-package-management",
];

pub const LEGACY_SKILLS: &str = ".qwen/skills";
pub const SKILLS: &str = ".donut/skills";

/// The worker skill that ships with Donut.
pub const WORKER_SKILL: &str = include_str!("../skills/swim-lane-worker/SKILL.md");
/// The planner skill that ships with Donut.
pub const PLANNER_SKILL: &str = include_str!("../skills/swim-lane-planner/SKILL.md");

fn exists_or_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// The `## heading` section of the bundled skill, up to the next `## ` heading, or None when it has none.
fn bundled_section<'a>(bundled: &'a str, heading: &str) -> Option<&'a str> {
    let marker = format!("## {heading}\n");
    let start = bundled.find(&marker)?;
    let after = start + marker.len();
    let end = bundled[after..]
        .find("\n## ")
        .map_or(bundled.len(), |at| after + at);
    Some(&bundled[start..end])
}

/// Add the on_hold and blocked guidance an older installed skill lacks, copying it from the bundled skill.
pub fn update_lane_rules(project: &Path, dest: &Path, bundled: &str) -> Result<()> {
    let file = dest.join("SKILL.md");
    // Preserve linked/custom instructions; add only the missing lane guidance.
    for path in std::iter::once(file.as_path())
        .chain(std::iter::once(dest))
        .chain(dest.ancestors().skip(1))
    {
        if path == project {
            break;
        }
        if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Ok(());
        }
    }
    let mut text = std::fs::read_to_string(&file)?;
    for (lane, heading) in [
        ("on_hold", "On-hold tickets"),
        ("blocked", "Blocked tickets"),
    ] {
        if text.contains(&format!("{lane}/")) {
            continue;
        }
        let Some(section) = bundled_section(bundled, heading) else {
            bail!("the bundled skill has no \"{heading}\" section");
        };
        text = format!("{}\n\n{}\n", text.trim_end(), section.trim_end());
        std::fs::write(&file, &text)?;
        let shown = file.strip_prefix(project).unwrap_or(&file);
        println!("Updated {lane} guidance: {}", shown.display());
    }
    Ok(())
}

/// Where worker skills live: the Donut directory, unless an older install already uses the Qwen one.
pub fn skills_dir(project: &Path) -> PathBuf {
    if project
        .join(LEGACY_SKILLS)
        .join("swim-lane-worker")
        .join("SKILL.md")
        .is_file()
    {
        project.join(LEGACY_SKILLS)
    } else {
        project.join(SKILLS)
    }
}

pub fn worker_skill(project: &Path) -> PathBuf {
    skills_dir(project).join("swim-lane-worker")
}

fn write_skill(dest: &Path, text: &str) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    std::fs::write(dest.join("SKILL.md"), text)?;
    Ok(())
}

fn shown(project: &Path, path: &Path) -> String {
    path.strip_prefix(project)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Install the skills and make the session's lanes, .donut/<session>/ (swim_lanes/ when session is None).
///
/// Returns the lanes folder.
pub fn install(project: &Path, home: &Path, quiet: bool, session: Option<&str>) -> Result<PathBuf> {
    let report = |line: String| {
        if !quiet {
            println!("{line}");
        }
    };
    if !project.is_dir() {
        bail!("Project directory does not exist: {}", project.display());
    }
    let mut unity: Vec<(&str, PathBuf)> = Vec::new();
    if project.join("ProjectSettings/ProjectVersion.txt").is_file() {
        for name in UNITY_SKILLS {
            let source = home.join(".agents/skills").join(name);
            if source.join("SKILL.md").is_file() {
                unity.push((name, source));
            }
        }
    }
    let target = skills_dir(project);
    let names = std::iter::once("swim-lane-worker").chain(unity.iter().map(|(name, _)| *name));
    for name in names {
        let dest = target.join(name);
        if exists_or_link(&dest) && !dest.join("SKILL.md").is_file() {
            bail!(
                "Existing skill is invalid; left untouched: {}",
                dest.display()
            );
        }
    }
    let qwen = project.join("QWEN.md");
    let qwen_meta = std::fs::symlink_metadata(&qwen).ok();
    if qwen_meta
        .as_ref()
        .is_some_and(|meta| meta.file_type().is_symlink())
        || (qwen.exists() && !qwen.is_file())
    {
        bail!("QWEN.md must be a regular file; left untouched");
    }
    std::fs::create_dir_all(&target)?;
    let dest = target.join("swim-lane-worker");
    if exists_or_link(&dest) {
        report(format!("Kept existing: {}", shown(project, &dest)));
        update_lane_rules(project, &dest, WORKER_SKILL)?;
    } else {
        write_skill(&dest, WORKER_SKILL)?;
        report(format!("Installed: {}", shown(project, &dest)));
    }
    for (name, source) in &unity {
        let dest = target.join(name);
        if exists_or_link(&dest) {
            report(format!("Kept existing: {}", shown(project, &dest)));
        } else {
            copy_tree(source, &dest)?;
            report(format!("Installed: {}", shown(project, &dest)));
        }
    }
    for relative in [
        "skills/donut-planner",
        ".claude/skills/donut-planner",
        ".cursor/skills/donut-planner",
    ] {
        let dest = project.join(relative);
        if !exists_or_link(&dest) {
            write_skill(&dest, PLANNER_SKILL)?;
            report(format!("Installed: {relative}"));
        } else {
            update_lane_rules(project, &dest, PLANNER_SKILL)?;
        }
    }
    let lanes = crate::workspace::create(project, session)?;
    let text = if qwen.exists() {
        std::fs::read_to_string(&qwen)?
    } else {
        "# Qwen project instructions\n".to_string()
    };
    let worker = worker_skill(project);
    let inside = worker.strip_prefix(project).unwrap_or(&worker);
    let parts: Vec<String> = inside
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    let mut imports = vec![format!("@./{}/SKILL.md", parts.join("/"))];
    if project.join("skills/project-rules/SKILL.md").is_file() {
        imports.push("@./skills/project-rules/SKILL.md".to_string());
    }
    let additions: Vec<&String> = imports
        .iter()
        .filter(|line| !text.contains(&line[2..]))
        .collect();
    if !additions.is_empty() {
        let joined = additions
            .iter()
            .map(|line| line.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        std::fs::write(
            &qwen,
            format!("{}\n\n## Swim-lane worker\n\n{joined}\n", text.trim_end()),
        )?;
    }
    report(format!(
        "Ready: {}\nTickets: {}",
        project.display(),
        crate::workspace::relative(project, &lanes)
    ));
    report(
        "Run donut, then /plan to write tickets or /work next 1 to build the next ticket."
            .to_string(),
    );
    Ok(lanes)
}

/// `python -m donut.install_skills add-skills [-p PROJECT]`. Returns the exit code.
pub fn main(args: &[String]) -> i32 {
    let mut project = ".".to_string();
    let mut action: Option<&str> = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-p" | "--project" => match rest.next() {
                Some(value) => project = value.clone(),
                None => return usage("argument -p/--project: expected one argument"),
            },
            "add-skills" => action = Some("add-skills"),
            other => {
                if let Some(value) = other.strip_prefix("--project=") {
                    project = value.to_string();
                } else {
                    return usage(&format!(
                        "invalid choice: {other:?} (choose from 'add-skills')"
                    ));
                }
            }
        }
    }
    if action.is_none() {
        return usage("the following arguments are required: action");
    }
    let run = || -> Result<()> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_default();
        let expanded = match project.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None if project == "~" => home.clone(),
            None => PathBuf::from(&project),
        };
        let project = dunce::canonicalize(&expanded).unwrap_or(expanded);
        let session = crate::workspace::default(&project);
        install(&project, &home, false, session.as_deref())?;
        Ok(())
    };
    match run() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

fn usage(message: &str) -> i32 {
    eprintln!(
        "usage: install_skills [-h] [-p PROJECT] {{add-skills}}\ninstall_skills: error: {message}"
    );
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_install_writes_skills_lanes_and_qwen_imports_without_replacing_files() {
        let temp = tempfile::tempdir().unwrap();
        let (project, home) = (temp.path().join("project"), temp.path().join("home"));
        std::fs::create_dir_all(&project).unwrap();
        let lanes = install(&project, &home, true, Some("s1")).unwrap();
        assert_eq!(lanes, project.join(".donut/s1"));
        assert!(lanes.join("blocked/.gitkeep").is_file());
        for relative in [
            ".donut/skills/swim-lane-worker/SKILL.md",
            "skills/donut-planner/SKILL.md",
            ".claude/skills/donut-planner/SKILL.md",
            ".cursor/skills/donut-planner/SKILL.md",
        ] {
            assert!(project.join(relative).is_file(), "{relative}");
        }
        assert_eq!(
            std::fs::read_to_string(project.join("skills/donut-planner/SKILL.md")).unwrap(),
            PLANNER_SKILL
        );
        let qwen = std::fs::read_to_string(project.join("QWEN.md")).unwrap();
        assert_eq!(
            qwen,
            "# Qwen project instructions\n\n## Swim-lane worker\n\n@./.donut/skills/swim-lane-worker/SKILL.md\n"
        );
        // A second run keeps what is there.
        std::fs::write(
            project.join(".donut/skills/swim-lane-worker/SKILL.md"),
            "custom on_hold/ blocked/\n",
        )
        .unwrap();
        install(&project, &home, true, Some("s1")).unwrap();
        assert_eq!(
            std::fs::read_to_string(project.join(".donut/skills/swim-lane-worker/SKILL.md"))
                .unwrap(),
            "custom on_hold/ blocked/\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("QWEN.md")).unwrap(),
            qwen
        );
    }

    #[test]
    fn test_an_old_skill_gets_the_lane_guidance_it_lacks() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        let dest = project.join("skills/donut-planner");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("SKILL.md"), "# Old planner\n").unwrap();
        update_lane_rules(project, &dest, PLANNER_SKILL).unwrap();
        let text = std::fs::read_to_string(dest.join("SKILL.md")).unwrap();
        assert!(text.contains("## On-hold tickets"));
        // The on_hold section mentions blocked/, so the second check finds its lane already covered.
        assert!(text.contains("blocked/"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn test_a_missing_project_and_an_invalid_skill_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        assert!(install(&temp.path().join("nope"), temp.path(), true, None).is_err());
        let project = temp.path().join("p");
        std::fs::create_dir_all(project.join(".donut/skills/swim-lane-worker")).unwrap();
        let error = install(&project, temp.path(), true, None).unwrap_err();
        assert!(error.to_string().starts_with("Existing skill is invalid"));
    }
}
