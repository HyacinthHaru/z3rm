# Extension Development

Extensions are ES modules loaded into QuickJS sandbox. Each extension gets an isolated runtime with capability-based API surface.

## Manifest

Every extension has `extension.toml`:

```toml
[extension]
name = "my-extension"
version = "0.1.0"
main = "main.js"
description = "My custom z3rm extension"
author = "Your Name"
license = "MIT"

[permissions]
network = false
wasm = false
filesystem = false
clipboard = false
notifications = false

[dependencies]
# Other extensions this depends on
# my-other-extension = "0.1.0"

[chrome]
# Extension contributes chrome surfaces
# tabs = "TabView"
# status_bar = "StatusView"
```

## API Surface

Extensions import `@z3rm/*` built-in modules:

```javascript
// @z3rm/mux - Mux server control
import { createSession, getGrid, splitPane } from '@z3rm/mux';

// @z3rm/chrome - Chrome UI (tab, status bar, command palette)
import { addTab, removeTab, getStatusBar, setStatusItem } from '@z3rm/chrome';

// @z3rm/terminal - Terminal grid access
import { write, readGrid, resizeGrid } from '@z3rm/terminal';

// @z3rm/ipc - Cross-extension messaging
import { send, subscribe } from '@z3rm/ipc';

// @z3rm/wasm - WASM module loader (requires manifest permission)
import { loadWasm } from '@z3rm/wasm';

// @z3rm/config - Config access
import { get, watch } from '@z3rm/config';
```

## Example: Status Bar Extension

```javascript
// main.js
import { setStatusItem, getConfig } from '@z3rm/chrome';

export function activate() {
    const config = await getConfig();
    setStatusItem('my-extension', `Mode: ${config.mode}`);
}

export function deactivate() {
    // Clean up status item
}
```

## Example: Tab Modifier

```javascript
// tabs.js
import { addTab, onTabChanged } from '@z3rm/chrome';

export function activate() {
    onTabChanged((tab) => {
        if (tab.title.startsWith('bash')) {
            tab.title = `$ ${tab.title}`;
        }
    });
}
```

## Module Loading

- Extensions are loaded from `$Z3RM_EXTENSIONS_DIR/<name>/`
- Each extension directory contains `extension.toml` + modules
- `main` field in `extension.toml` is entry point
- `export function activate()` called on load
- `export function deactivate()` called on unload
- Module resolution is relative to extension directory
- Cyclic dependencies not allowed

## Hot Reload

- `z3rm_extension_host` watches extension directories for file changes
- On change: `deactivate()` → teardown runtime → `activate()` with fresh runtime
- State is lost across reload unless persisted via `@z3rm/ipc`

## Debugging

- Log via `console.log` → output captured by `z3rm_extension_host`, forwarded to main process
- Set `Z3RM_LOG=z3rm_extension_host=debug` for verbose extension host logging
- Uncaught exceptions logged, do not crash host (sandboxed)

## Distribution

Extensions are directories in `$Z3RM_EXTENSIONS_DIR/`. Planned future distribution via TOML registry (Phase 12+). For now: git clone or copy to extensions dir.

## Restrictions

- No `eval` or `Function` constructor
- No direct FFI
- No `require()` (ES modules only)
- No dynamic imports of arbitrary paths
- No filesystem access without manifest permission
- No network access without manifest permission
- Memory limit: 64 MiB (configurable in manifest)
- CPU limit: 10M ops per event loop tick