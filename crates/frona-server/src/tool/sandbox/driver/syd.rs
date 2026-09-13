use std::net::{IpAddr, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::core::error::AppError;

use super::{ETC_READ_ALLOWLIST, SandboxConfig, SandboxDriver, linux};

pub fn syd_available() -> bool {
    Command::new("syd")
        .args(["-p", "lib", "-m", "sandbox/write:on", "--", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn parse_resolv_conf(path: &str) -> Vec<IpAddr> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to read {path}: {e}");
            return Vec::new();
        }
    };

    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let ip_str = line.strip_prefix("nameserver")?;
            ip_str.trim().parse::<IpAddr>().ok()
        })
        .collect()
}

fn resolve_destination(dest: &str, cache: &moka::sync::Cache<String, Vec<String>>) -> Vec<String> {
    if dest.contains('/') || dest.contains('!') {
        return vec![format!("allow/net/connect+{dest}")];
    }

    if let Ok(ip) = dest.parse::<IpAddr>() {
        let cidr = if ip.is_ipv4() { 32 } else { 128 };
        return vec![format!("allow/net/connect+{ip}/{cidr}!0-65535")];
    }

    cache.get_with(dest.to_string(), || resolve_to_rules(dest))
}

fn resolve_to_rules(dest: &str) -> Vec<String> {
    let with_port = if dest.contains(':') && !dest.starts_with('[') {
        dest.to_string()
    } else if dest.starts_with('[') {
        if dest.contains("]:") {
            dest.to_string()
        } else {
            format!("{dest}:0")
        }
    } else {
        format!("{dest}:0")
    };

    match with_port.to_socket_addrs() {
        Ok(addrs) => {
            let rules: Vec<_> = addrs
                .map(|addr| {
                    let ip = addr.ip();
                    let cidr = if ip.is_ipv4() { 32 } else { 128 };
                    let port = if addr.port() == 0 {
                        "0-65535".to_string()
                    } else {
                        addr.port().to_string()
                    };
                    format!("allow/net/connect+{ip}/{cidr}!{port}")
                })
                .collect();
            tracing::debug!("Resolved '{dest}' to {} address(es)", rules.len());
            rules
        }
        Err(e) => {
            tracing::warn!("Failed to resolve '{dest}': {e}");
            Vec::new()
        }
    }
}

pub struct SydDriver {
    dns_servers: Vec<IpAddr>,
    dest_cache: moka::sync::Cache<String, Vec<String>>,
}

impl Default for SydDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl SydDriver {
    pub fn new() -> Self {
        Self::with_resolv_conf("/etc/resolv.conf")
    }

    pub fn with_resolv_conf(path: &str) -> Self {
        let dns_servers = parse_resolv_conf(path);
        if dns_servers.is_empty() {
            tracing::warn!("No nameservers found in {path}");
        }
        Self {
            dns_servers,
            dest_cache: moka::sync::Cache::builder()
                .time_to_live(Duration::from_secs(300))
                .build(),
        }
    }
}

impl SandboxDriver for SydDriver {
    fn driver_id(&self) -> &'static str {
        "syd"
    }

    fn sandboxed_command(
        &self,
        program: &str,
        args: &[&str],
        config: &SandboxConfig,
    ) -> Result<Command, AppError> {
        let syd_args = SydArgsBuilder::new()
            .filesystem_rules(config)
            .network_rules(config, &self.dns_servers, &self.dest_cache)
            .build();

        let mut cmd = Command::new("syd");
        cmd.args(&syd_args);
        cmd.arg("--");
        cmd.arg(program);
        cmd.args(args);
        cmd.current_dir(
            config
                .working_dir
                .as_deref()
                .unwrap_or(&config.workspace_dir),
        );

        Ok(cmd)
    }
}

struct SydArgsBuilder {
    args: Vec<String>,
}

