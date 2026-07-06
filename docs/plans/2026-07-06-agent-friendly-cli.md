# Agent-Friendly CLI Improvements — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use `claude-superskills:executing-plans` to implement this plan task-by-task.

**Goal:** Make the `sendgrid` CLI legible and self-correcting for AI-agent operators by fixing the friction points three real agent sessions actually hit — starting with the most dangerous: a failed write that reports success.

**Architecture:** Changes concentrate in three seams: (1) the `execute()` response-mapping chokepoint in `sendgrid-core` (`crates/core/src/runtime/mod.rs` + `envelope.rs`), fed by a new curated per-op IR flag generated the same way as `reveal_response_fields` (`data/safety.toml` → `xtask/tables.rs` → `xtask/build.rs` → committed `crates/core/generated/ir.json`); (2) CLI rendering + arg handling (`crates/cli/src/{output,globals,main,tree}.rs`); (3) a `describe`/`explain` capability hoisted from `crates/mcp` for reuse. No change touches the "all 391 ops, no curated subset" operation set — only correctness, ergonomics, and safety defaults.

**Tech Stack:** Rust 2024 (workspace), clap (builder API, no derive), serde_json, tokio, `openapiv3`; codegen via `cargo xtask codegen`; tests via `cargo test -p <crate>` with `CannedDispatcher`/`execute_with` harness in `crates/core/tests/dispatch_harness.rs`.

**Evidence base:** `.research-notes/usage-driven-improvements.md` (mined from the 3 sessions that ran the binary; 4-expert Principal review). Phases are ordered by that ranking.

**Scope:** Phases 1–7 below. **Out of scope (deferred, needs a product decision):** read-modify-write `--merge` PATCH helper (P6) and the composite "recipe" layer (P7) — both break or bend the "no curated subset" principle and should be decided with Andrii before planning.

**Global conventions for every task:**
- TDD: write the failing test first, watch it fail, implement minimally, watch it pass, commit.
- After each task's tests pass, run the gate before committing:
  `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test -p sendgrid-core -p sendgrid-cli`
- Commit messages use Conventional Commits (`fix:`, `feat:`, `refactor:`).
- Never hand-edit `crates/core/generated/ir.json`; regenerate with `cargo xtask codegen` and commit the regenerated file as part of the same task.

---

## Phase 1 (P0): A 2xx-with-error body must fail loudly

**Problem (evidence):** `UpdateTemplateVersion` (PATCH) returned SendGrid's `{"error":"You cannot switch editors…"}` with a **2xx status**, so `map_response` (`crates/core/src/runtime/mod.rs:315`) classified it as success → `exit_code_for_status(200)=0`, and `--query` selected `null` over the error body. The agent believed a failed write succeeded. This is the single most dangerous agent failure mode.

**Approach:** Add an opt-in curated per-op flag `soft_error` (seeded with the known template-version ops). When a flagged op returns 2xx **and** the body matches SendGrid's error shape, route it to `Payload::Error` with a forced non-zero exit. Opt-in curation avoids false positives on legit 2xx-with-`errors` bodies (email-validation, batch partial-success).

### Task 1.1: IR field `soft_error` on `OperationIr`

**Files:**
- Modify: `crates/core/src/ir.rs` (add field near `reveal_response_fields`, ~line 238)

**Step 1: Write the failing test** — add to the existing `#[cfg(test)]` in `ir.rs` (or `crates/core/tests/contract.rs`):

```rust
#[test]
fn operation_ir_has_soft_error_default_false() {
    // Deserialize a minimal op without the field; it must default to false.
    let json = serde_json::json!({
        "id": "x", "operation_id": "X", "namespace": "n", "domain": "d",
        "subgroup": "s", "cli_path": ["d","s","x"], "hidden": false,
        "method": "GET", "path": "/v3/x", "tags": [], "side_effect": "read"
    });
    // Build via the real deserialization path used to load ir.json.
    let op: crate::ir::OperationIr = serde_json::from_value(fill_defaults(json)).unwrap();
    assert!(!op.soft_error);
}
```
(If `OperationIr` deserialization requires all fields, instead assert the field exists and defaults via `#[serde(default)]`.)

**Step 2: Run to verify it fails** — `cargo test -p sendgrid-core soft_error` → FAIL (`no field soft_error`).

**Step 3: Implement** — in `crates/core/src/ir.rs` add:

