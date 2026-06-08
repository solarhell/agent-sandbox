use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct WorkspaceStore {
    state_dir: PathBuf,
    allowed_bind_roots: Vec<PathBuf>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    Managed,
    LocalBound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    pub kind: WorkspaceKind,
    pub worktree_path: PathBuf,
    pub tree_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceMetadata {
    workspace_id: String,
    kind: WorkspaceKind,
    worktree_path: PathBuf,
    created_at_unix_ms: i64,
    updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationInfo {
    pub op_id: String,
    pub request_id: String,
    pub workspace_id: String,
    pub command: String,
    pub cwd: String,
    pub exposed_binaries: Vec<String>,
    pub policy_mode: String,
    pub timeout_ms: u64,
    pub duration_ms: u64,
    pub runner: String,
    pub started_at_unix_ms: i64,
    pub finished_at_unix_ms: i64,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub before_tree_hash: String,
    pub after_tree_hash: String,
    pub changed: bool,
    pub exit_code: i32,
}

#[derive(Debug, Clone)]
pub struct RunCommit {
    pub op_id: String,
    pub before_tree_hash: String,
    pub after_tree_hash: String,
    pub changed: bool,
}

#[derive(Debug, Clone)]
pub struct RunAuditInput {
    pub request_id: String,
    pub cwd: String,
    pub exposed_binaries: Vec<String>,
    pub policy_mode: String,
    pub timeout_ms: u64,
    pub duration_ms: u64,
    pub runner: String,
    pub started_at_unix_ms: i64,
    pub finished_at_unix_ms: i64,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ListOperationsResult {
    pub operations: Vec<OperationInfo>,
    pub next_page_token: String,
}

impl WorkspaceStore {
    #[cfg(test)]
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir: absolute_path(state_dir),
            allowed_bind_roots: Vec::new(),
        }
    }

    pub fn with_allowed_bind_roots<I, P>(
        state_dir: PathBuf,
        allowed_bind_roots: I,
    ) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let mut roots = Vec::new();
        for root in allowed_bind_roots {
            roots.push(canonical_bind_root(root.into())?);
        }
        Ok(Self {
            state_dir: absolute_path(state_dir),
            allowed_bind_roots: roots,
        })
    }

    pub fn create_workspace(&self, workspace_id: Option<String>) -> anyhow::Result<WorkspaceInfo> {
        let workspace_id = workspace_id.unwrap_or_else(|| format!("ws_{}", short_id()));
        validate_workspace_id(&workspace_id)?;
        let workspace_dir = self.workspace_dir(&workspace_id);
        let worktree_path = workspace_dir.join("worktree");
        fs::create_dir_all(workspace_dir.join("ops"))?;
        fs::create_dir_all(&worktree_path)?;

        let now = unix_ms();
        self.write_metadata(&WorkspaceMetadata {
            workspace_id: workspace_id.clone(),
            kind: WorkspaceKind::Managed,
            worktree_path: canonicalize_existing_dir(&worktree_path)?,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
        })?;
        self.status(&workspace_id)
    }

    pub fn bind_local_workspace(
        &self,
        workspace_id: &str,
        path: &Path,
        create_if_missing: bool,
    ) -> anyhow::Result<WorkspaceInfo> {
        validate_workspace_id(workspace_id)?;
        let worktree_path = self.canonicalize_bind_path(path, create_if_missing)?;
        let workspace_dir = self.workspace_dir(workspace_id);
        fs::create_dir_all(workspace_dir.join("ops"))?;

        let now = unix_ms();
        let created_at_unix_ms = self
            .read_metadata(workspace_id)
            .ok()
            .map(|metadata| metadata.created_at_unix_ms)
            .unwrap_or(now);
        self.write_metadata(&WorkspaceMetadata {
            workspace_id: workspace_id.to_string(),
            kind: WorkspaceKind::LocalBound,
            worktree_path,
            created_at_unix_ms,
            updated_at_unix_ms: now,
        })?;
        self.status(workspace_id)
    }

    pub fn ensure_workspace(&self, workspace_id: &str) -> anyhow::Result<WorkspaceInfo> {
        validate_workspace_id(workspace_id)?;
        if !self.metadata_file(workspace_id).exists() {
            return self.create_workspace(Some(workspace_id.to_string()));
        }
        self.status(workspace_id)
    }

    pub fn status(&self, workspace_id: &str) -> anyhow::Result<WorkspaceInfo> {
        validate_workspace_id(workspace_id)?;
        if !self.metadata_file(workspace_id).exists() {
            return self.create_workspace(Some(workspace_id.to_string()));
        }
        let metadata = self.read_metadata(workspace_id)?;
        fs::create_dir_all(&metadata.worktree_path)?;
        Ok(WorkspaceInfo {
            workspace_id: metadata.workspace_id,
            kind: metadata.kind,
            tree_hash: hash_tree(&metadata.worktree_path)?,
            worktree_path: metadata.worktree_path,
        })
    }

    pub fn commit_run(
        &self,
        workspace_id: &str,
        command: &str,
        before_tree_hash: &str,
        exit_code: i32,
        audit: RunAuditInput,
    ) -> anyhow::Result<RunCommit> {
        validate_workspace_id(workspace_id)?;
        let info = self.status(workspace_id)?;
        let after_tree_hash = hash_tree(&info.worktree_path)?;
        let changed = before_tree_hash != after_tree_hash;
        let op_id = format!("op_{}", short_id());
        self.write_operation(
            workspace_id,
            &OperationInfo {
                op_id: op_id.clone(),
                request_id: audit.request_id,
                workspace_id: workspace_id.to_string(),
                command: command.to_string(),
                cwd: audit.cwd,
                exposed_binaries: audit.exposed_binaries,
                policy_mode: audit.policy_mode,
                timeout_ms: audit.timeout_ms,
                duration_ms: audit.duration_ms,
                runner: audit.runner,
                started_at_unix_ms: audit.started_at_unix_ms,
                finished_at_unix_ms: audit.finished_at_unix_ms,
                stdout_bytes: audit.stdout_bytes,
                stderr_bytes: audit.stderr_bytes,
                before_tree_hash: before_tree_hash.to_string(),
                after_tree_hash: after_tree_hash.clone(),
                changed,
                exit_code,
            },
        )?;
        Ok(RunCommit {
            op_id,
            before_tree_hash: before_tree_hash.to_string(),
            after_tree_hash,
            changed,
        })
    }

    pub fn list_operations(
        &self,
        workspace_id: &str,
        page_size: u32,
        page_token: &str,
    ) -> anyhow::Result<ListOperationsResult> {
        validate_workspace_id(workspace_id)?;
        let offset = parse_page_token(page_token)?;
        let page_size = normalized_page_size(page_size);
        let ops_dir = self.workspace_dir(workspace_id).join("ops");
        if !ops_dir.exists() {
            return Ok(ListOperationsResult {
                operations: Vec::new(),
                next_page_token: String::new(),
            });
        }

        let mut operations = Vec::new();
        for entry in fs::read_dir(&ops_dir)
            .with_context(|| format!("failed to read {}", ops_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                operations.push(read_json::<OperationInfo>(&path)?);
            }
        }
        operations.sort_by(|a, b| {
            b.started_at_unix_ms
                .cmp(&a.started_at_unix_ms)
                .then_with(|| b.op_id.cmp(&a.op_id))
        });

        let end = offset.saturating_add(page_size).min(operations.len());
        let next_page_token = if end < operations.len() {
            end.to_string()
        } else {
            String::new()
        };
        Ok(ListOperationsResult {
            operations: operations
                .into_iter()
                .skip(offset)
                .take(page_size)
                .collect(),
            next_page_token,
        })
    }

    pub fn get_operation(
        &self,
        workspace_id: &str,
        op_id: &str,
    ) -> anyhow::Result<Option<OperationInfo>> {
        validate_workspace_id(workspace_id)?;
        validate_operation_id(op_id)?;
        let path = self.operation_file(workspace_id, op_id);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(read_json(&path)?))
    }

    fn canonicalize_bind_path(
        &self,
        path: &Path,
        create_if_missing: bool,
    ) -> anyhow::Result<PathBuf> {
        if self.allowed_bind_roots.is_empty() {
            bail!("no allowed bind roots configured");
        }
        if !path.is_absolute() {
            bail!("bind path must be absolute");
        }
        if path == Path::new("/") {
            bail!("bind path cannot be filesystem root");
        }

        let canonical_path = if path.exists() {
            canonicalize_existing_dir(path)?
        } else {
            if !create_if_missing {
                bail!("bind path does not exist: {}", path.display());
            }
            let canonical_parent = nearest_existing_parent(path)?;
            self.ensure_allowed_bind_path(&canonical_parent)?;
            fs::create_dir_all(path)
                .with_context(|| format!("failed to create bind path {}", path.display()))?;
            canonicalize_existing_dir(path)?
        };

        self.ensure_allowed_bind_path(&canonical_path)?;
        Ok(canonical_path)
    }

    fn ensure_allowed_bind_path(&self, path: &Path) -> anyhow::Result<()> {
        if self
            .allowed_bind_roots
            .iter()
            .any(|root| path.starts_with(root))
        {
            Ok(())
        } else {
            bail!(
                "bind path is outside allowed bind roots: {}",
                path.display()
            )
        }
    }

    fn workspace_dir(&self, workspace_id: &str) -> PathBuf {
        self.state_dir.join("workspaces").join(workspace_id)
    }

    fn metadata_file(&self, workspace_id: &str) -> PathBuf {
        self.workspace_dir(workspace_id).join("workspace.json")
    }

    fn operation_file(&self, workspace_id: &str, op_id: &str) -> PathBuf {
        self.workspace_dir(workspace_id)
            .join("ops")
            .join(format!("{op_id}.json"))
    }

    fn write_metadata(&self, metadata: &WorkspaceMetadata) -> anyhow::Result<()> {
        write_json(self.metadata_file(&metadata.workspace_id), metadata)
    }

    fn read_metadata(&self, workspace_id: &str) -> anyhow::Result<WorkspaceMetadata> {
        read_json(&self.metadata_file(workspace_id))
    }

    fn write_operation(&self, workspace_id: &str, op: &OperationInfo) -> anyhow::Result<()> {
        write_json(self.operation_file(workspace_id, &op.op_id), op)
    }
}

