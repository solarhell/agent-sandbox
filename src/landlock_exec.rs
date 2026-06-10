#[cfg(target_os = "linux")]
mod imp {
    use std::{ffi::OsStr, path::PathBuf, process::Command};

    use landlock::{
        AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus,
    };

    pub const HELPER_ARG: &str = "__agent-sandbox-landlock-exec";
    pub const PROBE_ARG: &str = "__agent-sandbox-landlock-probe";
    use std::os::unix::process::CommandExt;

    pub fn maybe_run_helper() -> anyhow::Result<bool> {
        let mut args = std::env::args_os();
        let _program = args.next();
        match args.next().as_deref() {
            Some(arg) if arg == OsStr::new(HELPER_ARG) => {}
            Some(arg) if arg == OsStr::new(PROBE_ARG) => {
                probe_current_process()?;
                return Ok(true);
            }
            _ => return Ok(false),
        }
        let script = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("{HELPER_ARG} requires a command script argument"))?;
        if args.next().is_some() {
            anyhow::bail!("{HELPER_ARG} accepts exactly one command script argument");
        }

        let mut allowed_paths = vec![
            PathBuf::from("/sandbox-runtime/bash"),
            PathBuf::from("/bin"),
        ];
        allowed_paths.extend(
            ["/lib", "/lib64", "/lib32", "/usr/lib", "/usr/lib64"]
                .into_iter()
                .map(PathBuf::from)
                .filter(|path| path.exists()),
        );
        restrict_execute(&allowed_paths)?;
        crate::seccomp_net::install_network_deny_filter()?;
        Err(Command::new("/sandbox-runtime/bash")
            .arg("--noprofile")
            .arg("--norc")
            .arg("-c")
            .arg(script)
            .exec()
            .into())
    }

    pub fn probe() -> anyhow::Result<()> {
        let current_exe = std::env::current_exe()
            .map_err(anyhow::Error::from)
            .and_then(|path| path.canonicalize().map_err(anyhow::Error::from))?;
        let output = Command::new(current_exe).arg(PROBE_ARG).output()?;
        if !output.status.success() {
            anyhow::bail!(
                "Landlock execute allowlist probe failed: status={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    fn probe_current_process() -> anyhow::Result<()> {
        let current_exe = std::env::current_exe()
            .map_err(anyhow::Error::from)
            .and_then(|path| path.canonicalize().map_err(anyhow::Error::from))?;
        restrict_execute(&[current_exe])
    }

    fn restrict_execute(allowed_paths: &[PathBuf]) -> anyhow::Result<()> {
        let mut ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::Execute)?
            .create()?;

        for path in allowed_paths {
            ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(path)?, AccessFs::Execute))?;
        }

        let status = ruleset
            .set_compatibility(CompatLevel::HardRequirement)
            .restrict_self()?;
        if status.ruleset != RulesetStatus::FullyEnforced || !status.no_new_privs {
            anyhow::bail!("Landlock execute allowlist was not fully enforced: {status:?}");
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub const HELPER_ARG: &str = "__agent-sandbox-landlock-exec";
    pub const PROBE_ARG: &str = "__agent-sandbox-landlock-probe";

    pub fn maybe_run_helper() -> anyhow::Result<bool> {
        if let Some(arg) = std::env::args_os().nth(1)
            && (arg == std::ffi::OsStr::new(HELPER_ARG) || arg == std::ffi::OsStr::new(PROBE_ARG))
        {
            anyhow::bail!("Landlock helpers are only supported on Linux");
        }
        Ok(false)
    }

    pub fn probe() -> anyhow::Result<()> {
        anyhow::bail!("Landlock execute allowlist requires Linux");
    }
}

pub use imp::{HELPER_ARG, maybe_run_helper, probe};