```rust
    /// When true AND the response is a 2xx whose body is SendGrid's error shape,
    /// the envelope is routed to `error` with a non-zero exit instead of `data`.
    /// Curated in `data/safety.toml` (`soft_error`); false for all but a tiny set
    /// of ops that return 200-with-error (e.g. template-version editor switch).
    #[serde(default)]
    pub soft_error: bool,
```

**Step 4: Run** — `cargo test -p sendgrid-core soft_error` → PASS.

**Step 5: Commit** — `git add -A && git commit -m "feat(ir): add soft_error per-op flag (default false)"`

### Task 1.2: Data table + codegen wiring

**Files:**
- Modify: `data/safety.toml` (add `[[soft_error]]` entries)
- Modify: `xtask/src/tables.rs` (parse the new table; mirror `reveal_response_fields` ~line 56/233)
- Modify: `xtask/src/build.rs` (assemble field ~line 450/616; validate op ids exist ~line 707)
- Regenerate: `crates/core/generated/ir.json`

**Step 1: Write the failing test** — in `crates/core/tests/contract.rs`:

```rust
#[test]
fn template_version_write_ops_are_soft_error() {
    let reg = sendgrid_core::Registry::load();
    for id in ["sg_templates_UpdateTemplateVersion", "sg_templates_CreateTemplateVersion"] {
        let op = reg.get(id).expect("op present");
        assert!(op.soft_error, "{id} should be flagged soft_error");
    }
}
```

**Step 2: Run to verify it fails** — `cargo test -p sendgrid-core template_version_write_ops_are_soft_error` → FAIL (flag false).

**Step 3: Implement**
- `data/safety.toml`, append:
  ```toml
  # Ops that return HTTP 2xx with an error body (SendGrid design quirk). Flagged so
  # execute() surfaces them as failures. Keep this list minimal and evidence-based.
  [[soft_error]]
  op = "UpdateTemplateVersion"
  [[soft_error]]
  op = "CreateTemplateVersion"
  ```
- `xtask/src/tables.rs`: add `pub soft_error: Vec<SoftError>` to the safety table struct and a `SoftError { op: String }` type (mirror `SecretResponse`).
- `xtask/src/build.rs`: read `tables.safety.soft_error` into a set keyed by `operation_id`, set `soft_error` on each matching `OperationIr` (mirror the `reveal_response_fields` block ~616), and add a validation pass (~707) that `bail!`s on an unknown op id.

**Step 4: Regenerate + run** — `cargo xtask codegen && cargo test -p sendgrid-core template_version_write_ops_are_soft_error` → PASS. Confirm `git diff --stat crates/core/generated/ir.json` shows only the two flags flipped.

**Step 5: Commit** — `git add -A && git commit -m "feat(codegen): wire soft_error table; flag template-version writes"`

### Task 1.3: `map_response` routes soft errors to a non-zero exit

**Files:**
- Modify: `crates/core/src/runtime/envelope.rs` (new constructor near `http_error`, ~line 85)
- Modify: `crates/core/src/runtime/mod.rs` (`map_response`, ~line 312-331)
- Test: `crates/core/tests/dispatch_harness.rs` (uses `CannedDispatcher` + `execute_with`)

**Step 1: Write the failing test** — in `dispatch_harness.rs`:

```rust
#[tokio::test]
async fn soft_error_2xx_body_becomes_error_with_nonzero_exit() {
    let reg = Registry::load();
    let op = reg.get("sg_templates_UpdateTemplateVersion").unwrap();
    let dispatcher = CannedDispatcher {
        status: 200,
        headers: http::HeaderMap::new(),
        body: json!({ "error": "You cannot switch editors once a dynamic template version has been created." }),
    };
    let result = execute_with(op, /* args */ minimal_patch_args(), &cfg(), &dispatcher).await;
    assert!(!result.is_success(), "soft-error 2xx must not be success");
    assert_ne!(result.exit_code, 0, "must be non-zero exit");
    assert!(result.error().is_some());
}

#[tokio::test]
async fn non_flagged_2xx_with_errors_key_stays_success() {
    // An email-validation-style op returning {"errors":[...]} but NOT flagged
    // soft_error must remain a success (no false positive).
    let reg = Registry::load();
    let op = reg.get("sg_templates_GetTemplateVersion").unwrap(); // read op, not flagged
    let dispatcher = CannedDispatcher {
        status: 200, headers: http::HeaderMap::new(),
        body: json!({ "errors": [] }),
    };
    let result = execute_with(op, minimal_get_args(), &cfg(), &dispatcher).await;
    assert!(result.is_success());
}
```
(Reuse arg-building helpers already present in the harness; if none, synthesize the required path params like the existing dry-run test does.)

