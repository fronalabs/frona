use std::process::Command;

use crate::error::AppError;

use super::{Sandbox, SandboxConfig};

enum ReadRule {
    Subpath(&'static str),
    Literal(&'static str),
}

const SYSTEM_READ_RULES: &[ReadRule] = &[
    ReadRule::Subpath("/bin"),
    ReadRule::Subpath("/usr"),
    ReadRule::Subpath("/sbin"),
    ReadRule::Subpath("/etc"),
    ReadRule::Subpath("/System"),
    ReadRule::Subpath("/Library"),
    ReadRule::Subpath("/opt"),
    ReadRule::Subpath("/private/etc"),
    ReadRule::Subpath("/private/tmp"),
    ReadRule::Subpath("/private/var/db"),
    ReadRule::Subpath("/private/var/select"),
    ReadRule::Subpath("/private/var/folders"),
    ReadRule::Subpath("/dev"),
    ReadRule::Subpath("/tmp"),
    ReadRule::Literal("/var"),
    ReadRule::Literal("/var/select"),
    ReadRule::Literal("/var/select/developer_dir"),
    ReadRule::Literal("/var/folders"),
];

pub struct SandboxProfileBuilder {
    read_literals: Vec<String>,
    read_subpaths: Vec<String>,
    write_subpaths: Vec<String>,
    network_rules: String,
}

impl SandboxProfileBuilder {
    pub fn new() -> Self {
        Self {
            read_literals: vec!["/".to_string()],
            read_subpaths: Vec::new(),
            write_subpaths: Vec::new(),
            network_rules: "(deny network*)".to_string(),
        }
    }

    pub fn read_literal(mut self, path: &str) -> Self {
        self.read_literals.push(path.to_string());
        self
    }

    pub fn read_subpath(mut self, path: &str) -> Self {
        self.read_subpaths.push(path.to_string());
        self
    }

    pub fn write_subpath(mut self, path: &str) -> Self {
        self.write_subpaths.push(path.to_string());
        self
    }

    pub fn system_reads(mut self) -> Self {
        for rule in SYSTEM_READ_RULES {
            match rule {
                ReadRule::Subpath(p) => self.read_subpaths.push(p.to_string()),
                ReadRule::Literal(p) => self.read_literals.push(p.to_string()),
            }
        }
        self
    }

    pub fn workspace(self, path: &str) -> Self {
        let canonical = std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string());

        self.read_subpath(&canonical).write_subpath(&canonical)
    }

    pub fn network(mut self, access: bool, destinations: &[String]) -> Self {
        if !access {
            self.network_rules = "(deny network*)".to_string();
        } else if destinations.is_empty() {
            self.network_rules = "(allow network*)".to_string();
        } else {
            let mut rules = String::from("(deny network*)\n");
            for dest in destinations {
                rules.push_str(&format!("(allow network* (remote ip \"{dest}:*\"))\n"));
            }
            self.network_rules = rules;
        }
        self
    }

    pub fn build(&self) -> String {
        let mut profile = String::from("(version 1)\n(deny default)\n\n");

        profile.push_str("(allow process*)\n");
        profile.push_str("(allow signal)\n");
        profile.push_str("(allow sysctl*)\n");
        profile.push_str("(allow mach*)\n");
        profile.push_str("(allow ipc*)\n\n");

        if !self.read_literals.is_empty() || !self.read_subpaths.is_empty() {
            profile.push_str("(allow file-read*\n");
            for lit in &self.read_literals {
                profile.push_str(&format!("    (literal \"{lit}\")\n"));
            }
            for sub in &self.read_subpaths {
                profile.push_str(&format!("    (subpath \"{sub}\")\n"));
            }
            profile.push_str(")\n\n");
        }

        if !self.write_subpaths.is_empty() {
            profile.push_str("(allow file-write*\n");
            for sub in &self.write_subpaths {
                profile.push_str(&format!("    (subpath \"{sub}\")\n"));
            }
            profile.push_str(")\n\n");
        }

        profile.push_str(&self.network_rules);
        profile.push('\n');

        profile
    }
}

pub struct MacOsSandbox;

impl Sandbox for MacOsSandbox {
    fn sandboxed_command(
        &self,
        program: &str,
        args: &[&str],
        config: &SandboxConfig,
    ) -> Result<Command, AppError> {
        let mut builder = SandboxProfileBuilder::new()
            .system_reads()
            .workspace(&config.workspace_dir)
            .write_subpath("/dev")
            .network(config.network_access, &config.allowed_network_destinations);

        for path in &config.additional_read_paths {
            builder = builder.read_subpath(path);
        }

        let profile = builder.build();

        let mut cmd = Command::new("sandbox-exec");
        cmd.arg("-p");
        cmd.arg(&profile);
        cmd.arg(program);
        cmd.args(args);
        cmd.current_dir(&config.workspace_dir);

        Ok(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_defaults() {
        let profile = SandboxProfileBuilder::new().build();
        assert!(profile.contains("(version 1)"));
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(allow process*)"));
        assert!(profile.contains("(literal \"/\")"));
        assert!(profile.contains("(deny network*)"));
    }

    #[test]
    fn test_builder_system_reads() {
        let profile = SandboxProfileBuilder::new().system_reads().build();
        assert!(profile.contains("(subpath \"/bin\")"));
        assert!(profile.contains("(subpath \"/usr\")"));
        assert!(profile.contains("(subpath \"/etc\")"));
        assert!(profile.contains("(subpath \"/System\")"));
        assert!(profile.contains("(literal \"/var\")"));
        assert!(profile.contains("(literal \"/var/select/developer_dir\")"));
    }

    #[test]
    fn test_builder_workspace() {
        let profile = SandboxProfileBuilder::new()
            .workspace("/tmp/test_ws")
            .build();
        let canonical = std::fs::canonicalize("/tmp/test_ws")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/tmp/test_ws".to_string());
        assert!(profile.contains(&format!("(subpath \"{canonical}\")")));
    }

    #[test]
    fn test_builder_network_deny() {
        let profile = SandboxProfileBuilder::new()
            .network(false, &[])
            .build();
        assert!(profile.contains("(deny network*)"));
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn test_builder_network_allow_all() {
        let profile = SandboxProfileBuilder::new()
            .network(true, &[])
            .build();
        assert!(profile.contains("(allow network*)"));
    }

    #[test]
    fn test_builder_network_ip_filter() {
        let profile = SandboxProfileBuilder::new()
            .network(true, &["1.2.3.4".to_string(), "5.6.7.8".to_string()])
            .build();
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains("(allow network* (remote ip \"1.2.3.4:*\"))"));
        assert!(profile.contains("(allow network* (remote ip \"5.6.7.8:*\"))"));
    }

    #[test]
    fn test_builder_full_profile() {
        let profile = SandboxProfileBuilder::new()
            .system_reads()
            .read_literal("/custom/literal")
            .read_subpath("/custom/read")
            .write_subpath("/custom/write")
            .network(false, &[])
            .build();

        assert!(profile.contains("(literal \"/custom/literal\")"));
        assert!(profile.contains("(subpath \"/custom/read\")"));
        assert!(profile.contains("(subpath \"/custom/write\")"));
        assert!(profile.contains("(subpath \"/bin\")"));
    }
}
