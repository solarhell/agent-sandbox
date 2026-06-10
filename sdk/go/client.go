package agentsandbox

import (
	"context"
	"errors"
	"time"

	agentsandboxv1 "github.com/solarhell/agent-sandbox/sdk/go/proto/agentsandbox/v1"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
)

const (
	defaultEndpoint  = "127.0.0.1:50051"
	defaultWorkspace = "default"
	defaultTimeout   = 30 * time.Second
)

type Client struct {
	conn       *grpc.ClientConn
	rpc        agentsandboxv1.AgentSandboxServiceClient
	workspace  string
	exposed    []string
	exposedSet bool
	policy     PolicyMode
	timeout    time.Duration
}

type Option func(*config)

type config struct {
	endpoint    string
	workspace   string
	exposed     []string
	exposedSet  bool
	policy      PolicyMode
	timeout     time.Duration
	dialOptions []grpc.DialOption
}

func New(ctx context.Context, options ...Option) (*Client, error) {
	cfg := config{
		endpoint:  defaultEndpoint,
		workspace: defaultWorkspace,
		policy:    PolicyModeReadWrite,
		timeout:   defaultTimeout,
		dialOptions: []grpc.DialOption{
			grpc.WithTransportCredentials(insecure.NewCredentials()),
		},
	}

	for _, option := range options {
		option(&cfg)
	}
	if cfg.endpoint == "" {
		return nil, errors.New("agent sandbox endpoint cannot be empty")
	}
	if cfg.workspace == "" {
		return nil, errors.New("agent sandbox workspace cannot be empty")
	}

	conn, err := grpc.NewClient(cfg.endpoint, cfg.dialOptions...)
	if err != nil {
		return nil, err
	}

	client := &Client{
		conn:       conn,
		rpc:        agentsandboxv1.NewAgentSandboxServiceClient(conn),
		workspace:  cfg.workspace,
		exposed:    cloneStrings(cfg.exposed),
		exposedSet: cfg.exposedSet,
		policy:     cfg.policy,
		timeout:    cfg.timeout,
	}

	conn.Connect()

	return client, nil
}

func WithEndpoint(endpoint string) Option {
	return func(cfg *config) {
		cfg.endpoint = endpoint
	}
}

func WithWorkspace(workspace string) Option {
	return func(cfg *config) {
		cfg.workspace = workspace
	}
}

func WithExposedBinaries(commands ...string) Option {
	return func(cfg *config) {
		cfg.exposed = cloneStrings(commands)
		cfg.exposedSet = true
	}
}

func WithPolicyMode(mode PolicyMode) Option {
	return func(cfg *config) {
		cfg.policy = mode
	}
}

func WithTimeout(timeout time.Duration) Option {
	return func(cfg *config) {
		cfg.timeout = timeout
	}
}

func WithDialOptions(options ...grpc.DialOption) Option {
	return func(cfg *config) {
		cfg.dialOptions = append([]grpc.DialOption{}, options...)
	}
}

func (c *Client) Close() error {
	if c == nil || c.conn == nil {
		return nil
	}
	return c.conn.Close()
}

func (c *Client) CreateWorkspace(ctx context.Context, options ...WorkspaceOption) (*Workspace, error) {
	workspaceID := c.workspace
	for _, option := range options {
		option(&workspaceID)
	}

	response, err := c.rpc.CreateWorkspace(ctx, &agentsandboxv1.CreateWorkspaceRequest{
		WorkspaceId: workspaceID,
	})
	if err != nil {
		return nil, err
	}
	return workspaceFromProto(response.GetWorkspace()), nil
}

