# AGENTS.md (src)

Frontend source for Leptos 0.8 (CSR). This folder compiles to WASM and runs in the Tauri webview.

## Module Structure

- **main.rs** - Frontend WASM entry point (mounts App to DOM)
- **app.rs** - App router with signal-based page selection
- **components/** - Reusable UI components (AppLayout, Sidebar, cards, panels, viewers)
- **pages/** - Page-level views, one module per view
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
- Render a list of stateful or clickable elements with Leptos's keyed list component and a key that
  names the item (the board keys cards by task). A list built with `.map(..)`
  inside a reactive closure is rebuilt in place by position: when an item
  leaves, every later DOM node is re-bound to the next item, and a click or
  drag in progress lands on whichever item the node holds when it ends. Key
  by id alone and hand the item's record to the row as a signal, so a changed
  record updates the row in place instead of replacing its node.
