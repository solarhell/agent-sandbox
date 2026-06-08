# agent-sandbox

A lightweight Rust gRPC daemon for AI-agent shell execution.

The first implementation follows the lighter Claude Code / Codex-style model:

- Linux-first sandboxing with `bubblewrap` when available.
- Commands are executed through real `bash` with `--noprofile --norc -c`; `ls`, `cat`, `rg`, and friends are not reimplemented.
- External bash tools and bash builtins are explicitly exposed per request with `exposed_binaries`; there is no default tool set.
- Each command runs inside a workspace worktree.
- After the command exits, the daemon records an operation log entry with `op_id`, exit code, and before/after tree hashes.
- Rollback and fork are intentionally not supported in this first cut.

The current runnable backend is local state on disk. Aliyun OSS and AWS S3 are intentionally kept as adapter slots only: their config shapes and factory path exist, but the adapters return a clear "not implemented yet" error until we choose the SDK/API strategy.

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
request_id workspace_id command cwd exposed_binaries timeout_ms duration_ms runner exit_code stdout_bytes stderr_bytes started_at_unix_ms finished_at_unix_ms
```

Policy rejects and runner failures include the same `request_id` in daemon logs and gRPC error messages.

Operation logs can be queried through gRPC, CLI, or SDK:

```bash
cargo run -- list-operations --workspace demo --page-size 20
cargo run -- get-operation --workspace demo op_abc123...
```

`ListOperations` returns the newest operations first. `page_token` is an opaque offset token for the next page; `page_size` defaults to 50 and is capped at 200.

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

## Backend Adapter Shape

The storage boundary is `ObjectBackend` in `src/backend.rs`:

```rust
#[async_trait]
pub trait ObjectBackend: Send + Sync {
    async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>>;
    async fn put(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()>;
    async fn delete(&self, key: &str) -> anyhow::Result<()>;
    fn kind(&self) -> BackendKind;
}
```

Supported now:

```text
LocalObjectBackend
```

Reserved adapter slots:

```text
Aliyun OSS
AWS S3
```
