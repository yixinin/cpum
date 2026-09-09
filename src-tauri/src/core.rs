//! 共享核心逻辑：亲和性规则的加载与应用
//! 供 Tauri 命令 (lib.rs) 和 Windows 服务 (bin/cpum_service.rs) 共同使用
//!
//! 注意：当前 bin/cpum_service.rs 为独立编译，无法直接 import lib crate，
//! 因此 core 模块中的函数主要供 lib.rs 内部复用。
//! 服务二进制内联了等价逻辑以保持零依赖。

#![allow(dead_code)]

use std::path::Path;

use crate::models::AffinityRule;
use crate::process;

/// 返回亲和性规则文件路径
pub fn rules_file_path(base_dir: &Path) -> std::path::PathBuf {
    base_dir.join("affinity_rules.json")
}

/// 从指定目录加载亲和性规则。文件不存在时返回空 Vec。
pub fn load_rules(base_dir: &Path) -> Result<Vec<AffinityRule>, String> {
    let path = rules_file_path(base_dir);
    if !path.exists() {
        return Ok(vec![]);
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取规则文件失败: {e}"))?;
    let rules: Vec<AffinityRule> = serde_json::from_str(&raw)
        .map_err(|e| format!("解析规则文件失败: {e}"))?;
    Ok(rules)
}

/// 将规则列表保存到指定目录
pub fn save_rules(base_dir: &Path, rules: &[AffinityRule]) -> Result<(), String> {
    if !base_dir.exists() {
        std::fs::create_dir_all(base_dir)
            .map_err(|e| format!("创建目录失败: {e}"))?;
    }
    let path = rules_file_path(base_dir);
    let json = serde_json::to_string_pretty(rules)
        .map_err(|e| format!("序列化规则失败: {e}"))?;
    std::fs::write(&path, json)
        .map_err(|e| format!("写入规则文件失败: {e}"))?;
    Ok(())
}

/// 检查进程名是否匹配规则名（不区分大小写，兼容有无 .exe 后缀）
pub fn name_matches(process_name: &str, rule_name: &str) -> bool {
    let p = process_name.to_lowercase();
    let r = rule_name.to_lowercase();
    p == r
        || p == format!("{}.exe", r)
        || p.trim_end_matches(".exe") == r
}

/// 将已加载的亲和性规则应用到当前运行的进程。
/// 返回 (成功数, 失败数)。
pub fn apply_rules(rules: &[AffinityRule]) -> Result<(u32, u32), String> {
    let processes = process::list_processes()?;
    let mut ok_count: u32 = 0;
    let mut fail_count: u32 = 0;

    for rule in rules {
        if !rule.enabled {
            continue;
        }
        let mask = process::parse_hex_mask(&rule.mask)?;
        for p in &processes {
            if name_matches(&p.name, &rule.process_name) {
                match process::set_process_affinity(p.pid, mask) {
                    Ok(_) => ok_count += 1,
                    Err(_) => fail_count += 1,
                }
            }
        }
    }

    Ok((ok_count, fail_count))
}

/// 从指定目录加载规则并立即应用。返回 (成功数, 失败数)。
pub fn apply_rules_from_dir(base_dir: &Path) -> Result<(u32, u32), String> {
    let rules = load_rules(base_dir)?;
    apply_rules(&rules)
}
