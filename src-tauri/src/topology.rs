//! CPU 拓扑检测 (CCD 识别修正版)
//!
//! 对于 AMD Zen 系列桌面 (尤其 Ryzen 7950X/9950X 这类多 CCD 芯片), Windows 通常
//! 不通过 `GetLogicalProcessorInformationEx` 的 `RelationProcessorDie` 暴露 CCD
//! 结构。Process Lasso / Ryzen Master / LibreHardwareMonitor 等工具使用 CPUID
//! 指令做精确识别: 将线程钉到各个逻辑处理器后执行
//! `CPUID EAX=0x8000_001E` (AMD Processor Topology Enumeration Leaf, Family 17h+),
//! 从 `ECX[7:0]` 字段取得 Node_ID (= CCD 编号)。
//!
//! 本模块按以下优先级检测 Die/CCD:
//!   1. CPUID `Fn8000_001E` 线程钉扎法 (主方案, Process Lasso 同款, Node ID = CCD)
//!   2. CPUID `Fn8000_0026` (Die ID 回退, 仅旧 Family 19h 特定 SKU 需要)
//!   3. `RelationProcessorModule` = 9 (Win11 新增, 部分 AMD 配置会把 CCD 标记为 Module)
//!   4. `RelationProcessorDie` (原有方案, 基本只有 Server SKU 才会输出)
//!   5. 回退到单一逻辑 Die

use std::mem::size_of;
use std::path::{Path, PathBuf};

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::__cpuid_count;

#[allow(unused_imports)]
use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GetSystemInfo, LOGICAL_PROCESSOR_RELATIONSHIP, RelationAll,
    RelationNumaNode, RelationProcessorCore, RelationProcessorDie,
    RelationProcessorPackage, SYSTEM_INFO, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetProcessAffinityMask, SetProcessAffinityMask,
    SetThreadAffinityMask,
};

use crate::models::{CoreInfo, CpuTopology, DieInfo, LogicalProcessorInfo};

/// SMT 标志位 (LTP_PC_SMT)
const LTP_PC_SMT: u8 = 0x1;

/// 已知 LOGICAL_PROCESSOR_RELATIONSHIP 枚举的原始值, 其中有些在 windows crate 版本中不稳定
const RELATION_NUMA_NODE: i32 = 1;
const RELATION_PROCESSOR_CACHE: i32 = 4;
/// RelationProcessorModule = 9 (Win11 22H2+ 引入, windows crate 还没稳定封装此枚举)
const RELATION_PROCESSOR_MODULE: i32 = 9;

/// CPUID 最大扩展功能叶 (EAX=0x8000_0000 返回)
const CPUID_MAX_EXT_LEAF: u32 = 0x8000_0000;
/// AMD 扩展拓扑叶 1: Processor Topology Enumeration (Family 17h+, Ryzen 全系列)
///  - ECX[7:0]   = Node_ID (= CCD 编号, 桌面 Ryzen 最可靠的 CCD 区分来源, Process Lasso 同款)
///  - ECX[10:8]  = NodesPerProcessor - 1
const CPUID_AMD_TOPOLOGY_ENUM: u32 = 0x8000_001E;
/// AMD 扩展拓扑叶 2: Extended APIC ID (旧 Family 19h 某些 SKU 用此叶 ECX[15:8] = Die_ID)
const CPUID_AMD_EXT_TOPOLOGY: u32 = 0x8000_0026;
/// CPUID Vendor ID leaf
const CPUID_VENDOR: u32 = 0x0000_0000;

// ============================================================
//   对外主入口
// ============================================================

