# Local Model Usage During Migration

## Role of Local Models

During the migration from Zed to z3rm, a local (inexpensive) LLM assists with **bulk output analysis** -- parsing compiler error output, categorizing broken references, and generating `#[z3rm_todo]` attribute stubs. The local model is **never** the primary orchestrator.

## Concurrency

Limited to **1 concurrent subagent** when using local models. This constraint avoids overwhelming the local inference server (e.g., Ollama, LM Studio) and ensures each migration step completes predictably.

## Primary Orchestrator

The **main agent** (cloud-hosted, full-reasoning) is always the orchestrator. It:
- Scopes each migration pass
- Dispatches one subagent at a time
- Reviews generated output before applying changes
- Runs verification (`cargo check`, `cargo test`)

The local model receives only the narrow task of parsing structured compiler output and emitting `#[z3rm_todo]` markers.

## Credentials

- API keys for cloud orchestrator: set via environment variables
- Never commit credentials to the repository
- Local inference servers (Ollama, etc.) bind to localhost and require no credentials

## Environment Variables

| Variable | Purpose |
|---|---|
| `Z3RM_API_KEY` | Cloud orchestrator API key |
| `Z3RM_LOCAL_MODEL_URL` | Local inference server URL (default `http://localhost:11434`) |
| `Z3RM_LOCAL_MODEL_NAME` | Model name for local inference |
| `Z3RM_LOCAL_MODEL_API_KEY` | API key for local inference (optional) |

## Workflow Example

```sh
# 1. Main agent scopes a crate for migration
# 2. Local model scans compiler errors
cargo check --features z3rm-migration -p some_crate 2>&1 | tee /tmp/errors.txt

# 3. Main agent reviews output, dispatches fix subagent
# 4. Fix subagent applies changes, then exits
# 5. Main agent runs verification
cargo check --features z3rm-migration
```

## Constraints

- No intelligence-critical decisions are delegated to the local model
- `#[z3rm_todo]` categories must be validated by the orchestrator before commit
- The `z3rm_macros` crate's proc macro is compiled by Rust, not by any LLM