**Step 2: Run to verify it fails** — `cargo test -p sendgrid-core soft_error_2xx` → FAIL (currently success, exit 0).

**Step 3: Implement**
- `envelope.rs`, add a constructor that forces exit class 1 (do NOT reuse `http_error(200,…)`, which maps 200→0):
  ```rust
  /// A 2xx response whose body is a SendGrid error (see `OperationIr::soft_error`).
  /// The body passes through verbatim under `error`; exit is forced non-zero.
  pub fn soft_error(status: u16, side_effect: SideEffect, body: Value) -> Self {
      ExecuteResult {
          status,
          side_effect,
          exit_code: 1,
          code: None,
          request_preview: None,
          next: None,
          warnings: Vec::new(),
          payload: Payload::Error(body),
      }
  }
  ```
- `mod.rs` `map_response`, in the `resp.status.is_success()` branch, before treating as success:
  ```rust
  if resp.status.is_success() {
      if op.soft_error && body_is_sendgrid_error(&body) {
          safety::redact_response(op, &mut body);
          return ExecuteResult::soft_error(status, op.side_effect, body);
      }
      safety::redact_response(op, &mut body);
      ExecuteResult::success(status, op.side_effect, body)
  } ...
  ```
  Add a small private helper:
  ```rust
  /// SendGrid's soft-error envelope shape: a top-level `error` string or a
  /// non-empty `errors` array. Only consulted for ops flagged `soft_error`.
  fn body_is_sendgrid_error(body: &Value) -> bool {
      body.get("error").is_some()
          || body.get("errors").and_then(Value::as_array).is_some_and(|a| !a.is_empty())
  }
  ```

**Step 4: Run** — `cargo test -p sendgrid-core soft_error && cargo test -p sendgrid-core non_flagged_2xx` → PASS.

**Step 5: Gate + commit** — run the gate; `git commit -m "fix(runtime): surface 2xx-with-error bodies as failures (P0)"`

> **Note:** Because errors now land in `Payload::Error`, the P1 `--query` fix is also covered — `output.rs` only applies `--query` to the success `data` path, so a soft-error body is never selected into. Verify this in Task 2.2.

---

## Phase 2 (P1): `--query` fails loud and documents its root

**Problem (evidence):** All 3 sessions abandoned `--query` for Python. `select()` returns `Value::Null` for a non-matching path indistinguishably from a genuinely-null field, and the help never states the selector is rooted at the response `data`.

### Task 2.1: `select` reports an unmatched intermediate key

**Files:**
- Modify: `crates/cli/src/output.rs` (`select`/`select_tokens`, lines 127-175)
- Test: unit tests in `output.rs` (`mod tests` already exists, 7 tests)

**Step 1: Write the failing test** — in `output.rs` `mod tests`:

```rust
#[test]
fn select_reports_missing_intermediate_key() {
    let data = serde_json::json!({ "result": [ {"id": 1} ] });
    // Agent guessed the wrong root: `data` instead of `result`.
    let outcome = select_reporting(&data, "data.id");
    assert!(matches!(outcome, Selection::NoMatch { .. }));
    if let Selection::NoMatch { available } = outcome {
        assert!(available.contains(&"result".to_string()));
    }
}

#[test]
fn select_terminal_null_is_not_reported_as_missing() {
    let data = serde_json::json!({ "active": null });
    assert!(matches!(select_reporting(&data, "active"), Selection::Value(_)));
}
```

**Step 2: Run to verify it fails** — `cargo test -p sendgrid-cli select_reports` → FAIL (no `select_reporting`/`Selection`).

**Step 3: Implement** — add a reporting wrapper that distinguishes "key absent" from "present-and-null" without changing existing `select()` callers' value semantics:

```rust
pub enum Selection { Value(Value), NoMatch { available: Vec<String> } }

/// Like `select`, but reports when an OBJECT key in the path was absent (as opposed
/// to a present-but-null value), returning the sibling keys available at that level.
pub fn select_reporting(value: &Value, expr: &str) -> Selection { /* walk tokens;
    on the first object-key miss, return NoMatch with map.keys() sorted */ }
```