impl SydArgsBuilder {
    fn new() -> Self {
        Self {
            args: vec![
                "-p".into(),
                "lib".into(),
                // Relax W^X enforcement so V8 (Node.js) can JIT-compile
                "-p".into(),
                "nomem".into(),
                "-m".into(),
                "sandbox/read:on".into(),
                "-m".into(),
                "sandbox/stat:on".into(),
                "-m".into(),
                "sandbox/write:on".into(),
                // Allow non-PIE executables (e.g. Node.js)
                "-m".into(),
                "trace/allow_unsafe_exec_nopie:1".into(),
                // Allow ld.so exec indirection (required by Python/uv)
                "-m".into(),
                "trace/allow_unsafe_exec_ldso:1".into(),
                // Syd's lib profile strips env vars whose names contain PASSWORD,
                // CREDENTIAL, TOKEN, or KEY. We manage secrets ourselves via
                // resolved vault credentials, so disable the filter.
                "-m".into(),
                "trace/allow_unsafe_env:1".into(),
                // uv uses base64-encoded temp filenames in its cache
                "-m".into(),
                "trace/allow_unsafe_filename:1".into(),
            ],
        }
    }

    fn allow_read(&mut self, path: &str) {
        self.args.push("-m".into());
        self.args.push(format!("allow/read+{path}"));
        self.args.push("-m".into());
        self.args.push(format!("allow/stat+{path}"));
    }

    fn allow_write(&mut self, path: &str) {
        self.args.push("-m".into());
        self.args.push(format!("allow/write+{path}"));
    }

    fn allow_read_write(&mut self, path: &str) {
        self.allow_read(path);
        self.allow_write(path);
    }

    fn filesystem_rules(mut self, config: &SandboxConfig) -> Self {
        for path in linux::SYSTEM_READ_DIRS {
            self.allow_read(&format!("{path}/***"));
        }
        for path in linux::PROC_READ_PATHS {
            self.allow_read(&format!("{path}/***"));
        }
        for path in ETC_READ_ALLOWLIST {
            self.allow_read(&format!("{path}/***"));
        }
        for path in &config.additional_read_paths {
            self.allow_read(&format!("{path}/***"));
        }
        for path in &config.additional_read_files {
            self.allow_read(path);
        }

        // Allow read+stat on each ancestor of the workspace dir so tools
        // (e.g. Node.js realpathSync) can traverse the directory tree.
        // Syd hides non-allowed siblings, so this doesn't leak other workspaces.
        {
            let mut ancestor = std::path::Path::new(&config.workspace_dir);
            while let Some(parent) = ancestor.parent() {
                if parent == std::path::Path::new("/") {
                    break;
                }
                self.allow_read(parent.to_str().unwrap_or_default());
                ancestor = parent;
            }
        }
        self.allow_read_write(&format!("{}/***", config.workspace_dir));

        for path in linux::READ_WRITE_DIRS {
            self.allow_read_write(&format!("{path}/***"));
        }
        for path in linux::READ_WRITE_DEVICES {
            self.allow_read_write(path);
        }
        for path in &config.additional_write_paths {
            self.allow_read_write(&format!("{path}/***"));
        }

        for path in &config.denied_paths {
            self.args.push("-m".into());
            self.args.push(format!("mask+{path}/***"));
        }

        self
    }

