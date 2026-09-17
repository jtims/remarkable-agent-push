use anyhow::{Context, Result};
use colored::Colorize;
use std::fs;
use std::path::{Path, PathBuf};

/// What installing one SKILL.md would do. Decided before anything is
/// written, so an existing file is never replaced by accident: the
/// destination is a fixed path (`~/.claude/skills/rr/SKILL.md` for Claude)
/// where an operator may keep a personalised skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillAction {
    /// No file at the destination yet.
    Install,
    /// The destination already holds the same bytes.
    AlreadyCurrent,
    /// The destination differs and `--force` was given.
    Overwrite,
    /// The destination differs and `--force` was not given.
    Refuse,
}

impl SkillAction {
    fn dry_run_label(self) -> &'static str {
        match self {
            SkillAction::Install => "Would install SKILL.md",
            SkillAction::AlreadyCurrent => "SKILL.md already current; nothing to do",
            SkillAction::Overwrite => "Differs; would overwrite (--force)",
            SkillAction::Refuse => "Differs; would refuse (use --force to replace)",
        }
    }
}

/// Compare the bundled skill with whatever is at the destination. Reads
/// only; never writes.
fn plan_skill_copy(source: &Path, dest: &Path, force: bool) -> Result<SkillAction> {
    if !dest.exists() {
        return Ok(SkillAction::Install);
    }
    let new = fs::read(source).with_context(|| format!("Failed to read {:?}", source))?;
    let old = fs::read(dest).with_context(|| format!("Failed to read {:?}", dest))?;
    if new == old {
        return Ok(SkillAction::AlreadyCurrent);
    }
    if force {
        Ok(SkillAction::Overwrite)
    } else {
        Ok(SkillAction::Refuse)
    }
}

/// Install rr skills for AI agents
pub fn install_skills(target: &str, dry_run: bool, force: bool) -> Result<()> {
    let targets = parse_targets(target);

    if targets.is_empty() {
        anyhow::bail!(
            "Unknown target '{}'. Use: claude, opencode, codex, or all",
            target
        );
    }

    let exe_path = std::env::current_exe()?;
    let exe_dir = exe_path.parent().context("Could not find exe directory")?;

    // Try to find skills directory - handle both dev and installed layouts
    let skills_dir = if exe_dir
        .file_name()
        .map(|n| n == "debug" || n == "release")
        .unwrap_or(false)
    {
        // Development: target/debug/rr -> project root
        exe_dir
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("skills"))
            .filter(|p| p.exists())
    } else {
        // Installed: binary and skills are side by side
        let side_by_side = exe_dir.join("skills");
        if side_by_side.exists() {
            Some(side_by_side)
        } else {
            // Or one level up
            exe_dir
                .parent()
                .map(|p| p.join("skills"))
                .filter(|p| p.exists())
        }
    };

    let skills_dir =
        skills_dir.context("Skills directory not found. Make sure rr is properly installed.")?;

    println!("{}", "rr Agent Skills Installer".bold().cyan());
    println!("{}", "========================".cyan());
    println!();

    let mut installed = 0;
    let mut skipped = 0;
    let mut refused = 0;

    for agent in &targets {
        let source_dir = skills_dir.join(agent.to_lowercase());
        let dest_dir = get_agent_skills_dir(agent)?;

        if !source_dir.exists() {
            println!(
                "{} Source directory not found: {:?}",
                "⚠".yellow(),
                source_dir
            );
            skipped += 1;
            continue;
        }

        println!(
            "{} {} {}",
            "→".blue(),
            agent.bold(),
            format!("({:?} → {:?})", source_dir, dest_dir).dimmed()
        );

        let skill_file = source_dir.join("SKILL.md");
        if !skill_file.exists() {
            println!("  {} No SKILL.md found in source", "✗".red());
            skipped += 1;
            continue;
        }

        // Decide before writing: a differing file at the destination is
        // left alone unless --force was given.
        let dest_file = dest_dir.join("SKILL.md");
        let action = plan_skill_copy(&skill_file, &dest_file, force)?;

        if dry_run {
            println!("  {} {}", "•".dimmed(), action.dry_run_label());
            if action == SkillAction::Refuse {
                refused += 1;
            } else {
                installed += 1;
            }
            continue;
        }

        if action == SkillAction::AlreadyCurrent {
            println!("  {} SKILL.md already current", "✓".green());
            installed += 1;
            continue;
        }
        if action == SkillAction::Refuse {
            println!(
                "  {} {:?} exists and differs from the bundled skill; not overwritten",
                "✗".red(),
                dest_file
            );
            println!("    Re-run with --force to replace it.");
            refused += 1;
            continue;
        }

        // Create destination directory
        fs::create_dir_all(&dest_dir)
            .with_context(|| format!("Failed to create directory {:?}", dest_dir))?;

        // Copy skill files
        fs::copy(&skill_file, &dest_file)
            .with_context(|| format!("Failed to copy {:?} to {:?}", skill_file, dest_file))?;
        println!("  {} Installed SKILL.md", "✓".green());
        installed += 1;
    }

    println!();
    if dry_run {
        println!(
            "{} Would install skills for {} agents (dry run)",
            "ℹ".blue(),
            installed
        );
    } else if skipped == 0 && refused == 0 {
        println!(
            "{} Successfully installed skills for {} agents!",
            "✓".green(),
            installed
        );
    } else {
        println!(
            "{} Installed {} skills, {} skipped",
            "ℹ".yellow(),
            installed,
            skipped
        );
    }

    if refused > 0 && !dry_run {
        anyhow::bail!(
            "{} existing skill file(s) differ and were not overwritten; use --force to replace",
            refused
        );
    }

    println!();
    println!("{}", "Next steps:".bold());
    println!("  1. Restart your AI agent (Claude, OpenCode, or Codex)");
    println!(
        "  2. Mention '{}' to trigger the skill",
        "push to remarkable".cyan()
    );
    println!();

    Ok(())
}