**Step 4: Run** — `cargo test -p sendgrid-cli select_reports select_terminal_null` → PASS.

**Step 5: Commit** — `git commit -m "feat(output): select_reporting distinguishes missing key from null"`

### Task 2.2: `render` emits the stderr hint and help names the root

**Files:**
- Modify: `crates/cli/src/output.rs` `render` (lines 55-58 success path; also the dry-run path lines 46-49)
- Modify: `crates/cli/src/globals.rs` (`--query` help text, ~line 89)

**Step 1: Write the failing test** — assert `render` returns the value AND that a `NoMatch` produces a warning line. Since `render` writes to real stderr, factor the decision into a testable helper:

```rust
#[test]
fn query_miss_produces_hint_message() {
    let data = serde_json::json!({ "result": [] });
    let msg = query_hint(&data, "data.id"); // Option<String>
    assert_eq!(msg, Some("--query: no match for `data`; available keys: result".into()));
}
```

**Step 2: Run** → FAIL (no `query_hint`).

**Step 3: Implement** — in `render`, replace `data = select(&data, q)` with the reporting variant; on `NoMatch`, `eprintln!("warning: --query: no match for `{first_missing}`; available keys: {csv}")` and fall back to `Value::Null`. Update the `--query` clap help in `globals.rs` to: `"jq-lite selector, rooted at the response `data` (e.g. `result[].id`)"`.

**Step 4: Run** — `cargo test -p sendgrid-cli query_hint` → PASS. Manual check: `cargo run -p sendgrid-cli -- --query data.id templates list-template --page_size 1 --dry-run` prints the hint.

**Step 5: Commit** — `git commit -m "feat(output): loud --query miss + document data root (P1)"`

---

## Phase 3 (P2): Recover from trailing global flags

**Problem (evidence):** `--query`/`--limit`/`--offset`/`--region`/`--on-behalf-of` still error `unexpected argument` when placed after the subcommand (they collide with real leaf params, so `global(true)` would panic at build — 38 ops declare `limit`, 27 `offset`, etc.). `--output`/`--dry-run`/`--all` are already globalized. `--allow` has zero leaf collisions.

### Task 3.1: Globalize `--allow` (the collision-free one)

**Files:**
- Modify: `crates/cli/src/globals.rs` (arg definition for `allow`; extend the `allow_explicit` detection ~line 191 to the after-subcommand match source)

**Step 1: Write the failing test** — a CLI integration test (new file `crates/cli/tests/flag_position.rs`, using `assert_cmd` or `Command::cargo_bin`):

```rust
#[test]
fn allow_after_subcommand_is_accepted() {
    let out = sendgrid(&["templates", "list-template", "--page_size", "1", "--allow", "read", "--dry-run"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
}
```
(Add `assert_cmd` as a dev-dependency to `crates/cli/Cargo.toml` if not present.)

**Step 2: Run** → FAIL (`unexpected argument '--allow'`).

**Step 3: Implement** — mark the `allow` arg `.global(true)`; ensure `allow_explicit` still reads `ValueSource::CommandLine` when set after the subcommand (already true per verification — keep the test as the guard).

**Step 4: Run** → PASS.

**Step 5: Commit** — `git commit -m "feat(cli): accept --allow in any position"`

### Task 3.2: argv pre-scan hoists collision-prone globals

**Files:**
- Modify: `crates/cli/src/main.rs` (`run`, after the `--include-legacy` scan ~line 33, before `get_matches`)
- Modify: `crates/cli/src/tree.rs` (expose, from the resolve map / registry, whether a given leaf declares a param name — needed for the "only hoist if the leaf lacks it" guard)

**Step 1: Write the failing test** — in `crates/cli/tests/flag_position.rs`:

```rust
#[test]
fn query_after_subcommand_is_hoisted_and_works() {
    let out = sendgrid(&["templates", "list-template", "--page_size", "1", "--query", "result", "--dry-run"]);
    assert!(out.status.success());
}

#[test]
fn leaf_param_named_like_a_global_is_not_hoisted() {
    // An op that has its OWN --limit/--query leaf param must keep it as a leaf value.
    // Pick such an op from the registry; assert the leaf value reaches the request.
}
```

