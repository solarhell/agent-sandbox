use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::{
    proto::v1::{
        BindWorkspaceRequest, BindWorkspaceResponse, CreateWorkspaceRequest,
        CreateWorkspaceResponse, GetOperationRequest, GetOperationResponse, ListOperationsRequest,
        ListOperationsResponse, Operation, RunAudit, RunRequest, RunResponse, StatusRequest,
        StatusResponse, Workspace, WorkspaceKind,
        agent_sandbox_service_server::{
            AgentSandboxService as AgentSandboxServiceRpc, AgentSandboxServiceServer,
        },
        workspace_binding,
    },
    runner::{
        ExposedBinary, FilesystemMode, RunSpec, SandboxRunner, is_bash_builtin, validate_user_env,
    },
    store::{RunAuditInput, WorkspaceKind as StoreWorkspaceKind, WorkspaceStore},
    workspace_locks::WorkspaceLocks,
};

#[derive(Clone)]
pub struct AgentSandboxService {
    store: Arc<WorkspaceStore>,
    runner: SandboxRunner,
    workspace_locks: Arc<WorkspaceLocks>,
}

impl AgentSandboxService {
    pub fn new(state_dir: PathBuf, allowed_bind_roots: Vec<PathBuf>) -> anyhow::Result<Self> {
        Ok(Self {
            store: Arc::new(WorkspaceStore::with_allowed_bind_roots(
                state_dir,
                allowed_bind_roots,
            )?),
            runner: SandboxRunner::new(),
            workspace_locks: Arc::new(WorkspaceLocks::default()),
        })
    }

    pub fn into_server(self) -> AgentSandboxServiceServer<Self> {
        AgentSandboxServiceServer::new(self)
    }
}

#[tonic::async_trait]
impl AgentSandboxServiceRpc for AgentSandboxService {
    async fn create_workspace(
        &self,
        request: Request<CreateWorkspaceRequest>,
    ) -> Result<Response<CreateWorkspaceResponse>, Status> {
        let request = request.into_inner();
        let info = self
            .store
            .create_workspace(nonempty(request.workspace_id))
            .map_err(internal)?;
        Ok(Response::new(CreateWorkspaceResponse {
            workspace: Some(workspace(info)),
        }))
    }

    async fn bind_workspace(
        &self,
        request: Request<BindWorkspaceRequest>,
    ) -> Result<Response<BindWorkspaceResponse>, Status> {
        let request = request.into_inner();
        let workspace_id = default_if_empty(request.workspace_id, "default");
        let binding = request
            .binding
            .ok_or_else(|| Status::invalid_argument("binding is required"))?;
        let source = binding
            .source
            .ok_or_else(|| Status::invalid_argument("binding source is required"))?;
        let info = match source {
            workspace_binding::Source::Local(local) => self
                .store
                .bind_local_workspace(
                    &workspace_id,
                    PathBuf::from(local.path).as_path(),
                    local.create_if_missing,
                )
                .map_err(invalid)?,
        };
        Ok(Response::new(BindWorkspaceResponse {
            workspace: Some(workspace(info)),
        }))
    }

