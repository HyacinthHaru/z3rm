# Z3rm Website Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build and deploy a bilingual Astro product/documentation site for Z3rm with Proto UI interactions, verified product media, an honest interactive demo, and a spec-to-implementation status audit.

**Architecture:** A self-contained `website/` Astro static application serves localized content under `/z3rm/en/` and `/z3rm/zh/`. Astro content collections hold typed guide/reference/status records; Proto UI Base Web Components provide interaction semantics under a custom GPUI-inspired style layer. A separate feasibility slice decides whether the hero uses a real GPUI WASM renderer or a clearly labeled structured walkthrough.

**Tech Stack:** Astro 6, TypeScript, Astro content collections, Proto UI 0.2 Web Components/Base protocols, Playwright, SVG, Rust/GPUI Web for the optional WASM demo, GitHub Pages Actions.

---

## File structure

- `website/package.json`, `website/pnpm-lock.yaml`: isolated Node workspace with exact versions.
- `website/astro.config.mjs`, `website/tsconfig.json`: static output, `/z3rm` base path, locales, sitemap.
- `website/src/content.config.ts`: typed bilingual content and implementation-status schemas.
- `website/src/content/{en,zh}/`: task-oriented user documentation with route parity.
- `website/src/layouts/`: landing and documentation shells.
- `website/src/components/`: navigation, search, media, status, CLI examples, demo loader.
- `website/src/styles/`: GPUI-inspired tokens, typography, layout, media, code.
- `website/src/pages/`: localized static route generation and root language redirect.
- `website/public/`: SVG illustrations, verified product media, fonts, demo artifacts.
- `website/scripts/`: locale parity, internal-link, claim/status, and media-manifest validation.
- `website/tests/`: Playwright functional, accessibility, and screenshot coverage.
- `crates/z3rm_web_demo/`: only if the WASM feasibility contract passes.
- `docs/implementation-status/z3rm-foundation.json`: checked-in requirement matrix.
- `.github/workflows/deploy_z3rm_site.yml`: Pages build and deployment.

### Task 1: Scaffold the isolated Astro site

**Files:**
- Create: `website/package.json`
- Create: `website/astro.config.mjs`
- Create: `website/tsconfig.json`
- Create: `website/src/pages/index.astro`
- Create: `website/src/pages/en/index.astro`
- Create: `website/src/pages/zh/index.astro`

- [ ] Create `website/package.json` with exact Astro, sitemap, TypeScript, Playwright, axe, and Proto UI 0.2 dependencies plus `dev`, `check`, `build`, `test`, and `test:visual` scripts.
- [ ] Run `corepack pnpm install --dir website` and commit the generated lockfile.
- [ ] Configure `site: "https://cyjin-yl.github.io"`, `base: "/z3rm"`, static output, trailing slashes, and prefixed `en`/`zh` locales.
- [ ] Add a root static language redirect that honors a stored explicit locale before `navigator.language`, while preserving ordinary links for no-JavaScript navigation.
- [ ] Run `corepack pnpm --dir website check && corepack pnpm --dir website build`; expect successful output under `website/dist/z3rm/` or Astro’s base-aware equivalent.
- [ ] Commit with `Scaffold Z3rm Astro website`.

### Task 2: Integrate Proto UI Base semantics and GPUI styling

**Files:**
- Create: `website/proto-ui.config.json`
- Create: `website/src/styles/tokens.css`
- Create: `website/src/styles/global.css`
- Create: `website/src/components/ThemeToggle.astro`
- Create: `website/src/components/LanguageSelect.astro`
- Test: `website/tests/controls.spec.ts`

- [ ] Initialize Proto UI with the Web Components host and consumer-owned styles (`--no-styles`), pinned to the exact 0.2 release.
- [ ] Generate local facades only through the Proto UI CLI for Button, Tabs, Select, Dialog, Toggle, and Scroll Area.
- [ ] Define light/dark GPUI-derived semantic tokens: graphite surfaces, one-pixel borders, restrained elevation, compact radii, and actual terminal accent colors.
- [ ] Implement language and theme controls using the generated Proto UI facades; persist explicit choices and keep server-rendered links usable.
- [ ] Add Playwright tests asserting keyboard activation, focus visibility, persistence, and accessible names.
- [ ] Run the controls test and production build; expect all checks to pass.
- [ ] If the CLI or Web Components adapter violates its documented contract, isolate the failure in a minimal fixture and file a Proto UI issue before changing local code.
- [ ] Commit with `Add GPUI styled Proto UI controls`.

### Task 3: Build typed bilingual content routing

**Files:**
- Create: `website/src/content.config.ts`
- Create: `website/src/lib/content.ts`
- Create: `website/src/pages/[lang]/[...slug].astro`
- Create: `website/scripts/check-locale-parity.mjs`
- Test: `website/tests/content.spec.ts`

