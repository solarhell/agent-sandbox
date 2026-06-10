use std::{net::SocketAddr, path::PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use tonic::transport::{Channel, Server};
use tracing_subscriber::EnvFilter;

use crate::{
    proto::v1::{
        BindWorkspaceRequest, CreateWorkspaceRequest, DeleteWorkspaceRequest, GetOperationRequest,
        ListOperationsRequest, LocalWorkspaceBinding, Operation, PolicyMode, RunRequest,
        StatusRequest, WorkspaceBinding, agent_sandbox_service_client::AgentSandboxServiceClient,
        workspace_binding,
    },
    runner::SandboxRunner,
    service::{AgentSandboxService, ServiceOptions},
};

#[derive(Debug, Parser)]
#[command(name = "agent-sandbox")]
#[command(about = "A lightweight gRPC sandbox daemon for AI agent shell tools")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve {
        #[arg(long, default_value = "127.0.0.1:50051")]
        addr: SocketAddr,
        #[arg(long, default_value = ".agent-sandbox")]
        state_dir: PathBuf,
        #[arg(long = "allow-bind-root")]
        allow_bind_roots: Vec<PathBuf>,
        /// Server-side ceiling for RunRequest.timeout_ms.
        #[arg(long, default_value_t = 1_800_000)]
        max_run_timeout_ms: u64,
        /// Operation log entries kept per workspace; 0 keeps everything.
        #[arg(long, default_value_t = 1_000)]
        max_ops_per_workspace: usize,
    },
    CreateWorkspace {
        #[arg(long, default_value = "http://127.0.0.1:50051")]
        endpoint: String,
        #[arg(long, default_value = "default")]
        workspace: String,
    },
    BindWorkspace {
        #[arg(long, default_value = "http://127.0.0.1:50051")]
        endpoint: String,
        #[arg(long, default_value = "default")]
        workspace: String,
        #[arg(long)]
        path: PathBuf,
        #[arg(long, default_value_t = false)]
        create_if_missing: bool,
    },
    Run {
        command: String,
        #[arg(long, default_value = "http://127.0.0.1:50051")]
        endpoint: String,
        #[arg(long, default_value = "default")]
        workspace: String,
        #[arg(long, default_value = "/")]
        cwd: String,
        #[arg(long = "expose-binary")]
        exposed_binaries: Vec<String>,
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
        #[arg(long, value_enum, default_value_t = PolicyModeArg::ReadWrite)]
        policy_mode: PolicyModeArg,
        /// Per-stream output caps in bytes; 0 uses the daemon default.
        #[arg(long, default_value_t = 0)]
        max_stdout_bytes: u64,
        #[arg(long, default_value_t = 0)]
        max_stderr_bytes: u64,
        /// Skip before/after tree hashing for this run.
        #[arg(long, default_value_t = false)]
        skip_tree_hash: bool,
    },
    Status {
        #[arg(long, default_value = "http://127.0.0.1:50051")]
        endpoint: String,
        #[arg(long, default_value = "default")]
        workspace: String,
    },
    ListOperations {
        #[arg(long, default_value = "http://127.0.0.1:50051")]
        endpoint: String,
        #[arg(long, default_value = "default")]
        workspace: String,
        #[arg(long, default_value_t = 50)]
        page_size: u64,
        #[arg(long, default_value = "")]
        page_token: String,
    },
    GetOperation {
        op_id: String,
        #[arg(long, default_value = "http://127.0.0.1:50051")]
        endpoint: String,
        #[arg(long, default_value = "default")]
        workspace: String,
    },
    DeleteWorkspace {
        #[arg(long, default_value = "http://127.0.0.1:50051")]
        endpoint: String,
        #[arg(long, default_value = "default")]
        workspace: String,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum PolicyModeArg {
    ReadWrite,
    ReadOnly,
}

pub async fn run() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Command::Serve {
            addr,
            state_dir,
            allow_bind_roots,
            max_run_timeout_ms,
            max_ops_per_workspace,
        } => {
            serve(
                addr,
                state_dir,
                allow_bind_roots,
                ServiceOptions {
                    max_run_timeout_ms,
                    max_ops_per_workspace,
                },
            )
            .await
        }
        Command::CreateWorkspace {
            endpoint,
            workspace,
        } => {
            let mut client = client(endpoint).await?;
            let response = client
                .create_workspace(CreateWorkspaceRequest {
                    workspace_id: workspace,
                })
                .await?
                .into_inner();
            let workspace = response
                .workspace
                .ok_or_else(|| anyhow::anyhow!("server returned empty workspace"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "workspace_id": workspace.workspace_id,
                    "worktree_path": workspace.worktree_path,
                    "tree_hash": workspace.tree_hash,
                    "kind": workspace_kind_name(workspace.kind()),
                }))?
            );
            Ok(())
        }
        Command::BindWorkspace {
            endpoint,
            workspace,
            path,
            create_if_missing,
        } => {
            let mut client = client(endpoint).await?;
            let response = client
                .bind_workspace(BindWorkspaceRequest {
                    workspace_id: workspace,
                    binding: Some(WorkspaceBinding {
                        source: Some(workspace_binding::Source::Local(LocalWorkspaceBinding {
                            path: path.display().to_string(),
                            create_if_missing,
                        })),
                    }),
                })
                .await?
                .into_inner();
            let workspace = response
                .workspace
                .ok_or_else(|| anyhow::anyhow!("server returned empty workspace"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "workspace_id": workspace.workspace_id,
                    "worktree_path": workspace.worktree_path,
                    "tree_hash": workspace.tree_hash,
                    "kind": workspace_kind_name(workspace.kind()),
                }))?
            );
            Ok(())
        }
        Command::Run {
            command,
            endpoint,
            workspace,
            cwd,
            exposed_binaries,
            timeout_ms,
            policy_mode,
            max_stdout_bytes,
            max_stderr_bytes,
            skip_tree_hash,
        } => {
            let mut client = client(endpoint).await?;
            let response = client
                .run(RunRequest {
                    workspace_id: workspace,
                    command,
                    cwd,
                    env: Default::default(),
                    exposed_binaries,
                    timeout_ms,
                    policy_mode: PolicyMode::from(policy_mode).into(),
                    max_stdout_bytes,
                    max_stderr_bytes,
                    skip_tree_hash,
                })
                .await?
                .into_inner();
            print!("{}", String::from_utf8_lossy(&response.stdout));
            eprint!("{}", String::from_utf8_lossy(&response.stderr));
            if let Some(audit) = response.audit {
                eprintln!(
                    "\n[agent-sandbox] request={} exit={} runner={} policy={} duration_ms={} cwd={} op={} before={} after={} changed={} timed_out={} stdout_truncated={} stderr_truncated={}",
                    audit.request_id,
                    response.exit_code,
                    response.runner,
                    policy_mode_name(audit.policy_mode()),
                    audit.duration_ms,
                    audit.cwd,
                    response.op_id,
                    response.before_tree_hash,
                    response.after_tree_hash,
                    response.changed,
                    response.timed_out,
                    response.stdout_truncated,
                    response.stderr_truncated
                );
            } else {
                eprintln!(
                    "\n[agent-sandbox] exit={} runner={} op={} before={} after={} changed={} timed_out={}",
                    response.exit_code,
                    response.runner,
                    response.op_id,
                    response.before_tree_hash,
                    response.after_tree_hash,
                    response.changed,
                    response.timed_out
                );
            }
            Ok(())
        }
        Command::Status {
            endpoint,
            workspace,
        } => {
            let mut client = client(endpoint).await?;
            let response = client
                .status(StatusRequest {
                    workspace_id: workspace,
                })
                .await?
                .into_inner();
            let workspace = response
                .workspace
                .ok_or_else(|| anyhow::anyhow!("server returned empty workspace"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "workspace_id": workspace.workspace_id,
                    "worktree_path": workspace.worktree_path,
                    "runner": response.runner,
                    "tree_hash": workspace.tree_hash,
                    "kind": workspace_kind_name(workspace.kind()),
                }))?
            );
            Ok(())
        }
        Command::ListOperations {
            endpoint,
            workspace,
            page_size,
            page_token,
        } => {
            let mut client = client(endpoint).await?;
            let response = client
                .list_operations(ListOperationsRequest {
                    workspace_id: workspace,
                    page_size,
                    page_token,
                })
                .await?
                .into_inner();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "operations": response.operations.into_iter().map(operation_json).collect::<Vec<_>>(),
                    "next_page_token": response.next_page_token,
                }))?
            );
            Ok(())
        }
        Command::GetOperation {
            op_id,
            endpoint,
            workspace,
        } => {
            let mut client = client(endpoint).await?;
            let response = client
                .get_operation(GetOperationRequest {
                    workspace_id: workspace,
                    op_id,
                })
                .await?
                .into_inner();
            let operation = response
                .operation
                .ok_or_else(|| anyhow::anyhow!("server returned empty operation"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&operation_json(operation))?
            );
            Ok(())
        }
        Command::DeleteWorkspace {
            endpoint,
            workspace,
        } => {
            let mut client = client(endpoint).await?;
            let response = client
                .delete_workspace(DeleteWorkspaceRequest {
                    workspace_id: workspace.clone(),
                })
                .await?
                .into_inner();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "workspace_id": workspace,
                    "deleted": response.deleted,
                }))?
            );
            Ok(())
        }
    }
}

