use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, bail};
use tokio::{process::Command, time};

use crate::landlock_exec;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum FilesystemMode {
    ReadWrite,
    ReadOnly,
}

#[derive(Debug, Clone)]
pub struct RunSpec {
    pub command: String,
    pub workspace_dir: PathBuf,
    pub cwd: String,
    pub env: HashMap<String, String>,
    pub timeout: Duration,
    pub filesystem_mode: FilesystemMode,
    pub exposed_binaries: Vec<ExposedBinary>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ExposedBinary {
    pub name: String,
    pub host_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct RunOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub runner: String,
}

#[derive(Debug, Clone)]
pub struct SandboxRunner;

impl SandboxRunner {
    pub fn new() -> Self {
        Self
    }

    pub fn preflight() -> anyhow::Result<()> {
        if !cfg!(target_os = "linux") {
            bail!("agent-sandbox is Linux-only and requires bubblewrap");
        }
        which::which("bwrap").context("failed to locate `bwrap`")?;
        which::which("bash").context("failed to locate `bash`")?;
        probe_bubblewrap().context("failed to verify bubblewrap namespace support")?;
        landlock_exec::probe().context("failed to verify Landlock execute allowlist support")?;
        Ok(())
    }

    pub fn effective_name(&self) -> String {
        "bubblewrap".to_string()
    }

    pub async fn run(&self, spec: RunSpec) -> anyhow::Result<RunOutput> {
        self.run_bubblewrap(spec).await
    }

    async fn run_bubblewrap(&self, spec: RunSpec) -> anyhow::Result<RunOutput> {
        if !cfg!(target_os = "linux") {
            bail!("agent-sandbox runner is Linux-only and requires bubblewrap");
        }
        let workspace_dir = spec
            .workspace_dir
            .canonicalize()
            .context("failed to canonicalize workspace directory")?;
        let chdir = sandbox_cwd(&spec.cwd)?;
        validate_user_env(&spec.env)?;

        let bwrap_path = which::which("bwrap").context("failed to locate `bwrap`")?;
        let host_bash = which::which("bash")
            .context("failed to locate `bash`")?
            .canonicalize()
            .context("failed to canonicalize `bash`")?;
        let helper_path = std::env::current_exe()
            .context("failed to locate current executable for Landlock helper")?
            .canonicalize()
            .context("failed to canonicalize Landlock helper executable")?;
        let mut command = Command::new(bwrap_path);
        command
            .env_clear()
            .arg("--die-with-parent")
            .arg("--unshare-user")
            .arg("--unshare-ipc")
            .arg("--unshare-pid")
            .arg("--unshare-uts")
            .arg("--unshare-cgroup")
            .arg("--new-session")
            .arg("--clearenv")
            .arg("--proc")
            .arg("/proc")
            .arg("--dev")
            .arg("/dev")
            .arg("--tmpfs")
            .arg("/tmp")
            .arg("--dir")
            .arg("/bin")
            .arg("--dir")
            .arg("/sandbox-runtime")
            .arg("--ro-bind")
            .arg(&host_bash)
            .arg("/sandbox-runtime/bash")
            .arg("--ro-bind")
            .arg(helper_path)
            .arg("/sandbox-runtime/agent-sandbox-helper")
            .arg("--chdir")
            .arg(chdir);
        for exposed in &spec.exposed_binaries {
            let Some(host_path) = &exposed.host_path else {
                continue;
            };
            command
                .arg("--ro-bind")
                .arg(host_path)
                .arg(format!("/bin/{}", exposed.name));
        }
        if spec.filesystem_mode == FilesystemMode::ReadOnly {
            command.arg("--ro-bind");
        } else {
            command.arg("--bind");
        }
        command.arg(&workspace_dir).arg("/workspace");

        if Path::new("/usr").exists() {
            command.arg("--dir").arg("/usr");
        }
        for path in readonly_system_paths() {
            if path.exists() {
                command.arg("--ro-bind").arg(path).arg(path);
            }
        }

        command
            .arg("--setenv")
            .arg("HOME")
            .arg("/workspace")
            .arg("--setenv")
            .arg("PWD")
            .arg("/workspace")
            .arg("--setenv")
            .arg("TMPDIR")
            .arg("/tmp")
            .arg("--setenv")
            .arg("PATH")
            .arg("/bin")
            .arg("--setenv")
            .arg("LANG")
            .arg("C.UTF-8");

        for (key, value) in &spec.env {
            command.arg("--setenv").arg(key).arg(value);
        }

        command
            .arg("/sandbox-runtime/agent-sandbox-helper")
            .arg(landlock_exec::HELPER_ARG)
            .arg(command_script(&spec.command, &spec.exposed_binaries))
            .kill_on_drop(true);

        run_child(command, spec.timeout, "bubblewrap").await
    }
}

fn probe_bubblewrap() -> anyhow::Result<()> {
    let bwrap_path = which::which("bwrap").context("failed to locate `bwrap`")?;
    let output = std::process::Command::new(bwrap_path)
        .env_clear()
        .arg("--die-with-parent")
        .arg("--unshare-user")
        .arg("--unshare-ipc")
        .arg("--unshare-pid")
        .arg("--unshare-uts")
        .arg("--unshare-cgroup")
        .arg("--new-session")
        .arg("--clearenv")
        .arg("--ro-bind")
        .arg("/")
        .arg("/")
        .arg("/bin/true")
        .output()
        .context("failed to run bubblewrap probe")?;
    if !output.status.success() {
        bail!(
            "bubblewrap probe failed: status={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

async fn run_child(
    mut command: Command,
    timeout: Duration,
    runner: &str,
) -> anyhow::Result<RunOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().context("failed to spawn sandbox command")?;
    let output = match time::timeout(timeout, child.wait_with_output()).await {
        Ok(output) => output?,
        Err(_) => bail!("command timed out after {} ms", timeout.as_millis()),
    };
    Ok(RunOutput {
        exit_code: output.status.code().unwrap_or(128),
        stdout: output.stdout,
        stderr: output.stderr,
        runner: runner.to_string(),
    })
}

fn sandbox_cwd(cwd: &str) -> anyhow::Result<String> {
    let cwd = cwd.trim();
    if cwd.is_empty() || cwd == "/" {
        return Ok("/workspace".to_string());
    }

    let mut path = PathBuf::from("/workspace");
    for component in Path::new(cwd.trim_start_matches('/')).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("cwd escapes workspace")
            }
        }
    }
    Ok(path.to_string_lossy().to_string())
}

