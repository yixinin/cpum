# CPU Manager

[中文文档](README.zh-CN.md)

CPU Manager is a Windows desktop application for viewing processes and managing their CPU affinity. It combines a Vue 3 user interface with a Rust/Tauri backend and an optional Windows service that reapplies saved affinity rules automatically.

> This project uses Windows APIs and is intended for Windows only.

## Features

- Inspect the CPU topology, including logical processors, physical cores, SMT threads, packages, and detected CCD/die information.
- Browse running processes in a hierarchical view with live CPU and memory metrics.
- Change the affinity mask of an individual process using a topology-aware editor.
- Create, enable, edit, delete, and apply persistent affinity rules.
- Match rules case-insensitively by executable name; `.exe` is optional in a rule.
- Install an optional `CpumAffinityService` Windows service that starts automatically and scans for matching processes every five seconds.
- Persist shared service rules in `C:\\ProgramData\\cpum\\affinity_rules.json`; older per-user rule locations are migrated when read.

## Screens and workflow

1. Start CPU Manager and let it load the CPU topology and process list.
2. Select a process to set its affinity immediately, or open the rule manager to save a reusable rule.
3. Use **Apply Rules** to apply every enabled rule to currently running matching processes.
4. Install the Windows service from the application if rules should also be applied after sign-in or reboot.

Changing affinity can affect responsiveness and throughput. Test masks carefully, especially on hybrid CPUs or machines running latency-sensitive workloads.

## Requirements

For development:

- Windows 10 or later
- Node.js, npm, and Yarn (the Tauri development hook uses Yarn)
- Rust stable with the MSVC toolchain
- Microsoft C++ Build Tools / Visual Studio Build Tools (required by the Rust Windows toolchain)
- WebView2 Runtime (normally included with current Windows installations)

The app requests administrator privileges. Some processes are protected by Windows or belong to another security context and may still reject affinity changes.

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

The release script refreshes the application icons, builds the frontend, compiles `cpum_service.exe`, and produces a per-machine NSIS installer:

```powershell
.\\build.bat
```

The installer is written to:

```text
src-tauri\\target\\release\\bundle\\nsis\\CPU Manager_0.1.0_x64-setup.exe
```

Alternatively, build the frontend and Tauri bundle manually. Build `cpum_service.exe` first so it can be included as a bundle resource:

```powershell
npm run build
cd src-tauri
cargo build --release --bin cpum_service
cd ..
npx tauri build --bundles nsis
```

## Affinity rules and service

Rules are JSON records containing a process name, a hexadecimal CPU mask, an enabled flag, and an optional note. The primary shared location is:

```text
C:\\ProgramData\\cpum\\affinity_rules.json
```

The application can install, start, stop, and uninstall the `CpumAffinityService` service. The service runs as `LocalSystem`, starts automatically, and receives the shared rule directory when installed. It attempts to enable `SeDebugPrivilege` so it can apply rules to processes in user sessions.

To verify an installation from an elevated PowerShell prompt:

```powershell
sc.exe qc CpumAffinityService
sc.exe query CpumAffinityService
Get-Content C:\\ProgramData\\cpum\\affinity_rules.json
```

`Running` only confirms that the service is running; it does not prove that a rule matched a process or that Windows accepted its affinity mask. Confirm the service binary path and arguments, the rule file, and the target process affinity when troubleshooting.

For a one-time diagnostic application of the rules, run the installed service executable with:

```powershell
& "<path-to-cpum_service.exe>" --apply-once C:\\ProgramData\\cpum
```

## Project layout

```text
src/                         Vue 3 frontend
  components/                Affinity and persistent-rule dialogs
  composables/               Topology, process, metrics, and process-tree state
  api.ts                     Tauri IPC wrappers
src-tauri/                   Rust/Tauri backend
  src/process.rs             Process enumeration, metrics, affinity operations
  src/topology.rs            CPU topology detection
  src/bin/cpum_service.rs    Windows service executable
build.bat                    NSIS release build script
```

## Notes and limitations

- Affinity masks use a 64-bit value. The UI reports whether the system is limited to a single processor group; multi-group systems have Windows affinity constraints.
- Network I/O counters are available only on supported Windows versions and can be reported as zero elsewhere.
- Saving a rule does not automatically apply it until you use **Apply Rules** or the service detects a matching process.

## License

CPU Manager is licensed under the [MIT License](LICENSE).