/// Parse target string into list of agents
fn parse_targets(target: &str) -> Vec<String> {
    let target = target.to_lowercase();
    match target.as_str() {
        "all" => vec![
            "claude".to_string(),
            "opencode".to_string(),
            "codex".to_string(),
        ],
        "claude" => vec!["claude".to_string()],
        "opencode" => vec!["opencode".to_string()],
        "codex" => vec!["codex".to_string()],
        _ => Vec::new(),
    }
}

/// Get the skills directory for a specific agent
fn get_agent_skills_dir(agent: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;

    match agent.to_lowercase().as_str() {
        "claude" => Ok(home.join(".claude").join("skills").join("rr")),
        "opencode" => Ok(home.join(".opencode").join("skills").join("rr")),
        "codex" => Ok(home.join(".codex").join("skills").join("rr")),
        _ => anyhow::bail!("Unknown agent: {}", agent),
    }
}

/// Check if skills are installed for a specific agent
pub fn check_skills(agent: &str) -> Result<bool> {
    let skills_dir = get_agent_skills_dir(agent)?;
    let skill_file = skills_dir.join("SKILL.md");
    Ok(skill_file.exists())
}

/// Show current skill installation status
pub fn status() -> Result<()> {
    println!("{}", "rr Agent Skills Status".bold().cyan());
    println!("{}", "=====================".cyan());
    println!();

    let agents = vec!["claude", "opencode", "codex"];

    for agent in &agents {
        let installed = check_skills(agent)?;
        let status = if installed {
            "installed ✓".green()
        } else {
            "not installed ✗".red()
        };
        println!("  {:12} {}", agent.bold(), status);
    }

    println!();
    println!(
        "Run {} to install skills for all agents",
        "rr skills --target all".cyan()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &Path, text: &str) {
        fs::write(path, text).expect("write fixture");
    }

    #[test]
    fn skill_copy_installs_when_nothing_is_there() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("bundled.md");
        let dst = dir.path().join("installed.md");
        fixture(&src, "bundled");
        let action = plan_skill_copy(&src, &dst, false).unwrap();
        assert_eq!(action, SkillAction::Install);
    }

    #[test]
    fn skill_copy_refuses_to_overwrite_a_different_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("bundled.md");
        let dst = dir.path().join("installed.md");
        fixture(&src, "bundled");
        fixture(&dst, "personalised");
        let action = plan_skill_copy(&src, &dst, false).unwrap();
        assert_eq!(action, SkillAction::Refuse);
        // Planning never writes: the personalised file is untouched.
        assert_eq!(fs::read_to_string(&dst).unwrap(), "personalised");
    }

    #[test]
    fn skill_copy_force_overwrites_a_different_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("bundled.md");
        let dst = dir.path().join("installed.md");
        fixture(&src, "bundled");
        fixture(&dst, "personalised");
        let action = plan_skill_copy(&src, &dst, true).unwrap();
        assert_eq!(action, SkillAction::Overwrite);
    }

    #[test]
    fn skill_copy_identical_file_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("bundled.md");
        let dst = dir.path().join("installed.md");
        fixture(&src, "same bytes");
        fixture(&dst, "same bytes");
        let action = plan_skill_copy(&src, &dst, true).unwrap();
        assert_eq!(action, SkillAction::AlreadyCurrent);
    }
}
