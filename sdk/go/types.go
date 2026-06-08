package agentsandbox

import agentsandboxv1 "github.com/solarhell/agent-sandbox/sdk/go/proto/agentsandbox/v1"

type Workspace struct {
	WorkspaceID  string
	WorktreePath string
	TreeHash     string
	Kind         WorkspaceKind
}

type WorkspaceKind string

const (
	WorkspaceKindManaged    WorkspaceKind = "managed"
	WorkspaceKindLocalBound WorkspaceKind = "local_bound"
	WorkspaceKindUnknown    WorkspaceKind = "unknown"
)

type WorkspaceStatus struct {
	WorkspaceID  string
	WorktreePath string
	Runner       string
	TreeHash     string
	Kind         WorkspaceKind
}

type RunResult struct {
	ExitCode       int32
	Stdout         []byte
	Stderr         []byte
	Runner         string
	OpID           string
	BeforeTreeHash string
	AfterTreeHash  string
	Changed        bool
	Audit          *RunAudit
}

type PolicyMode string

const (
	PolicyModeReadWrite PolicyMode = "read_write"
	PolicyModeReadOnly  PolicyMode = "read_only"
)

type RunAudit struct {
	RequestID        string
	WorkspaceID      string
	Cwd              string
	ExposedBinaries  []string
	PolicyMode       PolicyMode
	TimeoutMs        uint64
	DurationMs       uint64
	Runner           string
	StartedAtUnixMs  int64
	FinishedAtUnixMs int64
	StdoutBytes      uint64
	StderrBytes      uint64
}

type Operation struct {
	OpID           string
	Command        string
	ExitCode       int32
	BeforeTreeHash string
	AfterTreeHash  string
	Changed        bool
	Audit          *RunAudit
}

type ListOperationsResult struct {
	Operations    []*Operation
	NextPageToken string
}

func (r *RunResult) StdoutString() string {
	if r == nil {
		return ""
	}
	return string(r.Stdout)
}

func (r *RunResult) StderrString() string {
	if r == nil {
		return ""
	}
	return string(r.Stderr)
}

func workspaceFromProto(workspace *agentsandboxv1.Workspace) *Workspace {
	if workspace == nil {
		return nil
	}
	return &Workspace{
		WorkspaceID:  workspace.GetWorkspaceId(),
		WorktreePath: workspace.GetWorktreePath(),
		TreeHash:     workspace.GetTreeHash(),
		Kind:         workspaceKindFromProto(workspace.GetKind()),
	}
}

func statusFromProto(status *agentsandboxv1.StatusResponse) *WorkspaceStatus {
	if status == nil {
		return nil
	}
	workspace := status.GetWorkspace()
	return &WorkspaceStatus{
		WorkspaceID:  workspace.GetWorkspaceId(),
		WorktreePath: workspace.GetWorktreePath(),
		Runner:       status.GetRunner(),
		TreeHash:     workspace.GetTreeHash(),
		Kind:         workspaceKindFromProto(workspace.GetKind()),
	}
}

func workspaceKindFromProto(kind agentsandboxv1.WorkspaceKind) WorkspaceKind {
	switch kind {
	case agentsandboxv1.WorkspaceKind_WORKSPACE_KIND_MANAGED:
		return WorkspaceKindManaged
	case agentsandboxv1.WorkspaceKind_WORKSPACE_KIND_LOCAL_BOUND:
		return WorkspaceKindLocalBound
	default:
		return WorkspaceKindUnknown
	}
}

func runResultFromProto(result *agentsandboxv1.RunResponse) *RunResult {
	if result == nil {
		return nil
	}
	return &RunResult{
		ExitCode:       result.GetExitCode(),
		Stdout:         append([]byte(nil), result.GetStdout()...),
		Stderr:         append([]byte(nil), result.GetStderr()...),
		Runner:         result.GetRunner(),
		OpID:           result.GetOpId(),
		BeforeTreeHash: result.GetBeforeTreeHash(),
		AfterTreeHash:  result.GetAfterTreeHash(),
		Changed:        result.GetChanged(),
		Audit:          runAuditFromProto(result.GetAudit()),
	}
}

func runAuditFromProto(audit *agentsandboxv1.RunAudit) *RunAudit {
	if audit == nil {
		return nil
	}
	return &RunAudit{
		RequestID:        audit.GetRequestId(),
		WorkspaceID:      audit.GetWorkspaceId(),
		Cwd:              audit.GetCwd(),
		ExposedBinaries:  append([]string(nil), audit.GetExposedBinaries()...),
		PolicyMode:       policyModeFromProto(audit.GetPolicyMode()),
		TimeoutMs:        audit.GetTimeoutMs(),
		DurationMs:       audit.GetDurationMs(),
		Runner:           audit.GetRunner(),
		StartedAtUnixMs:  audit.GetStartedAtUnixMs(),
		FinishedAtUnixMs: audit.GetFinishedAtUnixMs(),
		StdoutBytes:      audit.GetStdoutBytes(),
		StderrBytes:      audit.GetStderrBytes(),
	}
}

func policyModeFromProto(mode agentsandboxv1.PolicyMode) PolicyMode {
	switch mode {
	case agentsandboxv1.PolicyMode_POLICY_MODE_READ_ONLY:
		return PolicyModeReadOnly
	default:
		return PolicyModeReadWrite
	}
}

func policyModeToProto(mode PolicyMode) agentsandboxv1.PolicyMode {
	switch mode {
	case PolicyModeReadOnly:
		return agentsandboxv1.PolicyMode_POLICY_MODE_READ_ONLY
	default:
		return agentsandboxv1.PolicyMode_POLICY_MODE_READ_WRITE
	}
}

func operationFromProto(operation *agentsandboxv1.Operation) *Operation {
	if operation == nil {
		return nil
	}
	return &Operation{
		OpID:           operation.GetOpId(),
		Command:        operation.GetCommand(),
		ExitCode:       operation.GetExitCode(),
		BeforeTreeHash: operation.GetBeforeTreeHash(),
		AfterTreeHash:  operation.GetAfterTreeHash(),
		Changed:        operation.GetChanged(),
		Audit:          runAuditFromProto(operation.GetAudit()),
	}
}

func listOperationsFromProto(response *agentsandboxv1.ListOperationsResponse) *ListOperationsResult {
	if response == nil {
		return nil
	}
	operations := make([]*Operation, 0, len(response.GetOperations()))
	for _, operation := range response.GetOperations() {
		operations = append(operations, operationFromProto(operation))
	}
	return &ListOperationsResult{
		Operations:    operations,
		NextPageToken: response.GetNextPageToken(),
	}
}
