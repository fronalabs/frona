use crate::prompt::render_template;

#[test]
fn renders_simple_variable() {
    let result = render_template("Hello {{name}}!", &[("name", "World")]).unwrap();
    assert_eq!(result, "Hello World!");
}

#[test]
fn case_insensitive_key_side() {
    let result = render_template("Hello {{agent_name}}!", &[("Agent_Name", "Bot")]).unwrap();
    assert_eq!(result, "Hello Bot!");
}

#[test]
fn case_insensitive_template_side() {
    let result = render_template("Hello {{NAME}}!", &[("name", "World")]).unwrap();
    assert_eq!(result, "Hello World!");
}

#[test]
fn missing_variable_returns_error() {
    let result = render_template("Hello {{missing}}!", &[]);
    assert!(result.is_err());
}

#[test]
fn empty_vars_no_placeholders_returns_unchanged() {
    let input = "No placeholders here.";
    let result = render_template(input, &[]).unwrap();
    assert_eq!(result, input);
}

#[test]
fn multiple_variables() {
    let result = render_template(
        "{{greeting}}, {{name}}! You are {{role}}.",
        &[("greeting", "Hi"), ("name", "Alice"), ("role", "admin")],
    )
    .unwrap();
    assert_eq!(result, "Hi, Alice! You are admin.");
}

#[test]
fn same_variable_used_twice() {
    let result = render_template("{{x}} and {{x}}", &[("x", "hello")]).unwrap();
    assert_eq!(result, "hello and hello");
}
#[test]
fn control_blocks_and_comments_are_evaluated_without_variables() {
    assert_eq!(
        render_template("{% if true %}yes{% endif %}{# hidden #}", &[]).unwrap(),
        "yes"
    );
    assert!(render_template("{% if missing %}yes{% endif %}", &[]).is_err());
    assert!(render_template("{% invalid %}", &[]).is_err());
}
