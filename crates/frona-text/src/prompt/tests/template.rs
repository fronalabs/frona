use crate::prompt::{PromptError, PromptLoader, PromptTemplate};

#[test]
fn required_templates_distinguish_missing_empty_and_render_errors() {
    let loader = PromptLoader::embedded([("empty", "  \n"), ("invalid", "{{missing}}")]);
    assert!(matches!(
        PromptTemplate::new("absent", &[]).render(&loader, &[]),
        Err(PromptError::Missing { .. })
    ));
    assert!(matches!(
        PromptTemplate::new("empty", &[]).render(&loader, &[]),
        Err(PromptError::Empty { .. })
    ));
    assert!(matches!(
        PromptTemplate::new("invalid", &[]).render(&loader, &[]),
        Err(PromptError::Render { .. })
    ));
}

#[test]
fn exact_contract_rejects_missing_extra_and_duplicate_arguments_before_rendering() {
    let loader = PromptLoader::embedded([("input", "{{name}}")]).with_var("name", "Default");
    let template = PromptTemplate::new("input", &["name"]);
    for vars in [
        vec![],
        vec![("name", "a"), ("extra", "b")],
        vec![("name", "a"), ("name", "b")],
        vec![("name", "a"), ("NAME", "b")],
    ] {
        assert!(matches!(
            template.render(&loader, &vars),
            Err(PromptError::Variables { .. })
        ));
    }
    assert_eq!(
        template.render(&loader, &[("NAME", "Caller")]).unwrap(),
        "Caller"
    );
}

#[test]
fn defaults_remain_available_beyond_declared_invocation_arguments() {
    let loader =
        PromptLoader::embedded([("input", "{{name}}/{{location}}")]).with_var("location", "Home");
    assert_eq!(
        PromptTemplate::new("input", &["name"])
            .render(&loader, &[("name", "Caller")])
            .unwrap(),
        "Caller/Home"
    );
}

#[test]
fn direct_source_preserves_contract_and_named_errors() {
    let template = PromptTemplate::new("input", &["name"]);
    for vars in [
        vec![],
        vec![("name", "a"), ("extra", "b")],
        vec![("name", "a"), ("NAME", "b")],
    ] {
        assert!(matches!(
            template.render_source("{{invalid}}", &vars),
            Err(PromptError::Variables { name, .. }) if name == "input"
        ));
    }
    assert_eq!(
        template
            .render_source("Hello {{name}}", &[("NAME", "Caller")])
            .unwrap(),
        "Hello Caller"
    );
    assert!(matches!(
        template.render_source("{{name}}", &[("name", " \n")]),
        Err(PromptError::Empty { name }) if name == "input"
    ));
    assert!(matches!(
        template.render_source("{{missing}}", &[("name", "Caller")]),
        Err(PromptError::Render { name: Some(name), .. }) if name == "input"
    ));
    assert!(matches!(
        PromptTemplate::new("input", &["name", "NAME"])
            .render_source("{{name}}", &[("name", "Caller")]),
        Err(PromptError::Variables { .. })
    ));
}
