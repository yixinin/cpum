# CPU Manager

[中文版](README.zh-CN.md)

CPU Manager is a Windows desktop application for inspecting CPU topology, browsing running processes, and managing their CPU affinity, scheduling hints, and priorities. It combines a Vue 3 user interface with a Rust/Tauri backend and an optional Windows service that reapplies saved rules automatically. A foreground-aware **ProBalance** engine is included to keep the system responsive when the foreground process comes under CPU contention.

> This project uses Windows APIs and is intended for Windows only.

## Features

- Inspect the CPU topology: logical processors, physical cores, SMT threads, packages, and detected CCD/die information. Intel hybrid P/E-core distinction is preserved.
- Browse running processes in a hierarchical view with live CPU, memory, disk, and (on Win11 24H2+) network metrics.
- Change the affinity mask of an individual process using a topology-aware editor. Multi-processor-group systems are handled with per-group masks rather than a single 64-bit value.
- Create, enable, edit, delete, and apply persistent affinity rules.
- Rule matching modes:
  - **Exact** — case-insensitive process name (`.exe` is optional), the v1 behavior.
  - **Wildcard** — glob pattern against the process name (`code*`, `*steam*`, `?` as a single character).
  - **Path** — glob pattern against the full executable path (e.g. `C:\Games\*\game.exe`).
