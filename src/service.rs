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
        CreateWorkspaceResponse, DeleteWorkspaceRequest, DeleteWorkspaceResponse,
        GetOperationRequest, GetOperationResponse, ListOperationsRequest, ListOperationsResponse,
        Operation, RunAudit, RunRequest, RunResponse, StatusRequest, StatusResponse, Workspace,
        WorkspaceKind,
        agent_sandbox_service_server::{
            AgentSandboxService as AgentSandboxServiceRpc, AgentSandboxServiceServer,
        },
        workspace_binding,
    },
    runner::{
        ExposedBinary, FilesystemMode, RunSpec, SandboxRunner, is_bash_builtin, validate_user_env,
    },
    store::{RunAuditInput, WorkspaceKind as StoreWorkspaceKind, WorkspaceStore, hash_tree},
    workspace_locks::WorkspaceLocks,
};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
// Caps keep the worst-case RunResponse comfortably under tonic's default
// 4 MiB message limit (stdout + stderr + envelope).
const DEFAULT_MAX_STDOUT_BYTES: u64 = 1_048_576;
const DEFAULT_MAX_STDERR_BYTES: u64 = 262_144;
const HARD_MAX_OUTPUT_BYTES: u64 = 1_572_864;

#[derive(Debug, Clone)]
pub struct ServiceOptions {
    /// Server-side ceiling for RunRequest.timeout_ms.
    pub max_run_timeout_ms: u64,
    /// Operation log entries kept per workspace; 0 keeps everything.
    pub max_ops_per_workspace: usize,
}

impl Default for ServiceOptions {
    fn default() -> Self {
        Self {
            max_run_timeout_ms: 1_800_000,
            max_ops_per_workspace: 1_000,
        }
    }
}

#[derive(Clone)]
pub struct AgentSandboxService {
    store: Arc<WorkspaceStore>,
    runner: SandboxRunner,
    workspace_locks: Arc<WorkspaceLocks>,
    max_run_timeout_ms: u64,
    /// Last committed after-tree-hash per managed workspace. Managed worktrees
    /// are only mutated by this daemon under the workspace lock, so the cached
    /// hash is a valid before-hash for the next run and saves a full tree walk.
    tree_hash_cache: Arc<std::sync::Mutex<HashMap<String, String>>>,
}

impl AgentSandboxService {
    pub fn new(
        state_dir: PathBuf,
        allowed_bind_roots: Vec<PathBuf>,
        options: ServiceOptions,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            store: Arc::new(WorkspaceStore::with_allowed_bind_roots(
                state_dir,
                allowed_bind_roots,
                options.max_ops_per_workspace,
            )?),
            runner: SandboxRunner::new(),
            workspace_locks: Arc::new(WorkspaceLocks::default()),
            max_run_timeout_ms: options.max_run_timeout_ms.max(1),
            tree_hash_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    pub fn into_server(self) -> AgentSandboxServiceServer<Self> {
        AgentSandboxServiceServer::new(self)
    }

    /// Runs a blocking store/filesystem closure off the async runtime.
    async fn run_blocking<T, F>(&self, task: F) -> anyhow::Result<T>
    where
        F: FnOnce(Arc<WorkspaceStore>) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || task(store))
            .await
            .context("store task panicked")?
    }

    fn cached_tree_hash(&self, workspace_id: &str) -> Option<String> {
        self.tree_hash_cache
            .lock()
            .ok()?
            .get(workspace_id)
            .cloned()
    }

    fn store_tree_hash(&self, workspace_id: &str, tree_hash: &str) {
        if let Ok(mut cache) = self.tree_hash_cache.lock() {
            cache.insert(workspace_id.to_string(), tree_hash.to_string());
        }
    }

    fn evict_tree_hash(&self, workspace_id: &str) {
        if let Ok(mut cache) = self.tree_hash_cache.lock() {
            cache.remove(workspace_id);
        }
    }
}

fn effective_output_cap(requested: u64, default: u64) -> usize {
    let cap = if requested == 0 { default } else { requested };
    cap.min(HARD_MAX_OUTPUT_BYTES) as usize
}

