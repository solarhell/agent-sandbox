package agentsandbox

import (
	"context"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"testing"
	"time"
)

var (
	buildDaemonOnce sync.Once
	buildDaemonErr  error
)

func TestIntegrationReActCorpusWorkflow(t *testing.T) {
	requireLinuxSandbox(t)
	repoRoot := findRepoRoot(t)
	binary := ensureDaemonBinary(t, repoRoot)
	readPolicy := PolicyModeReadOnly

	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	endpoint, stateDir, bindRoot, stop := startDaemonForTest(t, ctx, repoRoot, binary)
	defer stop()

	client := waitForClient(t, ctx, endpoint, "conv-react")
	defer client.Close()

	worktreePath := ConversationWorkspacePath(bindRoot, "conv-react")
	workspace, err := client.BindConversationWorkspace(ctx, bindRoot, "conv-react")
	if err != nil {
		t.Fatalf("BindConversationWorkspace: %v", err)
	}
	if workspace.Kind != WorkspaceKindLocalBound {
		t.Fatalf("workspace kind = %q, want %q", workspace.Kind, WorkspaceKindLocalBound)
	}

	seedReActCorpus(t, worktreePath)

	// Mirrors the production writing phase: the agent no longer calls web tools;
	// it uses real bash binaries to inspect the persisted corpus.
	retrieveCommand := strings.Join([]string{
		`rg -n "web_search|web_reader" queries.jsonl sources.jsonl`,
		`rg -n "Chrome 142|WebGPU" raw`,
		`sed -n '1,80p' raw/SRC-1.md`,
	}, " && ")
	result, err := client.RunBashReadOnly(ctx, retrieveCommand,
		ExposedBinaries("rg", "sed"),
		Timeout(5*time.Second),
	)
	if err != nil {
		t.Fatalf("RunBash corpus retrieval: %v", err)
	}
	if result.ExitCode != 0 {
		t.Fatalf("exit code = %d stderr=%q", result.ExitCode, result.StderrString())
	}
	assertExpectedRunner(t, result)
	if result.Changed {
		t.Fatal("read-only corpus retrieval reported a workspace change")
	}
	if result.Audit == nil || result.Audit.WorkspaceID != "conv-react" {
		t.Fatalf("audit = %#v", result.Audit)
	}
	if result.Audit.PolicyMode != readPolicy {
		t.Fatalf("audit policy mode = %q, want %q", result.Audit.PolicyMode, readPolicy)
	}
	stdout := result.StdoutString()
	for _, want := range []string{"web_search", "web_reader", "Chrome 142", "WebGPU"} {
		if !strings.Contains(stdout, want) {
			t.Fatalf("stdout missing %q:\n%s", want, stdout)
		}
	}

	status, err := client.Status(ctx)
	if err != nil {
		t.Fatalf("Status: %v", err)
	}
	if status.Kind != WorkspaceKindLocalBound {
		t.Fatalf("status kind = %q, want %q", status.Kind, WorkspaceKindLocalBound)
	}

	ops, err := client.ListOperations(ctx, PageSize(1))
	if err != nil {
		t.Fatalf("ListOperations: %v", err)
	}
	if len(ops.Operations) != 1 {
		t.Fatalf("operations len = %d, want 1", len(ops.Operations))
	}
	if ops.Operations[0].OpID != result.OpID {
		t.Fatalf("listed op = %q, want %q", ops.Operations[0].OpID, result.OpID)
	}

	op, err := client.GetOperation(ctx, result.OpID)
	if err != nil {
		t.Fatalf("GetOperation: %v", err)
	}
	if op.Command != retrieveCommand {
		t.Fatalf("operation command = %q", op.Command)
	}
	if _, err := os.Stat(filepath.Join(stateDir, "workspaces", "conv-react", "ops", result.OpID+".json")); err != nil {
		t.Fatalf("operation log was not written in daemon state dir: %v", err)
	}

	writeResult, err := client.Run(ctx, `printf "draft" > draft.md`,
		ExposedBinaries("printf"),
		RunPolicyMode(PolicyModeReadOnly),
		Timeout(5*time.Second),
	)
	if err != nil {
		t.Fatalf("read-only sandbox write returned RPC error: %v", err)
	}
	if writeResult.ExitCode == 0 {
		t.Fatal("read-only sandbox write exited successfully")
	}
	assertExpectedRunner(t, writeResult)
	if _, statErr := os.Stat(filepath.Join(worktreePath, "draft.md")); !os.IsNotExist(statErr) {
		t.Fatalf("draft.md exists after rejected read-only write: %v", statErr)
	}
}

