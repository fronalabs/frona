use std::process::Command;

use crate::error::AppError;

use super::{Sandbox, SandboxConfig};

pub struct LandlockSandbox;

impl Sandbox for LandlockSandbox {
    fn sandboxed_command(
        &self,
        program: &str,
        args: &[&str],
        config: &SandboxConfig,
    ) -> Result<Command, AppError> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;

            let workspace_dir = config.workspace_dir.clone();
            let network_access = config.network_access;
            let additional_read_paths = config.additional_read_paths.clone();

            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd.current_dir(&config.workspace_dir);

            unsafe {
                cmd.pre_exec(move || {
                    apply_landlock(&workspace_dir, network_access, &additional_read_paths)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
                });
            }

            Ok(cmd)
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (program, args, config);
            Err(AppError::Tool(
                "Landlock sandbox is only available on Linux".to_string(),
            ))
        }
    }
}

#[cfg(target_os = "linux")]
fn apply_landlock(workspace_dir: &str, network_access: bool, additional_read_paths: &[String]) -> Result<(), String> {
    use landlock::{
        Access, AccessFs, AccessNet, NetPort, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr, ABI,
    };

    let abi = ABI::V5;

    let mut fs_access = AccessFs::from_all(abi);
    let read_access = AccessFs::from_read(abi);

    let mut ruleset = Ruleset::default()
        .handle_access(fs_access)
        .map_err(|e| format!("Landlock ruleset creation failed: {e}"))?;

    if !network_access {
        ruleset = ruleset
            .handle_access(AccessNet::from_all(abi))
            .map_err(|e| format!("Landlock network access failed: {e}"))?;
    }

    let ruleset = ruleset
        .create()
        .map_err(|e| format!("Landlock ruleset create failed: {e}"))?;

    let read_only_paths = ["/usr", "/lib", "/lib64", "/etc", "/bin", "/sbin"];
    let read_write_paths = [workspace_dir, "/tmp"];

    let mut ruleset = ruleset;

    for path in &read_only_paths {
        if let Ok(fd) = PathFd::new(path) {
            let _ = ruleset.add_rule(PathBeneath::new(fd, read_access));
        }
    }

    for path in additional_read_paths {
        if let Ok(fd) = PathFd::new(path) {
            let _ = ruleset.add_rule(PathBeneath::new(fd, read_access));
        }
    }

    for path in &read_write_paths {
        if let Ok(fd) = PathFd::new(path) {
            let _ = ruleset.add_rule(PathBeneath::new(fd, fs_access));
        }
    }

    ruleset
        .restrict_self()
        .map_err(|e| format!("Landlock restrict_self failed: {e}"))?;

    Ok(())
}