- [ ] Define schemas for title, description, section, order, status, spec requirement IDs, and translation key.
- [ ] Implement static paths from the collections; reject languages outside `en` and `zh`.
- [ ] Implement locale-paired canonical and `hreflang` metadata.
- [ ] Write the parity checker so missing translation keys, mismatched route slugs, or missing required headings fail with exact paths.
- [ ] Add tests for every localized route, canonical URL, alternate locale, and 404 behavior.
- [ ] Run parity, Astro check, and route tests; expect no mismatches.
- [ ] Commit with `Add typed bilingual documentation routes`.

### Task 4: Build the product and documentation shells

**Files:**
- Create: `website/src/layouts/SiteLayout.astro`
- Create: `website/src/layouts/DocsLayout.astro`
- Create: `website/src/components/SiteHeader.astro`
- Create: `website/src/components/DocsSidebar.astro`
- Create: `website/src/components/PageToc.astro`
- Create: `website/src/components/SearchDialog.astro`
- Create: `website/src/components/SiteFooter.astro`
- Test: `website/tests/navigation.spec.ts`

- [ ] Implement the compact product header with Features, Guides, Reference, Status, GitHub, language, and theme controls.
- [ ] Implement a responsive documentation shell with sidebar, breadcrumb, in-page headings, previous/next links, and mobile dialog navigation.
- [ ] Generate a locale-specific static search index at build time and load it only when search opens.
- [ ] Test landmark structure, skip link, keyboard traversal, search result locale preservation, narrow viewport navigation, and no-JavaScript link targets.
- [ ] Run navigation tests at desktop and mobile widths; expect all checks to pass.
- [ ] Commit with `Build Z3rm documentation shell`.

### Task 5: Author the four user guides and references

**Files:**
- Create localized content under `website/src/content/{en,zh}/` for quick start, features, four guides, four concepts, four references, troubleshooting, and implementation status.
- Modify: `crates/z3rm/src/cli.rs` only if documentation-driven parser verification exposes a confirmed defect.
- Test: `website/scripts/check-cli-examples.mjs`

- [ ] Extract the current CLI command/option surface from the real parser and compare every guide command with parser acceptance.
- [ ] Author CLI, GUI, human, and agent guides with executable examples and explicit failure behavior.
- [ ] Author concepts for sessions/panes, server authority, local/remote paths, and shadow snapshots.
- [ ] Generate keybinding and configuration references from checked-in sources where feasible; record generated-source metadata.
- [ ] Write mirrored English and Chinese pages with matching translation keys and headings.
- [ ] Run CLI example validation, locale parity, and the production build.
- [ ] Commit with `Write Z3rm user guides and reference`.

### Task 6: Audit the foundation specification

**Files:**
- Create: `docs/implementation-status/z3rm-foundation.json`
- Create: `website/scripts/check-status-matrix.mjs`
- Create localized: `implementation-status` pages

- [ ] Assign stable requirement IDs to user-visible sections of `2026-07-14-z3rm-foundation-design.md`.
- [ ] For each requirement, record the citation, claim, implementation evidence, verification command/scenario, and one of `verified`, `experimental`, `missing`, `divergent`, or `not-user-visible`.
- [ ] Reproduce every missing/divergent behavior before filing an issue.
- [ ] File one focused GitHub issue per independent confirmed gap with spec citation, observed behavior, reproduction, implementation evidence, and acceptance criteria.
- [ ] Link issue numbers in the matrix and render the localized status page from the matrix.
- [ ] Add a validator that forbids landing-page claims without a `verified` matrix entry.
- [ ] Run matrix and claim validation; expect no unbacked claims.
- [ ] Commit with `Publish Z3rm implementation status`.

### Task 7: Capture verified product media and author SVGs

**Files:**
- Create: `website/public/media/manifest.json`
- Create: `website/public/media/product/*`
- Create: `website/public/media/diagrams/*.svg`
- Create: `website/scripts/check-media-manifest.mjs`

- [ ] Build and launch the current Z3rm client/server using the external Cargo target directory.
- [ ] Exercise each target gallery scenario end to end before capture.
- [ ] Record commit SHA, platform, viewport, commands, localized alt text, and matrix requirement IDs in the manifest.
- [ ] Capture lossless sources and generate optimized web variants without embedding explanatory text in raster images.
- [ ] Author original SVG architecture, session lifecycle, human/agent, and pane-layout diagrams using the site tokens.
- [ ] Validate that every media reference exists, every image has dimensions/alt text, and every capability maps to `verified` status.
- [ ] Commit with `Add verified Z3rm product media`.

### Task 8: Build the landing page