func TestIntegrationExposedBinariesControlExternalToolsAndBuiltins(t *testing.T) {
	requireLinuxSandbox(t)
	repoRoot := findRepoRoot(t)
	binary := ensureDaemonBinary(t, repoRoot)

	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	endpoint, _, bindRoot, stop := startDaemonForTest(t, ctx, repoRoot, binary)
	defer stop()

	client := waitForClient(t, ctx, endpoint, "conv-exposed")
	defer client.Close()

	worktreePath := ConversationWorkspacePath(bindRoot, "conv-exposed")
	if _, err := client.BindConversationWorkspace(ctx, bindRoot, "conv-exposed"); err != nil {
		t.Fatalf("BindConversationWorkspace: %v", err)
	}
	writeFile(t, worktreePath, "a.md", "hello\n")

	missingTool, err := client.RunBashReadWrite(ctx, `rg -n hello a.md`,
		ExposedBinaries("cat"),
		Timeout(5*time.Second),
	)
	if err != nil {
		t.Fatalf("RunBashReadWrite with missing external tool returned RPC error: %v", err)
	}
	if missingTool.ExitCode == 0 {
		t.Fatalf("unallowed external tool succeeded: stdout=%q stderr=%q", missingTool.StdoutString(), missingTool.StderrString())
	}
	assertExpectedRunner(t, missingTool)
	if !strings.Contains(missingTool.StderrString(), "rg: command not found") {
		t.Fatalf("missing external tool stderr = %q", missingTool.StderrString())
	}

	absoluteExposed, err := client.RunBashReadWrite(ctx, `/bin/cat a.md`,
		ExposedBinaries("cat"),
		Timeout(5*time.Second),
	)
	if err != nil {
		t.Fatalf("RunBashReadWrite with exposed /bin/cat returned RPC error: %v", err)
	}
	if absoluteExposed.ExitCode != 0 || absoluteExposed.StdoutString() != "hello\n" {
		t.Fatalf("/bin/cat result exit=%d stdout=%q stderr=%q", absoluteExposed.ExitCode, absoluteExposed.StdoutString(), absoluteExposed.StderrString())
	}

	absoluteMissing, err := client.RunBashReadWrite(ctx, `/usr/bin/rg -n hello a.md`,
		ExposedBinaries("cat"),
		Timeout(5*time.Second),
	)
	if err != nil {
		t.Fatalf("RunBashReadWrite with missing /usr/bin/rg returned RPC error: %v", err)
	}
	if absoluteMissing.ExitCode == 0 {
		t.Fatalf("unexposed /usr/bin/rg succeeded: stdout=%q stderr=%q", absoluteMissing.StdoutString(), absoluteMissing.StderrString())
	}

	missingBuiltin, err := client.RunBashReadWrite(ctx, `echo builtin-ok`,
		ExposedBinaries("cat"),
		Timeout(5*time.Second),
	)
	if err != nil {
		t.Fatalf("RunBashReadWrite with missing bash builtin returned RPC error: %v", err)
	}
	if missingBuiltin.ExitCode == 0 {
		t.Fatalf("unexposed bash builtin succeeded: stdout=%q stderr=%q", missingBuiltin.StdoutString(), missingBuiltin.StderrString())
	}
	if !strings.Contains(missingBuiltin.StderrString(), "echo: command not found") {
		t.Fatalf("missing bash builtin stderr = %q", missingBuiltin.StderrString())
	}

	builtin, err := client.RunBashReadWrite(ctx, `echo builtin-ok`,
		ExposedBinaries("cat", "echo"),
		Timeout(5*time.Second),
	)
	if err != nil {
		t.Fatalf("RunBashReadWrite with exposed bash builtin: %v", err)
	}
	if builtin.ExitCode != 0 || builtin.StdoutString() != "builtin-ok\n" {
		t.Fatalf("exposed bash builtin result exit=%d stdout=%q stderr=%q", builtin.ExitCode, builtin.StdoutString(), builtin.StderrString())
	}
	assertExpectedRunner(t, builtin)

	builtinOnly, err := client.RunBashReadWrite(ctx, `command echo builtin-ok`,
		ExposedBinaries("echo"),
		Timeout(5*time.Second),
	)
	if err != nil {
		t.Fatalf("RunBashReadWrite with command echo returned RPC error: %v", err)
	}
	if builtinOnly.ExitCode == 0 {
		t.Fatalf("command echo unexpectedly found external echo: stdout=%q stderr=%q", builtinOnly.StdoutString(), builtinOnly.StderrString())
	}
	assertExpectedRunner(t, builtinOnly)

	_, err = client.RunBashReadWrite(ctx, `cat a.md`,
		ExposedBinaries("cat"),
		EnvVar("PATH", "/workspace"),
		Timeout(5*time.Second),
	)
	if err == nil {
		t.Fatal("RunBashReadWrite with reserved PATH env succeeded")
	}
	if !strings.Contains(err.Error(), "environment variable `PATH` is reserved") {
		t.Fatalf("reserved env error = %v", err)
	}

	writeFile(t, worktreePath, "workspace-tool", "#!/sandbox-runtime/bash\necho workspace-tool\n")
	if err := os.Chmod(filepath.Join(worktreePath, "workspace-tool"), 0o755); err != nil {
		t.Fatalf("chmod workspace-tool: %v", err)
	}
	workspaceTool, err := client.RunBashReadWrite(ctx, `./workspace-tool`,
		ExposedBinaries("echo"),
		Timeout(5*time.Second),
	)
	if err != nil {
		t.Fatalf("RunBashReadWrite with workspace executable returned RPC error: %v", err)
	}
	if workspaceTool.ExitCode == 0 {
		t.Fatalf("workspace executable succeeded: stdout=%q stderr=%q", workspaceTool.StdoutString(), workspaceTool.StderrString())
	}

	network, err := client.RunBashReadWrite(ctx, `: >/dev/tcp/127.0.0.1/1`,
		ExposedBinaries(":"),
		Timeout(5*time.Second),
	)
	if err != nil {
		t.Fatalf("RunBashReadWrite with bash /dev/tcp returned RPC error: %v", err)
	}
	if network.ExitCode == 0 {
		t.Fatalf("network socket unexpectedly succeeded: stdout=%q stderr=%q", network.StdoutString(), network.StderrString())
	}
	networkStderr := network.StderrString()
	if !strings.Contains(networkStderr, "Permission denied") && !strings.Contains(networkStderr, "Operation not permitted") {
		t.Fatalf("network socket stderr = %q", networkStderr)
	}
}

