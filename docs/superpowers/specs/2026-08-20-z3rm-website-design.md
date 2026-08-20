# Z3rm Website Design

## Product and audience

Z3rm (pronounced “zerm”; `3` replaces `e`) is a GPU-rendered terminal and persistent multiplexer built from Zed’s GPUI. The site must make the product understandable before it asks visitors to read architecture documentation.

The primary audiences are:

1. terminal users deciding whether Z3rm fits their workflow;
2. humans learning the GUI and persistent-session model;
3. automation and coding agents controlling sessions through the CLI;
4. contributors checking whether the foundation specification matches the implementation.

The site launches in English and Simplified Chinese. It deploys as a GitHub Pages project site at `https://cyjin-yl.github.io/z3rm/`, with a configurable base path so a future custom domain does not require content or route changes.

## Product statement

English:

> Your shells outlive the window. Z3rm keeps terminal sessions on the server, then gives humans and agents the same way back in.

Chinese:

> 窗口可以关闭，Shell 继续运行。Z3rm 把终端会话留在服务器上，让人和 Agent 通过同一套接口回来继续。

Every feature statement must be backed by a current implementation path and an exercised scenario. The site must not market planned behavior as shipped.

## Competitive lessons

The design combines lessons without copying any competitor’s layout or artwork:

- Ghostty makes the product itself the hero and keeps the landing page sparse.
- Warp uses real interface imagery and first-class agent workflows, but Z3rm avoids its enterprise funnel and generic AI marketing.
- Zellij connects feature demonstrations to tutorials and reference pages.
- kitty and WezTerm provide stable, searchable command and configuration references.
- Kill AI Slop demonstrates restrained editorial rhythm and deliberate whitespace.
- Proto UI supplies explicit interaction semantics, adapters, and accessible primitives.

## Technology

The site is a new Astro application in the repository. Astro owns static routing, content collections, localization, metadata, sitemap generation, and GitHub Pages output.

Interactive controls use Proto UI’s Base protocols through the Web Components adapter. The site does not use the Brutalist visual preset: Z3rm is a Zed/GPUI-derived product, and the website should feel continuous with the application. Proto UI is initialized in consumer-styled mode and receives a Z3rm-specific token layer.

The Proto UI version is pinned exactly. Proto UI is currently a v0 ecosystem, so integration failures are handled by:

1. producing a minimal reproduction;
2. checking the documented protocol and adapter boundary;
3. filing an upstream `Proto-UI/Proto-UI` issue when the library is defective or missing a promised capability;
4. linking the issue from the local implementation note;
5. avoiding copied private component implementations or silent semantic workarounds.

Astro pages remain useful without JavaScript. Language and content navigation are ordinary links. Search, dialogs, tabs, copy buttons, and the optional web demo progressively enhance the static document.

## Visual direction

The visual system follows Zed/GPUI rather than neo-brutalism:

- quiet graphite surfaces;
- subtle one-pixel separators;
- restrained shadows only where GPUI uses elevation;
- compact controls and precise spacing;
- a neutral sans-serif text face and a high-quality monospace face;
- low-saturation semantic accents derived from actual Z3rm theme colors;
- rounded corners matching the application instead of exaggerated pills;
- dense information where it improves scanning, balanced by editorial whitespace on the landing page.

Proto UI components retain their interaction semantics while receiving this consumer-owned style layer. Components are used only for genuine semantics: buttons for actions, tabs for alternate views, select for language/version choice, dialogs for expanded media, scroll areas for bounded output, and status labels only for verified/experimental/planned states.

No indigo gradient, glassmorphism, glowing decoration, invented metrics, decorative badge grids, or generic terminal-green aesthetic is allowed.

## Vector and media strategy

Logos, architecture diagrams, pane diagrams, protocol flows, workflow illustrations, and icons are authored as SVG. SVG artwork uses a small shared token palette and remains legible in light and dark themes.

Actual software surfaces are captured from Z3rm and stored as optimized AVIF/WebP with a PNG source retained only when necessary for lossless text rendering. Each capture records:

- commit SHA;
- platform and window size;
- exact scenario and commands;
- status of the demonstrated capability;
- localized caption and alternative text.

