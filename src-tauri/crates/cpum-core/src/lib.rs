//! cpum shared core crate.
//!
//! Consolidates the "rule model / matching / persistence / Win32 process write
//! operations / rule application engine" into a single implementation, shared
//! by the Tauri GUI (cpum) and the Windows service (cpum_service). Previously
//! the same logic drifted independently in three places: lib.rs / core.rs /
//! cpum_service.rs.
//!
//! This crate does not depend on tauri, so the service binary can reuse it
//! without pulling in a GUI framework.
//!
//! Module layout:
//! - [`rule`]:       Rule data model (schema v2) and value validation
//! - [`matcher`]:    Process name / path matching (exact / wildcard / path)
//! - [`store`]:      Rule file persistence (v1 auto-migrated to v2 envelope)
//! - [`procwin`]:    Win32 process operations (affinity / CPU Sets / three
//!                   priority classes / lightweight enumeration)
//! - [`engine`]:     Rule application engine (enumerate -> match -> apply -> report)
//! - [`monitor`]:    Lightweight monitor (process CPU differential sampling +
//!                   foreground process identification)
//! - [`probalance`]: Dynamic optimization engine (contention detection ->
//!                   background downgrade -> automatic restore)

pub mod engine;
pub mod matcher;
pub mod monitor;
pub mod probalance;
pub mod procwin;
pub mod rule;
pub mod store;

pub use rule::{AffinityRule, MatchType, RuleMode};