func seedReActCorpus(t *testing.T, worktreePath string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Join(worktreePath, "raw"), 0o755); err != nil {
		t.Fatalf("mkdir raw corpus dir: %v", err)
	}
	writeFile(t, worktreePath, "run.json", `{"topic":"Assess the WebGPU status in Chrome 142"}`+"\n")
	writeJSONL(t, worktreePath, "queries.jsonl", []map[string]any{
		{
			"tool":       "web_search",
			"query":      "Chrome 142 WebGPU release notes",
			"result_ids": []string{"SRC-1", "SRC-2"},
		},
	})
	writeJSONL(t, worktreePath, "sources.jsonl", []map[string]any{
		{
			"src_id":  "SRC-1",
			"tool":    "web_search",
			"title":   "Chrome 142 WebGPU status",
			"url":     "https://example.com/chrome-142-webgpu",
			"domain":  "example.com",
			"tier":    "primary",
			"summary": "web_reader captured the Chrome 142 WebGPU compatibility note.",
		},
		{
			"src_id":  "SRC-2",
			"tool":    "web_search",
			"title":   "WebGPU developer guidance",
			"url":     "https://example.com/webgpu-guidance",
			"domain":  "example.com",
			"tier":    "secondary",
			"summary": "web_reader captured migration guidance for WebGPU applications.",
		},
	})
	writeFile(t, worktreePath, "raw/SRC-1.md", strings.Join([]string{
		"# Chrome 142 WebGPU status",
		"",
		"Source tool: web_reader",
		"",
		"Chrome 142 keeps WebGPU enabled for stable desktop channels.",
		"Teams should validate shader compilation and adapter selection before rollout.",
		"",
	}, "\n"))
	writeFile(t, worktreePath, "raw/SRC-2.md", strings.Join([]string{
		"# WebGPU developer guidance",
		"",
		"Source tool: web_reader",
		"",
		"WebGPU applications should keep a WebGL fallback for unsupported devices.",
		"",
	}, "\n"))
}

