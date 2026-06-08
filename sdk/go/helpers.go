package agentsandbox

import (
	"context"
	"errors"
	"path/filepath"
)

func ConversationWorkspacePath(root, conversationID string) string {
	return filepath.Join(root, "conversations", conversationID, "head")
}

func (c *Client) BindConversationWorkspace(ctx context.Context, root, conversationID string) (*Workspace, error) {
	if root == "" {
		return nil, errors.New("agent sandbox conversation workspace root cannot be empty")
	}
	if conversationID == "" {
		return nil, errors.New("agent sandbox conversation id cannot be empty")
	}
	return c.BindLocalWorkspace(
		ctx,
		ConversationWorkspacePath(root, conversationID),
		BindWorkspaceID(conversationID),
		CreateIfMissing(true),
	)
}

func (c *Client) RunBashReadOnly(ctx context.Context, command string, options ...RunOption) (*RunResult, error) {
	return c.runBash(ctx, command, PolicyModeReadOnly, options...)
}

func (c *Client) RunBashReadWrite(ctx context.Context, command string, options ...RunOption) (*RunResult, error) {
	return c.runBash(ctx, command, PolicyModeReadWrite, options...)
}

func (c *Client) runBash(ctx context.Context, command string, mode PolicyMode, options ...RunOption) (*RunResult, error) {
	req := runConfig{}
	for _, option := range options {
		option(&req)
	}
	if !req.exposedSet {
		return nil, errors.New("agent sandbox bash helpers require explicit ExposedBinaries")
	}

	runOptions := make([]RunOption, 0, len(options)+1)
	runOptions = append(runOptions, options...)
	runOptions = append(runOptions, RunPolicyMode(mode))
	return c.Run(ctx, command, runOptions...)
}
