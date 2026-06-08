# agent-sandbox Go SDK

Go client for the `agent-sandbox` gRPC daemon.

The daemon runs commands with real `bash --noprofile --norc -c`, so host shell profiles and rc files are not loaded.
It also starts commands with a cleared environment. SDK callers may set ordinary env values, but reserved runner keys such as `PATH`, `HOME`, `PWD`, `TMPDIR`, `BASH_ENV`, `ENV`, `SHELLOPTS`, `BASHOPTS`, and `CDPATH` are rejected.
On Linux, the bubblewrap runner also applies a Landlock execute allowlist so files created in the workspace cannot be run as `./tool`.
Network socket syscalls are denied inside sandboxed bash commands with a Linux seccomp filter.

```go
package main

import (
	"context"
	"fmt"
	"log"
	"time"

	agentsandbox "github.com/agent-sandbox/agent-sandbox/sdk/go"
)

func main() {
	ctx := context.Background()

	client, err := agentsandbox.New(
		ctx,
		agentsandbox.WithEndpoint("127.0.0.1:50051"),
		agentsandbox.WithWorkspace("conv-demo"),
		agentsandbox.WithTimeout(30*time.Second),
	)
	if err != nil {
		log.Fatal(err)
	}
	defer client.Close()

	if _, err := client.BindConversationWorkspace(ctx, "/data1/workspaces/dev", "conv-demo"); err != nil {
		log.Fatal(err)
	}

	// The research phase can persist web_search/web_reader output under the
	// bound workspace: sources.jsonl, queries.jsonl, raw/SRC-N.md, and so on.
	result, err := client.RunBashReadOnly(
		ctx,
		`rg -n "policy|pricing" sources.jsonl raw && sed -n '1,120p' raw/SRC-1.md`,
		agentsandbox.ExposedBinaries("rg", "sed"),
	)
	if err != nil {
		log.Fatal(err)
	}

	fmt.Print(result.StdoutString())

	ops, err := client.ListOperations(ctx, agentsandbox.PageSize(20))
	if err != nil {
		log.Fatal(err)
	}
	for _, op := range ops.Operations {
		fmt.Println(op.OpID, op.ExitCode)
	}
}
```

Production helpers:

- `BindConversationWorkspace(ctx, root, conversationID)` binds `<root>/conversations/<conversationID>/head` to the same workspace id.
- `RunBashReadOnly(ctx, command, ...)` sets `PolicyModeReadOnly`; callers still choose which tools to expose with `ExposedBinaries(...)`.
- `RunBashReadWrite(ctx, command, ...)` sets `PolicyModeReadWrite`; callers still choose which tools to expose with `ExposedBinaries(...)`.
- `ExposedBinaries(...)` exposes external binaries inside the sandbox command `PATH`; bash builtins such as `echo`, `cd`, and `test` must also be listed explicitly.
- The daemon validates exposed external binaries against the daemon host PATH. Known bash builtins are enabled inside bash without exposing same-name host binaries.

The SDK is configured in code. There is no config file path in this package.

Generated protobuf bindings live under `sdk/go/proto`. From the repository root, run `make proto-check` after changing files under `proto`.