async fn serve(
    addr: SocketAddr,
    state_dir: PathBuf,
    allow_bind_roots: Vec<PathBuf>,
    options: ServiceOptions,
) -> anyhow::Result<()> {
    SandboxRunner::preflight()?;
    let service = AgentSandboxService::new(state_dir, allow_bind_roots, options)?;
    tracing::info!(%addr, "starting agent sandbox daemon");
    Server::builder()
        .add_service(service.into_server())
        .serve_with_shutdown(addr, shutdown_signal())
        .await?;
    Ok(())
}

async fn client(endpoint: String) -> anyhow::Result<AgentSandboxServiceClient<Channel>> {
    Ok(AgentSandboxServiceClient::connect(endpoint).await?)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();
}

fn operation_json(operation: Operation) -> serde_json::Value {
    let audit = operation.audit.map(|audit| {
        let policy_mode = audit.policy_mode();
        serde_json::json!({
            "request_id": audit.request_id,
            "workspace_id": audit.workspace_id,
            "cwd": audit.cwd,
            "exposed_binaries": audit.exposed_binaries,
            "policy_mode": policy_mode_name(policy_mode),
            "timeout_ms": audit.timeout_ms,
            "duration_ms": audit.duration_ms,
            "runner": audit.runner,
            "started_at_unix_ms": audit.started_at_unix_ms,
            "finished_at_unix_ms": audit.finished_at_unix_ms,
            "stdout_bytes": audit.stdout_bytes,
            "stderr_bytes": audit.stderr_bytes,
            "timed_out": audit.timed_out,
        })
    });
    serde_json::json!({
        "op_id": operation.op_id,
        "command": operation.command,
        "exit_code": operation.exit_code,
        "before_tree_hash": operation.before_tree_hash,
        "after_tree_hash": operation.after_tree_hash,
        "changed": operation.changed,
        "audit": audit,
    })
}

fn policy_mode_name(mode: PolicyMode) -> &'static str {
    match mode {
        PolicyMode::ReadOnly => "read_only",
        PolicyMode::Unspecified | PolicyMode::ReadWrite => "read_write",
    }
}

fn workspace_kind_name(kind: crate::proto::v1::WorkspaceKind) -> &'static str {
    match kind {
        crate::proto::v1::WorkspaceKind::Managed => "managed",
        crate::proto::v1::WorkspaceKind::LocalBound => "local_bound",
        crate::proto::v1::WorkspaceKind::Unspecified => "unspecified",
    }
}

impl From<PolicyModeArg> for PolicyMode {
    fn from(value: PolicyModeArg) -> Self {
        match value {
            PolicyModeArg::ReadWrite => PolicyMode::ReadWrite,
            PolicyModeArg::ReadOnly => PolicyMode::ReadOnly,
        }
    }
}
