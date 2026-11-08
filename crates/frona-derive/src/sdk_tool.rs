use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{FnArg, ItemFn, LitStr, Pat, ReturnType, Type, parse_quote, spanned::Spanned};

pub fn expand(attributes: TokenStream, input: TokenStream) -> syn::Result<TokenStream> {
    let mut function: ItemFn = syn::parse2(input)?;
    let mut runtime: Option<syn::Path> = None;
    let mut name = None;
    let mut description = None;
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("crate") {
            runtime = Some(meta.value()?.parse::<LitStr>()?.parse()?);
        } else if meta.path.is_ident("name") {
            name = Some(meta.value()?.parse::<LitStr>()?);
        } else if meta.path.is_ident("description") {
            description = Some(meta.value()?.parse::<LitStr>()?);
        } else {
            return Err(meta.error("expected name, description, or crate"));
        }
        Ok(())
    });
    syn::parse::Parser::parse2(parser, attributes)?;
    let signature = &mut function.sig;
    if signature.asyncness.is_none()
        || matches!(signature.safety, syn::Safety::Unsafe(_))
        || signature.abi.is_some()
        || signature.constness.is_some()
    {
        return Err(syn::Error::new(
            signature.span(),
            "#[tool] requires a safe async Rust function",
        ));
    }
    if !signature.generics.params.is_empty() || signature.generics.where_clause.is_some() {
        return Err(syn::Error::new(
            signature.generics.span(),
            "#[tool] requires a concrete function without generic parameters",
        ));
    }
    if signature.inputs.len() > 16 {
        return Err(syn::Error::new(
            signature.inputs.span(),
            "#[tool] supports at most 16 parameters, including ToolContext",
        ));
    }
    let ReturnType::Type(_, output) = &signature.output else {
        return Err(syn::Error::new(
            signature.output.span(),
            "#[tool] requires a Result return type",
        ));
    };
    let output = output.clone();
    let identifier = &signature.ident;
    let metadata = format_ident!(
        "__FronaToolMetadata_{}",
        identifier.to_string().trim_start_matches("r#")
    );
    let arguments_type = format_ident!(
        "__FronaToolArguments_{}",
        identifier.to_string().trim_start_matches("r#")
    );
    let callable_type = format_ident!(
        "__FronaToolCallable_{}",
        identifier.to_string().trim_start_matches("r#")
    );
    let name = name.unwrap_or_else(|| {
        LitStr::new(
            identifier.to_string().trim_start_matches("r#"),
            identifier.span(),
        )
    });
    let docs = function
        .attrs
        .iter()
        .filter_map(|attr| {
            if !attr.path().is_ident("doc") {
                return None;
            }
            let syn::Meta::NameValue(meta) = &attr.meta else {
                return None;
            };
            let syn::Expr::Lit(lit) = &meta.value else {
                return None;
            };
            let syn::Lit::Str(text) = &lit.lit else {
                return None;
            };
            Some(text.value().trim().to_owned())
        })
        .collect::<Vec<_>>()
        .join("\n");
    let description = description.unwrap_or_else(|| LitStr::new(&docs, identifier.span()));
    let visibility = &function.vis;
    let configuration = function
        .attrs
        .iter()
        .filter_map(|attribute| match configuration_meta(&attribute.meta) {
            Ok(Some(meta)) => Some(Ok(parse_quote!(#[#meta]))),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<syn::Result<Vec<syn::Attribute>>>()?;
    let mut fields = Vec::new();
    let mut invocation = Vec::new();
    let mut types = Vec::new();
    let mut context = false;
    for input in &mut signature.inputs {
        let FnArg::Typed(argument) = input else {
            return Err(syn::Error::new(
                input.span(),
                "#[tool] supports free functions, not methods; bind application state with the closure helper",
            ));
        };
        let Pat::Ident(pattern) = argument.pat.as_ref() else {
            return Err(syn::Error::new(
                argument.pat.span(),
                "tool parameters must be named identifiers",
            ));
        };
        if pattern.by_ref.is_some() || pattern.subpat.is_some() {
            return Err(syn::Error::new(
                pattern.span(),
                "tool parameters must be plain named identifiers",
            ));
        }
        let field = &pattern.ident;
        if is_context(&argument.ty) {
            if context {
                return Err(syn::Error::new(
                    argument.span(),
                    "a tool may request only one ToolContext",
                ));
            }
            context = true;
            validate_context(&argument.ty)?;
            let borrowed = matches!(argument.ty.as_ref(), Type::Reference(_));
            let path = match argument.ty.as_ref() {
                Type::Reference(reference) => reference.elem.as_ref(),
                other => other,
            };
            let Type::Path(path) = path else {
                unreachable!()
            };
            let mut path = path.clone();
            path.path.segments.last_mut().unwrap().arguments =
                syn::PathArguments::AngleBracketed(parse_quote!(<'__frona_call>));
            argument.ty = if borrowed {
                Box::new(parse_quote!(&'__frona_call #path))
            } else {
                Box::new(Type::Path(path))
            };
            invocation.push(if borrowed {
                quote!(&context)
            } else {
                quote!(context)
            });
        } else {
            if matches!(
                argument.ty.as_ref(),
                Type::Reference(_) | Type::ImplTrait(_)
            ) {
                return Err(syn::Error::new(
                    argument.ty.span(),
                    "model arguments must use owned concrete types; only ToolContext is injected",
                ));
            }
            let ty = &argument.ty;
            let docs: Vec<_> = argument
                .attrs
                .iter()
                .filter(|a| a.path().is_ident("doc"))
                .collect();
            fields.push(quote!(#(#docs)* pub #field: #ty));
            invocation.push(quote!(_arguments.#field));
        }
        types.push(argument.ty.clone());
    }
    let runtime = match runtime {
        Some(path) => path,
        None => match proc_macro_crate::crate_name("frona-harness") {
            Ok(proc_macro_crate::FoundCrate::Itself) => parse_quote!(::frona_harness),
            Ok(proc_macro_crate::FoundCrate::Name(name)) => {
                let name = syn::Ident::new(&name, identifier.span());
                parse_quote!(::#name)
            }
            Err(error) => {
                return Err(syn::Error::new(
                    identifier.span(),
                    format!(
                        "cannot locate frona-harness dependency: {error}; use #[tool(crate = \"path\")] for an explicit path"
                    ),
                ));
            }
        },
    };
    let lifetime = if context {
        quote!('__frona_call)
    } else {
        quote!('static)
    };
    let bound = if context {
        quote!(for<'__frona_call>)
    } else {
        quote!()
    };
    if context {
        signature.generics.params.push(parse_quote!('__frona_call));
    }
    signature.asyncness = None;
    signature.output =
        parse_quote!(-> #runtime::__private::AnnotatedFuture<#lifetime, #output, #metadata>);
    let body = &function.block;
    *function.block = parse_quote!({
        #runtime::__private::AnnotatedFuture::new(async move #body)
    });
    let context_binding = if context {
        quote!(let context = _context.clone();)
    } else {
        quote!()
    };
    let serde_path = LitStr::new(
        &quote!(#runtime::__private::serde).to_string(),
        identifier.span(),
    );
    let schema_path = LitStr::new(
        &quote!(#runtime::__private::schemars).to_string(),
        identifier.span(),
    );
    Ok(quote! {
        #function
        #(#configuration)*
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        #[derive(#runtime::__private::serde::Deserialize, #runtime::__private::schemars::JsonSchema)]
        #[serde(crate = #serde_path)]
        #[schemars(crate = #schema_path)]
        #visibility struct #arguments_type { #(#fields,)* }
        #(#configuration)*
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        #visibility struct #metadata;
        #(#configuration)*
        impl<#callable_type> #runtime::__private::AnnotatedTool<#callable_type> for #metadata
        where
            #callable_type: #bound ::std::ops::FnMut(#(#types),*) -> #runtime::__private::AnnotatedFuture<#lifetime, #output, #metadata> + ::std::marker::Send,
        {
            type Arguments = #arguments_type;
            const NAME: &'static str = #name;
            const DESCRIPTION: &'static str = #description;
            fn invoke<'call>(
                function: &'call mut #callable_type,
                _arguments: #arguments_type,
                _context: &#runtime::ToolContext<'call>,
            ) -> #runtime::ToolFuture<'call, #runtime::ToolOutput> {
                #context_binding
                ::std::boxed::Box::pin(async move {
                    let output = function(#(#invocation),*).await.map_err(#runtime::ToolError::failed)?;
                    #runtime::IntoToolOutput::into_tool_output(output)
                })
            }
        }
    })
}

/// Propagate only conditional compilation to the generated items. Other
/// attributes describe the user's function and may be invalid on types/impls.
fn configuration_meta(meta: &syn::Meta) -> syn::Result<Option<syn::Meta>> {
    if meta.path().is_ident("cfg") {
        return Ok(Some(meta.clone()));
    }
    if !meta.path().is_ident("cfg_attr") {
        return Ok(None);
    }
    let syn::Meta::List(list) = meta else {
        return Err(syn::Error::new(
            meta.span(),
            "expected cfg_attr(predicate, attributes...)",
        ));
    };
    let mut arguments = list
        .parse_args_with(
            syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
        )?
        .into_iter();
    let Some(predicate) = arguments.next() else {
        return Err(syn::Error::new(
            meta.span(),
            "cfg_attr requires a predicate",
        ));
    };
    let mut retained = Vec::new();
    for argument in arguments {
        if let Some(configuration) = configuration_meta(&argument)? {
            retained.push(configuration);
        }
    }
    if retained.is_empty() {
        Ok(None)
    } else {
        Ok(Some(parse_quote!(cfg_attr(#predicate, #(#retained),*))))
    }
}

fn is_context(ty: &Type) -> bool {
    let ty = match ty {
        Type::Reference(reference) => reference.elem.as_ref(),
        other => other,
    };
    matches!(ty, Type::Path(path) if path.qself.is_none() && path.path.segments.last().is_some_and(|segment| segment.ident == "ToolContext"))
}

fn validate_context(ty: &Type) -> syn::Result<()> {
    let ty = if let Type::Reference(reference) = ty {
        if reference.mutability.is_some() {
            return Err(syn::Error::new(
                reference.span(),
                "ToolContext injection supports owned or shared references, not &mut",
            ));
        }
        if reference
            .lifetime
            .as_ref()
            .is_some_and(|lifetime| lifetime.ident != "_")
        {
            return Err(syn::Error::new(
                reference.span(),
                "ToolContext must borrow the current call; use an elided or '_ lifetime",
            ));
        }
        reference.elem.as_ref()
    } else {
        ty
    };
    let Type::Path(path) = ty else { unreachable!() };
    match &path.path.segments.last().unwrap().arguments {
        syn::PathArguments::None => Ok(()),
        syn::PathArguments::AngleBracketed(arguments)
            if arguments.args.len() == 1
                && matches!(arguments.args.first(), Some(syn::GenericArgument::Lifetime(lifetime)) if lifetime.ident == "_") =>
        {
            Ok(())
        }
        arguments => Err(syn::Error::new(
            arguments.span(),
            "ToolContext must use the current call lifetime: ToolContext<'_>",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conditional_attributes_keep_only_recursive_compilation_gates() {
        let input: syn::Meta = parse_quote!(cfg_attr(
            all(),
            inline,
            cfg_attr(any(), cfg(feature = "nested"), tracing::instrument),
            cfg(test)
        ));
        let filtered = configuration_meta(&input).unwrap().unwrap();
        let expected: syn::Meta = parse_quote!(cfg_attr(
            all(),
            cfg_attr(any(), cfg(feature = "nested")),
            cfg(test)
        ));
        assert_eq!(quote!(#filtered).to_string(), quote!(#expected).to_string());
        assert!(
            configuration_meta(&parse_quote!(cfg_attr(all(), inline)))
                .unwrap()
                .is_none()
        );
        let expansion = expand(
            quote!(crate = "::frona_harness"),
            quote!(
                #[cfg_attr(all(), inline)]
                async fn example(value: String) -> Result<String, Error> {
                    Ok(value)
                }
            ),
        )
        .unwrap();
        let file: syn::File = syn::parse2(expansion).unwrap();
        let syn::Item::Fn(function) = &file.items[0] else {
            panic!("function was not preserved")
        };
        assert!(
            function
                .attrs
                .iter()
                .any(|attribute| attribute.path().is_ident("cfg_attr"))
        );
        for item in &file.items[1..] {
            let attributes = match item {
                syn::Item::Struct(item) => &item.attrs,
                syn::Item::Impl(item) => &item.attrs,
                _ => panic!("unexpected generated item"),
            };
            assert!(
                !attributes
                    .iter()
                    .any(|attribute| attribute.path().is_ident("cfg_attr"))
            );
        }
    }

    #[test]
    fn rejects_unsupported_signatures_with_actionable_messages() {
        for (source, message) in [
            (
                "fn sync(x: String) -> Result<String, E> { todo!() }",
                "safe async",
            ),
            (
                "async fn generic<T>(x: T) -> Result<T, E> { todo!() }",
                "without generic",
            ),
            (
                "async fn borrowed(x: &str) -> Result<String, E> { todo!() }",
                "owned concrete",
            ),
            (
                "async fn pair((x,y): (u32,u32)) -> Result<u32, E> { todo!() }",
                "named identifiers",
            ),
            (
                "async fn mutable(a: &mut ToolContext<'_>) -> Result<(), E> { todo!() }",
                "not &mut",
            ),
            (
                "async fn static_context(a: ToolContext<'static>) -> Result<(), E> { todo!() }",
                "current call lifetime",
            ),
            (
                "async fn static_ref(a: &'static ToolContext<'_>) -> Result<(), E> { todo!() }",
                "current call",
            ),
            (
                "async fn double(a: ToolContext<'_>, b: ToolContext<'_>) -> Result<(), E> { todo!() }",
                "only one",
            ),
        ] {
            let error = expand(quote!(), source.parse().unwrap()).unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
        }
    }
}