- Rule scheduling mode: **Strict** (hard `SetProcessAffinityMask`) or **Soft** (`SetProcessDefaultCpuSets`, Win10 1803+; the scheduler may temporarily drift the process to other cores under load).
- Rules may also pin **CPU priority class**, **I/O priority**, and **memory priority**. ProBalance automatically excludes processes whose priorities are managed by a rule so the two engines do not fight.
- ProBalance dynamic optimization: when the foreground process's CPU stays above a configurable threshold, background processes that exceed a CPU threshold are temporarily downgraded (priority class + I/O priority) and restored automatically once contention clears, the process exits, or the feature is disabled. The status, statistics, and JSONL journal are exposed in the GUI.
- Install an optional `CpumAffinityService` Windows service that starts automatically and scans for matching processes every five seconds. The same service hosts the ProBalance runtime.
- Persist shared rules and ProBalance configuration in `C:\ProgramData\cpum\` (`affinity_rules.json`, `probalance.json`, journal/status files). Per-user and Tauri identifier rule locations are migrated to the machine-level directory on install.

## Screens and workflow

1. Start CPU Manager and let it load the CPU topology and process list.
2. Open the **Process** tab to set a single process's affinity and priorities immediately, or switch to the **Rules** tab to save a reusable rule.
3. Use **Apply Rules** to apply every enabled rule to currently running matching processes.
4. Use the **ProBalance** tab to configure foreground contention thresholds, allowlists, and view the live status / log.
5. Install the Windows service from the application if rules and ProBalance should also be enforced after sign-in or reboot.

Changing affinity can affect responsiveness and throughput. Test masks carefully, especially on hybrid CPUs or machines running latency-sensitive workloads. ProBalance downgrades priorities only; it never kills or hard-pins background processes.

## Requirements

For development:

- Windows 10 or later
- Node.js and npm
- Yarn (the Tauri dev hook runs `yarn dev`)
- Rust stable with the MSVC toolchain
- Microsoft C++ Build Tools / Visual Studio Build Tools (required by the Rust Windows toolchain)
- WebView2 Runtime (normally included with current Windows installations)

The app requests administrator privileges (the desktop binary embeds a `requireAdministrator` manifest). Some processes are protected by Windows or belong to another security context and may still reject affinity or priority changes.

## Development

Install the JavaScript dependencies:

```powershell
yarn install
```

Start the app in development mode:

```powershell
npx tauri dev
```

Run the frontend type check and Rust compilation check independently:

```powershell
npx vue-tsc --noEmit
cd src-tauri
cargo check
```

## Production build

The release script refreshes the application icons, builds the frontend, compiles `cpum_service.exe` from the workspace, and produces a per-machine NSIS installer:

```powershell
.\build.bat
```

The installer is written to:

```text
src-tauri\target\release\bundle\nsis\CPU Manager_0.1.0_x64-setup.exe
```

Alternatively, build the frontend and Tauri bundle manually. Build the Windows service binary first so it can be picked up as a Tauri bundle resource (the path is declared in `tauri.conf.json`):

```powershell
npm run build
cd src-tauri
cargo build --release -p cpum-service --bin cpum_service
cd ..
npx tauri build --bundles nsis
```

## Rules, ProBalance, and the service

### Rule schema (v2)

Rule files use a v2 envelope and live at:

```text
C:\ProgramData\cpum\affinity_rules.json
```

The on-disk format:

```json
{
  "version": 2,
  "rules": [
    {
      "id": "<uuid>",
      "process_name": "code",
      "mask": "0xFF",
      "group_masks": ["0xF", "0xF0"],
      "enabled": true,
      "created_at": 1736000000,
      "note": "front-end on P-cores",
      "match_type": "exact",
      "mode": "strict",
      "priority_class": "0x80",
      "io_priority": 1,
      "memory_priority": 5
    }
  ]
}
```

- `group_masks` is optional. When omitted, `mask` is treated as the legacy group-0 value, so every existing rule file is still readable.
- `match_type` is one of `exact` (default), `wildcard`, or `path`.
- `mode` is one of `strict` (default) or `soft`.
- `priority_class`, `io_priority`, and `memory_priority` are all optional; `null`/missing means "do not adjust". When any of them is set, ProBalance treats the matched process as protected and will not downgrade it.
- The legacy v1 format (a bare JSON array) is still parsed and auto-migrated to v2 defaults on read; the file is rewritten in v2 form on the next save.

### Windows service

The application can install, start, stop, and uninstall the `CpumAffinityService` service. The service runs as `LocalSystem`, starts automatically, receives the shared rule directory (`C:\ProgramData\cpum`) when installed, and hosts both the rule engine and the ProBalance runtime. It attempts to enable `SeDebugPrivilege` so it can apply rules to processes in user sessions.

To verify an installation from an elevated PowerShell prompt:

```powershell
sc.exe qc CpumAffinityService
sc.exe query CpumAffinityService
Get-Content C:\ProgramData\cpum\affinity_rules.json
```

`Running` only confirms that the service is running; it does not prove that a rule matched a process or that Windows accepted its affinity mask. Confirm the service binary path and arguments, the rule file, and the target process affinity when troubleshooting.

For a one-time diagnostic application of the rules, run the installed service executable with:

```powershell
& "<path-to-cpum_service.exe>" --apply-once C:\ProgramData\cpum
```

## Project layout

```text
src/                              Vue 3 frontend
  components/                     Affinity editor, rule manager, ProBalance panel
  composables/                    Topology, process, metrics, process-tree state
  api.ts                          Tauri IPC wrappers
  i18n.ts                         Bilingual (zh-CN / en-US) dictionary
src-tauri/                        Tauri desktop binary
  src/
    lib.rs                        Tauri commands (topology, process, rules, ProBalance)
    models.rs                     Shared serde data models
    process/                      Process enumeration, metrics, sampling
    topology.rs                   CPU topology detection
  crates/
    cpum-core/                    Rule model, matcher, store, ProBalance, procwin
    cpum-service/                 cpum_service.exe (Windows service + ProBalance runtime)
  installer-hooks.nsh             NSIS hooks: migrate legacy rule files to %ProgramData%
build.bat                         NSIS release build script
```

## Notes and limitations

- On multi-processor-group systems, rules carry a `group_masks` array (one entry per active group). The single-value `mask` field remains the legacy group-0 view and is kept for backward compatibility.
- Network I/O counters require Win11 24H2+ (NtQueryInformationProcess / `ProcessNetworkIoCounters`); on older Windows versions the values are reported as zero.
- Saving a rule does not automatically apply it until you use **Apply Rules** or the service detects a matching process.
- ProBalance only downgrades CPU/IO priority; it never modifies affinity masks. Rule-managed priorities are excluded from ProBalance actions to keep the two engines from undoing each other.

## License

CPU Manager is licensed under the [MIT License](LICENSE).