The initial gallery targets:

1. session creation, detach, and attach;
2. pane split, resize, and focus;
3. CLI `send-keys` and `capture-pane` controlling the same session;
4. scrollback and search;
5. file tree and read-only diff review;
6. an agent change followed by human accept/decline review;
7. local and remote sessions;
8. QuickJS chrome and native fallback controls.

A capture enters the site only after the scenario works end to end. Missing or broken behavior becomes an implementation-status entry and a GitHub issue.

## Information architecture

The root route uses the browser language for the first visit and redirects to `/z3rm/en/` or `/z3rm/zh/`. A stored explicit language choice takes precedence. Every page has a visible language switcher and canonical/hreflang metadata.

```text
/z3rm/
├── en/
│   ├── index
│   ├── features
│   ├── quick-start
│   ├── guide/
│   │   ├── cli
│   │   ├── gui
│   │   ├── for-humans
│   │   └── for-agents
│   ├── concepts/
│   │   ├── sessions-and-panes
│   │   ├── server-canonical-model
│   │   ├── local-and-remote
│   │   └── shadow-snapshots
│   ├── reference/
│   │   ├── cli
│   │   ├── keybindings
│   │   ├── configuration
│   │   └── extension-runtime
│   ├── troubleshooting
│   └── implementation-status
└── zh/
    └── the same route structure
```

Top-level navigation contains Features, Guides, Reference, Status, GitHub, language selection, and theme selection. The existing inherited mdBook is not published as the Z3rm user site.

Both locales use one typed content schema. CI fails when a route or required heading exists in one locale but not the other.

## Landing page

The landing page is structured as:

1. interactive workspace hero;
2. why Z3rm exists;
3. one server, many ways back;
4. human and agent workflows;
5. feature gallery;
6. architecture in one diagram;
7. quick start;
8. verified implementation status.

The page begins with the product, not a decorative logo. A static real Z3rm screenshot is rendered immediately and remains the fallback for reduced motion, disabled JavaScript, slow networks, or web-demo failure.

### Interactive workspace hero

The hero offers an embedded, keyboard-accessible Z3rm workspace. It supports pane focus, tab selection, scrolling, selection/copy, a bounded command set, and switching between GUI and CLI control views.

The demo commands mirror real CLI spelling:

```sh
z3rm new -s demo
z3rm split-window -h
z3rm send-keys -t demo "cargo test" Enter
z3rm capture-pane -p
z3rm detach
z3rm attach -t demo
```

The demo is labeled “Browser demo — no local shell is started.” It must not imply arbitrary command execution, daemon persistence, SSH, or filesystem access.

### WebAssembly feasibility and boundary

The repository contains GPUI browser support and WASM examples, but the complete Z3rm binary is not currently browser-ready. It unconditionally includes PTY, local socket, SSH, filesystem, native-window, QuickJS, and daemon dependencies.

Implementation starts with a time-box-independent feasibility spike that proves behavior rather than merely compiling. A dedicated `z3rm_web_demo` crate may include:

- GPUI Web canvas/platform;
- the reusable terminal drawing path;
- structured grid snapshots and diffs;
- in-memory session/layout state;
- a bounded demo-command interpreter;
- deterministic recorded output fixtures.

It must exclude PTYs, daemon startup, sockets, SSH, arbitrary execution, filesystem writes, and extension hosting.

The WASM version ships only if it reuses the real terminal rendering and key/mouse path, works in Chromium and Firefox with the documented WebGPU/WebGL2 behavior, and lazy-loads without delaying primary content. Otherwise the approved fallback is a Proto UI/Astro interactive product walkthrough driven by the same structured fixtures. The fallback must identify itself as a walkthrough rather than the application.

## Guides

### CLI guide

The CLI guide starts with installation and daemon discovery, then teaches one persistent workflow before listing commands:

1. create a named session;
2. list sessions;
3. split and target panes;
4. send keys;
5. capture output;
6. detach and reattach;
7. kill panes, windows, sessions, and the server;
8. target syntax, format strings, and exit/error behavior;
9. local socket and remote connection troubleshooting.