fn readonly_system_paths() -> Vec<&'static Path> {
    ["/usr/lib", "/usr/lib64", "/lib", "/lib64", "/lib32"]
        .into_iter()
        .map(Path::new)
        .collect()
}

fn command_script(command: &str, exposed_binaries: &[ExposedBinary]) -> String {
    let exposed: HashSet<&str> = exposed_binaries
        .iter()
        .map(|binary| binary.name.as_str())
        .collect();
    let mut script = String::new();
    for builtin in BASH_BUILTINS {
        if *builtin != "enable" && !exposed.contains(*builtin) {
            script.push_str("enable -n ");
            script.push_str(&shell_quote(builtin));
            script.push_str(" 2>/dev/null\n");
        }
    }
    if !exposed.contains("enable") {
        script.push_str("enable -n enable 2>/dev/null\n");
    }
    script.push_str(command);
    script
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

const BASH_BUILTINS: &[&str] = &[
    ".",
    ":",
    "[",
    "alias",
    "bg",
    "bind",
    "break",
    "builtin",
    "caller",
    "cd",
    "command",
    "compgen",
    "complete",
    "compopt",
    "continue",
    "declare",
    "dirs",
    "disown",
    "echo",
    "enable",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "fc",
    "fg",
    "getopts",
    "hash",
    "help",
    "history",
    "jobs",
    "kill",
    "let",
    "local",
    "logout",
    "mapfile",
    "popd",
    "printf",
    "pushd",
    "pwd",
    "read",
    "readarray",
    "readonly",
    "return",
    "set",
    "shift",
    "shopt",
    "source",
    "suspend",
    "test",
    "times",
    "trap",
    "true",
    "type",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unset",
    "wait",
];

fn validate_env_key(key: &str) -> anyhow::Result<()> {
    if key.is_empty()
        || key.contains('=')
        || !key
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        || key.chars().next().is_some_and(|ch| ch.is_ascii_digit())
    {
        bail!("invalid environment variable name `{key}`");
    }
    Ok(())
}

pub fn validate_user_env(env: &HashMap<String, String>) -> anyhow::Result<()> {
    for key in env.keys() {
        validate_env_key(key)?;
        if is_reserved_env_key(key) {
            bail!("environment variable `{key}` is reserved and cannot be set by requests");
        }
    }
    Ok(())
}

fn is_reserved_env_key(key: &str) -> bool {
    matches!(
        key,
        "PATH"
            | "HOME"
            | "PWD"
            | "TMPDIR"
            | "BASH_ENV"
            | "ENV"
            | "SHELLOPTS"
            | "BASHOPTS"
            | "CDPATH"
    )
}

pub fn is_bash_builtin(name: &str) -> bool {
    BASH_BUILTINS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_cwd_rejects_workspace_escape() {
        assert!(sandbox_cwd("../etc").is_err());
        assert!(sandbox_cwd("a/../../etc").is_err());
    }

    #[test]
    fn sandbox_cwd_maps_relative_paths_under_workspace() {
        assert_eq!(sandbox_cwd("").unwrap(), "/workspace");
        assert_eq!(sandbox_cwd("/").unwrap(), "/workspace");
        assert_eq!(sandbox_cwd("src").unwrap(), "/workspace/src");
        assert_eq!(sandbox_cwd("/src/bin").unwrap(), "/workspace/src/bin");
    }

    #[test]
    fn rejects_invalid_env_keys() {
        assert!(validate_env_key("OK_1").is_ok());
        assert!(validate_env_key("1_BAD").is_err());
        assert!(validate_env_key("BAD=1").is_err());
    }

    #[test]
    fn rejects_reserved_user_env_keys() {
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/workspace".to_string());
        let err = validate_user_env(&env).unwrap_err().to_string();
        assert!(err.contains("reserved"));
    }

    #[test]
    fn accepts_regular_user_env_keys() {
        let mut env = HashMap::new();
        env.insert("AGENT_SANDBOX_TEST".to_string(), "ok".to_string());
        validate_user_env(&env).unwrap();
    }

    #[test]
    fn runner_name_is_bubblewrap() {
        assert_eq!(SandboxRunner::new().effective_name(), "bubblewrap");
    }
}
