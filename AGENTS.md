# Repository Guidelines

## Project Structure & Module Organization

This is a **Tauri desktop application** for managing CPU affinity on Windows.

```
cpum/
├── src/                                # Vue 3 + TypeScript frontend
│   ├── api.ts                          # Tauri IPC command wrappers
│   ├── App.vue                         # Main application shell (orchestrator)
│   ├── i18n.ts                         # Bilingual (zh-CN / en-US) message dictionary & t() helper
│   ├── types.ts                        # Shared TypeScript types (mirrors Rust models)
│   ├── constants.ts                    # Shared constants and type aliases
│   ├── components/
│   │   ├── AffinityEditor.vue          # Per-process affinity editor dialog
│   │   └── AffinityRuleManager.vue     # Persistent affinity rules dialog
│   └── composables/
│       ├── useTopology.ts              # CPU topology cache + refresh lifecycle
│       ├── useProcessManager.ts        # Process list bootstrap, refresh, search
│       ├── useMetricsStream.ts         # Real-time metrics event handling
│       └── useProcessTree.ts           # Process tree/flattening logic
├── src-tauri/                          # Rust backend (Tauri commands)
│   ├── src/
│   │   ├── lib.rs                      # Tauri command registrations & entry point
│   │   ├── models.rs                   # Shared data models (serde)
│   │   ├── process.rs                  # Process enumeration, affinity read/write, metrics
│   │   └── topology.rs                 # CPU topology detection (CCD, cores, SMT)
│   └── Cargo.toml
└── package.json
```

## Build, Test, and Development Commands

| Command | Description |
|---------|-------------|
| `cd src-tauri && cargo check` | Check Rust code for compilation errors |
| `npx vue-tsc --noEmit` | Type-check frontend TypeScript/Vue files |
| `cargo tauri dev` | Run the full Tauri app in development mode |
| `cargo tauri build` | Build the production application |

## Coding Style & Naming Conventions

- **Rust**: Standard `rustfmt` style. `snake_case` for functions/variables, `PascalCase` for types.
- **TypeScript/Vue**: Vue 3 `<script setup>` syntax. Components use `PascalCase` filenames. Composables prefixed with `use`.
- **Indentation**: 2 spaces (frontend), 4 spaces (Rust).
- **Encoding**: All files must be **UTF-8 without BOM**. Never use PowerShell `Set-Content` for non-ASCII content — use Python or Node.js instead.
- **i18n (MANDATORY)**: ALL user-visible strings MUST go through the dictionary in `src/i18n.ts` — never hardcode Chinese or English UI text in `.vue` templates, `<script>` logic, or composables. Details below.

## Internationalization (i18n)

The UI is bilingual (**zh-CN / en-US**), managed by `src/i18n.ts`:

- **Dictionary**: every message is a key with entries in both `messages["zh-CN"]` and `messages["en-US"]`. When adding a key, add it to **both locales** (keep them in sync).
- **Interpolation**: `t()` supports `{param}` placeholders — `t("rulesAppliedTo", { count })`. Use this instead of string concatenation.
- **Usage**:
  - In components: `const { t, toggleLocale } = useI18n()` — `t` is reactive, so texts update instantly when the locale toggles.
  - Outside components (composables / plain `.ts`): `import { t } from "../i18n"` — the module-level `t` reads the current locale ref directly.
- **Persistence**: locale is stored in `localStorage` under key `cpum-locale`.
- **Code comments must be in English**: this is an open-source project, so all non-user-facing comments (Rust `//` / `///` / `//!`, TypeScript `//` / `/* */`, Vue templates, Markdown, and user-visible Rust strings such as `eprintln!` / `format!` / `panic!` macros) MUST be in English. Only the zh-CN translation values inside `src/i18n.ts` and the Chinese-localized `README.zh-CN.md` stay in Chinese by design.
- **Technical terms stay as-is in both locales**: `PID`, `LP`, `CCD`, `Mask`, `0xFF` etc.
- **Known limitation**: success messages returned by the Rust backend (e.g. service install/start) bypass the frontend dictionary; translating them requires backend-side changes.
- **Verification**: after i18n changes, run `npx vue-tsc --noEmit` and grep the repo tree to confirm no Chinese string literals remain outside `i18n.ts` (zh-CN values) and `README.zh-CN.md` (the intentionally Chinese readme).

## Architecture Notes

- **Composables**: Each composable manages a single concern (`useTopology`, `useProcessManager`, `useMetricsStream`, `useProcessTree`). App.vue orchestrates them.
- **Data flow**: Frontend calls Tauri commands via `invoke()` in `api.ts`. Rust backend executes Windows API calls and returns results.
- **Events**: Backend pushes real-time metrics via `app.emit()`; frontend subscribes with `listen()`.
- **Process rules**: Persisted as JSON at `%APPDATA%/com.eason.cpum/affinity_rules.json` (the Tauri per-user data directory; the same directory holds `probalance.json` and the ProBalance journal/status files). A rule can manage affinity (hex mask + strict/soft schedule mode) and CPU/IO/memory priorities; processes are matched by exact name, wildcard, or full executable path (case-insensitive).
- **Privilege model**: the desktop app runs un-elevated (`asInvoker` manifest) and ships as a per-user NSIS installer into `%LOCALAPPDATA%` (`installMode: "currentUser"`). Only Windows service management needs administrator rights: `install_service` always re-launches `sc.exe` through an elevated helper, and `uninstall/start/stop_service` fall back to elevation on access-denied. The NSIS hooks in `installer-hooks.nsh` touch the service only when it already exists, so a fresh install (and a plain app update without the service) shows no UAC prompt.
- **Never move rules/config back to a machine-level directory** such as `%ProgramData%`: an un-elevated GUI cannot write there, and the service reads the directory passed as its startup argument.
- **Privileged fallback**: `set_process_affinity` / `set_process_priority` first run locally; on `ERROR_ACCESS_DENIED` they go through `cpum_core::ipc` (named-pipe bridge to the LocalSystem service, token-authenticated) and only then fall back to a one-off elevated launch of `cpum_service.exe --set-affinity/--set-priority`. `apply_affinity_rules` uses the bridge for the processes it could not handle, but never the UAC path (a batch operation must not raise one prompt per process).