//! Rule file persistence: read / write + v1 -> v2 auto-migration.
//!
//! On-disk format (the v2 envelope is the canonical form):
//! ```json
//! { "version": 2, "rules": [ { "id": "...", "process_name": "...", "mask": "0xFF", ... } ] }
//! ```
//! Reading is backward compatible with two historical shapes:
//! - v1 bare array `[...]`: the new fields are filled in via
//!   `serde(default)`, completing the migration automatically.
//! - v2 envelope: validate `version`; future versions are rejected to
//!   avoid silently dropping fields.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::procwin::parse_hex_mask;
use crate::rule::{validate_rule, AffinityRule, MatchType, RuleMode, RULES_SCHEMA_VERSION};

/// Rule file name (always placed under the supplied `base_dir`).
pub const RULES_FILE_NAME: &str = "affinity_rules.json";

pub fn rules_file_path(base_dir: &Path) -> PathBuf {
    base_dir.join(RULES_FILE_NAME)
}

/// Input for a new rule (the service generates id / created_at).
pub struct RuleDraft {
    pub process_name: String,
    pub mask: String,
    pub note: String,
    pub match_type: MatchType,
    pub mode: RuleMode,
    pub priority_class: Option<u32>,
    pub io_priority: Option<u32>,
    pub memory_priority: Option<u32>,
}

/// Build a full rule from a draft (validation + id / timestamp generation).
pub fn build_rule(draft: RuleDraft) -> Result<AffinityRule, String> {
    let rule = AffinityRule {
        id: uuid::Uuid::new_v4().to_string(),
        process_name: draft.process_name,
        mask: draft.mask,
        group_masks: None,
        enabled: true,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        note: draft.note,
        match_type: draft.match_type,
        mode: draft.mode,
        priority_class: draft.priority_class,
        io_priority: draft.io_priority,
        memory_priority: draft.memory_priority,
    };
    validate_rule(&rule)?;
    Ok(rule)
}

/// Parse rule JSON text (v1 bare array or v2 envelope) -> v2 rule list.
pub fn parse_rules(raw: &str) -> Result<Vec<AffinityRule>, String> {
    let value: Value = serde_json::from_str(raw).map_err(|e| format!("failed to parse rules file: {e}"))?;
    match value {
        Value::Array(_) => {
            // v1: bare array. New fields use serde(default), so
            // deserializing directly into the v2 structure completes the
            // migration.
            serde_json::from_value(value).map_err(|e| format!("failed to parse rules: {e}"))
        }
        Value::Object(_) => {
            let file: crate::rule::RulesFile =
                serde_json::from_value(value).map_err(|e| format!("failed to parse rules file: {e}"))?;
            if file.version > RULES_SCHEMA_VERSION {
                return Err(format!(
                    "rules file version {} is newer than the supported version {}, please upgrade the app",
                    file.version, RULES_SCHEMA_VERSION
                ));
            }
            // An envelope with `version < 2` is theoretically impossible
            // (v1 was a bare array), so no further migration is needed;
            // the entry point is kept for a future v3.
            Ok(file.rules)
        }
        _ => Err("rules file has an invalid format (expected a JSON array or object)".to_string()),
    }
}

/// Load rules from the given directory. A missing file returns an empty
/// list.
pub fn load_rules(base_dir: &Path) -> Result<Vec<AffinityRule>, String> {
    let path = rules_file_path(base_dir);
    if !path.exists() {
        return Ok(vec![]);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("failed to read rules file: {e}"))?;
    parse_rules(&raw)
}

/// Save the rule list in v2 envelope format (creates the directory if
/// needed).
pub fn save_rules(base_dir: &Path, rules: &[AffinityRule]) -> Result<(), String> {
    if !base_dir.exists() {
        std::fs::create_dir_all(base_dir).map_err(|e| format!("failed to create directory: {e}"))?;
    }
    let file = crate::rule::RulesFile {
        version: RULES_SCHEMA_VERSION,
        rules: rules.to_vec(),
    };
    let json = serde_json::to_string_pretty(&file).map_err(|e| format!("failed to serialize rules: {e}"))?;
    std::fs::write(rules_file_path(base_dir), json)
        .map_err(|e| format!("failed to write rules file: {e}"))
}

/// Quick "mask is non-zero" check (used by the GUI form for instant
/// validation).
pub fn mask_selects_anything(mask: &str) -> bool {
    parse_hex_mask(mask).map(|m| m != 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cpum-core-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const V1_JSON: &str = r#"[
        {"id":"a1","process_name":"code","mask":"0xFF","enabled":true,"created_at":100,"note":"old"}
    ]"#;

    #[test]
    fn parse_v1_bare_array_migrates_to_v2_defaults() {
        let rules = parse_rules(V1_JSON).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].process_name, "code");
        assert_eq!(rules[0].match_type, MatchType::Exact);
        assert_eq!(rules[0].mode, RuleMode::Strict);
        assert_eq!(rules[0].priority_class, None);
        assert_eq!(rules[0].io_priority, None);
        assert_eq!(rules[0].memory_priority, None);
    }

    #[test]
    fn parse_v2_envelope_roundtrip() {
        let dir = temp_dir();
        let rules = parse_rules(V1_JSON).unwrap();
        save_rules(&dir, &rules).unwrap();

        let raw = std::fs::read_to_string(rules_file_path(&dir)).unwrap();
        assert!(raw.contains(r#""version": 2"#), "on-disk format must be the v2 envelope: {raw}");

        let loaded = load_rules(&dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "a1");
        assert_eq!(loaded[0].mask, "0xFF");
        assert_eq!(loaded[0].match_type, MatchType::Exact);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn group_masks_roundtrip_without_losing_legacy_mask() {
        let mut rules = parse_rules(V1_JSON).unwrap();
        rules[0].group_masks = Some(vec!["0x3".into(), "0x5".into()]);
        let dir = temp_dir();
        save_rules(&dir, &rules).unwrap();
        let loaded = load_rules(&dir).unwrap();
        assert_eq!(loaded[0].mask, "0xFF");
        assert_eq!(loaded[0].group_masks.as_deref(), Some(&["0x3".into(), "0x5".into()][..]));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_future_version_rejected() {
        let raw = r#"{"version":99,"rules":[]}"#;
        assert!(parse_rules(raw).is_err());
    }

    #[test]
    fn parse_garbage_rejected() {
        assert!(parse_rules("not json").is_err());
        assert!(parse_rules("123").is_err());
        assert!(parse_rules("\"text\"").is_err());
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = temp_dir();
        assert!(load_rules(&dir).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_rule_generates_id_and_validates() {
        let ok = build_rule(RuleDraft {
            process_name: "code".into(),
            mask: "0xFF".into(),
            note: String::new(),
            match_type: MatchType::Wildcard,
            mode: RuleMode::Soft,
            priority_class: Some(0x20),
            io_priority: Some(1),
            memory_priority: Some(5),
        })
        .unwrap();
        assert!(!ok.id.is_empty());
        assert!(ok.enabled);
        assert_eq!(ok.match_type, MatchType::Wildcard);
        assert_eq!(ok.mode, RuleMode::Soft);

        // Zero mask -> rejected
        let bad = build_rule(RuleDraft {
            process_name: "x".into(),
            mask: "0x0".into(),
            note: String::new(),
            match_type: MatchType::Exact,
            mode: RuleMode::Strict,
            priority_class: None,
            io_priority: None,
            memory_priority: None,
        });
        assert!(bad.is_err());
    }
}
