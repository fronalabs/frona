use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use std::collections::HashSet;
use syn::{
    Attribute, Data, DeriveInput, Fields, GenericArgument, Lit, LitStr, PathArguments, Type,
};

const DIALECTS: &[(&str, &str)] = &[
    ("bedrock", "Bedrock"),
    ("open_ai_chat", "OpenAiChat"),
    ("legacy_chat", "LegacyChat"),
    ("responses", "Responses"),
    ("anthropic", "Anthropic"),
    ("gemini", "Gemini"),
    ("ollama", "Ollama"),
    ("cohere", "Cohere"),
    ("hugging_face", "HuggingFace"),
];

#[derive(Default)]
struct Options {
    prefix: Option<LitStr>,
    path: Option<LitStr>,
    skip: bool,
    root: bool,
    nested: bool,
    dialects: Vec<(syn::Ident, Option<LitStr>)>,
}

fn options(attrs: &[Attribute], container: bool) -> syn::Result<Options> {
    let mut result = Options::default();
    let mut seen = HashSet::new();
    for attr in attrs
        .iter()
        .filter(|attr| attr.path().is_ident("parameter"))
    {
        attr.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .ok_or_else(|| meta.error("expected parameter option"))?
                .to_string();
            if !seen.insert(key.clone()) {
                return Err(meta.error("duplicate parameter option"));
            }
            match key.as_str() {
                "prefix" if container => {
                    result.prefix = Some(path_literal(meta.value()?.parse()?)?)
                }
                "path" if !container => result.path = Some(path_literal(meta.value()?.parse()?)?),
                "skip" if !container => result.skip = true,
                "root" if !container => result.root = true,
                "nested" if !container => result.nested = true,
                key if !container => {
                    let variant = DIALECTS
                        .iter()
                        .find(|(name, _)| *name == key)
                        .ok_or_else(|| meta.error("unknown parameter option or protocol"))?
                        .1;
                    let value: Lit = meta.value()?.parse()?;
                    let path = match value {
                        Lit::Str(path) => Some(path_literal(path)?),
                        Lit::Bool(value) if !value.value => None,
                        _ => return Err(meta.error("expected a request path string or false")),
                    };
                    result.dialects.push((format_ident!("{variant}"), path));
                }
                _ => return Err(meta.error("only prefix is supported on a settings struct")),
            }
            Ok(())
        })?;
    }
    if result.skip && seen.len() != 1 {
        return Err(syn::Error::new_spanned(
            &attrs[0],
            "skip cannot be combined with other parameter options",
        ));
    }
    Ok(result)
}

fn path_literal(path: LitStr) -> syn::Result<LitStr> {
    if path
        .value()
        .split('.')
        .any(|part| part.is_empty() || part.trim() != part)
    {
        return Err(syn::Error::new_spanned(
            path,
            "request paths require nonempty dot-separated segments",
        ));
    }
    Ok(path)
}

// Ignore unrelated Serde options, but reject shape-changing options we cannot
// describe. Read Serde's names instead of requiring a second config-name label.
fn serde_name(attrs: &[Attribute], option: &str) -> syn::Result<Option<LitStr>> {
    let mut result = None;
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("serde")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident(option) {
                if result.is_some() {
                    return Err(meta.error("duplicate serde naming option"));
                }
                result = Some(meta.value()?.parse::<LitStr>()?);
            } else if [
                "flatten",
                "skip",
                "skip_serializing",
                "serialize_with",
                "with",
                "into",
                "transparent",
            ]
            .iter()
            .any(|option| meta.path.is_ident(option))
            {
                return Err(meta.error("this serde shape needs an explicit #[parameter(skip)]"));
            } else if meta.input.peek(syn::Token![=]) {
                let _: syn::Expr = meta.value()?.parse()?;
            } else if meta.input.peek(syn::token::Paren) {
                return Err(meta.error("split serde names are not supported by ParameterMetadata"));
            }
            Ok(())
        })?;
    }
    Ok(result)
}

fn rename(name: &str, rule: Option<&LitStr>) -> syn::Result<String> {
    Ok(match rule.map(LitStr::value).as_deref() {
        None => name.into(),
        Some("lowercase" | "snake_case") => name.into(),
        Some("UPPERCASE") => name.to_uppercase(),
        Some("SCREAMING_SNAKE_CASE") => name.to_uppercase(),
        Some("kebab-case") => name.replace('_', "-"),
        Some("SCREAMING-KEBAB-CASE") => name.to_uppercase().replace('_', "-"),
        Some("camelCase" | "PascalCase") => {
            let mut result = String::new();
            let mut capitalize = true;
            for ch in name.chars() {
                if ch == '_' {
                    capitalize = true;
                } else if capitalize {
                    result.push(ch.to_ascii_uppercase());
                    capitalize = false;
                } else {
                    result.push(ch);
                }
            }
            if rule.unwrap().value() == "camelCase"
                && let Some(first) = result.get_mut(..1)
            {
                first.make_ascii_lowercase();
            }
            result
        }
        _ => {
            return Err(syn::Error::new_spanned(
                rule.unwrap(),
                "unsupported serde rename_all rule",
            ));
        }
    })
}

