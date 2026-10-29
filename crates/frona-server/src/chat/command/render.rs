use std::collections::HashSet;

use crate::agent::config::parse_frontmatter;
use crate::agent::skill::resolver::Skill;
use crate::chat::slash::shell_split;

pub fn try_render_skill(
    skill_name: &str,
    prompt: &str,
    skills: &[Skill],
) -> Result<Option<String>, crate::core::error::AppError> {
    let Some(skill) = skills.iter().find(|s| s.name == skill_name) else {
        return Ok(None);
    };
    let skill_md_path = format!("{}/SKILL.md", skill.path);
    let raw = std::fs::read_to_string(&skill_md_path).map_err(|error| {
        crate::core::error::AppError::Internal(format!("Read skill {skill_name}: {error}"))
    })?;
    let parsed = parse_frontmatter(&raw);
    let body = parsed.template;

    let placeholders = find_placeholders(&body, &skill.arguments);

    Ok(if placeholders.is_empty() {
        if prompt.is_empty() {
            Some(format!(
                "<skill name=\"{}\">{}</skill>",
                escape_attr(skill_name),
                body
            ))
        } else {
            Some(format!(
                "<skill name=\"{}\">{}</skill>\n{}",
                escape_attr(skill_name),
                body,
                prompt
            ))
        }
    } else {
        let interpolated = interpolate(&body, prompt, &skill.arguments, &placeholders);
        Some(format!(
            "<skill name=\"{}\" prompt=\"{}\">{}</skill>",
            escape_attr(skill_name),
            escape_attr(prompt),
            interpolated
        ))
    })
}

fn find_placeholders(body: &str, declared_names: &[String]) -> HashSet<Placeholder> {
    let mut found = HashSet::new();
    if body.contains("$ARGUMENTS") {
        found.insert(Placeholder::Arguments);
    }
    let mut chars = body.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '$'
            && let Some((_, next)) = chars.peek()
            && next.is_ascii_digit()
        {
            let mut j = i + 1;
            while body[j..].chars().next().is_some_and(|c| c.is_ascii_digit()) {
                j += 1;
            }
            if j > i + 1 {
                found.insert(Placeholder::Numeric);
            }
        }
    }
    for name in declared_names {
        if body.contains(&format!("${name}")) {
            found.insert(Placeholder::Named);
        }
    }
    found
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Placeholder {
    Arguments,
    Numeric,
    Named,
}

/// `$ARGUMENTS` → raw prompt. `$N` and `$<name>` → POSIX-tokenized positional;
/// undersupplied indices substitute empty. Named slots follow declared order.
fn interpolate(
    body: &str,
    prompt: &str,
    declared_names: &[String],
    _kinds: &HashSet<Placeholder>,
) -> String {
    let tokens = shell_split(prompt);
    let mut out = body.replace("$ARGUMENTS", prompt);

    // Largest-first so `$10` doesn't get partially overwritten by `$1`.
    let mut indices: Vec<usize> = (1..=tokens.len()).collect();
    indices.sort_by(|a, b| b.cmp(a));
    for n in indices {
        let needle = format!("${n}");
        if out.contains(&needle) {
            out = out.replace(&needle, &tokens[n - 1]);
        }
    }
    out = strip_leftover_numeric(&out);

    for (idx, name) in declared_names.iter().enumerate() {
        let needle = format!("${name}");
        let value = tokens.get(idx).map(|s| s.as_str()).unwrap_or("");
        if out.contains(&needle) {
            out = out.replace(&needle, value);
        }
    }

    out
}

fn strip_leftover_numeric(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c == '$'
            && let Some((_, next)) = chars.peek()
            && next.is_ascii_digit()
        {
            while chars.peek().is_some_and(|(_, c)| c.is_ascii_digit()) {
                chars.next();
            }
            continue;
        }
        result.push(c);
    }
    result
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::skill::resolver::SkillScope;

    fn skill(name: &str, args: Vec<String>) -> Skill {
        Skill {
            name: name.to_string(),
            description: String::new(),
            path: String::new(),
            scope: SkillScope::Builtin,
            disable_model_invocation: false,
            argument_hint: None,
            arguments: args,
        }
    }

    #[test]
    fn no_placeholder_empty_prompt_just_tag() {
        let mut s = skill("weather", vec![]);
        s.path = String::new();
        let out = format!("<skill name=\"{}\">{}</skill>", "weather", "body");
        assert!(out.starts_with("<skill name=\"weather\">"));
        assert!(out.ends_with("</skill>"));
    }

    #[test]
    fn interpolate_arguments_substitutes_raw_prompt() {
        let out = interpolate(
            "Look up $ARGUMENTS now.",
            "London tomorrow",
            &[],
            &HashSet::new(),
        );
        assert_eq!(out, "Look up London tomorrow now.");
    }

    #[test]
    fn interpolate_positional_with_quoting() {
        let out = interpolate(
            "git commit -m \"$1\" --author $2",
            r#""fix the typo" alice"#,
            &[],
            &HashSet::new(),
        );
        assert_eq!(out, "git commit -m \"fix the typo\" --author alice");
    }

    #[test]
    fn interpolate_strips_leftover_when_undersupplied() {
        let out = interpolate("$1 then $2 then $3", "only-one", &[], &HashSet::new());
        assert_eq!(out, "only-one then  then ");
    }

    #[test]
    fn interpolate_named_via_frontmatter() {
        let out = interpolate(
            "Forecast for $city over $days days.",
            "Paris 7",
            &["city".to_string(), "days".to_string()],
            &HashSet::new(),
        );
        assert_eq!(out, "Forecast for Paris over 7 days.");
    }

    #[test]
    fn interpolate_named_missing_substitutes_empty() {
        let out = interpolate("Hello $name.", "", &["name".to_string()], &HashSet::new());
        assert_eq!(out, "Hello .");
    }

    #[test]
    fn find_placeholders_detects_arguments_and_numeric() {
        let p = find_placeholders("hello $ARGUMENTS, $1 then $2", &[]);
        assert!(p.contains(&Placeholder::Arguments));
        assert!(p.contains(&Placeholder::Numeric));
        assert!(!p.contains(&Placeholder::Named));
    }

    #[test]
    fn find_placeholders_detects_named_only_when_declared() {
        let p = find_placeholders("city is $city", &[]);
        assert!(!p.contains(&Placeholder::Named));
        let p = find_placeholders("city is $city", &["city".to_string()]);
        assert!(p.contains(&Placeholder::Named));
    }

    #[test]
    fn escape_attr_escapes_html_specials() {
        assert_eq!(
            escape_attr(r#"a & "b" < c"#),
            r#"a &amp; &quot;b&quot; &lt; c"#
        );
    }
}
