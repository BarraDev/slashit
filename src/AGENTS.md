# CLAUDE.md (src)

Frontend source for Leptos 0.8 (CSR). This folder compiles to WASM and runs in the Tauri webview.

## Module Structure

- **main.rs** - Frontend WASM entry point (mounts App to DOM)
- **app.rs** - App router with signal-based page selection
- **components/** - Reusable UI components (AppLayout, Sidebar, cards, panels, viewers)
- **pages/** - Page-level views (Dashboard, Agent, Spec, Context, Settings)
- **services/** - Tauri IPC wrappers for backend commands
- **models/** - Frontend domain models mirroring backend domain

## Routing Pattern

The app uses signal-based routing (not URL-based). `current_page` signal in `app.rs` determines which page component renders. Navigation updates this signal via `set_current_page`.

## Tauri IPC

Frontend calls backend via `invoke()` from `window.__TAURI__.core` (global Tauri enabled). Service functions in `src/services/` wrap these calls with proper typing.

## Leptos Patterns

- Use `leptos::prelude::*` for components and signals
- Signals are `(value, setter)` tuples from `signal()` or `signal(initial_value)`
- Use `view! { }` macro for JSX-like syntax
- Components accept `#[prop]` attributes for props
