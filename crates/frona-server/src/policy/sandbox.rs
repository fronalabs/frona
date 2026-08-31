use cedar_policy::{Entities, Entity, PolicySet};

use crate::core::error::AppError;
use crate::storage::StorageService;
use crate::storage::path::{Namespace, VirtualPath};
use crate::tool::sandbox::driver::SandboxConfig;

use super::schema::{action_entity_uid, entity_type_name};

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SandboxPolicy {
    #[serde(default)]
    pub read_paths: Vec<String>,
    #[serde(default)]
    pub write_paths: Vec<String>,
    #[serde(default)]
    pub network_access: bool,
    #[serde(default)]
    pub network_destinations: Vec<String>,
    #[serde(default)]
    pub bind_ports: Vec<u16>,
    #[serde(default)]
    pub denied_paths: Vec<String>,
    #[serde(default)]
    pub blocked_networks: Vec<String>,
}

impl SandboxPolicy {
    pub fn apply(&self, config: &mut SandboxConfig) {
        config
            .additional_read_paths
            .extend(self.read_paths.iter().cloned());
        config
            .additional_write_paths
            .extend(self.write_paths.iter().cloned());
        if self.network_access {
            config.network_access = true;
            config
                .allowed_network_destinations
                .extend(self.network_destinations.iter().cloned());
        }
        config.allowed_bind_ports.extend(self.bind_ports.iter());
        config
            .denied_paths
            .extend(self.denied_paths.iter().cloned());
        config
            .blocked_networks
            .extend(self.blocked_networks.iter().cloned());
    }

    pub fn permissive() -> Self {
        Self {
            network_access: true,
            ..Self::default()
        }
    }

    /// Validates each entry of `read_paths`, `write_paths`, and `denied_paths`
    /// against ownership rules, before reconciling into Cedar policies. An
    /// entry must be either:
    ///
    /// - An absolute filesystem path (`/...`).
    /// - A `user://{handle}/...` URI where `handle == requester_handle`.
    /// - An `agent://{id}/...` URI where `is_owned_agent(id) == true`.
    pub fn validate_paths(
        &self,
        requester_handle: &crate::core::Handle,
        is_owned_agent: impl Fn(&str) -> bool,
    ) -> Result<(), AppError> {
        for list in [&self.read_paths, &self.write_paths, &self.denied_paths] {
            for raw in list {
                validate_path_entry(raw, requester_handle.as_ref(), &is_owned_agent)?;
            }
        }
        Ok(())
    }

    /// Translates `user://...` and `agent://...` entries in `read_paths`,
    /// `write_paths`, and `denied_paths` into absolute host paths via
    /// `StorageService`. Absolute paths pass through unchanged. Entries that
    /// fail to parse or resolve are dropped with a warning.
    pub fn resolve_virtual_paths(
        &mut self,
        storage: &StorageService,
        user_handle: &crate::core::Handle,
    ) {
        for list in [
            &mut self.read_paths,
            &mut self.write_paths,
            &mut self.denied_paths,
        ] {
            *list = list
                .iter()
                .filter_map(|raw| match storage.resolve_for_user(user_handle, raw) {
                    Ok(p) => Some(p.to_string_lossy().into_owned()),
                    Err(_) => {
                        tracing::warn!(path = %raw, "dropping unresolvable sandbox path entry");
                        None
                    }
                })
                .collect();
        }
    }
}

fn validate_path_entry(
    raw: &str,
    requester_username: &str,
    is_owned_agent: &impl Fn(&str) -> bool,
) -> Result<(), AppError> {
    if raw.starts_with('/') {
        return Ok(());
    }
    if raw.starts_with("user://") || raw.starts_with("agent://") {
        let parsed = VirtualPath::parse(raw)
            .map_err(|e| AppError::Validation(format!("invalid sandbox path '{raw}': {e}")))?;
        match &parsed.namespace {
            Namespace::User(name) => {
                if name != requester_username {
                    return Err(AppError::Validation(format!(
                        "sandbox path '{raw}' references files owned by another user"
                    )));
                }
            }
            Namespace::Agent(id) => {
                if !is_owned_agent(id) {
                    return Err(AppError::Validation(format!(
                        "sandbox path '{raw}' references an agent you don't own"
                    )));
                }
            }
        }
        return Ok(());
    }
    Err(AppError::Validation(format!(
        "sandbox path '{raw}' must be absolute (start with /) or a user:// / agent:// URI"
    )))
}