    async fn run(&self, request: Request<RunRequest>) -> Result<Response<RunResponse>, Status> {
        let request = request.into_inner();
        let request_id = format!("req_{}", short_id());
        let started_at_unix_ms = unix_ms();
        let started = Instant::now();
        let policy_mode = request.policy_mode();
        let workspace_id = default_if_empty(request.workspace_id, "default");
        let command = request.command;
        let cwd = if request.cwd.is_empty() {
            "/".to_string()
        } else {
            request.cwd
        };
        let requested_exposed_binaries = request.exposed_binaries;
        let env = request.env;
        let timeout_ms = if request.timeout_ms == 0 {
            30_000
        } else {
            request.timeout_ms
        };
        validate_user_env(&env).map_err(|error| {
            tracing::warn!(%request_id, %workspace_id, error = %error, "environment validation rejected request");
            invalid_with_request_id(&request_id, error)
        })?;
        let resolved_exposed_binaries =
            resolve_exposed_binaries(&requested_exposed_binaries).map_err(|error| {
            tracing::warn!(%request_id, %workspace_id, error = %error, "exposed binary validation rejected request");
            invalid_with_request_id(&request_id, error)
        })?;

        let _workspace_lock = self.workspace_locks.lock(&workspace_id).await;
        let info = self
            .store
            .ensure_workspace(&workspace_id)
            .map_err(|error| internal_with_request_id(&request_id, error))?;
        let before_tree_hash = info.tree_hash.clone();
        let timeout = Duration::from_millis(timeout_ms);
        let output = self
            .runner
            .run(RunSpec {
                command: command.clone(),
                workspace_dir: info.worktree_path,
                cwd: cwd.clone(),
                env: HashMap::from_iter(env),
                timeout,
                filesystem_mode: filesystem_mode(policy_mode),
                exposed_binaries: resolved_exposed_binaries,
            })
            .await
            .map_err(|error| {
                tracing::warn!(%request_id, %workspace_id, command = %command, error = %error, "runner failed request");
                internal_with_request_id(&request_id, error)
            })?;
        let finished_at_unix_ms = unix_ms();
        let duration_ms = started.elapsed().as_millis() as u64;
        let audit = RunAudit {
            request_id: request_id.clone(),
            workspace_id: workspace_id.clone(),
            cwd: cwd.clone(),
            exposed_binaries: requested_exposed_binaries.clone(),
            timeout_ms,
            duration_ms,
            runner: output.runner.clone(),
            started_at_unix_ms,
            finished_at_unix_ms,
            stdout_bytes: output.stdout.len() as u64,
            stderr_bytes: output.stderr.len() as u64,
            policy_mode: policy_mode.into(),
        };
        let commit = self
            .store
            .commit_run(
                &workspace_id,
                &command,
                &before_tree_hash,
                output.exit_code,
                RunAuditInput {
                    request_id: request_id.clone(),
                    cwd,
                    exposed_binaries: requested_exposed_binaries,
                    policy_mode: policy_mode_name(policy_mode).to_string(),
                    timeout_ms,
                    duration_ms,
                    runner: output.runner.clone(),
                    started_at_unix_ms,
                    finished_at_unix_ms,
                    stdout_bytes: output.stdout.len() as u64,
                    stderr_bytes: output.stderr.len() as u64,
                },
            )
            .map_err(|error| internal_with_request_id(&request_id, error))?;
        tracing::info!(
            %request_id,
            %workspace_id,
            op_id = %commit.op_id,
            exit_code = output.exit_code,
            duration_ms,
            changed = commit.changed,
            "completed sandbox command"
        );

        Ok(Response::new(RunResponse {
            exit_code: output.exit_code,
            stdout: output.stdout,
            stderr: output.stderr,
            runner: output.runner,
            op_id: commit.op_id,
            before_tree_hash: commit.before_tree_hash,
            after_tree_hash: commit.after_tree_hash,
            changed: commit.changed,
            audit: Some(audit),
        }))
    }

    async fn status(
        &self,
        request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let request = request.into_inner();
        let workspace_id = default_if_empty(request.workspace_id, "default");
        let info = self.store.status(&workspace_id).map_err(internal)?;
        Ok(Response::new(StatusResponse {
            workspace: Some(workspace(info)),
            runner: self.runner.effective_name(),
        }))
    }

    async fn list_operations(
        &self,
        request: Request<ListOperationsRequest>,
    ) -> Result<Response<ListOperationsResponse>, Status> {
        let request = request.into_inner();
        let workspace_id = default_if_empty(request.workspace_id, "default");
        let page = self
            .store
            .list_operations(&workspace_id, request.page_size, &request.page_token)
            .map_err(internal)?;
        Ok(Response::new(ListOperationsResponse {
            operations: page.operations.into_iter().map(operation).collect(),
            next_page_token: page.next_page_token,
        }))
    }

    async fn get_operation(
        &self,
        request: Request<GetOperationRequest>,
    ) -> Result<Response<GetOperationResponse>, Status> {
        let request = request.into_inner();
        let workspace_id = default_if_empty(request.workspace_id, "default");
        let op_id = request.op_id;
        if op_id.is_empty() {
            return Err(Status::invalid_argument("op_id cannot be empty"));
        }
        let op = self
            .store
            .get_operation(&workspace_id, &op_id)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("operation `{op_id}` not found")))?;
        Ok(Response::new(GetOperationResponse {
            operation: Some(operation(op)),
        }))
    }
}

fn workspace(info: crate::store::WorkspaceInfo) -> Workspace {
    Workspace {
        workspace_id: info.workspace_id,
        worktree_path: info.worktree_path.display().to_string(),
        tree_hash: info.tree_hash,
        kind: proto_workspace_kind(info.kind).into(),
    }
}

fn proto_workspace_kind(kind: StoreWorkspaceKind) -> WorkspaceKind {
    match kind {
        StoreWorkspaceKind::Managed => WorkspaceKind::Managed,
        StoreWorkspaceKind::LocalBound => WorkspaceKind::LocalBound,
    }
}