func (c *Client) BindLocalWorkspace(ctx context.Context, path string, options ...BindWorkspaceOption) (*Workspace, error) {
	if path == "" {
		return nil, errors.New("agent sandbox bind path cannot be empty")
	}
	req := bindWorkspaceConfig{
		workspace: c.workspace,
	}
	for _, option := range options {
		option(&req)
	}

	response, err := c.rpc.BindWorkspace(ctx, &agentsandboxv1.BindWorkspaceRequest{
		WorkspaceId: req.workspace,
		Binding: &agentsandboxv1.WorkspaceBinding{
			Source: &agentsandboxv1.WorkspaceBinding_Local{
				Local: &agentsandboxv1.LocalWorkspaceBinding{
					Path:            path,
					CreateIfMissing: req.createIfMissing,
				},
			},
		},
	})
	if err != nil {
		return nil, err
	}
	return workspaceFromProto(response.GetWorkspace()), nil
}

func (c *Client) Status(ctx context.Context, options ...WorkspaceOption) (*WorkspaceStatus, error) {
	workspaceID := c.workspace
	for _, option := range options {
		option(&workspaceID)
	}

	response, err := c.rpc.Status(ctx, &agentsandboxv1.StatusRequest{
		WorkspaceId: workspaceID,
	})
	if err != nil {
		return nil, err
	}
	return statusFromProto(response), nil
}

func (c *Client) ListOperations(ctx context.Context, options ...ListOperationsOption) (*ListOperationsResult, error) {
	req := listOperationsConfig{
		workspace: c.workspace,
		pageSize:  50,
	}
	for _, option := range options {
		option(&req)
	}

	response, err := c.rpc.ListOperations(ctx, &agentsandboxv1.ListOperationsRequest{
		WorkspaceId: req.workspace,
		PageSize:    req.pageSize,
		PageToken:   req.pageToken,
	})
	if err != nil {
		return nil, err
	}
	return listOperationsFromProto(response), nil
}

func (c *Client) GetOperation(ctx context.Context, opID string, options ...WorkspaceOption) (*Operation, error) {
	if opID == "" {
		return nil, errors.New("agent sandbox operation id cannot be empty")
	}
	workspaceID := c.workspace
	for _, option := range options {
		option(&workspaceID)
	}

	response, err := c.rpc.GetOperation(ctx, &agentsandboxv1.GetOperationRequest{
		WorkspaceId: workspaceID,
		OpId:        opID,
	})
	if err != nil {
		return nil, err
	}
	return operationFromProto(response.GetOperation()), nil
}

func (c *Client) Run(ctx context.Context, command string, options ...RunOption) (*RunResult, error) {
	if command == "" {
		return nil, errors.New("agent sandbox command cannot be empty")
	}

	req := runConfig{
		workspace:  c.workspace,
		cwd:        "/",
		env:        map[string]string{},
		exposed:    cloneStrings(c.exposed),
		exposedSet: c.exposedSet,
		policy:     c.policy,
		timeout:    c.timeout,
	}

	for _, option := range options {
		option(&req)
	}
	if !req.exposedSet || len(req.exposed) == 0 {
		return nil, errors.New("agent sandbox run requires explicit ExposedBinaries")
	}

	response, err := c.rpc.Run(ctx, &agentsandboxv1.RunRequest{
		WorkspaceId:     req.workspace,
		Command:         command,
		Cwd:             req.cwd,
		Env:             req.env,
		ExposedBinaries: req.exposed,
		TimeoutMs:       uint64(req.timeout.Milliseconds()),
		PolicyMode:      policyModeToProto(req.policy),
		MaxStdoutBytes:  req.maxStdoutBytes,
		MaxStderrBytes:  req.maxStderrBytes,
		SkipTreeHash:    req.skipTreeHash,
	})
	if err != nil {
		return nil, err
	}
	return runResultFromProto(response), nil
}

// DeleteWorkspace removes the daemon-side workspace state. For local-bound
// workspaces the bound worktree itself is left untouched. Returns false when
// the workspace did not exist.
func (c *Client) DeleteWorkspace(ctx context.Context, options ...WorkspaceOption) (bool, error) {
	workspaceID := c.workspace
	for _, option := range options {
		option(&workspaceID)
	}

	response, err := c.rpc.DeleteWorkspace(ctx, &agentsandboxv1.DeleteWorkspaceRequest{
		WorkspaceId: workspaceID,
	})
	if err != nil {
		return false, err
	}
	return response.GetDeleted(), nil
}