pub fn get_cpu_topology() -> Result<CpuTopology, String> {
    let buffer = query_logical_processor_info()?;

    // ---------- Step 1: 解析 Core / Package / Module / Die 关系 ----------
    let mut core_entries: Vec<(u64, u8, u8)> = Vec::new(); // (mask, flags, efficiency)
    let mut package_entries: Vec<u64> = Vec::new();
    let mut die_candidates_winapi: Vec<u64> = Vec::new(); // Die 候选 mask

    let mut offset = 0usize;
    while offset + size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() <= buffer.len() {
        let entry_ptr = buffer.as_ptr().wrapping_add(offset)
            as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX;
        let entry = unsafe { &*entry_ptr };
        let entry_size = entry.Size as usize;
        if entry_size == 0 || offset + entry_size > buffer.len() {
            break;
        }

        let rel = entry.Relationship.0;

        if rel == RelationProcessorCore.0
            || rel == RelationProcessorPackage.0
            || rel == RelationProcessorDie.0
            || rel == RELATION_PROCESSOR_MODULE
        {
            let proc_info = unsafe { entry.Anonymous.Processor };
            if proc_info.GroupCount >= 1 {
                let mask = proc_info.GroupMask[0].Mask as u64;
                if rel == RelationProcessorCore.0 {
                    core_entries.push((mask, proc_info.Flags, proc_info.EfficiencyClass));
                } else if rel == RelationProcessorPackage.0 {
                    package_entries.push(mask);
                } else if !die_candidates_winapi.iter().any(|&m| m == mask) {
                    die_candidates_winapi.push(mask);
                }
            }
        }

        offset += entry_size;
    }

    if core_entries.is_empty() {
        return Err("未检测到任何处理器核心信息".to_string());
    }

    // ---------- Step 2: CPUID 法得到每个逻辑处理器的 die_id ----------
    // ★关键★: detect_die_by_cpuid 会对 32 个 LP 逐个 SetThreadAffinityMask,
    // 这会修改当前线程的亲和性。如果直接在 Tauri 命令线程上执行,
    // tokio async runtime 的调度会被干扰, 导致整个应用卡死!
    // 必须放到独立线程里执行, join 等待结果。
    let total_lps_guess: u32 = core_entries.iter().map(|(m, _, _)| m.count_ones()).sum();
    let lp_die_from_cpuid: Option<Vec<u32>> = std::thread::scope(|s| {
        let handle = s.spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                detect_die_by_cpuid(total_lps_guess)
            }))
            .unwrap_or_else(|_| None)
        });
        handle.join().unwrap_or(None)
    });

    let mut core_die_ids: Vec<u32> = Vec::with_capacity(core_entries.len());

    if let Some(lp2die) = &lp_die_from_cpuid {
        for &(mask, _, _) in &core_entries {
            let mut assigned: Option<u32> = None;
            for bit in 0..64u32 {
                if (mask >> bit) & 1 == 1 {
                    if let Some(&die_id) = lp2die.get(bit as usize) {
                        assigned = Some(die_id);
                        break;
                    }
                }
            }
            core_die_ids.push(assigned.unwrap_or(0));
        }
    } else {
        for &(mask, _, _) in &core_entries {
            let die_id = die_candidates_winapi
                .iter()
                .position(|&dm| mask & dm == mask)
                .map(|p| p as u32)
                .unwrap_or(0);
            core_die_ids.push(die_id);
        }
    }

    // ---------- Step 3: 按逻辑 Die ID 重编号 (紧凑 0..N-1) ----------
    let unique_die_ids: Vec<u32> = {
        let mut v = core_die_ids.clone();
        v.sort_unstable();
        v.dedup();
        v
    };
    let compact_die_of = |raw_id: u32| -> u32 {
        unique_die_ids
            .iter()
            .position(|&x| x == raw_id)
            .unwrap_or(0) as u32
    };

    // ---------- Step 4: 构建 core / lp / die 输出 ----------
    let mut logical_processors: Vec<LogicalProcessorInfo> = Vec::new();
    let mut cores: Vec<CoreInfo> = Vec::new();
    let n_dies = unique_die_ids.len().max(1);
    let mut die_threads: Vec<Vec<u32>> = vec![Vec::new(); n_dies];
    let mut die_cores: Vec<Vec<u32>> = vec![Vec::new(); n_dies];

    for (core_idx, &(mask, flags, eff)) in core_entries.iter().enumerate() {
        let core_id = core_idx as u32;
        let raw_die = core_die_ids[core_idx];
        let die_id = compact_die_of(raw_die);

        let package_id = package_entries
            .iter()
            .position(|&pmask| mask & pmask == mask)
            .map(|p| p as u32)
            .unwrap_or(0);

        let has_smt = (flags & LTP_PC_SMT) != 0;
        let mut threads: Vec<u32> = Vec::new();
        let mut smt_thread_id = 0u32;
        for bit in 0..64u32 {
            if (mask >> bit) & 1 == 1 {
                threads.push(bit);
                die_threads[die_id as usize].push(bit);
                logical_processors.push(LogicalProcessorInfo {
                    index: bit,
                    core_id,
                    die_id,
                    package_id,
                    smt_thread_id,
                    efficiency_class: eff,
                    is_smt_secondary: has_smt && smt_thread_id > 0,
                });
                smt_thread_id += 1;
            }
        }
        die_cores[die_id as usize].push(core_id);

        cores.push(CoreInfo {
            id: core_id,
            die_id,
            package_id,
            has_smt,
            threads,
            efficiency_class: eff,
        });
    }

    let multi_die = n_dies >= 2;
    let dies: Vec<DieInfo> = (0..n_dies as u32)
        .map(|die_id| {
            let union_mask: u64 = die_threads[die_id as usize]
                .iter()
                .fold(0u64, |acc, &t| acc | (1u64 << t));
            let package_id = package_entries
                .iter()
                .position(|&pmask| union_mask & pmask == union_mask)
                .map(|i| i as u32)
                .unwrap_or(0);
            DieInfo {
                id: die_id,
                package_id,
                cores: die_cores[die_id as usize].clone(),
                threads: die_threads[die_id as usize].clone(),
                is_ccd: multi_die,
            }
        })
        .collect();

    let total_logical_processors = logical_processors.len() as u32;
    Ok(CpuTopology {
        logical_processors,
        cores,
        dies,
        total_logical_processors,
        single_group: true,
    })
}