fn read_json<T>(path: &Path) -> anyhow::Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_json<T>(path: PathBuf, value: &T) -> anyhow::Result<()>
where
    T: Serialize,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value)?;
    atomic_write(&path, &bytes)?;
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("path has invalid filename: {}", path.display()))?;
    let tmp_path = parent.join(format!(".{filename}.{}.tmp", short_id()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .with_context(|| format!("failed to create {}", tmp_path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", tmp_path.display()))?;
        drop(file);
        fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "failed to rename {} to {}",
                tmp_path.display(),
                path.display()
            )
        })?;
        sync_dir(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn sync_dir(path: &Path) -> anyhow::Result<()> {
    let dir = fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    dir.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))
}

fn normalized_page_size(page_size: u32) -> usize {
    match page_size {
        0 => 50,
        1..=200 => page_size as usize,
        _ => 200,
    }
}

fn parse_page_token(page_token: &str) -> anyhow::Result<usize> {
    if page_token.trim().is_empty() {
        return Ok(0);
    }
    page_token
        .parse()
        .context("page_token must be an operation list offset")
}

fn validate_workspace_id(workspace_id: &str) -> anyhow::Result<()> {
    validate_id(workspace_id, "workspace_id", false)
}

fn validate_operation_id(op_id: &str) -> anyhow::Result<()> {
    validate_id(op_id, "op_id", true)
}