fn nested_type(ty: &Type) -> &Type {
    if let Type::Path(path) = ty
        && let Some(segment) = path.path.segments.last()
        && segment.ident == "Option"
        && let PathArguments::AngleBracketed(args) = &segment.arguments
        && let Some(GenericArgument::Type(inner)) = args.args.first()
    {
        inner
    } else {
        ty
    }
}

pub fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input,
            "ParameterMetadata requires a named-field struct",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            &input,
            "ParameterMetadata requires a named-field struct",
        ));
    };
    let container = options(&input.attrs, true)?;
    let rename_all = serde_name(&input.attrs, "rename_all")?;
    let mut statements = Vec::new();
    let mut names = HashSet::new();
    for field in &fields.named {
        let opts = options(&field.attrs, false)?;
        if opts.skip {
            continue;
        }
        let ident = field.ident.as_ref().unwrap().to_string();
        let config = serde_name(&field.attrs, "rename")?
            .map(|name| name.value())
            .unwrap_or(rename(ident.trim_start_matches("r#"), rename_all.as_ref())?);
        if !names.insert(config.clone()) {
            return Err(syn::Error::new_spanned(
                field,
                "duplicate serialized parameter name",
            ));
        }
        let default = opts
            .path
            .as_ref()
            .map(LitStr::value)
            .unwrap_or_else(|| config.clone());
        let arms = opts.dialects.iter().map(|(dialect, path)| match path {
            Some(path) => quote! { frona::inference::protocol::parameters::WireDialect::#dialect => Some(#path), },
            None => quote! { frona::inference::protocol::parameters::WireDialect::#dialect => None, },
        });
        let prefix = if opts.root {
            String::new()
        } else {
            container
                .prefix
                .as_ref()
                .map(LitStr::value)
                .unwrap_or_default()
        };
        let emit = if opts.nested {
            let ty = nested_type(&field.ty);
            quote! {
                for child in <#ty as frona::inference::protocol::parameters::ParameterMetadata>::parameter_bindings(dialect) {
                    let mut child_path = wire_path.clone();
                    child_path.extend(child.wire_path);
                    result.push(frona::inference::protocol::parameters::ParameterBinding {
                        config_path: format!("{}.{}", #config, child.config_path),
                        wire_path: child_path,
                    });
                }
            }
        } else {
            quote! { result.push(frona::inference::protocol::parameters::ParameterBinding {
                config_path: #config.to_owned(), wire_path,
            }); }
        };
        statements.push(quote! {
            if let Some(path) = match dialect { #(#arms)* _ => Some(#default) } {
                let mut wire_path: Vec<String> = if #prefix.is_empty() { Vec::new() } else {
                    #prefix.split('.').map(str::to_owned).collect()
                };
                wire_path.extend(path.split('.').map(str::to_owned));
                #emit
            }
        });
    }
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let body = if statements.is_empty() {
        quote! { let _ = dialect; Vec::new() }
    } else {
        quote! {
            let mut result = Vec::new();
            #(#statements)*
            result
        }
    };
    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics frona::inference::protocol::parameters::ParameterMetadata for #name #ty_generics #where_clause {
            fn parameter_bindings(dialect: frona::inference::protocol::parameters::WireDialect)
                -> Vec<frona::inference::protocol::parameters::ParameterBinding>
            {
                #body
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_annotations_and_shapes() {
        for source in [
            "enum Settings { A }",
            "struct Settings(u64);",
            "struct Settings { #[parameter(unknown = false)] value: u64 }",
            "struct Settings { #[parameter(responses = true)] value: u64 }",
            "struct Settings { #[parameter(path = \"a..b\")] value: u64 }",
            "struct Settings { #[parameter(skip, nested)] value: u64 }",
            "struct Settings { #[parameter(root, root)] value: u64 }",
            "struct Settings { #[serde(flatten)] value: u64 }",
            "struct Settings { #[serde(rename = \"b\")] a: u64, b: u64 }",
            "#[parameter(path = \"a\")] struct Settings { value: u64 }",
        ] {
            assert!(expand(syn::parse_str(source).unwrap()).is_err(), "{source}");
        }
    }

    #[test]
    fn field_case_rules_preserve_serde_spelling() {
        for (rule, expected) in [
            ("lowercase", "some_URL"),
            ("snake_case", "some_URL"),
            ("camelCase", "someURL"),
            ("PascalCase", "SomeURL"),
            ("SCREAMING_SNAKE_CASE", "SOME_URL"),
            ("kebab-case", "some-URL"),
            ("SCREAMING-KEBAB-CASE", "SOME-URL"),
        ] {
            let rule = LitStr::new(rule, proc_macro2::Span::call_site());
            assert_eq!(rename("some_URL", Some(&rule)).unwrap(), expected);
        }
    }
}