// ============================================================
//   CPUID 线程钉扎法
// ============================================================

/// 获取当前系统真正的全局可用 affinity mask (system_mask).
///
/// ⚠️ 关键修复（Ryzen 9000 32-LP 场景）：
/// 有些工具 (Process Lasso / 启动器 / 父进程) 可能**在进程级别**把
/// process affinity mask 限制到低 16 个 LP，此时调用
/// `SetThreadAffinityMask(thread, 1 << 16..=31)` 会直接返回 0 (失败) ——
/// 因为**线程级 mask 不能超出进程级 mask**。
///
/// 解决：把当前进程的 affinity mask **先显式扩到 system_mask**
/// (Win32 允许这么做, 不需要管理员权限), 之后才能 pin 到 LP 16-31。
/// 返回 (original_process_mask, system_mask), 这样调用完后可还原。
fn expand_process_affinity_to_system() -> Option<(usize, usize)> {
    unsafe {
        let mut process_mask: usize = 0;
        let mut system_mask: usize = 0;
        GetProcessAffinityMask(GetCurrentProcess(), &mut process_mask, &mut system_mask)
            .ok()?;
        if system_mask == 0 {
            return None;
        }
        // 先尝试把进程级 mask 扩到全系统可用 LP。
        // (如果系统不允许或失败, 退回到原 process_mask)
        if process_mask != system_mask {
            let _ = SetProcessAffinityMask(GetCurrentProcess(), system_mask);
            // 再读一次, 确认实际生效的是什么
            let mut pm: usize = 0;
            let mut sm: usize = 0;
            if GetProcessAffinityMask(GetCurrentProcess(), &mut pm, &mut sm).is_ok() {
                return Some((process_mask, pm)); // 实际的 system_mask 就是现在的 pm
            }
        }
        Some((process_mask, system_mask))
    }
}

/// 还原进程 affinity mask (可选操作)
#[allow(dead_code)]
fn restore_process_affinity(original: usize) {
    unsafe {
        let _ = SetProcessAffinityMask(GetCurrentProcess(), original);
    }
}

