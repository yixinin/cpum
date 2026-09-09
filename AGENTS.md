# Repository Guidelines

## Project Structure & Module Organization

This is a **Tauri desktop application** for managing CPU affinity on Windows.

```
cpum/
├── src/                                # Vue 3 + TypeScript frontend
│   ├── api.ts                          # Tauri IPC command wrappers
│   ├── App.vue                         # Main application shell (orchestrator)
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

## Architecture Notes

- **Composables**: Each composable manages a single concern (`useTopology`, `useProcessManager`, `useMetricsStream`, `useProcessTree`). App.vue orchestrates them.
- **Data flow**: Frontend calls Tauri commands via `invoke()` in `api.ts`. Rust backend executes Windows API calls and returns results.
- **Events**: Backend pushes real-time metrics via `app.emit()`; frontend subscribes with `listen()`.
- **Affinity rules**: Persisted as JSON at `%APPDATA%/cpum/affinity_rules.json`. Applied by matching process names (case-insensitive).