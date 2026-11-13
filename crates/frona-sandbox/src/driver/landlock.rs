use std::process::Command;

use crate::Error;

use super::{SandboxConfig, SandboxDriver};

pub struct LandlockDriver;

impl SandboxDriver for LandlockDriver {
    fn driver_id(&self) -> &'static str {
        "landlock"
    }

    fn sandboxed_command(
        &self,
        program: &str,
        args: &[&str],
        config: &SandboxConfig,
    ) -> Result<Command, Error> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;

            let workspace_dir = config.workspace_dir.clone();
            let network_access = config.network_access;
            let additional_read_paths = config.additional_read_paths.clone();
            let additional_read_files = config.additional_read_files.clone();
            let additional_write_paths = config.additional_write_paths.clone();

            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd.current_dir(
                config
                    .working_dir
                    .as_deref()
                    .unwrap_or(&config.workspace_dir),
            );

            unsafe {
                cmd.pre_exec(move || {
                    apply_landlock(
                        &workspace_dir,
                        network_access,
                        &additional_read_paths,
                        &additional_read_files,
                        &additional_write_paths,
                    )
                    .map_err(std::io::Error::other)
                });
            }

            Ok(cmd)
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (program, args, config);
            Err(Error::Tool(
                "Landlock sandbox is only available on Linux".to_string(),
            ))
        }
    }
}

#[cfg(target_os = "linux")]
fn apply_landlock(
    workspace_dir: &str,
    network_access: bool,
    additional_read_paths: &[String],
    additional_read_files: &[String],
    additional_write_paths: &[String],
) -> Result<(), String> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr,
    };

    let abi = ABI::V5;

    let fs_access = AccessFs::from_all(abi);
    let read_access = AccessFs::from_read(abi);

    let mut ruleset = Ruleset::default()
        .handle_access(fs_access)
        .map_err(|e| format!("Landlock ruleset creation failed: {e}"))?;

    if !network_access {
        ruleset = ruleset
            .handle_access(AccessNet::from_all(abi))
            .map_err(|e| format!("Landlock network access failed: {e}"))?;
    }

    let mut ruleset = ruleset
        .create()
        .map_err(|e| format!("Landlock ruleset create failed: {e}"))?;

    let read_only_paths: Vec<&str> = super::linux::SYSTEM_READ_DIRS
        .iter()
        .chain(super::linux::PROC_READ_PATHS.iter())
        .copied()
        .collect();
    let read_write_paths: Vec<&str> = std::iter::once(workspace_dir)
        .chain(super::linux::READ_WRITE_DIRS.iter().copied())
        .chain(super::linux::READ_WRITE_DEVICES.iter().copied())
        .collect();

    for path in read_only_paths {
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, read_access))
                .map_err(|e| format!("Landlock add_rule failed for {path}: {e}"))?;
        }
    }

    for path in super::ETC_READ_ALLOWLIST {
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, read_access))
                .map_err(|e| format!("Landlock add_rule failed for {path}: {e}"))?;
        }
    }

    for path in additional_read_paths {
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, read_access))
                .map_err(|e| format!("Landlock add_rule failed for {path}: {e}"))?;
        }
    }

    // File-only grants. `PathBeneath` on a regular file scopes read access
    // to exactly that file — siblings in the parent directory remain hidden.
    for path in additional_read_files {
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, read_access))
                .map_err(|e| format!("Landlock add_rule failed for file {path}: {e}"))?;
        }
    }

    for path in additional_write_paths {
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, fs_access))
                .map_err(|e| format!("Landlock add_rule failed for {path}: {e}"))?;
        }
    }

    for path in &read_write_paths {
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, fs_access))
                .map_err(|e| format!("Landlock add_rule failed for {path}: {e}"))?;
        }
    }

    ruleset
        .restrict_self()
        .map_err(|e| format!("Landlock restrict_self failed: {e}"))?;

    Ok(())
}
