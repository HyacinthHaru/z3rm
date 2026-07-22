# 0008 - Layered Licensing

**Status:** Accepted

## Context

z3rm is a hard fork of Zed editor. Zed's license is primarily GPL-3.0-or-later with Apache-2.0 components (notably GPUI, gpui, which is Apache-2.0). z3rm's long-term goal is Apache-2.0 for new code while respecting upstream license obligations. The combined binary must comply with the most restrictive license.

## Decision

Per-crate licensing with three tiers:

- **Retained Zed crates** (untouched, reused as-is): These retain their original license headers.
  - `gpui` crate → Apache-2.0
  - Theme crates → GPL-3.0-or-later (match upstream)
  - UI component crate (`ui`) → GPL-3.0-or-later

- **Modified Zed crates** (pruned from upstream): These retain GPL-3.0-or-later + copyright attribution note in `LICENSE` file.
  - `z3rm_editor` (pruned from Zed editor) → GPL-3.0-or-later
  - `z3rm_workspace` (pruned from Zed workspace) → GPL-3.0-or-later
  - All files retain upstream copyright + add "Modified from Zed editor" note

- **New z3rm crates** (written fresh): Apache-2.0.
  - `z3rm_mux`, `z3rm_mux_proto`, `z3rm_client` → Apache-2.0
  - `z3rm_terminal` → Apache-2.0
  - `z3rm_terminal_view` → Apache-2.0
  - `z3rm_shadow` → Apache-2.0
  - `z3rm_chrome` → Apache-2.0
  - `z3rm_extension_host`, `z3rm_extension_api` → Apache-2.0
  - `z3rm_commands`, `z3rm_config`, `z3rm_ipc` → Apache-2.0

License header in each `Cargo.toml`:
```toml
# For new crates:
license = "Apache-2.0"

# For modified Zed crates:
license = "GPL-3.0-or-later"
```

Combined binary (`z3rm`) is distributed under GPL-3.0-or-later (most restrictive upstream license). Source-level declaration in per-crate `Cargo.toml` metadata. Root `LICENSE` file explains the layering with references to each crate's license.

## Consequences

- **Positive:** New code is Apache-2.0 (permissive, no copyleft restrictions for downstream). GPUI stays Apache-2.0 (ecosystem compatible). License layering is transparent: per-crate `license` field in `Cargo.toml`. Combined binary is GPL-3.0-or-later, meeting upstream obligation.
- **Negative:** Downstream users building only Apache-2.0 crates for non-GPL use must avoid linking GPL parts. "Combined binary is GPL" may discourage some users. Copyright attribution needs careful maintenance across cherry-picks.
- **Mitigation:** `README.md` and `LICENSE` root file clearly state combined binary = GPL-3.0-or-later. Per-crate license icons in crate-level README. Cherry-pick commit messages note original Zed author. Automated header checker ensures license consistency.