func writeJSONL(t *testing.T, root, name string, records []map[string]any) {
	t.Helper()
	var b strings.Builder
	encoder := json.NewEncoder(&b)
	for _, record := range records {
		if err := encoder.Encode(record); err != nil {
			t.Fatalf("encode %s: %v", name, err)
		}
	}
	writeFile(t, root, name, b.String())
}

func writeFile(t *testing.T, root, name, content string) {
	t.Helper()
	path := filepath.Join(root, name)
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatalf("mkdir parent for %s: %v", name, err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatalf("write %s: %v", name, err)
	}
}

func waitForClient(t *testing.T, ctx context.Context, endpoint, workspace string) *Client {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	var lastErr error
	for time.Now().Before(deadline) {
		client, err := New(ctx, WithEndpoint(endpoint), WithWorkspace(workspace), WithTimeout(5*time.Second))
		if err == nil {
			if _, err = client.Status(ctx); err == nil {
				return client
			}
			lastErr = err
			_ = client.Close()
		} else {
			lastErr = err
		}
		time.Sleep(100 * time.Millisecond)
	}
	t.Fatalf("daemon did not become ready: %v", lastErr)
	return nil
}

func startDaemonForTest(t *testing.T, ctx context.Context, repoRoot, binary string) (endpoint, stateDir, bindRoot string, stop func()) {
	t.Helper()
	root := t.TempDir()
	stateDir = filepath.Join(root, "state")
	bindRoot = filepath.Join(root, "bind-root")
	if err := os.MkdirAll(bindRoot, 0o755); err != nil {
		t.Fatalf("mkdir bind root: %v", err)
	}
	addr := freeTCPAddr(t)
	endpoint = fmt.Sprintf("127.0.0.1:%d", addr.Port)
	return endpoint, stateDir, bindRoot, startDaemon(t, ctx, repoRoot, binary, endpoint, stateDir, bindRoot)
}

func startDaemon(t *testing.T, ctx context.Context, repoRoot, binary, endpoint, stateDir, bindRoot string) func() {
	t.Helper()
	cmd := exec.CommandContext(ctx, binary,
		"serve",
		"--addr", endpoint,
		"--state-dir", stateDir,
		"--allow-bind-root", bindRoot,
	)
	cmd.Dir = repoRoot
	var output strings.Builder
	cmd.Stdout = &output
	cmd.Stderr = &output
	if err := cmd.Start(); err != nil {
		t.Fatalf("start daemon: %v", err)
	}

	return func() {
		if cmd.Process != nil {
			_ = cmd.Process.Signal(os.Interrupt)
			done := make(chan error, 1)
			go func() { done <- cmd.Wait() }()
			select {
			case <-done:
			case <-time.After(3 * time.Second):
				_ = cmd.Process.Kill()
				_ = <-done
			}
		}
		if t.Failed() {
			t.Logf("daemon output:\n%s", output.String())
		}
	}
}