fn validate_id(value: &str, label: &str, require_op_prefix: bool) -> anyhow::Result<()> {
    if value.is_empty() || value.len() > 128 {
        bail!("{label} must be 1-128 characters");
    }
    if require_op_prefix && !value.starts_with("op_") {
        bail!("{label} must start with `op_`");
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        bail!("{label} contains unsupported characters");
    }
    Ok(())
}

fn canonical_bind_root(path: PathBuf) -> anyhow::Result<PathBuf> {
    if !path.is_absolute() {
        bail!("allowed bind root must be absolute: {}", path.display());
    }
    if path == Path::new("/") {
        bail!("allowed bind root cannot be filesystem root");
    }
    canonicalize_existing_dir(&path)
}

fn canonicalize_existing_dir(path: &Path) -> anyhow::Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("path is not a directory: {}", canonical.display());
    }
    Ok(canonical)
}

fn nearest_existing_parent(path: &Path) -> anyhow::Result<PathBuf> {
    let mut current = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("bind path has no parent: {}", path.display()))?;
    while !current.exists() {
        current = current.parent().ok_or_else(|| {
            anyhow::anyhow!("bind path has no existing parent: {}", path.display())
        })?;
    }
    canonicalize_existing_dir(current)
}

fn absolute_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    }
}

fn hash_tree(root: &Path) -> anyhow::Result<String> {
    let mut entries = BTreeMap::new();
    if !root.exists() {
        return Ok(hex::encode(Sha256::digest([])));
    }

    for entry in WalkDir::new(root).follow_links(false).min_depth(1) {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        if entry.file_type().is_dir() {
            entries.insert(relative, "dir".to_string());
        } else if entry.file_type().is_file() {
            let mut file = fs::File::open(path)
                .with_context(|| format!("failed to open {}", path.display()))?;
            let mut hasher = Sha256::new();
            let mut buffer = [0u8; 8192];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            entries.insert(relative, format!("file:{}", hex::encode(hasher.finalize())));
        } else if entry.file_type().is_symlink() {
            entries.insert(
                relative,
                format!("symlink:{}", fs::read_link(path)?.to_string_lossy()),
            );
        }
    }

    let mut hasher = Sha256::new();
    for (path, digest) in entries {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(digest.as_bytes());
        hasher.update([0]);
    }
    Ok(hex::encode(hasher.finalize()))
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
    fn creates_managed_workspace_with_metadata() {
        let root = std::env::temp_dir().join(format!("agent-sandbox-test-{}", short_id()));
        let store = WorkspaceStore::new(root.clone());
        let ws = store
            .create_workspace(Some("ws".to_string()))
            .expect("create workspace");
        assert_eq!(ws.kind, WorkspaceKind::Managed);
        assert!(ws.worktree_path.ends_with("worktree"));
        assert!(root.join("workspaces/ws/workspace.json").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn binds_local_workspace_inside_allowed_root() {
        let root = std::env::temp_dir().join(format!("agent-sandbox-test-{}", short_id()));
        let bind_root = root.join("bind-root");
        let external = bind_root.join("conversation/head");
        fs::create_dir_all(&bind_root).unwrap();
        let store =
            WorkspaceStore::with_allowed_bind_roots(root.join("state"), [bind_root.clone()])
                .unwrap();

        let ws = store
            .bind_local_workspace("conv", &external, true)
            .expect("bind workspace");
        assert_eq!(ws.kind, WorkspaceKind::LocalBound);
        assert_eq!(ws.worktree_path, external.canonicalize().unwrap());
        assert!(root.join("state/workspaces/conv/ops").exists());
        assert!(root.join("state/workspaces/conv/workspace.json").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_bind_workspace_outside_allowed_roots() {
        let root = std::env::temp_dir().join(format!("agent-sandbox-test-{}", short_id()));
        let bind_root = root.join("bind-root");
        let outside = root.join("outside/head");
        fs::create_dir_all(&bind_root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let store =
            WorkspaceStore::with_allowed_bind_roots(root.join("state"), [bind_root.clone()])
                .unwrap();
        let err = store
            .bind_local_workspace("conv", &outside, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside allowed bind roots"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)]
    fn rejects_symlink_escape_from_allowed_roots() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("agent-sandbox-test-{}", short_id()));
        let bind_root = root.join("bind-root");
        let outside = root.join("outside");
        fs::create_dir_all(&bind_root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, bind_root.join("escape")).unwrap();
        let store =
            WorkspaceStore::with_allowed_bind_roots(root.join("state"), [bind_root.clone()])
                .unwrap();
        let err = store
            .bind_local_workspace("conv", &bind_root.join("escape"), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside allowed bind roots"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn commits_run_with_operation_log_only() {
        let root = std::env::temp_dir().join(format!("agent-sandbox-test-{}", short_id()));
        let store = WorkspaceStore::new(root.clone());
        let ws = store
            .create_workspace(Some("ws".to_string()))
            .expect("create workspace");
        fs::write(ws.worktree_path.join("a.md"), "hello").unwrap();
        let commit = store
            .commit_run(
                "ws",
                "printf hello > a.md",
                &ws.tree_hash,
                0,
                RunAuditInput {
                    request_id: "req_test".to_string(),
                    cwd: "/".to_string(),
                    exposed_binaries: vec!["printf".to_string()],
                    policy_mode: "read_write".to_string(),
                    timeout_ms: 30_000,
                    duration_ms: 3,
                    runner: "bubblewrap".to_string(),
                    started_at_unix_ms: 1,
                    finished_at_unix_ms: 4,
                    stdout_bytes: 0,
                    stderr_bytes: 0,
                },
            )
            .unwrap();
        assert!(commit.changed);
        assert_ne!(commit.before_tree_hash, commit.after_tree_hash);
        let op_path = root
            .join("workspaces/ws/ops")
            .join(format!("{}.json", commit.op_id));
        assert!(op_path.exists());
        let op: OperationInfo = read_json(&op_path).unwrap();
        assert_eq!(op.op_id, commit.op_id);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lists_and_gets_operations_newest_first() {
        let root = std::env::temp_dir().join(format!("agent-sandbox-test-{}", short_id()));
        let store = WorkspaceStore::new(root.clone());
        let ws = store
            .create_workspace(Some("ws".to_string()))
            .expect("create workspace");
        let first = store
            .commit_run(
                "ws",
                "printf first",
                &ws.tree_hash,
                0,
                RunAuditInput {
                    request_id: "req_first".to_string(),
                    cwd: "/".to_string(),
                    exposed_binaries: vec!["printf".to_string()],
                    policy_mode: "read_write".to_string(),
                    timeout_ms: 30_000,
                    duration_ms: 1,
                    runner: "bubblewrap".to_string(),
                    started_at_unix_ms: 10,
                    finished_at_unix_ms: 11,
                    stdout_bytes: 5,
                    stderr_bytes: 0,
                },
            )
            .unwrap();
        let second = store
            .commit_run(
                "ws",
                "printf second",
                &ws.tree_hash,
                0,
                RunAuditInput {
                    request_id: "req_second".to_string(),
                    cwd: "/".to_string(),
                    exposed_binaries: vec!["printf".to_string()],
                    policy_mode: "read_write".to_string(),
                    timeout_ms: 30_000,
                    duration_ms: 1,
                    runner: "bubblewrap".to_string(),
                    started_at_unix_ms: 20,
                    finished_at_unix_ms: 21,
                    stdout_bytes: 6,
                    stderr_bytes: 0,
                },
            )
            .unwrap();

        let page = store.list_operations("ws", 1, "").unwrap();
        assert_eq!(page.operations[0].op_id, second.op_id);
        assert_eq!(page.next_page_token, "1");
        let page = store
            .list_operations("ws", 1, &page.next_page_token)
            .unwrap();
        assert_eq!(page.operations[0].op_id, first.op_id);
        assert!(page.next_page_token.is_empty());

        let fetched = store
            .get_operation("ws", &second.op_id)
            .unwrap()
            .expect("operation");
        assert_eq!(fetched.command, "printf second");
        assert!(
            store
                .get_operation("ws", "op_000000000000")
                .unwrap()
                .is_none()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn list_operations_ignores_tmp_files() {
        let root = std::env::temp_dir().join(format!("agent-sandbox-test-{}", short_id()));
        let store = WorkspaceStore::new(root.clone());
        let ws = store
            .create_workspace(Some("ws".to_string()))
            .expect("create workspace");
        let commit = store
            .commit_run(
                "ws",
                "printf ok",
                &ws.tree_hash,
                0,
                RunAuditInput {
                    request_id: "req_ok".to_string(),
                    cwd: "/".to_string(),
                    exposed_binaries: vec!["printf".to_string()],
                    policy_mode: "read_write".to_string(),
                    timeout_ms: 30_000,
                    duration_ms: 1,
                    runner: "bubblewrap".to_string(),
                    started_at_unix_ms: 1,
                    finished_at_unix_ms: 2,
                    stdout_bytes: 2,
                    stderr_bytes: 0,
                },
            )
            .unwrap();
        fs::write(
            root.join("workspaces/ws/ops/.op_partial.json.tmp"),
            b"{not json",
        )
        .unwrap();
        let page = store.list_operations("ws", 50, "").unwrap();
        assert_eq!(page.operations.len(), 1);
        assert_eq!(page.operations[0].op_id, commit.op_id);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_unsafe_ids() {
        let store = WorkspaceStore::new(std::env::temp_dir());
        assert!(store.status("../escape").is_err());
        assert!(store.get_operation("ws", "../escape").is_err());
    }
}