**Step 2: Run** → FAIL (`unexpected argument '--query'`).

**Step 3: Implement** — write `hoist_globals(argv: Vec<String>, leaf_declares: impl Fn(&str,&str)->bool) -> Vec<String>`:
1. Identify the subcommand path (first non-flag tokens).
2. For each known root-global long (`--query`,`--limit`,`--offset`,`--region`,`--on-behalf-of`) appearing *after* the subcommand, move the flag (and its value) to before the subcommand **only if** the resolved leaf op does not itself declare that param name.
3. Parse the hoisted argv with `try_get_matches_from`; on residual `UnknownArgument`, fall through to clap's normal error (no worse than today).
Keep it pure and unit-testable; call it in `run` before building matches.

**Step 4: Run** → PASS both tests; run full `cargo test -p sendgrid-cli`.

**Step 5: Gate + commit** — `git commit -m "feat(cli): recover trailing global flags via argv pre-scan (P2)"`

---

## Phase 4 (P3): Frictionless command discovery

**Problem (evidence):** ~6 `--help` calls/session; `template` vs `templates` → `unrecognized subcommand`.

### Task 4.1: Enable subcommand inference + a no-ambiguity regen guard

**Files:**
- Modify: `crates/cli/src/tree.rs` (`.infer_subcommands(true)` on the command builder)
- Test: `crates/cli/tests/flag_position.rs` or new `discovery.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn singular_noun_is_inferred() {
    let out = sendgrid(&["template", "list-template", "--page_size", "1", "--dry-run"]);
    assert!(out.status.success(), "`template` should infer to `templates`");
}

#[test]
fn no_ambiguous_top_level_prefixes() {
    // Regen guard: assert inference cannot silently mis-route. Build the tree and
    // assert no two top-level subcommands share a prefix that is itself a full name.
}
```

**Step 2: Run** → FAIL (`unrecognized subcommand 'template'`).

**Step 3: Implement** — add `.infer_subcommands(true)` where the root `Command` is assembled in `tree.rs`. Implement the ambiguity guard over the generated command names.

**Step 4: Run** → PASS.

**Step 5: Commit** — `git commit -m "feat(cli): infer subcommands (singular/plural tolerance) (P3)"`

### Task 4.2: Copy-paste-ready examples in group help

**Files:**
- Modify: `crates/cli/src/tree.rs` (attach `.after_help(...)` per domain group with 1-2 runnable examples drawn from the registry)

**Step 1: Write the failing test** — assert `templates --help` output contains a runnable example string:

```rust
#[test]
fn group_help_shows_runnable_example() {
    let out = sendgrid(&["templates", "--help"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("sendgrid templates get-template --template_id"));
}
```

**Step 2: Run** → FAIL.

**Step 3: Implement** — for each domain group, synthesize one example from the first read op's `cli_path` + a representative required param (reuse the example synthesis that already exists in `crates/mcp/src/describe.rs` if feasible; otherwise a minimal formatter). Attach via `.after_help`.

**Step 4: Run** → PASS.

**Step 5: Commit** — `git commit -m "feat(cli): runnable examples in group --help"`

---

## Phase 5 (P4): Bring `describe`/`--explain` to the CLI

**Problem (evidence):** MCP `describe_operation` returns a synthesized example, an `invoke_hint`, and a response field-menu; CLI leaf `--help` shows raw flags only, so the agent guesses bodies and query roots (feeds P1).

### Task 5.1: Hoist `describe` into `sendgrid-core`

**Files:**
- Create: `crates/core/src/describe.rs` (move the op-description logic out of `crates/mcp/src/describe.rs`)
- Modify: `crates/mcp/src/describe.rs` (call the core function; keep the MCP-tool JSON shape)
- Modify: `crates/core/src/lib.rs` (export `describe`)

**Step 1: Write the failing test** — in `crates/core/tests/`:

```rust
#[test]
fn describe_returns_example_and_response_menu() {
    let reg = sendgrid_core::Registry::load();
    let op = reg.get("sg_templates_GetTemplateVersion").unwrap();
    let d = sendgrid_core::describe::describe(op);
    assert!(!d.example.is_empty());
    assert!(!d.response_fields.is_empty()); // the field-menu that fixes query-root guessing
}
```

**Step 2: Run** → FAIL (no `sendgrid_core::describe`).

