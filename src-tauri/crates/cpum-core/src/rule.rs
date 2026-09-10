//! Rule data model (schema v2) and value validation.
//!
//! Schema evolution:
//! - v1: `affinity_rules.json` is a bare array `[AffinityRule, ...]`, with
//!   only the affinity fields.
//! - v2: introduces an `{ "version": 2, "rules": [...] }` envelope; rules
//!   gain a matching mode (`match_type`), scheduling mode (`mode`), and
//!   the three priority classes.
//!
//! Compatibility: every v2-added field carries `#[serde(default)]`, so a
//! v1 bare array can be deserialized directly into v2 rules (see
//! [`crate::store::parse_rules`]). Existing users upgrade transparently.

use serde::{Deserialize, Serialize};

/// Current rule file schema version.
pub const RULES_SCHEMA_VERSION: u32 = 2;

/// Process matching mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchType {
    /// Exact process-name match (case-insensitive, automatically tolerates
    /// the presence/absence of `.exe`) - the v1 behavior.
    #[default]
    Exact,
    /// Glob-style match against the process name (`code*`, `*steam*`, `?`
    /// as a single-character placeholder).
    Wildcard,
    /// Match against the full executable path (supports wildcards, e.g.
    /// `C:\Games\*\game.exe`).
    Path,
}

/// Affinity scheduling mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleMode {
    /// Strict mode: hard affinity mask (`SetProcessAffinityMask`); the
    /// process is pinned to the selected cores.
    #[default]
    Strict,
    /// Soft mode: CPU Sets (`SetProcessDefaultCpuSets`); the scheduler may
    /// temporarily drift the process to other cores under load peaks,
    /// preventing the pinned cores from saturating (Win10 1803+).
    Soft,
}

/// Affinity rule (schema v2).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AffinityRule {
    /// Rule ID (UUID).
    pub id: String,
    /// Process name / wildcard pattern / path pattern (interpreted based
    /// on `match_type`).
    pub process_name: String,
    /// Affinity mask (hex string, e.g. "0xFF").
    pub mask: String,
    /// Per-processor-group masks (schema v3). When absent, `mask` is the
    /// legacy group-0 value; this keeps all existing rule files readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_masks: Option<Vec<String>>,
    /// Whether the rule is enabled.
    pub enabled: bool,
    /// Creation time (Unix seconds).
    pub created_at: u64,
    /// Free-form note.
    pub note: String,
    /// Matching mode (added in v2; default = Exact).
    #[serde(default)]
    pub match_type: MatchType,
    /// Scheduling mode (added in v2; default = Strict hard mask).
    #[serde(default)]
    pub mode: RuleMode,
    /// CPU priority class managed by the rule (None = do not adjust).
    #[serde(default)]
    pub priority_class: Option<u32>,
    /// IO priority managed by the rule (None = do not adjust).
    #[serde(default)]
    pub io_priority: Option<u32>,
    /// Memory priority managed by the rule (None = do not adjust).
    #[serde(default)]
    pub memory_priority: Option<u32>,
}

/// Rule file envelope `{ "version": 2, "rules": [...] }` - the on-disk
/// format since v2.
#[derive(Serialize, Deserialize, Debug)]
pub struct RulesFile {
    pub version: u32,
    pub rules: Vec<AffinityRule>,
}

impl AffinityRule {
    /// Whether the rule manages any priority class. ProBalance uses this to
    /// build its protection list: matched processes are excluded from
    /// background downgrades so the rule engine's 5-second priority reset
    /// does not fight with ProBalance's downgrade.
    pub fn manages_priorities(&self) -> bool {
        self.priority_class.is_some()
            || self.io_priority.is_some()
            || self.memory_priority.is_some()
    }
}

// =========================================================================
// Value validation (shared by the GUI command layer and the rule engine;
// single source of truth)
// =========================================================================

/// Allowed CPU priority class raw values (Win32):
/// 0x40=Idle, 0x4000=Below Normal, 0x20=Normal, 0x8000=Above Normal,
/// 0x80=High, 0x100=Realtime.
pub const VALID_PRIORITY_CLASSES: [u32; 6] = [0x40, 0x4000, 0x20, 0x8000, 0x80, 0x100];

pub fn valid_priority_class(pc: u32) -> bool {
    VALID_PRIORITY_CLASSES.contains(&pc)
}

pub fn valid_io_priority(io: u32) -> bool {
    // 0=Very Low, 1=Low, 2=Normal (3=High is reserved for the system
    // and not exposed).
    io <= 2
}

pub fn valid_memory_priority(mp: u32) -> bool {
    // 1=Very Low, 2=Low, 3=Medium, 4=Below Normal, 5=Normal
    (1..=5).contains(&mp)
}

/// Validate a rule's fields (mask is parseable and non-zero, priority
/// values are legal). Mask parsing delegates to
/// [`crate::procwin::parse_hex_mask`].
pub fn validate_rule(rule: &AffinityRule) -> Result<(), String> {
    let masks = match &rule.group_masks {
        Some(values) => crate::procwin::GroupMasks::from_hex_list(values)?.0,
        None => vec![crate::procwin::parse_hex_mask(&rule.mask)
            .map_err(|e| format!("invalid mask for rule {}: {}", rule.process_name, e))?],
    };
    if masks.iter().all(|mask| *mask == 0) {
        return Err(format!(
            "rule {} must select at least one logical processor",
            rule.process_name
        ));
    }
    if let Some(pc) = rule.priority_class {
        if !valid_priority_class(pc) {
            return Err(format!(
                "invalid CPU priority class for rule {}: 0x{:X}",
                rule.process_name, pc
            ));
        }
    }
    if let Some(io) = rule.io_priority {
        if !valid_io_priority(io) {
            return Err(format!(
                "invalid IO priority for rule {}: {} (allowed: 0=Very Low, 1=Low, 2=Normal)",
                rule.process_name, io
            ));
        }
    }
    if let Some(mp) = rule.memory_priority {
        if !valid_memory_priority(mp) {
            return Err(format!(
                "invalid memory priority for rule {}: {} (allowed 1-5)",
                rule.process_name, mp
            ));
        }
    }
    Ok(())
}
