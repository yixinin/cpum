//! Process matcher: three modes - exact name / wildcard / full path.

use crate::rule::{AffinityRule, MatchType};

/// Exact match (v1 behavior): case-insensitive, automatically tolerates
/// the presence/absence of the `.exe` suffix.
pub fn name_matches(process_name: &str, rule_name: &str) -> bool {
    let p = process_name.to_lowercase();
    let r = rule_name.to_lowercase();
    p == r || p == format!("{}.exe", r) || trim_exe(&p) == r
}

/// Decide whether a process is matched by a rule, using the matching mode
/// declared on the rule.
///
/// - `Exact`:   exact process-name match (see [`name_matches`])
/// - `Wildcard`: glob-style match against the process name (`code*` hits
///   `codex.exe`)
/// - `Path`:    glob-style match against the full executable path (no match
///   when the path is unavailable)
///
/// Wildcard syntax: `*` matches any sequence of characters, `?` matches a
/// single character, everything else is a literal comparison (case
/// insensitive).
pub fn rule_matches(rule: &AffinityRule, process_name: &str, image_path: Option<&str>) -> bool {
    match rule.match_type {
        MatchType::Exact => name_matches(process_name, &rule.process_name),
        MatchType::Wildcard => {
            let pattern = rule.process_name.to_lowercase();
            let name = process_name.to_lowercase();
            // Also try the original name and the `.exe`-stripped name so
            // the tolerance matches the exact-match behavior.
            glob_match(&pattern, &name) || glob_match(&pattern, trim_exe(&name))
        }
        MatchType::Path => match image_path {
            Some(path) => glob_match(&rule.process_name.to_lowercase(), &path.to_lowercase()),
            None => false,
        },
    }
}

fn trim_exe(name: &str) -> &str {
    name.strip_suffix(".exe").unwrap_or(name)
}

/// Wildcard matching (case-insensitive, `.exe` suffix tolerated), used by
/// the ProBalance user allowlist. Semantics align with the rule's
/// `Wildcard` mode: `precious*` matches `precious_app.exe`.
pub fn wildcard_matches(process_name: &str, pattern: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let name = process_name.to_lowercase();
    glob_match(&pattern, &name) || glob_match(&pattern, trim_exe(&name))
}

/// Classic two-pointer backtracking wildcard matcher (`*` / `?`), avoids
/// pulling in a regex dependency.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // Position of the most recent '*' and its backtracking anchor.
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            // Backtrack: let '*' consume one more character.
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- glob_match basics ----------

    #[test]
    fn glob_exact_literal() {
        assert!(glob_match("chrome", "chrome"));
        assert!(!glob_match("chrome", "chromium"));
    }

    #[test]
    fn glob_star_prefix() {
        assert!(glob_match("code*", "codex"));
        assert!(glob_match("code*", "codex.exe"));
        assert!(!glob_match("code*", "vscode"));
    }

    #[test]
    fn glob_star_contains() {
        assert!(glob_match("*steam*", "steamclient64.exe"));
        assert!(glob_match("*steam*", "my_steam_helper"));
        assert!(!glob_match("*steam*", "epicgames"));
    }

    #[test]
    fn glob_star_suffix_and_middle() {
        assert!(glob_match("*.exe", "game.exe"));
        assert!(!glob_match("*.exe", "game.dll"));
        assert!(glob_match("a*b*c", "a-x-x-b-y-y-c"));
        assert!(!glob_match("a*b*c", "a-x-x-c"));
    }

    #[test]
    fn glob_question_mark() {
        assert!(glob_match("?", "a"));
        assert!(!glob_match("?", "ab"));
        assert!(glob_match("cod?", "code"));
        assert!(glob_match("c?de*", "codex.exe"));
    }

    #[test]
    fn glob_lone_star_matches_everything() {
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything.exe"));
    }

    // ---------- name_matches (v1 exact semantics) ----------

    #[test]
    fn exact_match_exe_suffix_compatible() {
        assert!(name_matches("code.exe", "code"));
        assert!(name_matches("code", "code"));
        assert!(name_matches("CODE.EXE", "code"));
        assert!(name_matches("code.exe", "code.exe"));
        assert!(!name_matches("vscode.exe", "code"));
    }

    // ---------- rule_matches (ROADMAP M1 acceptance: `code*` hits codex.exe) ----------

    fn rule(match_type: MatchType, name: &str) -> AffinityRule {
        AffinityRule {
            id: "t".into(),
            process_name: name.into(),
            mask: "0xFF".into(),
            group_masks: None,
            enabled: true,
            created_at: 0,
            note: String::new(),
            match_type,
            mode: Default::default(),
            priority_class: None,
            io_priority: None,
            memory_priority: None,
        }
    }

    #[test]
    fn wildcard_rule_acceptance() {
        let r = rule(MatchType::Wildcard, "code*");
        assert!(rule_matches(&r, "codex.exe", None));
        assert!(rule_matches(&r, "code.exe", None));
        assert!(!rule_matches(&r, "vscode.exe", None));
    }

    #[test]
    fn wildcard_rule_matches_trimmed_name() {
        // A pattern without any wildcards degenerates to literal match,
        // but still tolerates the `.exe` suffix.
        let r = rule(MatchType::Wildcard, "code");
        assert!(rule_matches(&r, "code.exe", None));
        assert!(rule_matches(&r, "code", None));
    }

    #[test]
    fn path_rule_matches_full_image_path() {
        let r = rule(MatchType::Path, r"c:\games\*\game.exe");
        assert!(rule_matches(&r, "game.exe", Some(r"C:\Games\App1\Game.exe")));
        assert!(!rule_matches(&r, "game.exe", Some(r"C:\Tools\Game.exe")));
    }

    #[test]
    fn path_rule_without_path_never_matches() {
        let r = rule(MatchType::Path, r"c:\x\y.exe");
        assert!(!rule_matches(&r, "y.exe", None));
    }
}
