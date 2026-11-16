//! Docent's own skills, which the harness runs around every coding turn
//! rather than the agent choosing them as it does a project's. Each is a
//! SKILL.md like a project's, built into the binary: its body is what the
//! model is told at that step, and docent does the step's work itself.
//!
//! - `pre-run`, before the code is written: the current API of the
//!   libraries the change uses, looked up on the web when search is on.
//! - `post-run`, once it's written and checks: the project's tests, and the
//!   notes the user asked to remember, kept in `.docent/memory.md`.

// Wired into the turn by the steps that use them.
#![cfg_attr(not(test), allow(dead_code))]

pub const PRE_RUN: &str = include_str!("skills/pre-run/SKILL.md");
pub const POST_RUN: &str = include_str!("skills/post-run/SKILL.md");

/// A skill's text without its frontmatter: what the model is told.
pub fn body(skill: &str) -> &str {
    skill
        .strip_prefix("---")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map_or(skill, |(_, body)| body)
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_skills_are_named_and_told_without_frontmatter() {
        for (skill, name) in [(PRE_RUN, "pre-run"), (POST_RUN, "post-run")] {
            assert!(skill.starts_with(&format!("---\nname: {name}\ndescription: ")));
            assert!(!body(skill).starts_with("---") && !body(skill).contains("description:"));
        }
        assert!(body(POST_RUN).contains("```remember\n"));
        assert_eq!(body("no frontmatter\n"), "no frontmatter");
    }
}