**Files:**
- Create: `website/src/components/landing/Hero.astro`
- Create: `website/src/components/landing/Workflow.astro`
- Create: `website/src/components/landing/FeatureGallery.astro`
- Create: `website/src/components/landing/Architecture.astro`
- Create: `website/src/components/landing/QuickStart.astro`
- Modify: localized index pages
- Test: `website/tests/landing.spec.ts`

- [ ] Implement the immediate static product screenshot and product statement.
- [ ] Add server-authority, return paths, human/agent workflows, verified feature gallery, architecture SVG, quick start, and status summary.
- [ ] Use Proto UI only for genuine controls; avoid decorative cards, badges, gradients, glow, and invented metrics.
- [ ] Test heading hierarchy, CTA targets, reduced motion, image fallback, light/dark themes, both locales, and narrow/wide layouts.
- [ ] Run landing functional and screenshot tests; inspect screenshots for clipping, overflow, contrast, and product-media legibility.
- [ ] Commit with `Build Z3rm product landing page`.

### Task 9: Run the GPUI WASM feasibility slice

**Files:**
- Create only on success: `crates/z3rm_web_demo/Cargo.toml`
- Create only on success: `crates/z3rm_web_demo/z3rm_web_demo.rs`
- Create only on success: `website/src/components/demo/WebDemo.astro`
- Create: `docs/implementation-status/web-demo-evidence.json`

- [ ] Create a throwaway, uncommitted GPUI Web probe that renders the real terminal drawing path from a structured fixture.
- [ ] Verify wasm32 compilation, browser WebGPU rendering, WebGL2 fallback, keyboard input, pointer focus, scroll, selection, and deterministic pane layout.
- [ ] Measure lazy-loaded compressed artifacts and record exact results in the evidence JSON.
- [ ] If all behavioral gates pass, implement the dedicated no-PTY/no-daemon crate and bounded command interpreter, then test the six documented demo commands.
- [ ] If any reuse gate fails, delete the probe and implement the approved Proto UI/Astro structured walkthrough labeled as such; do not ship a fake shell.
- [ ] Add failure fallback and reduced-motion tests.
- [ ] Commit the successful branch with `Add interactive Z3rm browser demo` or the fallback with `Add interactive Z3rm product walkthrough`.

### Task 10: Add complete site verification

**Files:**
- Create: `website/playwright.config.ts`
- Create: `website/tests/accessibility.spec.ts`
- Create: `website/tests/visual.spec.ts`
- Create: `website/scripts/check-links.mjs`
- Modify: `website/package.json`

- [ ] Configure Playwright to serve the production build under `/z3rm/`.
- [ ] Add axe checks plus explicit landmark, focus order, reduced motion, accessible-name, and contrast assertions.
- [ ] Add deterministic screenshots for landing, guide, reference, status, English/Chinese, light/dark, desktop/mobile.
- [ ] Crawl generated HTML and validate internal routes, fragments, assets, canonical links, and locale alternates.
- [ ] Run `pnpm check`, content validators, production build, link checker, functional tests, accessibility tests, and visual tests.
- [ ] Commit with `Add Z3rm website regression coverage`.

### Task 11: Deploy through GitHub Pages

**Files:**
- Create: `.github/workflows/deploy_z3rm_site.yml`
- Modify: `.github/workflows/run_tests.yml` only to add a non-deploying website check if repository conventions require it.

- [ ] Add PR/main build checks using the lockfile and exact Node/pnpm versions.
- [ ] Add main-only Pages deployment with `contents: read`, `pages: write`, `id-token: write`, environment URL output, and deployment concurrency cancellation.
- [ ] Use the official Astro/Pages artifact path and ensure pull requests cannot deploy.
- [ ] Validate the workflow syntax and run the exact production build locally.
- [ ] Commit with `Deploy Z3rm website to GitHub Pages`.
- [ ] Push through a normal pull request, wait for checks, review the rendered artifact, and merge only after verification.
- [ ] Enable Pages with GitHub Actions as its source if repository settings are not already configured.
- [ ] Open `https://cyjin-yl.github.io/z3rm/` and verify redirect, both locales, deep links, assets, search, demo/fallback, canonical URLs, and 404 behavior.

### Task 12: Final evidence and cleanup

**Files:**
- Modify only evidence/status records whose verification changed.

- [ ] Confirm every spec design section has a corresponding completed task and observable artifact.
- [ ] Confirm every filed Z3rm and Proto UI issue contains current evidence and no duplicate exists.
- [ ] Remove probe files, stale captures, unused generated facades, inherited Zed content links, and build output.
- [ ] Re-run the full site verification command and the specific Rust checks affected by any web-demo or bug-fix changes.
- [ ] Confirm a clean worktree, merged PR, deployed Pages SHA, and live URL.