**Step 3: Implement** — move the pure logic to `core::describe` returning a struct (`{ params, example, invoke_hint, response_fields }`); have MCP serialize it. No behavior change to the MCP tool (guard with the existing MCP tests).

**Step 4: Run** — `cargo test -p sendgrid-core describe && cargo test -p sendgrid-mcp` → PASS.

**Step 5: Commit** — `git commit -m "refactor: hoist describe into core for CLI reuse"`

### Task 5.2: `sendgrid <domain> <verb-noun> --explain`

**Files:**
- Modify: `crates/cli/src/main.rs` (intercept `--explain` before dispatch) and `crates/cli/src/tree.rs` (register a global `--explain` flag)
- Modify: `crates/cli/src/globals.rs` (parse `explain`)

**Step 1: Write the failing test**

```rust
#[test]
fn explain_prints_example_and_response_fields() {
    let out = sendgrid(&["templates", "get-template", "--explain"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Example:") && s.contains("Response fields:"));
    assert!(out.status.success());
}
```

**Step 2: Run** → FAIL.

**Step 3: Implement** — a global `--explain` flag that, when present on an operation leaf, prints `core::describe(op)` (human-formatted) and exits 0 without dispatching.

**Step 4: Run** → PASS.

**Step 5: Commit** — `git commit -m "feat(cli): --explain shows example + response field-menu (P4)"`

---

## Phase 6 (P5): Fail closed on destructive/send by default

**Problem (evidence):** CLI default with no `--allow` = `Policy::all()` (`globals.rs:203`) — an agent can `SendMail` or `EraseRecipientEmailData` with zero opt-in. Only MCP fails closed.

> **Decision gate:** this is workflow-breaking. Confirm the target default with Andrii before implementing — recommended: default `read,write`, require explicit `--allow destructive,send`. The task below assumes that choice.

### Task 6.1: Default policy = read+write; destructive/send require opt-in

**Files:**
- Modify: `crates/cli/src/globals.rs` (`policy`, lines 202-205)
- Test: `crates/cli/tests/` + existing `crates/core/tests/security.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn destructive_op_blocked_without_explicit_allow() {
    let out = sendgrid(&["mail", "batch", "cancel-scheduled-send", "--batch_id", "x"]); // a Send/Destructive op
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("E_SIDE_EFFECT") ||
            String::from_utf8_lossy(&out.stderr).contains("--allow"));
}

#[test]
fn write_op_still_allowed_by_default() {
    // A plain write op (e.g. templates create) should NOT require --allow.
    let out = sendgrid(&["templates", "create", "template", "--name", "x", "--dry-run"]);
    assert!(out.status.success());
}
```

**Step 2: Run** → FAIL (`destructive` currently allowed).

**Step 3: Implement** — change the no-`--allow` default in `policy()`:
```rust
let Some(raw) = self.allow.as_deref() else {
    // Fail closed on irreversible/outbound classes; agents drive this surface.
    return Ok((Policy::from_classes(vec![SideEffect::Read, SideEffect::Write]), self.allow_bulk));
};
```
Update the doc comment and `docs/safety.md`. Note any existing test asserting `direct_cli_op_stays_allow_all_without_allow` must be updated to the new posture.

**Step 4: Run** → PASS; run `cargo test -p sendgrid-core -p sendgrid-cli` (fix the old allow-all test).

**Step 5: Gate + commit** — `git commit -m "fix(safety): CLI fails closed on destructive/send by default (P5)"`

---

## Phase 7 (P8): Lower-effort agent-safety wins

### Task 7.1: `--api-key-stdin` (non-leaking credential input)

**Files:**
- Modify: `crates/cli/src/globals.rs` (add flag; `api_key()` reads stdin when set — note `crates/core/src/runtime/auth.rs:60` already references such a path)

**Step 1: Write the failing test** — pipe a key on stdin, assert `auth doctor` reports it present without the key appearing in argv:

```rust
#[test]
fn api_key_stdin_is_accepted() {
    let out = sendgrid_stdin(&["--api-key-stdin", "auth", "doctor", "--output", "json"],
                             "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123\n");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("fingerprint"));
}
```

**Step 2: Run** → FAIL.

**Step 3: Implement** — add `--api-key-stdin` (conflicts with `--api-key`); when set, read one line from stdin and use it as the key. Emit a notice if `--api-key` is on argv (already partially done at `globals.rs:225`).