    fn network_rules(
        mut self,
        config: &SandboxConfig,
        dns_servers: &[IpAddr],
        dest_cache: &moka::sync::Cache<String, Vec<String>>,
    ) -> Self {
        if !config.network_access {
            self.args.push("-m".into());
            self.args.push("sandbox/net:on".into());
        } else if !config.allowed_network_destinations.is_empty() {
            self.args.push("-m".into());
            self.args.push("sandbox/net:on".into());

            // Allow ephemeral port binding (port 0) for outbound connections.
            self.args.push("-m".into());
            self.args.push("allow/net/bind+0.0.0.0/0!0".into());
            self.args.push("-m".into());
            self.args.push("allow/net/bind+::/0!0".into());
            // Allow unnamed unix socketpairs — glibc's DNS resolver opens one
            // to talk to nscd / systemd-resolved before reaching the network.
            // Without this, getaddrinfo() fails with "thread failed to start"
            // and even allowed destinations can't be resolved.
            self.args.push("-m".into());
            self.args.push("allow/net/bind+!unnamed".into());

            for ip in dns_servers {
                let cidr = if ip.is_ipv4() { 32 } else { 128 };
                self.args.push("-m".into());
                self.args.push(format!("allow/net/connect+{ip}/{cidr}!53"));
            }

            for dest in &config.allowed_network_destinations {
                for rule in resolve_destination(dest, dest_cache) {
                    self.args.push("-m".into());
                    self.args.push(rule);
                }
            }

            for port in &config.allowed_bind_ports {
                self.args.push("-m".into());
                self.args.push(format!("allow/net/bind+0.0.0.0/0!{port}"));
                self.args.push("-m".into());
                self.args.push(format!("allow/net/bind+::/0!{port}"));
            }
        }

        for dest in &config.blocked_networks {
            for resolved in resolve_destination(dest, dest_cache) {
                let denied = resolved.replacen("allow/", "deny/", 1);
                self.args.push("-m".into());
                self.args.push(denied);
            }
        }

        self
    }

    fn build(self) -> Vec<String> {
        self.args
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn test_config() -> SandboxConfig {
        SandboxConfig {
            workspace_dir: "/workspace/agent_1".into(),
            ..Default::default()
        }
    }

    fn test_driver() -> SydDriver {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"nameserver 8.8.8.8\nnameserver 8.8.4.4\n")
            .unwrap();
        file.flush().unwrap();
        SydDriver::with_resolv_conf(file.path().to_str().unwrap())
    }

    #[test]
    fn test_builder_starts_with_lib_profile() {
        let args = SydArgsBuilder::new().build();
        assert_eq!(args[0], "-p");
        assert_eq!(args[1], "lib");
    }

    #[test]
    fn test_builder_enables_sandboxes() {
        let args = SydArgsBuilder::new().build();
        assert!(args.contains(&"sandbox/read:on".to_string()));
        assert!(args.contains(&"sandbox/stat:on".to_string()));
        assert!(args.contains(&"sandbox/write:on".to_string()));
    }

    #[test]
    fn test_filesystem_includes_system_paths() {
        let args = SydArgsBuilder::new()
            .filesystem_rules(&test_config())
            .build();
        assert!(args.contains(&"allow/read+/usr/***".to_string()));
        assert!(args.contains(&"allow/stat+/usr/***".to_string()));
        assert!(args.contains(&"allow/read+/bin/***".to_string()));
        assert!(args.contains(&"allow/read+/lib/***".to_string()));
        assert!(args.contains(&"allow/read+/sbin/***".to_string()));
    }

    #[test]
    fn test_filesystem_includes_proc() {
        let args = SydArgsBuilder::new()
            .filesystem_rules(&test_config())
            .build();
        assert!(args.contains(&"allow/read+/proc/self/***".to_string()));
        assert!(args.contains(&"allow/read+/proc/cpuinfo/***".to_string()));
        assert!(args.contains(&"allow/read+/proc/meminfo/***".to_string()));
    }

    #[test]
    fn test_filesystem_includes_etc_allowlist() {
        let args = SydArgsBuilder::new()
            .filesystem_rules(&test_config())
            .build();
        assert!(args.contains(&"allow/read+/etc/resolv.conf/***".to_string()));
        assert!(args.contains(&"allow/read+/etc/ssl/***".to_string()));
        assert!(args.contains(&"allow/read+/etc/hosts/***".to_string()));
    }

    #[test]
    fn test_filesystem_workspace_read_and_write() {
        let args = SydArgsBuilder::new()
            .filesystem_rules(&test_config())
            .build();
        assert!(args.contains(&"allow/read+/workspace/agent_1/***".to_string()));
        assert!(args.contains(&"allow/write+/workspace/agent_1/***".to_string()));
    }