Every documented invocation is validated against the actual parser. Generated command metadata is preferred over duplicated hand-maintained option tables.

### GUI guide

The GUI guide covers startup, session attachment, tabs, panes, resize, focus, scrollback, search, copy/paste, file tree, read-only viewer, diff review, settings, themes, remote sessions, extension chrome, fallback controls, and reconnect behavior. Keybindings are generated from current keymaps where possible.

### Human guide

The human guide is workflow-oriented:

- start work and name the session;
- organize long-running tasks;
- close the window without killing work;
- return locally or remotely;
- inspect an agent’s changed files;
- accept, decline, or undo through shadow snapshots;
- recover from daemon or transport interruption.

It explains the server-canonical mental model without exposing internal crate details.

### Agent guide

The agent guide defines a deterministic contract:

- discover sessions and panes before targeting;
- use explicit targets;
- use `send-keys` for input and `capture-pane` for observation;
- prefer structured output/format options where available;
- bound polling and recognize completion markers;
- avoid stealing human focus unless required;
- use file/diff review and snapshot operations for reversible changes;
- report transport and command failures instead of retrying blindly;
- coordinate with a human attached to the same canonical session.

Examples include shell snippets and machine-readable patterns but do not claim an LLM integration inside Z3rm.

## Reference and search

Reference pages provide stable headings and URLs for every CLI command, default keybinding, server/client setting, and public extension capability. The site generates a static search index covering both languages. Search results preserve locale and identify feature status.

Reference content distinguishes:

- shipped and verified;
- experimental but executable;
- planned or specified only;
- removed/inapplicable inherited Zed behavior.

## Specification audit and issues

The foundation specification is converted into an implementation matrix with stable requirement IDs. Each requirement records:

- specification citation;
- user-visible claim;
- implementation symbols/files;
- direct verification command or scenario;
- screenshot/media evidence when applicable;
- status: verified, experimental, missing, divergent, or not user-visible;
- linked issue for missing/divergent behavior.

The audit proceeds by product slice: mux lifecycle, CLI, GUI, scrollback/search, remote transport, shadow snapshots, extension runtime/chrome, accessibility, and rendering/testing infrastructure.

An issue is filed only after current-state investigation and reproduction. Each issue contains:

- concise user impact;
- exact spec citation;
- observed behavior and reproduction;
- implementation evidence;
- acceptance criteria;
- no speculative implementation plan unless the root cause is established.

The status page is generated from the checked-in matrix. The landing page can only claim statuses marked verified.

## GitHub Pages deployment

A dedicated GitHub Actions workflow builds the Astro site and deploys it through the official Pages artifact/deploy actions. It runs with least-privilege permissions and concurrency cancellation.

The Astro configuration receives:

- `site = https://cyjin-yl.github.io`;
- `base = /z3rm`;
- static output;
- trailing-slash behavior fixed consistently;
- sitemap and canonical URL generation aware of the base path.

Pull requests build the site without deploying. Main-branch deployment runs only after site checks succeed. Future custom-domain migration changes `site`, `base`, and optionally `CNAME`; content routes remain unchanged.

## Verification

Before deployment:

- Astro type/content validation passes;
- production build succeeds with `/z3rm/` base path;
- internal link and asset checks pass on generated output;
- locale parity passes;
- HTML landmarks, accessible names, keyboard navigation, focus order, reduced motion, and contrast are checked;
- desktop and mobile browser smoke tests exercise navigation, language/theme persistence, search, copy actions, dialogs, and demo fallback;
- screenshot regression covers landing, guide, reference, status, light/dark, English/Chinese, and narrow/wide viewports;
- the WASM demo, if shipped, is exercised separately for loading, keyboard/mouse behavior, WebGL2 fallback, and failure fallback;
- the deployed GitHub Pages URL is opened and its canonical links, assets, routes, and 404 behavior are checked.

## Scope exclusions

The first release does not add pricing, accounts, analytics, a blog, enterprise pages, an online shell, arbitrary browser command execution, cloud persistence, or a public extension marketplace. It does not publish inherited Zed documentation as if it described Z3rm.
