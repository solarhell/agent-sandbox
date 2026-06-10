# agent-sandbox

A lightweight Rust gRPC daemon for AI-agent shell execution.

The first implementation follows the lighter Claude Code / Codex-style model:

- Linux-first sandboxing with `bubblewrap` when available.
- Commands are executed through real `bash` with `--noprofile --norc -c`; `ls`, `cat`, `rg`, and friends are not reimplemented.
- External bash tools and bash builtins are explicitly exposed per request with `exposed_binaries`; there is no default tool set.
- Each command runs inside a workspace worktree.
- After the command exits, the daemon records an operation log entry with `op_id`, exit code, and before/after tree hashes.
- Rollback and fork are intentionally not supported in this first cut.

All state lives on local disk under the daemon state directory.

## Run

```bash
make test
```

```bash
cargo run -- serve --state-dir .agent-sandbox
```

In another terminal:

```bash
cargo run -- create-workspace --workspace demo
cargo run -- run --workspace demo --expose-binary printf 'printf "hello\n" > a.md'
cargo run -- run --workspace demo --expose-binary cat 'cat a.md'
cargo run -- status --workspace demo
```

The daemon prints an operation ID and before/after tree hashes after each `run`.

## Workspace Binding

By default, workspaces are managed under the daemon state directory. A workspace can also be bound to an existing local worktree. The daemon controls which roots may be bound:

```bash
cargo run -- serve \
  --state-dir .agent-sandbox \
  --allow-bind-root /data1/workspaces/dev
```

Then bind a stable `workspace_id` to a physical path:

```bash
cargo run -- bind-workspace \
  --workspace conv-abc-123 \
  --path /data1/workspaces/dev/conversations/conv-abc-123/head \
  --create-if-missing
```

Command execution and operation logs still use `workspace_id`; operation logs stay in the daemon state directory.

Each successful run is also written to the workspace operation log with audit fields:

```text
request_id workspace_id command cwd exposed_binaries timeout_ms duration_ms runner exit_code stdout_bytes stderr_bytes timed_out started_at_unix_ms finished_at_unix_ms
```

Policy rejects and runner failures include the same `request_id` in daemon logs and gRPC error messages.

Operation logs can be queried through gRPC, CLI, or SDK:

```bash
cargo run -- list-operations --workspace demo --page-size 20
cargo run -- get-operation --workspace demo op_abc123...
```

`ListOperations` returns the newest operations first. `page_token` is an opaque offset token for the next page; `page_size` defaults to 50 and is capped at 200.

Operation logs are pruned per workspace: `serve --max-ops-per-workspace` (default 1000) keeps only the newest entries; `0` keeps everything. `DeleteWorkspace` removes all daemon-side workspace state (operation logs, metadata, and managed worktrees); a bound local worktree is left untouched:

```bash
cargo run -- delete-workspace --workspace demo
```

## Run Semantics

- stdout/stderr are truncated server-side. `RunRequest.max_stdout_bytes` / `max_stderr_bytes` control the caps (0 means the daemon defaults of 1 MiB / 256 KiB; hard ceiling 1.5 MiB each), and `RunResponse.stdout_truncated` / `stderr_truncated` report when a cap was hit. Responses therefore never exceed the default 4 MiB gRPC message limit.
- A command that hits its timeout is killed and still returns a normal response: `timed_out = true`, `exit_code = 124`, with the output produced before the kill. Timeouts are not gRPC errors.
- Commands killed by a signal report `exit_code = 128 + signal`.
- `RunRequest.timeout_ms` is clamped to `serve --max-run-timeout-ms` (default 30 minutes).
- Each run records before/after workspace tree hashes in the operation log. Set `RunRequest.skip_tree_hash = true` to skip hashing when the caller does not consume tree hashes — for large workspaces this removes the dominant per-run cost. For managed workspaces the daemon reuses the previous after-hash as the next before-hash (worktrees only change under the per-workspace run lock); local-bound worktrees can be modified externally and are always re-hashed.

## Proto Workflow

The gRPC API is managed by Buf. Proto files live under `proto/<package>/<version>/`.
The module is named `buf.build/agent-sandbox/agent-sandbox` for publishing to the Buf Schema Registry.

```bash
make proto-check
```

`make proto-check` runs Buf linting and generation, then fails if generated files changed. `scripts/proto-generate.sh` uses Buf Schema Registry remote plugins, so local `protoc-gen-go` binaries are not required. It clears `sdk/go/proto` before generation to avoid stale generated files.

To publish the schema module after logging in to Buf:

```bash
buf push --create --create-visibility private
```

## Sandbox Policy

```bash
cargo run -- run --workspace demo --expose-binary cat --expose-binary ls 'cat a.md'
```

`--expose-binary` controls which external binaries are mounted inside the sandbox command `PATH`. Bash builtins such as `echo`, `cd`, and `test` must also be listed explicitly; known builtins are enabled inside bash without exposing same-name host binaries. Unlisted builtins are disabled before the user command runs. This is not a read-only policy and it does not parse bash syntax.

### Threat Model

The enforced security boundaries are the bubblewrap mount namespace (only the workspace, the exposed binaries, and read-only system libraries are visible), the Landlock execute allowlist (workspace-created files cannot be executed), and the seccomp filter (network syscalls are denied). These hold regardless of what the command does.

The bash builtin disabling is **advisory, not a security boundary**: `/sandbox-runtime/bash` is itself on the Landlock execute allowlist, so a command can start a fresh bash (`/sandbox-runtime/bash -c ...`) where no builtins are disabled. Treat the builtin list as a guardrail that keeps well-behaved agents on the intended toolset, not as containment.

Use read-only policy mode when the agent should inspect an existing workspace without writing files:

```bash
cargo run -- run --workspace demo --expose-binary cat --policy-mode read-only 'cat a.md >/dev/null'
```

In read-only mode, Linux bubblewrap mounts `/workspace` read-only and keeps `/tmp` writable. There is no command-level write-redirection scanner; filesystem protection is enforced by the sandbox.

The runner starts commands with a cleared environment and then sets a small baseline environment. Request env values may add ordinary application variables, but cannot set reserved runner variables: `PATH`, `HOME`, `PWD`, `TMPDIR`, `BASH_ENV`, `ENV`, `SHELLOPTS`, `BASHOPTS`, or `CDPATH`.

## Runner

The runner is Linux-only and always uses `bubblewrap`. Install `bubblewrap`; there is no local runner fallback.

The bubblewrap runner:

- verifies that `bash` exists on the daemon host and runs it as `/sandbox-runtime/bash --noprofile --norc -c "<command>"`;
- bind-mounts the workspace at `/workspace`, either read-write or read-only according to `policy_mode`;
- exposes only the requested external binaries under a clean sandbox `/bin`;
- disables unlisted bash builtins before executing the user command;
- applies a Linux Landlock execute allowlist inside the sandbox so workspace-created executables such as `./tool` cannot run;
- applies a Linux seccomp filter that denies network socket syscalls;
- mounts common system library directories read-only;
- uses an isolated `/tmp`, `/proc`, and `/dev`;
- clears inherited environment variables and sets `HOME`, `PWD`, `TMPDIR`, `PATH`, and `LANG`;
- rejects `cwd` values that escape `/workspace`.

```bash
cargo run -- serve
```

## Storage

All daemon state is plain files under `--state-dir`: workspace metadata (`workspace.json`), operation logs (`ops/*.json`, written atomically), and managed worktrees. There is no object-storage abstraction; if content-addressed snapshots (restore to a recorded tree hash) become a real requirement, that layer should be designed against the operation log's before/after hashes rather than as a generic key-value backend.