const SANDBOX_ACTIONS: &[(&str, RuleKind)] = &[
    ("read", RuleKind::Read),
    ("write", RuleKind::Write),
    ("connect", RuleKind::Connect),
    ("bind", RuleKind::Bind),
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuleEffect {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuleKind {
    Read,
    Write,
    Connect,
    Bind,
}

#[derive(Debug, Clone)]
struct Rule {
    effect: RuleEffect,
    kind: RuleKind,
    target: String,
}

pub fn evaluate_sandbox_policy(policy_set: &PolicySet, principal: Entity) -> SandboxPolicy {
    let principal_uid = principal.uid();

    let base_entities =
        Entities::from_entities([principal], None).unwrap_or_else(|_| Entities::empty());

    let authorizer = cedar_policy::Authorizer::new();
    let mut rules = Vec::new();

    for (action_name, kind) in SANDBOX_ACTIONS {
        let action_uid = action_entity_uid(action_name);

        let resource_type = match kind {
            RuleKind::Read | RuleKind::Write => entity_type_name("Path"),
            RuleKind::Connect | RuleKind::Bind => entity_type_name("NetworkDestination"),
        };

        let request = cedar_policy::Request::builder()
            .principal(principal_uid.clone())
            .action(action_uid)
            .unknown_resource_with_type(resource_type)
            .build();

        let partial = authorizer.is_authorized_partial(&request, policy_set, &base_entities);

        for residual in partial.nontrivial_residuals() {
            let effect = match residual.effect() {
                cedar_policy::Effect::Permit => RuleEffect::Allow,
                cedar_policy::Effect::Forbid => RuleEffect::Deny,
            };

            if let cedar_policy::ResourceConstraint::Eq(ref uid) = residual.resource_constraint() {
                rules.push(Rule {
                    effect,
                    kind: kind.clone(),
                    target: uid.id().unescaped().to_string(),
                });
                continue;
            }

            let Ok(json) = residual.to_json() else {
                continue;
            };
            for entity in extract_resource_entities_from_residual(&json) {
                let effective = if entity.negated {
                    match &effect {
                        RuleEffect::Allow => RuleEffect::Deny,
                        RuleEffect::Deny => RuleEffect::Allow,
                    }
                } else {
                    effect.clone()
                };
                rules.push(Rule {
                    effect: effective,
                    kind: kind.clone(),
                    target: entity.id,
                });
            }
        }

        for satisfied in partial.definitely_satisfied() {
            let effect = match satisfied.effect() {
                cedar_policy::Effect::Permit => RuleEffect::Allow,
                cedar_policy::Effect::Forbid => RuleEffect::Deny,
            };

            let target = match satisfied.resource_constraint() {
                cedar_policy::ResourceConstraint::Eq(ref uid) => uid.id().unescaped().to_string(),
                _ => String::new(),
            };
            rules.push(Rule {
                effect,
                kind: kind.clone(),
                target,
            });
        }
    }

    rules_to_policy(&rules)
}

fn rules_to_policy(rules: &[Rule]) -> SandboxPolicy {
    let mut policy = SandboxPolicy::default();

    let has_connect_allow = rules
        .iter()
        .any(|r| r.kind == RuleKind::Connect && r.effect == RuleEffect::Allow);
    policy.network_access = has_connect_allow;

    for rule in rules {
        if rule.target.is_empty() {
            continue;
        }
        match (&rule.effect, &rule.kind) {
            (RuleEffect::Allow, RuleKind::Read) => policy.read_paths.push(rule.target.clone()),
            (RuleEffect::Allow, RuleKind::Write) => policy.write_paths.push(rule.target.clone()),
            (RuleEffect::Allow, RuleKind::Connect) => {
                policy.network_destinations.push(rule.target.clone())
            }
            (RuleEffect::Allow, RuleKind::Bind) => {
                if let Ok(port) = rule.target.parse::<u16>() {
                    policy.bind_ports.push(port);
                }
            }
            (RuleEffect::Deny, RuleKind::Read | RuleKind::Write) => {
                policy.denied_paths.push(rule.target.clone())
            }
            (RuleEffect::Deny, RuleKind::Connect) => {
                policy.blocked_networks.push(rule.target.clone())
            }
            (RuleEffect::Deny, RuleKind::Bind) => {}
        }
    }

    policy
}

#[derive(Debug)]
enum CedarExpr {
    Bool(bool),
    Entity { id: String },
    Unknown(String),
    Eq(Box<CedarExpr>, Box<CedarExpr>),
    Not(Box<CedarExpr>),
    And(Box<CedarExpr>, Box<CedarExpr>),
    Or(Box<CedarExpr>, Box<CedarExpr>),
    Other,
}

impl CedarExpr {
    fn parse(json: &serde_json::Value) -> Self {
        if let Some(val) = json.get("Value") {
            if let Some(b) = val.as_bool() {
                return Self::Bool(b);
            }
            if let Some(id) = val
                .get("__entity")
                .and_then(|e| e.get("id"))
                .and_then(|i| i.as_str())
            {
                return Self::Entity { id: id.to_string() };
            }
            return Self::Other;
        }
        if let Some(arr) = json.get("unknown").and_then(|u| u.as_array()) {
            let name = arr
                .first()
                .and_then(|v| v.get("Value"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            return Self::Unknown(name.to_string());
        }
        if let Some(eq) = json.get("==") {
            return Self::Eq(
                Box::new(Self::parse(eq.get("left").unwrap_or(json))),
                Box::new(Self::parse(eq.get("right").unwrap_or(json))),
            );
        }
        if let Some(not) = json.get("!") {
            return Self::Not(Box::new(Self::parse(not.get("arg").unwrap_or(json))));
        }
        if let Some(and) = json.get("&&") {
            return Self::And(
                Box::new(Self::parse(and.get("left").unwrap_or(json))),
                Box::new(Self::parse(and.get("right").unwrap_or(json))),
            );
        }
        if let Some(or) = json.get("||") {
            return Self::Or(
                Box::new(Self::parse(or.get("left").unwrap_or(json))),
                Box::new(Self::parse(or.get("right").unwrap_or(json))),
            );
        }
        Self::Other
    }

    fn is_unsatisfiable(&self) -> bool {
        match self {
            Self::Bool(false) => true,
            Self::And(left, right) => left.is_unsatisfiable() || right.is_unsatisfiable(),
            _ => false,
        }
    }
}

struct ResidualEntity {
    id: String,
    negated: bool,
}

fn collect_resource_entities(expr: &CedarExpr, negated: bool, out: &mut Vec<ResidualEntity>) {
    match expr {
        CedarExpr::Eq(left, right) => match (left.as_ref(), right.as_ref()) {
            (CedarExpr::Unknown(name), CedarExpr::Entity { id })
            | (CedarExpr::Entity { id }, CedarExpr::Unknown(name))
                if name == "resource" =>
            {
                out.push(ResidualEntity {
                    id: id.clone(),
                    negated,
                });
            }
            _ => {}
        },
        CedarExpr::Not(inner) => collect_resource_entities(inner, !negated, out),
        CedarExpr::And(left, right) | CedarExpr::Or(left, right) => {
            collect_resource_entities(left, negated, out);
            collect_resource_entities(right, negated, out);
        }
        _ => {}
    }
}

fn extract_resource_entities_from_residual(json: &serde_json::Value) -> Vec<ResidualEntity> {
    let mut entities = Vec::new();
    let Some(conditions) = json.get("conditions").and_then(|c| c.as_array()) else {
        return entities;
    };
    for condition in conditions {
        if let Some(body) = condition.get("body") {
            let expr = CedarExpr::parse(body);
            if expr.is_unsatisfiable() {
                continue;
            }
            collect_resource_entities(&expr, false, &mut entities);
        }
    }
    entities
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use cedar_policy::PolicySet;

    use super::*;

    fn parse_policies(text: &str) -> PolicySet {
        PolicySet::from_str(text).expect("valid policy text")
    }

    fn eval(policies: &str, agent_id: &str, tools: &[&str]) -> SandboxPolicy {
        let ps = parse_policies(policies);
        let tool_strings: Vec<String> = tools.iter().map(|s| s.to_string()).collect();
        let principal = super::super::schema::build_agent_principal_entity(
            &crate::handle!("test-user"),
            &crate::core::Handle::try_new(agent_id).unwrap(),
            &tool_strings,
        );
        evaluate_sandbox_policy(&ps, principal)
    }

    #[test]
    fn test_eval_simple_permit_read() {
        let p = eval(
            r#"permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/tmp");"#,
            "aa",
            &[],
        );
        assert!(p.read_paths.contains(&"/tmp".to_string()));
    }

    #[test]
    fn test_eval_tool_conditional_with_matching_tool() {
        let p = eval(
            r#"permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/browser-data")
               when { principal.tools.contains("browser") };"#,
            "aa",
            &["browser"],
        );
        assert!(p.read_paths.contains(&"/browser-data".to_string()));
    }

    #[test]
    fn test_eval_tool_conditional_without_matching_tool() {
        let p = eval(
            r#"permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/browser-data")
               when { principal.tools.contains("browser") };"#,
            "aa",
            &["web_search"],
        );
        assert!(!p.read_paths.contains(&"/browser-data".to_string()));
    }

    #[test]
    fn test_eval_agent_specific() {
        let policies = r#"permit(principal == Policy::Agent::"test-user/aa", action == Policy::Action::"write", resource == Policy::Path::"/a-data");"#;
        assert!(
            eval(policies, "aa", &[])
                .write_paths
                .contains(&"/a-data".to_string())
        );
        assert!(eval(policies, "bb", &[]).write_paths.is_empty());
    }

    #[test]
    fn test_eval_permit_and_forbid_nested() {
        let p = eval(
            r#"
            permit(principal, action == Policy::Action::"write", resource == Policy::Path::"/workspace");
            forbid(principal, action == Policy::Action::"write", resource == Policy::Path::"/workspace/secrets");
        "#,
            "aa",
            &[],
        );
        assert!(p.write_paths.contains(&"/workspace".to_string()));
        assert!(p.denied_paths.contains(&"/workspace/secrets".to_string()));
    }

    #[test]
    fn test_eval_network_permit_and_forbid() {
        let p = eval(
            r#"
            permit(principal, action == Policy::Action::"connect", resource == Policy::NetworkDestination::"0.0.0.0/0!0-65535");
            forbid(principal, action == Policy::Action::"connect", resource == Policy::NetworkDestination::"10.0.0.0/8");
        "#,
            "aa",
            &[],
        );
        assert!(p.network_access);
        assert!(
            p.network_destinations
                .contains(&"0.0.0.0/0!0-65535".to_string())
        );
        assert!(p.blocked_networks.contains(&"10.0.0.0/8".to_string()));
    }

    #[test]
    fn test_eval_wildcard_resource_no_paths() {
        let p = eval(
            r#"permit(principal, action == Policy::Action::"connect", resource);"#,
            "aa",
            &[],
        );
        assert!(p.network_destinations.is_empty());
    }

    #[test]
    fn test_eval_no_policies() {
        let p = eval("", "aa", &[]);
        assert_eq!(p, SandboxPolicy::default());
    }

    #[test]
    fn test_eval_mixed_tools_multiple_dirs() {
        let policies = r#"
            permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/shared");
            permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/browser-data")
                when { principal.tools.contains("browser") };
            permit(principal, action == Policy::Action::"connect", resource == Policy::NetworkDestination::"api.example.com")
                when { principal.tools.contains("web_search") };
        "#;
        let browser = eval(policies, "aa", &["browser"]);
        assert!(browser.read_paths.contains(&"/shared".to_string()));
        assert!(browser.read_paths.contains(&"/browser-data".to_string()));
        assert!(browser.network_destinations.is_empty());

        let search = eval(policies, "aa", &["web_search"]);
        assert!(search.read_paths.contains(&"/shared".to_string()));
        assert!(!search.read_paths.contains(&"/browser-data".to_string()));
        assert!(
            search
                .network_destinations
                .contains(&"api.example.com".to_string())
        );
    }

    #[test]
    fn test_eval_many_agents_isolated() {
        let policies = r#"
            permit(principal == Policy::Agent::"test-user/alice", action == Policy::Action::"write", resource == Policy::Path::"/alice-home");
            permit(principal == Policy::Agent::"test-user/bob", action == Policy::Action::"write", resource == Policy::Path::"/bob-home");
        "#;
        assert_eq!(
            eval(policies, "alice", &[]).write_paths,
            vec!["/alice-home"]
        );
        assert_eq!(eval(policies, "bob", &[]).write_paths, vec!["/bob-home"]);
        assert!(eval(policies, "charlie", &[]).write_paths.is_empty());
    }

    #[test]
    fn test_eval_permit_with_unless() {
        let policies = r#"
            permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/data")
                unless { principal.tools.contains("restricted") };
        "#;
        assert!(
            eval(policies, "aa", &["browser"])
                .read_paths
                .contains(&"/data".to_string())
        );
        assert!(
            !eval(policies, "aa", &["restricted"])
                .read_paths
                .contains(&"/data".to_string())
        );
    }

    #[test]
    fn test_eval_multiple_when_conditions() {
        let policies = r#"
            permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/secure")
                when { principal.tools.contains("browser") }
                when { principal.tools.contains("auth") };
        "#;
        assert!(eval(policies, "aa", &["browser"]).read_paths.is_empty());
        assert!(
            eval(policies, "aa", &["browser", "auth"])
                .read_paths
                .contains(&"/secure".to_string())
        );
    }

    #[test]
    fn test_eval_when_boolean_or() {
        let policies = r#"
            permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/media")
                when { principal.tools.contains("browser") || principal.tools.contains("player") };
        "#;
        assert!(
            eval(policies, "aa", &["browser"])
                .read_paths
                .contains(&"/media".to_string())
        );
        assert!(
            eval(policies, "aa", &["player"])
                .read_paths
                .contains(&"/media".to_string())
        );
        assert!(eval(policies, "aa", &["search"]).read_paths.is_empty());
    }

    #[test]
    fn test_eval_tool_containsall() {
        let policies = r#"
            permit(principal, action == Policy::Action::"write", resource == Policy::Path::"/deploy")
                when { principal.tools.containsAll(["cli", "deploy"]) };
        "#;
        assert!(eval(policies, "aa", &["cli"]).write_paths.is_empty());
        assert!(
            eval(policies, "aa", &["cli", "deploy"])
                .write_paths
                .contains(&"/deploy".to_string())
        );
    }

    #[test]
    fn test_eval_tool_containsany() {
        let policies = r#"
            permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/logs")
                when { principal.tools.containsAny(["admin_tool", "monitoring"]) };
        "#;
        assert!(eval(policies, "aa", &["browser"]).read_paths.is_empty());
        assert!(
            eval(policies, "aa", &["admin_tool"])
                .read_paths
                .contains(&"/logs".to_string())
        );
        assert!(
            eval(policies, "aa", &["monitoring"])
                .read_paths
                .contains(&"/logs".to_string())
        );
    }

    #[test]
    fn test_eval_forbid_conditional_on_tool() {
        let policies = r#"
            permit(principal, action == Policy::Action::"connect", resource == Policy::NetworkDestination::"0.0.0.0/0!0-65535");
            forbid(principal, action == Policy::Action::"connect", resource == Policy::NetworkDestination::"10.0.0.0/8")
                unless { principal.tools.contains("admin_tool") };
        "#;
        assert!(
            eval(policies, "aa", &["browser"])
                .blocked_networks
                .contains(&"10.0.0.0/8".to_string())
        );
        assert!(
            !eval(policies, "aa", &["admin_tool"])
                .blocked_networks
                .contains(&"10.0.0.0/8".to_string())
        );
    }

    #[test]
    fn test_eval_multiple_forbids_stack() {
        let p = eval(
            r#"
            permit(principal, action == Policy::Action::"write", resource == Policy::Path::"/workspace");
            forbid(principal, action == Policy::Action::"write", resource == Policy::Path::"/workspace/secrets");
            forbid(principal, action == Policy::Action::"write", resource == Policy::Path::"/workspace/config");
            forbid(principal, action == Policy::Action::"write", resource == Policy::Path::"/workspace/.env");
        "#,
            "aa",
            &[],
        );
        assert_eq!(p.write_paths, vec!["/workspace"]);
        assert_eq!(p.denied_paths.len(), 3);
    }

    #[test]
    fn test_eval_read_permitted_write_forbidden_same_path() {
        let p = eval(
            r#"
            permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/config");
            forbid(principal, action == Policy::Action::"write", resource == Policy::Path::"/config");
        "#,
            "aa",
            &[],
        );
        assert!(p.read_paths.contains(&"/config".to_string()));
        assert!(p.denied_paths.contains(&"/config".to_string()));
    }

    #[test]
    fn test_eval_all_four_actions() {
        let p = eval(
            r#"
            permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/r");
            permit(principal, action == Policy::Action::"write", resource == Policy::Path::"/w");
            permit(principal, action == Policy::Action::"connect", resource == Policy::NetworkDestination::"api.com");
            permit(principal, action == Policy::Action::"bind", resource == Policy::NetworkDestination::"8080");
        "#,
            "aa",
            &[],
        );
        assert_eq!(p.read_paths, vec!["/r"]);
        assert_eq!(p.write_paths, vec!["/w"]);
        assert_eq!(p.network_destinations, vec!["api.com"]);
        assert_eq!(p.bind_ports, vec![8080]);
    }

    #[test]
    fn test_eval_forbid_unless_permits_exception() {
        let p = eval(
            r#"
            permit(principal, action == Policy::Action::"connect", resource);
            forbid(principal == Policy::Agent::"test-user/xx", action == Policy::Action::"connect", resource)
                unless { resource == Policy::NetworkDestination::"gmail.com" };
        "#,
            "xx",
            &[],
        );
        assert!(p.network_destinations.contains(&"gmail.com".to_string()));
    }

    #[test]
    fn test_eval_forbid_unless_other_agent_unaffected() {
        let p = eval(
            r#"
            permit(principal, action == Policy::Action::"connect", resource);
            forbid(principal == Policy::Agent::"test-user/xx", action == Policy::Action::"connect", resource)
                unless { resource == Policy::NetworkDestination::"gmail.com" };
        "#,
            "yy",
            &[],
        );
        assert!(!p.network_destinations.contains(&"gmail.com".to_string()));
    }

    #[test]
    fn test_eval_forbid_unless_multiple_exceptions() {
        let p = eval(
            r#"
            permit(principal, action == Policy::Action::"connect", resource);
            forbid(principal == Policy::Agent::"test-user/xx", action == Policy::Action::"connect", resource)
                unless { resource == Policy::NetworkDestination::"gmail.com"
                      || resource == Policy::NetworkDestination::"api.google.com" };
        "#,
            "xx",
            &[],
        );
        assert!(p.network_destinations.contains(&"gmail.com".to_string()));
        assert!(
            p.network_destinations
                .contains(&"api.google.com".to_string())
        );
    }

    #[test]
    fn test_eval_forbid_unless_with_tool() {
        let policies = r#"
            permit(principal, action == Policy::Action::"read", resource);
            forbid(principal, action == Policy::Action::"read", resource)
                unless { resource == Policy::Path::"/public"
                      || principal.tools.contains("admin_tool") };
        "#;
        let normal = eval(policies, "aa", &["browser"]);
        assert!(normal.read_paths.contains(&"/public".to_string()));

        let admin = eval(policies, "aa", &["admin_tool"]);
        assert!(admin.denied_paths.is_empty());
    }

    #[test]
    fn test_apply_to_sandbox_config() {
        let policy = SandboxPolicy {
            read_paths: vec!["/data".into()],
            write_paths: vec!["/output".into()],
            network_access: true,
            network_destinations: vec!["api.com".into()],
            bind_ports: vec![8080],
            denied_paths: vec!["/secrets".into()],
            blocked_networks: vec!["10.0.0.0/8".into()],
        };
        let mut config = SandboxConfig::default();
        policy.apply(&mut config);
        assert_eq!(config.additional_read_paths, vec!["/data"]);
        assert_eq!(config.additional_write_paths, vec!["/output"]);
        assert!(config.network_access);
        assert_eq!(config.allowed_network_destinations, vec!["api.com"]);
    }

    #[test]
    fn test_apply_no_network() {
        let policy = SandboxPolicy {
            network_access: false,
            ..Default::default()
        };
        let mut config = SandboxConfig {
            network_access: true,
            ..Default::default()
        };
        policy.apply(&mut config);
        assert!(config.network_access);
    }

    #[test]
    fn test_apply_merges_with_existing() {
        let policy = SandboxPolicy {
            read_paths: vec!["/policy-path".into()],
            network_access: true,
            network_destinations: vec!["policy-dest".into()],
            ..Default::default()
        };
        let mut config = SandboxConfig {
            additional_read_paths: vec!["/existing".into()],
            network_access: true,
            allowed_network_destinations: vec!["existing-dest".into()],
            ..Default::default()
        };
        policy.apply(&mut config);
        assert_eq!(
            config.additional_read_paths,
            vec!["/existing", "/policy-path"]
        );
        assert_eq!(
            config.allowed_network_destinations,
            vec!["existing-dest", "policy-dest"]
        );
    }

    fn no_owned_agents(_id: &str) -> bool {
        false
    }

    #[test]
    fn validate_paths_accepts_absolute() {
        let policy = SandboxPolicy {
            read_paths: vec!["/data".into(), "/var".into()],
            write_paths: vec!["/output".into()],
            denied_paths: vec!["/secrets".into()],
            ..Default::default()
        };
        assert!(
            policy
                .validate_paths(&crate::handle!("test-user"), no_owned_agents)
                .is_ok()
        );
    }

    #[test]
    fn validate_paths_accepts_owned_user_uri() {
        let policy = SandboxPolicy {
            read_paths: vec!["user://test-user/foo.csv".into()],
            ..Default::default()
        };
        assert!(
            policy
                .validate_paths(&crate::handle!("test-user"), no_owned_agents)
                .is_ok()
        );
    }

    #[test]
    fn validate_paths_rejects_other_user_uri() {
        let policy = SandboxPolicy {
            read_paths: vec!["user://other/foo.csv".into()],
            ..Default::default()
        };
        assert!(
            policy
                .validate_paths(&crate::handle!("test-user"), no_owned_agents)
                .is_err()
        );
    }

    #[test]
    fn validate_paths_accepts_owned_agent_uri() {
        let policy = SandboxPolicy {
            write_paths: vec!["agent://my-agent/data".into()],
            ..Default::default()
        };
        assert!(
            policy
                .validate_paths(&crate::handle!("test-user"), |id| id == "my-agent")
                .is_ok()
        );
    }

    #[test]
    fn validate_paths_rejects_unowned_agent_uri() {
        let policy = SandboxPolicy {
            write_paths: vec!["agent://other-agent/data".into()],
            ..Default::default()
        };
        assert!(
            policy
                .validate_paths(&crate::handle!("test-user"), |id| id == "my-agent")
                .is_err()
        );
    }

    #[test]
    fn validate_paths_rejects_relative() {
        let policy = SandboxPolicy {
            read_paths: vec!["relative/path".into()],
            ..Default::default()
        };
        assert!(
            policy
                .validate_paths(&crate::handle!("test-user"), no_owned_agents)
                .is_err()
        );
    }

    #[test]
    fn validate_paths_rejects_unknown_scheme() {
        let policy = SandboxPolicy {
            read_paths: vec!["ftp://server/foo".into()],
            ..Default::default()
        };
        assert!(
            policy
                .validate_paths(&crate::handle!("test-user"), no_owned_agents)
                .is_err()
        );
    }

    fn test_storage() -> StorageService {
        StorageService::new(&crate::core::config::Config::default())
    }

    #[test]
    fn resolve_virtual_paths_passes_absolute_through() {
        let mut policy = SandboxPolicy {
            read_paths: vec!["/data".into()],
            ..Default::default()
        };
        policy.resolve_virtual_paths(&test_storage(), &crate::handle!("test-user"));
        assert_eq!(policy.read_paths, vec!["/data".to_string()]);
    }

    #[test]
    fn resolve_virtual_paths_translates_user_uri() {
        let mut policy = SandboxPolicy {
            read_paths: vec!["user://test-user/foo.csv".into()],
            ..Default::default()
        };
        policy.resolve_virtual_paths(&test_storage(), &crate::handle!("test-user"));
        assert_eq!(policy.read_paths.len(), 1);
        assert!(policy.read_paths[0].ends_with("data/users/test-user/files/foo.csv"));
    }

    #[test]
    fn resolve_virtual_paths_translates_agent_uri() {
        let mut policy = SandboxPolicy {
            write_paths: vec!["agent://dev/output.csv".into()],
            ..Default::default()
        };
        policy.resolve_virtual_paths(&test_storage(), &crate::handle!("test-user"));
        assert_eq!(policy.write_paths.len(), 1);
        assert!(policy.write_paths[0].ends_with("data/users/test-user/agents/dev/output.csv"));
    }

    #[test]
    fn resolve_virtual_paths_drops_unparseable_entries() {
        let mut policy = SandboxPolicy {
            read_paths: vec!["bad-entry".into(), "/keepme".into()],
            ..Default::default()
        };
        policy.resolve_virtual_paths(&test_storage(), &crate::handle!("test-user"));
        assert_eq!(policy.read_paths, vec!["/keepme".to_string()]);
    }

    #[test]
    fn test_eval_complex_real_world() {
        let policies = r#"
            permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/tmp");
            permit(principal, action == Policy::Action::"write", resource == Policy::Path::"/tmp");
            permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/browser-profiles")
                when { principal.tools.contains("browser") };
            permit(principal, action == Policy::Action::"connect", resource == Policy::NetworkDestination::"0.0.0.0/0!0-65535")
                when { principal.tools.contains("browser") || principal.tools.contains("web_search") };
            forbid(principal, action == Policy::Action::"connect", resource == Policy::NetworkDestination::"10.0.0.0/8")
                unless { principal.tools.contains("admin_tool") };
            permit(principal == Policy::Agent::"test-user/deployer", action == Policy::Action::"write", resource == Policy::Path::"/deploy");
            forbid(principal, action == Policy::Action::"write", resource == Policy::Path::"/deploy/secrets")
                unless { principal.tools.containsAll(["deploy", "auth"]) };
        "#;

        let browser = eval(policies, "web-agent", &["browser"]);
        assert!(browser.read_paths.contains(&"/tmp".to_string()));
        assert!(browser.write_paths.contains(&"/tmp".to_string()));
        assert!(
            browser
                .read_paths
                .contains(&"/browser-profiles".to_string())
        );
        assert!(browser.network_access);
        assert!(
            browser
                .network_destinations
                .contains(&"0.0.0.0/0!0-65535".to_string())
        );
        assert!(browser.blocked_networks.contains(&"10.0.0.0/8".to_string()));
        assert!(!browser.write_paths.contains(&"/deploy".to_string()));

        let deployer = eval(policies, "deployer", &["deploy", "auth", "browser"]);
        assert!(deployer.write_paths.contains(&"/deploy".to_string()));
        assert!(
            !deployer
                .denied_paths
                .contains(&"/deploy/secrets".to_string())
        );

        let deployer_no_auth = eval(policies, "deployer", &["deploy"]);
        assert!(
            deployer_no_auth
                .denied_paths
                .contains(&"/deploy/secrets".to_string())
        );

        let no_tools = eval(policies, "basic", &[]);
        assert!(no_tools.read_paths.contains(&"/tmp".to_string()));
        assert!(
            !no_tools
                .read_paths
                .contains(&"/browser-profiles".to_string())
        );
        assert!(no_tools.network_destinations.is_empty());
    }

    fn make_default_network_policy() -> cedar_policy::Policy {
        cedar_policy::Policy::from_json(
            Some(cedar_policy::PolicyId::new("default-network-access")),
            serde_json::json!({
                "effect": "permit",
                "principal": { "op": "All" },
                "action": { "op": "==", "entity": { "type": "Policy::Action", "id": "connect" } },
                "resource": { "op": "All" },
                "annotations": {
                    "description": "Default outbound network access for all agents",
                    "config": "sandbox.default_network_access",
                    "readonly": "true"
                },
                "conditions": []
            }),
        )
        .expect("valid policy")
    }

    fn eval_with_managed(
        policies: &str,
        agent_id: &str,
        tools: &[&str],
        managed: &[cedar_policy::Policy],
    ) -> SandboxPolicy {
        let mut ps = parse_policies(if policies.is_empty() {
            "// empty"
        } else {
            policies
        });
        for p in managed {
            ps.add(p.clone()).expect("add managed policy");
        }
        let tool_strings: Vec<String> = tools.iter().map(|s| s.to_string()).collect();
        let principal = super::super::schema::build_agent_principal_entity(
            &crate::handle!("test-user"),
            &crate::core::Handle::try_new(agent_id).unwrap(),
            &tool_strings,
        );
        evaluate_sandbox_policy(&ps, principal)
    }

    #[test]
    fn test_managed_default_network_grants_access() {
        let managed = make_default_network_policy();
        let p = eval_with_managed("", "any-agent", &[], &[managed]);
        assert!(p.network_access);
    }

    #[test]
    fn test_no_managed_no_network() {
        let p = eval_with_managed("", "any-agent", &[], &[]);
        assert!(!p.network_access);
    }

    #[test]
    fn test_managed_network_with_user_forbid_unless() {
        let managed = make_default_network_policy();
        let user_policies = r#"
            forbid(principal == Policy::Agent::"test-user/restricted",
                   action == Policy::Action::"connect", resource)
                unless { resource == Policy::NetworkDestination::"gmail.com:443" };
        "#;
        let restricted = eval_with_managed(
            user_policies,
            "restricted",
            &[],
            std::slice::from_ref(&managed),
        );
        assert!(
            restricted
                .network_destinations
                .contains(&"gmail.com:443".to_string())
        );

        let normal = eval_with_managed(
            user_policies,
            "normal-agent",
            &[],
            std::slice::from_ref(&managed),
        );
        assert!(normal.network_access);
        assert!(
            !normal
                .network_destinations
                .contains(&"gmail.com:443".to_string())
        );
    }

    #[test]
    fn test_managed_policy_annotations() {
        let policy = make_default_network_policy();
        assert_eq!(
            policy.annotation("config"),
            Some("sandbox.default_network_access")
        );
        assert_eq!(policy.annotation("readonly"), Some("true"));
        assert_eq!(
            policy.annotation("description"),
            Some("Default outbound network access for all agents")
        );
    }
}