**Step 4: Run** → PASS.

**Step 5: Commit** — `git commit -m "feat(cli): --api-key-stdin to avoid argv/env key leaks (P8)"`

### Task 7.2: `--all` injects a default page size when the op requires one

**Files:**
- Modify: `crates/core/src/runtime/apply_defaults.rs` (inject when `--all` set and a required page/limit param is omitted; respect schema `maximum`)
- Modify/inspect: `crates/core/src/runtime/paginate.rs:58`
- Test: `crates/core/tests/runtime_exec.rs`

**Step 1: Write the failing test**

```rust
#[tokio::test]
async fn all_injects_page_size_for_required_param() {
    // list-template requires page_size; with --all and no page_size it should
    // build+validate successfully (dry-run), not fail validation.
    let reg = Registry::load();
    let op = reg.get("sg_mc_templates_ListTemplate").unwrap(); // confirm exact id
    let mut cfg = cfg(); cfg.paginate_all = true; cfg.dry_run = true;
    let result = execute_with(op, no_page_size_args(), &cfg, &NeverDispatcher).await;
    assert!(result.is_success(), "expected build to pass, got {:?}", result.error());
}
```

**Step 2: Run** → FAIL (validation: required `page_size`).

**Step 3: Implement** — in `apply_defaults`, when `cfg.paginate_all` and the op declares a required page-size/limit param the caller omitted, inject a per-page value bounded by the schema `maximum` (fall back to `max_items`).

**Step 4: Run** → PASS.

**Step 5: Commit** — `git commit -m "feat(runtime): --all injects default page size (P8)"`

### Task 7.3: Impersonation + destructive/send audit line

**Files:**
- Modify: `crates/core/src/runtime/mod.rs` (in `execute`, after policy resolution, before dispatch)
- Test: `crates/core/tests/security.rs`

**Step 1: Write the failing test** — assert a scrubbed audit line is produced (route it through a `Vec<String>` sink or capture stderr) when `on_behalf_of` is set or the op is destructive/send:

```rust
#[test]
fn impersonated_call_emits_scrubbed_audit_line() {
    let line = audit_line(op_send, Some("subuser-a"), /*key*/ CONFIG_KEY);
    assert!(line.contains("op=") && line.contains("obo=subuser-a") && line.contains("side_effect="));
    assert!(!line.contains(CONFIG_KEY)); // scrubbed
}
```

**Step 2: Run** → FAIL (no `audit_line`).

**Step 3: Implement** — add a pure `audit_line(op, obo, key) -> String` scrubbed via `auth::scrub`; emit to stderr in `execute` when `on_behalf_of.is_some()` or `side_effect` ∈ {Destructive, Send}. (Structured, one line, no external deps.)

**Step 4: Run** → PASS.

**Step 5: Gate + commit** — `git commit -m "feat(runtime): audit log for impersonated + destructive/send ops (P8)"`

---

## Validation (whole plan)

After all phases:
1. `cargo fmt --all -- --check`
2. `cargo clippy --all-targets --all-features -- -D warnings`
3. `cargo test --workspace`
4. `cargo xtask codegen` then `git diff --exit-code crates/core/generated/ir.json` (generated file must already be committed and stable)
5. Manual agent-legibility smoke (no live key needed — use `--dry-run`):
   - `sendgrid templates get-template --explain` → shows example + response fields
   - `sendgrid templates list-template --page_size 1 --query data.id --dry-run` → prints the `--query` miss hint
   - `sendgrid templates list-template --page_size 1 --output json --dry-run` (trailing globals accepted)
   - `sendgrid template list-template --page_size 1 --dry-run` (singular inferred)
6. Update `.research-notes/usage-driven-improvements.md` to mark P0–P5,P8 as landed; update `docs/limitations.md` if any listed limitation is now resolved.

## Deferred (needs product decision, not planned here)
- **P6 — read-modify-write `--merge` PATCH helper + immutable-field stripping.** Requires either capturing `readOnly` in the xtask emitter (absent from `schemas.json` today) or a curated `immutable.toml`; deep-merge semantics need array-replace rules.
- **P7 — composite "recipe" layer** (`template-edit-version`, `suppression-status`). Sits beside `await_job` in core; genuine tension with "all APIs, no curated subset" — decide scope with Andrii first.