    #[test]
    fn test_filesystem_writable_system_paths() {
        let args = SydArgsBuilder::new()
            .filesystem_rules(&test_config())
            .build();
        assert!(args.contains(&"allow/write+/tmp/***".to_string()));
        assert!(args.contains(&"allow/write+/dev/null".to_string()));
        assert!(args.contains(&"allow/write+/dev/urandom".to_string()));
    }

    #[test]
    fn test_filesystem_additional_paths() {
        let mut config = test_config();
        config.additional_read_paths = vec!["/data/shared".into()];
        config.additional_write_paths = vec!["/output".into()];

        let args = SydArgsBuilder::new().filesystem_rules(&config).build();
        assert!(args.contains(&"allow/read+/data/shared/***".to_string()));
        assert!(args.contains(&"allow/write+/output/***".to_string()));
    }

    #[test]
    fn test_network_off_blocks_all() {
        let driver = test_driver();
        let mut config = test_config();
        config.network_access = false;

        let args = SydArgsBuilder::new()
            .network_rules(&config, &driver.dns_servers, &driver.dest_cache)
            .build();
        assert!(args.contains(&"sandbox/net:on".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("allow/net")));
    }

    #[test]
    fn test_network_on_no_destinations_allows_all() {
        let driver = test_driver();
        let mut config = test_config();
        config.network_access = true;
        config.allowed_network_destinations = vec![];

        let args = SydArgsBuilder::new()
            .network_rules(&config, &driver.dns_servers, &driver.dest_cache)
            .build();
        assert!(!args.contains(&"sandbox/net:on".to_string()));
    }

    #[test]
    fn test_network_on_with_ip_destinations() {
        let driver = test_driver();
        let mut config = test_config();
        config.network_access = true;
        config.allowed_network_destinations = vec!["1.2.3.4".into(), "5.6.7.8".into()];

        let args = SydArgsBuilder::new()
            .network_rules(&config, &driver.dns_servers, &driver.dest_cache)
            .build();
        assert!(args.contains(&"sandbox/net:on".to_string()));
        assert!(args.contains(&"allow/net/connect+1.2.3.4/32!0-65535".to_string()));
        assert!(args.contains(&"allow/net/connect+5.6.7.8/32!0-65535".to_string()));
        // Ephemeral bind always allowed when destinations are set
        assert!(args.contains(&"allow/net/bind+0.0.0.0/0!0".to_string()));
        assert!(args.contains(&"allow/net/bind+::/0!0".to_string()));
        // Unnamed unix socketpair (used by glibc DNS resolver IPC).
        assert!(args.contains(&"allow/net/bind+!unnamed".to_string()));
    }

    #[test]
    fn test_network_destinations_allow_dns_from_resolv_conf() {
        let driver = test_driver();
        let mut config = test_config();
        config.network_access = true;
        config.allowed_network_destinations = vec!["1.2.3.4".into()];

        let args = SydArgsBuilder::new()
            .network_rules(&config, &driver.dns_servers, &driver.dest_cache)
            .build();
        assert!(args.contains(&"allow/net/connect+8.8.8.8/32!53".to_string()));
        assert!(args.contains(&"allow/net/connect+8.8.4.4/32!53".to_string()));
        assert!(!args.contains(&"allow/net/connect+0.0.0.0/0!53".to_string()));
    }

    #[test]
    fn test_network_destination_with_cidr() {
        let driver = test_driver();
        let mut config = test_config();
        config.network_access = true;
        config.allowed_network_destinations = vec!["10.0.0.0/8!443".into()];

        let args = SydArgsBuilder::new()
            .network_rules(&config, &driver.dns_servers, &driver.dest_cache)
            .build();
        assert!(args.contains(&"allow/net/connect+10.0.0.0/8!443".to_string()));
    }