type WorkspaceOption func(*string)

func WorkspaceID(workspace string) WorkspaceOption {
	return func(current *string) {
		*current = workspace
	}
}

type RunOption func(*runConfig)

type ListOperationsOption func(*listOperationsConfig)

type BindWorkspaceOption func(*bindWorkspaceConfig)

type runConfig struct {
	workspace      string
	cwd            string
	env            map[string]string
	exposed        []string
	exposedSet     bool
	policy         PolicyMode
	timeout        time.Duration
	maxStdoutBytes uint64
	maxStderrBytes uint64
	skipTreeHash   bool
}

type listOperationsConfig struct {
	workspace string
	pageSize  uint64
	pageToken string
}

type bindWorkspaceConfig struct {
	workspace       string
	createIfMissing bool
}

func RunInWorkspace(workspace string) RunOption {
	return func(cfg *runConfig) {
		cfg.workspace = workspace
	}
}

func Cwd(cwd string) RunOption {
	return func(cfg *runConfig) {
		cfg.cwd = cwd
	}
}

func Env(env map[string]string) RunOption {
	return func(cfg *runConfig) {
		cfg.env = cloneMap(env)
	}
}

func EnvVar(key, value string) RunOption {
	return func(cfg *runConfig) {
		if cfg.env == nil {
			cfg.env = map[string]string{}
		}
		cfg.env[key] = value
	}
}

func ExposedBinaries(commands ...string) RunOption {
	return func(cfg *runConfig) {
		cfg.exposed = cloneStrings(commands)
		cfg.exposedSet = true
	}
}

func RunPolicyMode(mode PolicyMode) RunOption {
	return func(cfg *runConfig) {
		cfg.policy = mode
	}
}

func Timeout(timeout time.Duration) RunOption {
	return func(cfg *runConfig) {
		cfg.timeout = timeout
	}
}

// MaxStdoutBytes caps stdout server-side; 0 uses the daemon default. The
// daemon sets RunResult.StdoutTruncated when the cap is hit.
func MaxStdoutBytes(limit uint64) RunOption {
	return func(cfg *runConfig) {
		cfg.maxStdoutBytes = limit
	}
}

// MaxStderrBytes caps stderr server-side; 0 uses the daemon default.
func MaxStderrBytes(limit uint64) RunOption {
	return func(cfg *runConfig) {
		cfg.maxStderrBytes = limit
	}
}

// SkipTreeHash disables before/after worktree hashing for this run. Use it
// when tree hashes are not consumed and the workspace is large.
func SkipTreeHash(skip bool) RunOption {
	return func(cfg *runConfig) {
		cfg.skipTreeHash = skip
	}
}

func ListOperationsWorkspace(workspace string) ListOperationsOption {
	return func(cfg *listOperationsConfig) {
		cfg.workspace = workspace
	}
}

func PageSize(pageSize uint64) ListOperationsOption {
	return func(cfg *listOperationsConfig) {
		cfg.pageSize = pageSize
	}
}

func PageToken(pageToken string) ListOperationsOption {
	return func(cfg *listOperationsConfig) {
		cfg.pageToken = pageToken
	}
}

func BindWorkspaceID(workspace string) BindWorkspaceOption {
	return func(cfg *bindWorkspaceConfig) {
		cfg.workspace = workspace
	}
}

func CreateIfMissing(create bool) BindWorkspaceOption {
	return func(cfg *bindWorkspaceConfig) {
		cfg.createIfMissing = create
	}
}

func cloneStrings(values []string) []string {
	if len(values) == 0 {
		return nil
	}
	out := make([]string, len(values))
	copy(out, values)
	return out
}

func cloneMap(values map[string]string) map[string]string {
	if len(values) == 0 {
		return map[string]string{}
	}
	out := make(map[string]string, len(values))
	for key, value := range values {
		out[key] = value
	}
	return out
}