func freeTCPAddr(t *testing.T) *net.TCPAddr {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	addr, ok := listener.Addr().(*net.TCPAddr)
	if !ok {
		t.Fatalf("listener addr = %T, want *net.TCPAddr", listener.Addr())
	}
	if err := listener.Close(); err != nil {
		t.Fatalf("close listener: %v", err)
	}
	return addr
}

func supportsLinuxSandbox() bool {
	if runtime.GOOS != "linux" {
		return false
	}
	_, err := exec.LookPath("bwrap")
	return err == nil
}

func requireLinuxSandbox(t *testing.T) {
	t.Helper()
	if !supportsLinuxSandbox() {
		t.Skip("agent-sandbox integration tests require Linux with bubblewrap")
	}
}

func assertExpectedRunner(t *testing.T, result *RunResult) {
	t.Helper()
	want := "bubblewrap"
	if result.Runner != want {
		t.Fatalf("runner = %q, want %q", result.Runner, want)
	}
	if result.Audit != nil && result.Audit.Runner != want {
		t.Fatalf("audit runner = %q, want %q", result.Audit.Runner, want)
	}
}

func ensureDaemonBinary(t *testing.T, repoRoot string) string {
	t.Helper()
	binary := filepath.Join(repoRoot, "target", "debug", "agent-sandbox")
	buildDaemonOnce.Do(func() {
		cargo := exec.Command("cargo", "build", "--quiet")
		cargo.Dir = repoRoot
		output, err := cargo.CombinedOutput()
		if err != nil {
			buildDaemonErr = fmt.Errorf("cargo build failed: %w\n%s", err, output)
		}
	})
	if buildDaemonErr != nil {
		t.Fatal(buildDaemonErr)
	}
	return binary
}

func findRepoRoot(t *testing.T) string {
	t.Helper()
	dir, err := os.Getwd()
	if err != nil {
		t.Fatalf("getwd: %v", err)
	}
	for {
		if _, err := os.Stat(filepath.Join(dir, "Cargo.toml")); err == nil {
			return dir
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			t.Fatal("could not find repo root")
		}
		dir = parent
	}
}

func TestIntegrationRejectsBindOutsideAllowedRoot(t *testing.T) {
	requireLinuxSandbox(t)
	repoRoot := findRepoRoot(t)
	binary := ensureDaemonBinary(t, repoRoot)
	root := t.TempDir()
	stateDir := filepath.Join(root, "state")
	bindRoot := filepath.Join(root, "bind-root")
	outsideRoot := filepath.Join(root, "outside")
	if err := os.MkdirAll(bindRoot, 0o755); err != nil {
		t.Fatalf("mkdir bind root: %v", err)
	}
	if err := os.MkdirAll(outsideRoot, 0o755); err != nil {
		t.Fatalf("mkdir outside root: %v", err)
	}
	addr := freeTCPAddr(t)
	endpoint := fmt.Sprintf("127.0.0.1:%d", addr.Port)

	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	stop := startDaemon(t, ctx, repoRoot, binary, endpoint, stateDir, bindRoot)
	defer stop()

	client := waitForClient(t, ctx, endpoint, "conv-outside")
	defer client.Close()
	_, err := client.BindLocalWorkspace(ctx, filepath.Join(outsideRoot, "head"), CreateIfMissing(true))
	if err == nil {
		t.Fatal("BindLocalWorkspace outside allowed root succeeded")
	}
	if !strings.Contains(err.Error(), "outside allowed bind roots") {
		t.Fatalf("BindLocalWorkspace error = %v", err)
	}
}