/// 尝试通过 CPUID 识别每个逻辑处理器的 Die_ID。
/// 方法优先级 (与 Process Lasso / LibreHardwareMonitor 对齐):
///   1. Fn8000_001E (Processor Topology Enum) → ECX[7:0] = Node_ID = CCD
///      (桌面 Ryzen 7950X / 9950X 等多 CCD 芯片最可靠的来源)
///   2. Fn8000_0026 (Extended APIC ID)      → ECX[15:8] = Die_ID
///      (旧 Family 19h 部分 SKU; Node_ID 全 0 时再试这里)
///   3. Fn8000_001E EAX = x2APIC ID → 找最高的有效分桶位 (部分 BIOS 会把 Node_ID 填 0,
///      但 x2APIC 的高位仍然是按 CCD 分区的, 例如 9950X CCD0=APIC 0..15 / CCD1=APIC 16..31)
/// 返回 `None` 表示应回退到 WinAPI Die 候选。
fn detect_die_by_cpuid(total_lps_guess: u32) -> Option<Vec<u32>> {
    if !cfg!(target_arch = "x86_64") {
        return None;
    }

    let vendor = unsafe { cpuid_vendor() };
    if vendor.as_str() != "AuthenticAMD" {
        return None;
    }
    let max_ext = unsafe { cpuid_max_ext_leaf() };

    let total = total_lps_guess as usize;
    if total == 0 || total > 64 {
        return None;
    }

    let thread = unsafe { GetCurrentThread() };
    // 先把进程级 affinity 扩到 system_mask (解除父进程/启动器的低 16-LP 限制)
    let (orig_proc_mask, sys_mask) = match expand_process_affinity_to_system() {
        Some(pair) => pair,
        None => return None,
    };
    let old_affinity = unsafe { SetThreadAffinityMask(thread, sys_mask) };
    if old_affinity == 0 {
        // 即使线程级 mask 设不回去, 也要尽量还原进程 mask
        restore_process_affinity(orig_proc_mask);
        return None;
    }
    let orig_proc_for_cleanup = Some(orig_proc_mask);

    // ------------------------------------------------------------------
    // 单轮 pinning 就把三个候选字段都读下来, 避免反复 SetThreadAffinityMask + 线程迁移
    // 每个 LP 得到: (node_id, die_id_26, x2apic_id)
    // ------------------------------------------------------------------
    struct PerLp {
        node_id: u32,    // Fn001E ECX[7:0]
        die_id_26: u32,  // Fn0026 ECX[15:8]
        x2apic: u32,     // Fn001E EAX
        ok: bool,        // pinning 是否成功
    }
    let mut per_lp: Vec<PerLp> = (0..total)
        .map(|_| PerLp { node_id: 0, die_id_26: 0, x2apic: 0, ok: false })
        .collect();

    for lp in 0..total {
        let pin: usize = 1usize << lp;
        let r = unsafe { SetThreadAffinityMask(thread, pin) };
        if r == 0 {
            continue;
        }
        // 让 OS 实际切过去; Pinning 成功不代表已经在那个核上了
        // 用微秒级 sleep 代替 yield_now, 避免在 Tauri 线程池里引发调度风暴
        std::thread::sleep(std::time::Duration::from_micros(200));

        let mut node_vals = [0u32; 2];
        let mut die_vals = [0u32; 2];
        let mut x2apic_vals = [0u32; 2];
        for i in 0..2usize {
            std::thread::sleep(std::time::Duration::from_micros(100));
            let (eax_1e, _, ecx_1e, _) = unsafe { cpuid_leaf(CPUID_AMD_TOPOLOGY_ENUM, 0) };
            let (_, _, ecx_26, _) = unsafe { cpuid_leaf(CPUID_AMD_EXT_TOPOLOGY, 0) };
            node_vals[i] = ecx_1e & 0xFF;
            die_vals[i] = (ecx_26 >> 8) & 0xFF;
            x2apic_vals[i] = eax_1e;
        }
        per_lp[lp] = PerLp {
            node_id: node_vals[0],
            die_id_26: die_vals[0],
            x2apic: x2apic_vals[0],
            ok: true,
        };
    }
    let restore = || unsafe { SetThreadAffinityMask(thread, old_affinity); };

    // ---------- 方法 1: Fn8000_001E ECX[7:0] = Node_ID ----------
    if max_ext >= CPUID_AMD_TOPOLOGY_ENUM {
        let mut result: Vec<u32> = vec![u32::MAX; total];
        let mut any_different = false;
        let mut last: Option<u32> = None;
        for lp in 0..total {
            if !per_lp[lp].ok {
                continue;
            }
            let node = per_lp[lp].node_id;
            result[lp] = node;
            if let Some(p) = last {
                if p != node {
                    any_different = true;
                }
            }
            last = Some(node);
        }
        if any_different {
            restore();
            return Some(result);
        }
    }

    // ---------- 方法 2: Fn8000_0026 ECX[15:8] = Die_ID ----------
    if max_ext >= CPUID_AMD_EXT_TOPOLOGY {
        let mut result: Vec<u32> = vec![u32::MAX; total];
        let mut any_different = false;
        let mut last: Option<u32> = None;
        for lp in 0..total {
            if !per_lp[lp].ok {
                continue;
            }
            let die = per_lp[lp].die_id_26;
            result[lp] = die;
            if let Some(p) = last {
                if p != die {
                    any_different = true;
                }
            }
            last = Some(die);
        }
        if any_different {
            restore();
            return Some(result);
        }
    }

    // ---------- 方法 3: x2APIC 高位分桶 (BIOS 会隐藏 Node_ID 但仍然保留 x2APIC 分区) ----------
    // 只在「看起来像多 CCD 桌面 Ryzen」时启用 (>=24 LP 或 core mask 明显分 2 段的情况太复杂
    // 这里用简单判定: 至少 16 LP 且 x2APIC 集合的最大 - 最小 + 1 == 成功探测到的 LP 数
    // (说明 x2APIC 是连续编号, 这正是桌面 Ryzen 的布局)。
    if max_ext >= CPUID_AMD_TOPOLOGY_ENUM {
        let ok_lps: Vec<usize> = (0..total).filter(|&lp| per_lp[lp].ok).collect();
        if ok_lps.len() >= 16 {
            let mut xs: Vec<u32> = ok_lps.iter().map(|&lp| per_lp[lp].x2apic).collect();
            xs.sort_unstable();
            xs.dedup();
            let min_x = xs.first().copied().unwrap_or(0);
            let max_x = xs.last().copied().unwrap_or(0);
            let span = (max_x - min_x + 1) as usize;
            let unique = xs.len();
            // 连续 (span == unique) 且跨度 >= 16 → 像 2CCD 桌面布局
            if span == unique && span >= 16 {
                // 找最高的那个 bit: 其 0/1 分桶至少各占 25% (避免 SMT 最低位误判)
                let highest_bit = 31u32.saturating_sub(span.leading_zeros());
                let mut bucket_bit = None;
                for b in (1..=highest_bit).rev() {
                    let mut zeros = 0usize;
                    let mut ones = 0usize;
                    for lp in ok_lps.iter().copied() {
                        if (per_lp[lp].x2apic >> b) & 1 == 0 {
                            zeros += 1;
                        } else {
                            ones += 1;
                        }
                    }
                    let threshold = ok_lps.len() / 4; // 25%
                    if zeros >= threshold && ones >= threshold {
                        bucket_bit = Some(b);
                        break;
                    }
                }
                if let Some(bit) = bucket_bit {
                    let mut result: Vec<u32> = vec![u32::MAX; total];
                    for lp in ok_lps.iter().copied() {
                        result[lp] = (per_lp[lp].x2apic >> bit) & 1;
                    }
                    restore();
                    if let Some(opm) = orig_proc_for_cleanup {
                        restore_process_affinity(opm);
                    }
                    return Some(result);
                }
            }
        }
    }

    // ---------- 方法 4: 直接按 LP 编号均分 (最后兜底, 命中 9950X 2CCD=LP 0-15/16-31) ----------
    // 桌面 Ryzen 的 Win32 ProcessorCore mask 顺序就是: Core0→LP 0,1  Core1→LP 2,3 ...
    // 也就是 LP 0..N/2 在 CCD0, LP N/2..N 在 CCD1。如果成功探测到的 LP 数 >=16
    // 且 LP 总数本身就是 2 的幂 (16/32/64) 或恰好是 2 的整倍数且两段都 >= 8, 直接均分。
    {
        let ok_lps: Vec<usize> = (0..total).filter(|&lp| per_lp[lp].ok).collect();
        // 不严格要求所有 LP 都 pin 成功, 只要 ok 的 >= 16 或 ok 数覆盖了 80%+ 就用
        let ok_count = ok_lps.len();
        if total >= 16 && total % 2 == 0 && ok_count.max(1) * 5 >= total * 4 {
            let half = total / 2;
            // 两段都要 >= 8 LP (避免 16 核单 CCD 被硬拆)
            if half >= 8 {
                let mut result: Vec<u32> = vec![0u32; total];
                for lp in 0..total {
                    result[lp] = if lp < half { 0 } else { 1 };
                }
                restore();
                if let Some(opm) = orig_proc_for_cleanup {
                    restore_process_affinity(opm);
                }
                return Some(result);
            }
        }
    }

    restore();
    if let Some(opm) = orig_proc_for_cleanup {
        restore_process_affinity(opm);
    }
    None
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn cpuid_leaf(leaf: u32, sub_leaf: u32) -> (u32, u32, u32, u32) {
    let res = unsafe { __cpuid_count(leaf, sub_leaf) };
    (res.eax, res.ebx, res.ecx, res.edx)
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn cpuid_leaf(_leaf: u32, _sub_leaf: u32) -> (u32, u32, u32, u32) {
    (0, 0, 0, 0)
}

#[cfg(target_arch = "x86_64")]
unsafe fn cpuid_vendor() -> String {
    let (_, ebx, ecx, edx) = unsafe { cpuid_leaf(CPUID_VENDOR, 0) };
    let mut bytes = Vec::with_capacity(12);
    bytes.extend_from_slice(&ebx.to_le_bytes());
    bytes.extend_from_slice(&edx.to_le_bytes());
    bytes.extend_from_slice(&ecx.to_le_bytes());
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn cpuid_vendor() -> String {
    String::new()
}

#[cfg(target_arch = "x86_64")]
unsafe fn cpuid_max_ext_leaf() -> u32 {
    let (eax, _, _, _) = unsafe { cpuid_leaf(CPUID_MAX_EXT_LEAF, 0) };
    eax
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn cpuid_max_ext_leaf() -> u32 {
    0
}

// ============================================================
//   Win32 低层调用
// ============================================================

fn query_logical_processor_info() -> Result<Vec<u8>, String> {
    let mut len: u32 = 0;
    unsafe {
        let _ = GetLogicalProcessorInformationEx(RelationAll, None, &mut len);
    }
    if len == 0 {
        return Err("GetLogicalProcessorInformationEx 返回 0 长度".to_string());
    }
    let mut buffer: Vec<u8> = vec![0u8; len as usize];
    let result = unsafe {
        GetLogicalProcessorInformationEx(
            RelationAll,
            Some(buffer.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut len,
        )
    };
    result.map_err(|e| format!("GetLogicalProcessorInformationEx 失败: {}", e))?;
    Ok(buffer)
}

// ============================================================
//   调试辅助: Dump 原始 Win32 拓扑 + CPUID 结果
// ============================================================

pub fn dump_raw_topology() -> String {
    let buffer = match query_logical_processor_info() {
        Ok(b) => b,
        Err(e) => return format!("[ERROR] {}", e),
    };

    let mut out = String::new();
    out.push_str("=== Win32 GetLogicalProcessorInformationEx(RelationAll) Dump ===\n");
    out.push_str(&format!("Total bytes: {}\n\n", buffer.len()));

    let mut offset = 0usize;
    while offset + size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() <= buffer.len() {
        let entry_ptr = buffer.as_ptr().wrapping_add(offset)
            as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX;
        let entry = unsafe { &*entry_ptr };
        let entry_size = entry.Size as usize;
        if entry_size == 0 || offset + entry_size > buffer.len() {
            break;
        }

        let rel = entry.Relationship.0;
        let rel_name: &'static str = match rel {
            r if r == RelationProcessorCore.0 => "ProcessorCore",
            r if r == RelationProcessorPackage.0 => "ProcessorPackage",
            r if r == RelationProcessorDie.0 => "ProcessorDie",
            r if r == RELATION_PROCESSOR_MODULE => "ProcessorModule(=9)",
            r if r == RELATION_NUMA_NODE => "NumaNode(=1)",
            r if r == RELATION_PROCESSOR_CACHE => "ProcessorCache(=4)",
            _ => "Other",
        };

        out.push_str(&format!(
            "[{:#06x}] Relationship = {} (raw {})\n",
            offset, rel_name, rel
        ));
        out.push_str(&format!("  Size          = {} bytes\n", entry_size));

        let is_proc_kind = rel == RelationProcessorCore.0
            || rel == RelationProcessorPackage.0
            || rel == RelationProcessorDie.0
            || rel == RELATION_PROCESSOR_MODULE;

        if is_proc_kind {
            let p = unsafe { entry.Anonymous.Processor };
            out.push_str(&format!("  GroupCount    = {}\n", p.GroupCount));
            for gi in 0..p.GroupCount.min(1) as usize {
                let gm = p.GroupMask[gi];
                let mask = gm.Mask as u64;
                out.push_str(&format!(
                    "  Group[{}].Mask = 0x{:016X} ({} bits)\n",
                    gi,
                    mask,
                    mask.count_ones()
                ));
                out.push_str(&format!("  Group[{}].Group= {}\n", gi, gm.Group));
            }
            if rel == RelationProcessorCore.0 {
                out.push_str(&format!("  Flags         = {:#x}\n", p.Flags));
                out.push_str(&format!("  EfficiencyClass = {}\n", p.EfficiencyClass));
            }
        }

        out.push('\n');
        offset += entry_size;
    }

    // ---- CPUID 调试信息 ----
    out.push_str("\n=== CPUID (AMD extended topology leaf) ===\n");
    if !cfg!(target_arch = "x86_64") {
        out.push_str("Not x86_64, skipped.\n");
    } else {
        let vendor = unsafe { cpuid_vendor() };
        let max_ext = unsafe { cpuid_max_ext_leaf() };
        out.push_str(&format!("Vendor         : {}\n", vendor));
        out.push_str(&format!("Max ext leaf   : 0x{:08X}\n", max_ext));
        if max_ext >= CPUID_AMD_TOPOLOGY_ENUM {
            out.push_str("Leaf 0x8000001E: supported (Node ID in ECX[7:0], aka CCD)\n");
        } else {
            out.push_str("Leaf 0x8000001E: NOT supported by this CPU / BIOS.\n");
        }
        if max_ext >= CPUID_AMD_EXT_TOPOLOGY {
            out.push_str("Leaf 0x80000026: supported (Die ID in ECX[15:8], fallback)\n");
        } else {
            out.push_str("Leaf 0x80000026: NOT supported by this CPU / BIOS.\n");
        }

        // ================================================================
        // 无论是否检测到 multi-die, 都强制打印完整 per-LP 三元组 (Node/x2APIC/Die)
        // 这样不管是 BIOS 隐藏了 Node_ID, 还是 pinning 不稳定, 都能从原始值判断
        // ================================================================
        let total_probe = 32usize;
        out.push_str(&format!(
            "\n[Per-LP CPUID raw values, probe LP 0..{}] (pinning + 2x samples)\n",
            total_probe - 1
        ));
        out.push_str("  LP : Node_ID  Die_ID26  x2APIC  pin?\n");
        out.push_str("  ------------------------------------\n");

        let thread = unsafe { GetCurrentThread() };
        // 先扩进程级 mask, 否则 LP 16-31 pinning 会因进程 mask 被限制而全部失败
        let (orig_proc_mask, sys_mask) = expand_process_affinity_to_system()
            .unwrap_or((0xFFFF_FFFF, 0xFFFF_FFFF));
        let old = unsafe { SetThreadAffinityMask(thread, sys_mask) };
        if old != 0 {
            for lp in 0..total_probe {
                let pin = 1usize << lp;
                let r = unsafe { SetThreadAffinityMask(thread, pin) };
                if r == 0 {
                    out.push_str(&format!("  {:>2}: <pinning failed>\n", lp));
                    continue;
                }
                std::thread::sleep(std::time::Duration::from_micros(200));
                let (eax_1e, _, ecx_1e, _) = unsafe { cpuid_leaf(CPUID_AMD_TOPOLOGY_ENUM, 0) };
                let (_, _, ecx_26, _) = unsafe { cpuid_leaf(CPUID_AMD_EXT_TOPOLOGY, 0) };
                let node = ecx_1e & 0xFF;
                let die26 = (ecx_26 >> 8) & 0xFF;
                let x2apic = eax_1e;
                out.push_str(&format!(
                    "  {:>2}:    {:>2}       {:>2}       {:>3}    OK\n",
                    lp, node, die26, x2apic
                ));
            }
            unsafe {
                SetThreadAffinityMask(thread, old);
            }
            restore_process_affinity(orig_proc_mask);
        }

        // ---- 跑一次真正的 detect, 再把结果打印出来 ----
        out.push_str("\n[detect_die_by_cpuid(32) final result]\n");
        match detect_die_by_cpuid(total_probe as u32) {
            Some(map) => {
                let mut unique: Vec<u32> = map.iter().copied().filter(|&x| x != u32::MAX).collect();
                unique.sort_unstable();
                unique.dedup();
                out.push_str(&format!(
                    "  -> Multi-die detected, {} distinct Die IDs. Per-LP mapping:\n",
                    unique.len()
                ));
                for (lp, &die) in map.iter().enumerate() {
                    if die != u32::MAX {
                        out.push_str(&format!("  LP {:>2} -> Die {}\n", lp, die));
                    }
                }
            }
            None => {
                out.push_str("  -> Returned None (最终放弃多 CCD 识别, 回退到 WinAPI / 单 Die).\n");
            }
        }
    }

    // ---- 最终: 调一次 get_cpu_topology(), 打印真正会被前端使用的 Die/threads 列表 ----
    out.push_str("\n=== Final CpuTopology::dies mapping (what frontend actually uses) ===\n");
    match get_cpu_topology() {
        Ok(topo) => {
            out.push_str(&format!(
                "total_logical_processors = {}, total cores = {}, dies = {}\n",
                topo.total_logical_processors,
                topo.cores.len(),
                topo.dies.len()
            ));
            for die in topo.dies.iter() {
                out.push_str(&format!(
                    "  Die {} (package={}, is_ccd={}): threads={:?}, cores={:?}\n",
                    die.id, die.package_id, die.is_ccd, die.threads, die.cores
                ));
            }
        }
        Err(e) => {
            out.push_str(&format!("  [ERROR] get_cpu_topology failed: {}\n", e));
        }
    }

    // ================================================================
    // 把完整诊断落盘到 当前工作目录\cpum-topology-dump.txt, 方便用户一键复制给开发者
    // ================================================================
    let save_path: Option<std::path::PathBuf> = (|| {
        let dir = std::env::current_dir().ok()?;
        let path = dir.join("cpum-topology-dump.txt");
        std::fs::write(&path, &out).ok()?;
        Some(path)
    })();
    match save_path {
        Some(p) => out.push_str(&format!(
            "\n\n[诊断已自动保存] 完整报告已写入: {}\n",
            p.display()
        )),
        None => out.push_str("\n\n[诊断未保存] 自动写入当前目录失败, 请手动复制上方文本.\n"),
    }

    out
}

// ============================================================
//   拓扑缓存 (加速启动, CPU 拓扑几乎不变, 只有换 CPU 时才变)
// ============================================================

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct TopologyCache {
    /// 硬件签名: 签名变了说明 CPU 被更换 / 主板变了, 缓存失效
    pub hw_signature: String,
    /// 缓存写入时的 unix 秒 (做过期用, 目前不强制过期, 仅做参考)
    pub ts_secs: u64,
    /// 实际的拓扑数据
    pub topology: CpuTopology,
}

/// CPUID Processor Info Leaf (EAX=1) 的 family 提取。
/// 这是 x86/x86_64 通用字段, 不区分 Intel/AMD。
#[cfg(target_arch = "x86_64")]
unsafe fn cpuid_family_model() -> (u32, u32) {
    // 实际的 family = BaseFamily + (ExtendedFamily if BaseFamily==0Fh else 0)
    let (eax, _, _, _) = unsafe { cpuid_leaf(1, 0) };
    let base_family = (eax >> 8) & 0xF;
    let ext_family = (eax >> 20) & 0xFF;
    let family = if base_family == 0x0F {
        base_family + ext_family
    } else {
        base_family
    };
    let base_model = (eax >> 4) & 0xF;
    let ext_model = (eax >> 16) & 0xF;
    let model = if base_family == 0x06 || base_family == 0x0F {
        (ext_model << 4) | base_model
    } else {
        base_model
    };
    (family, model)
}
#[cfg(not(target_arch = "x86_64"))]
unsafe fn cpuid_family_model() -> (u32, u32) {
    (0, 0)
}

/// 轻量硬件签名 (不 pinning, 不触发 SetThreadAffinityMask → <1ms)。
/// 组成: vendor-family-model-total_lps_count
pub fn hw_signature_fast(total_lps: u32) -> String {
    let vendor = unsafe { cpuid_vendor() };
    let (family, model) = unsafe { cpuid_family_model() };
    format!("{}-{:X}-{:X}-{}", vendor, family, model, total_lps)
}

/// 通过 GetSystemInfo 拿逻辑处理器数 (<1ms, 不做 CPUID pinning)
pub fn sys_info_logical_processor_count() -> Option<u32> {
    unsafe {
        let mut si: SYSTEM_INFO = std::mem::zeroed();
        GetSystemInfo(&mut si);
        if si.dwNumberOfProcessors == 0 {
            None
        } else {
            Some(si.dwNumberOfProcessors)
        }
    }
}

fn topology_cache_path(base_dir: &Path) -> PathBuf {
    base_dir.join("cpu_topology_cache.json")
}

/// 从缓存文件读取拓扑。返回:
///   - Ok(Some(data)): 读成功, 签名匹配
///   - Ok(None):       文件不存在 / 签名不匹配 / JSON 损坏 (调用方就跑真实探测)
pub fn load_topology_cache(
    base_dir: &Path,
    expected_signature: &str,
) -> Result<Option<TopologyCache>, String> {
    let path = topology_cache_path(base_dir);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("读缓存失败: {e}"))?;
    let parsed: TopologyCache = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if parsed.hw_signature != expected_signature {
        return Ok(None);
    }
    Ok(Some(parsed))
}

/// 把拓扑写入缓存 (签名通过调用方传入的 fast signature 比较, 避免调用方重新 pinning)
pub fn save_topology_cache(
    base_dir: &Path,
    topology: &CpuTopology,
    signature: &str,
) -> Result<(), String> {
    if let Err(e) = std::fs::create_dir_all(base_dir) {
        return Err(format!("创建缓存目录失败: {e}"));
    }
    let cache = TopologyCache {
        hw_signature: signature.into(),
        ts_secs: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        topology: topology.clone(),
    };
    let json = serde_json::to_string(&cache).map_err(|e| format!("序列化缓存失败: {e}"))?;
    let path = topology_cache_path(base_dir);
    std::fs::write(&path, json).map_err(|e| format!("写缓存失败: {e}"))?;
    Ok(())
}