    #[test]
    fn test_parse_resolv_conf() {
        let dir = std::env::temp_dir().join("frona_test_resolv");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("resolv.conf");
        std::fs::write(
            &path,
            "# comment\nnameserver 8.8.8.8\nnameserver 8.8.4.4\nnameserver 2001:4860:4860::8888\n",
        )
        .unwrap();

        let servers = parse_resolv_conf(path.to_str().unwrap());
        assert_eq!(servers.len(), 3);
        assert_eq!(servers[0], "8.8.8.8".parse::<IpAddr>().unwrap());
        assert_eq!(servers[1], "8.8.4.4".parse::<IpAddr>().unwrap());
        assert_eq!(
            servers[2],
            "2001:4860:4860::8888".parse::<IpAddr>().unwrap()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_resolv_conf_ignores_invalid() {
        let dir = std::env::temp_dir().join("frona_test_resolv_invalid");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("resolv.conf");
        std::fs::write(
            &path,
            "search example.com\nnameserver 1.1.1.1\noptions ndots:5\nnameserver notanip\n",
        )
        .unwrap();

        let servers = parse_resolv_conf(path.to_str().unwrap());
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0], "1.1.1.1".parse::<IpAddr>().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_destination_plain_ip() {
        let d = test_driver();
        assert_eq!(
            resolve_destination("1.2.3.4", &d.dest_cache),
            vec!["allow/net/connect+1.2.3.4/32!0-65535"]
        );
    }

    #[test]
    fn test_resolve_destination_ipv6() {
        let d = test_driver();
        assert_eq!(
            resolve_destination("::1", &d.dest_cache),
            vec!["allow/net/connect+::1/128!0-65535"]
        );
    }

    #[test]
    fn test_resolve_destination_cidr_passthrough() {
        let d = test_driver();
        assert_eq!(
            resolve_destination("10.0.0.0/8!443", &d.dest_cache),
            vec!["allow/net/connect+10.0.0.0/8!443"]
        );
    }

    #[test]
    fn test_resolve_destination_hostname() {
        let d = test_driver();
        let rules = resolve_destination("localhost", &d.dest_cache);
        assert!(!rules.is_empty(), "localhost should resolve");
        assert!(rules.iter().all(|r| r.starts_with("allow/net/connect+")));
        assert!(rules.iter().all(|r| r.ends_with("!0-65535")));
    }

    #[test]
    fn test_resolve_destination_hostname_with_port() {
        let d = test_driver();
        let rules = resolve_destination("localhost:443", &d.dest_cache);
        assert!(!rules.is_empty(), "localhost:443 should resolve");
        assert!(rules.iter().all(|r| r.ends_with("!443")));
    }

    #[test]
    fn test_resolve_destination_bracketed_ipv6_with_port() {
        let d = test_driver();
        assert_eq!(
            resolve_destination("[::1]:443", &d.dest_cache),
            vec!["allow/net/connect+::1/128!443"]
        );
    }

    #[test]
    fn test_network_bind_ports() {
        let driver = test_driver();
        let mut config = test_config();
        config.network_access = true;
        config.allowed_network_destinations = vec!["127.0.0.1:8080".into()];
        config.allowed_bind_ports = vec![8080];

        let args = SydArgsBuilder::new()
            .network_rules(&config, &driver.dns_servers, &driver.dest_cache)
            .build();
        assert!(args.contains(&"allow/net/bind+0.0.0.0/0!8080".to_string()));
        assert!(args.contains(&"allow/net/bind+::/0!8080".to_string()));
    }

    #[test]
    fn test_network_bind_ports_not_added_when_no_destinations() {
        let driver = test_driver();
        let mut config = test_config();
        config.network_access = true;
        config.allowed_network_destinations = vec![];
        config.allowed_bind_ports = vec![8080];

        let args = SydArgsBuilder::new()
            .network_rules(&config, &driver.dns_servers, &driver.dest_cache)
            .build();
        // No sandbox/net:on means all network ops allowed, no bind rule needed
        assert!(!args.iter().any(|a| a.starts_with("allow/net/bind")));
    }

    #[test]
    fn test_syd_available_consistent() {
        let a = syd_available();
        let b = syd_available();
        assert_eq!(a, b);
    }
}