#[tonic::async_trait]
impl AgentSandboxServiceRpc for AgentSandboxService {
    async fn create_workspace(
        &self,
        request: Request<CreateWorkspaceRequest>,
    ) -> Result<Response<CreateWorkspaceResponse>, Status> {
        let request = request.into_inner();
        let workspace_id = nonempty(request.workspace_id);
        let info = self
            .run_blocking(move |store| store.create_workspace(workspace_id))
            .await
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
            workspace_binding::Source::Local(local) => {
                let id = workspace_id.clone();
                self.run_blocking(move |store| {
                    store.bind_local_workspace(
                        &id,
                        PathBuf::from(local.path).as_path(),
                        local.create_if_missing,
                    )
                })
                .await
                .map_err(invalid)?
            }
        };
        // The workspace may now point at a different (externally writable)
        // worktree; any cached hash is stale.
        self.evict_tree_hash(&workspace_id);
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
            DEFAULT_TIMEOUT_MS
        } else {
            request.timeout_ms
        }
        .min(self.max_run_timeout_ms);
        let skip_tree_hash = request.skip_tree_hash;
        let max_stdout_bytes =
            effective_output_cap(request.max_stdout_bytes, DEFAULT_MAX_STDOUT_BYTES);
        let max_stderr_bytes =
            effective_output_cap(request.max_stderr_bytes, DEFAULT_MAX_STDERR_BYTES);
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
        let info = {
            let workspace_id = workspace_id.clone();
            self.run_blocking(move |store| store.ensure_workspace_meta(&workspace_id))
                .await
                .map_err(|error| internal_with_request_id(&request_id, error))?
        };
        // Managed worktrees only change under this lock, so the previous
        // after-hash is a valid before-hash and saves one full tree walk.
        // Local-bound worktrees can be modified externally and are always
        // re-hashed.
        let before_tree_hash = if skip_tree_hash {
            String::new()
        } else {
            let cached = match info.kind {
                StoreWorkspaceKind::Managed => self.cached_tree_hash(&workspace_id),
                StoreWorkspaceKind::LocalBound => None,
            };
            match cached {
                Some(hash) => hash,
                None => {
                    let path = info.worktree_path.clone();
                    self.run_blocking(move |_| hash_tree(&path))
                        .await
                        .map_err(|error| internal_with_request_id(&request_id, error))?
                }
            }
        };
        let timeout = Duration::from_millis(timeout_ms);
        let output = self
            .runner
            .run(RunSpec {
                command: command.clone(),
                workspace_dir: info.worktree_path.clone(),
                cwd: cwd.clone(),
                env: HashMap::from_iter(env),
                timeout,
                filesystem_mode: filesystem_mode(policy_mode),
                exposed_binaries: resolved_exposed_binaries,
                max_stdout_bytes,
                max_stderr_bytes,
            })
            .await
            .map_err(|error| {
                tracing::warn!(%request_id, %workspace_id, command = %command, error = %error, "runner failed request");
                internal_with_request_id(&request_id, error)
            })?;
        let after_tree_hash = if skip_tree_hash {
            String::new()
        } else {
            let path = info.worktree_path.clone();
            let hash = self
                .run_blocking(move |_| hash_tree(&path))
                .await
                .map_err(|error| internal_with_request_id(&request_id, error))?;
            if info.kind == StoreWorkspaceKind::Managed {
                self.store_tree_hash(&workspace_id, &hash);
            }
            hash
        };
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
            timed_out: output.timed_out,
        };
        let commit = {
            let workspace_id = workspace_id.clone();
            let command = command.clone();
            let audit_input = RunAuditInput {
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
                timed_out: output.timed_out,
            };
            let exit_code = output.exit_code;
            self.run_blocking(move |store| {
                store.commit_run(
                    &workspace_id,
                    &command,
                    &before_tree_hash,
                    &after_tree_hash,
                    exit_code,
                    audit_input,
                )
            })
            .await
            .map_err(|error| internal_with_request_id(&request_id, error))?
        };
        tracing::info!(
            %request_id,
            %workspace_id,
            op_id = %commit.op_id,
            exit_code = output.exit_code,
            duration_ms,
            changed = commit.changed,
            timed_out = output.timed_out,
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
            stdout_truncated: output.stdout_truncated,
            stderr_truncated: output.stderr_truncated,
            timed_out: output.timed_out,
        }))
    }

    async fn status(
        &self,
        request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let request = request.into_inner();
        let workspace_id = default_if_empty(request.workspace_id, "default");
        let info = self
            .run_blocking(move |store| store.status(&workspace_id))
            .await
            .map_err(internal)?;
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
            .run_blocking(move |store| {
                store.list_operations(&workspace_id, request.page_size, &request.page_token)
            })
            .await
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
        let op = {
            let requested_op_id = op_id.clone();
            self.run_blocking(move |store| store.get_operation(&workspace_id, &requested_op_id))
                .await
                .map_err(internal)?
                .ok_or_else(|| Status::not_found(format!("operation `{op_id}` not found")))?
        };
        Ok(Response::new(GetOperationResponse {
            operation: Some(operation(op)),
        }))
    }

    async fn delete_workspace(
        &self,
        request: Request<DeleteWorkspaceRequest>,
    ) -> Result<Response<DeleteWorkspaceResponse>, Status> {
        let request = request.into_inner();
        let workspace_id = default_if_empty(request.workspace_id, "default");
        let _workspace_lock = self.workspace_locks.lock(&workspace_id).await;
        let deleted = {
            let workspace_id = workspace_id.clone();
            self.run_blocking(move |store| store.delete_workspace(&workspace_id))
                .await
                .map_err(internal)?
        };
        self.evict_tree_hash(&workspace_id);
        tracing::info!(%workspace_id, deleted, "deleted workspace state");
        Ok(Response::new(DeleteWorkspaceResponse { deleted }))
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
            timed_out: info.timed_out,
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

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