fn operation(info: crate::store::OperationInfo) -> Operation {
    Operation {
        op_id: info.op_id,
        command: info.command,
        exit_code: info.exit_code,
        before_tree_hash: info.before_tree_hash,
        after_tree_hash: info.after_tree_hash,
        changed: info.changed,
        audit: Some(RunAudit {
            request_id: info.request_id,
            workspace_id: info.workspace_id,
            cwd: info.cwd,
            exposed_binaries: info.exposed_binaries,
            timeout_ms: info.timeout_ms,
            duration_ms: info.duration_ms,
            runner: info.runner,
            started_at_unix_ms: info.started_at_unix_ms,
            finished_at_unix_ms: info.finished_at_unix_ms,
            stdout_bytes: info.stdout_bytes,
            stderr_bytes: info.stderr_bytes,
            policy_mode: proto_policy_mode(&info.policy_mode).into(),
        }),
    }
}

fn filesystem_mode(policy_mode: crate::proto::v1::PolicyMode) -> FilesystemMode {
    match policy_mode {
        crate::proto::v1::PolicyMode::ReadOnly => FilesystemMode::ReadOnly,
        crate::proto::v1::PolicyMode::Unspecified | crate::proto::v1::PolicyMode::ReadWrite => {
            FilesystemMode::ReadWrite
        }
    }
}

fn policy_mode_name(policy_mode: crate::proto::v1::PolicyMode) -> &'static str {
    match policy_mode {
        crate::proto::v1::PolicyMode::ReadOnly => "read_only",
        crate::proto::v1::PolicyMode::Unspecified | crate::proto::v1::PolicyMode::ReadWrite => {
            "read_write"
        }
    }
}

fn proto_policy_mode(value: &str) -> crate::proto::v1::PolicyMode {
    match value {
        "read_only" => crate::proto::v1::PolicyMode::ReadOnly,
        _ => crate::proto::v1::PolicyMode::ReadWrite,
    }
}

fn resolve_exposed_binaries(commands: &[String]) -> anyhow::Result<Vec<ExposedBinary>> {
    if commands.is_empty() {
        bail!("exposed_binaries must be explicitly configured");
    }
    let mut resolved = BTreeMap::new();
    for command in commands {
        let command = command.trim();
        if command.is_empty() {
            bail!("exposed_binaries must not contain empty command names");
        }
        if command.contains('/') {
            bail!("exposed binary `{command}` must be a bare binary name");
        }
        let host_path = if is_bash_builtin(command) {
            tracing::debug!(%command, "exposing bash builtin without host binary");
            None
        } else {
            match which::which(command) {
                Ok(path) => Some(path.canonicalize().with_context(|| {
                    format!("failed to canonicalize exposed binary `{command}`")
                })?),
                Err(_) => {
                    bail!(
                        "exposed binary `{command}` was not found on host PATH or in bash builtins"
                    );
                }
            }
        };
        resolved.insert(
            command.to_string(),
            ExposedBinary {
                name: command.to_string(),
                host_path,
            },
        );
    }
    Ok(resolved.into_values().collect())
}

fn nonempty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn default_if_empty(value: String, default: &str) -> String {
    if value.trim().is_empty() {
        default.to_string()
    } else {
        value
    }
}

fn invalid(error: anyhow::Error) -> Status {
    Status::invalid_argument(error.to_string())
}

fn internal(error: anyhow::Error) -> Status {
    Status::internal(error.to_string())
}

fn invalid_with_request_id(request_id: &str, error: anyhow::Error) -> Status {
    invalid(anyhow::anyhow!("request_id={request_id}: {error}"))
}

fn internal_with_request_id(request_id: &str, error: anyhow::Error) -> Status {
    internal(anyhow::anyhow!("request_id={request_id}: {error}"))
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposed_binaries_must_be_explicit() {
        let err = resolve_exposed_binaries(&[]).unwrap_err().to_string();
        assert!(err.contains("explicitly configured"));
    }

    #[test]
    fn exposed_binaries_must_exist_on_host_path() {
        let err = resolve_exposed_binaries(&["agent-sandbox-missing-binary".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found on host PATH or in bash builtins"));
    }

    #[test]
    fn exposed_binaries_must_be_bare_binary_names() {
        let err = resolve_exposed_binaries(&["/bin/sh".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("bare binary name"));
    }

    #[test]
    fn exposed_binaries_accept_existing_host_binaries() {
        let resolved = resolve_exposed_binaries(&["sh".to_string()]).unwrap();
        assert_eq!(resolved[0].name, "sh");
        assert!(resolved[0].host_path.is_some());
    }

    #[test]
    fn exposed_binaries_accept_bash_builtins() {
        let resolved = resolve_exposed_binaries(&["echo".to_string()]).unwrap();
        assert_eq!(resolved[0].name, "echo");
        assert!(resolved[0].host_path.is_none());
    }
}
