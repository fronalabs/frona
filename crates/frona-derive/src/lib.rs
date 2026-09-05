use heck::ToSnakeCase;
use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{DeriveInput, Expr, ImplItem, ItemImpl, Lit, Meta, Token, parse_macro_input};

mod migration;
mod parameter_metadata;

/// Generate protocol metadata from serialized settings fields.
#[proc_macro_derive(ParameterMetadata, attributes(parameter))]
pub fn derive_parameter_metadata(input: TokenStream) -> TokenStream {
    parameter_metadata::expand(parse_macro_input!(input as DeriveInput))
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// `#[channel(id = "...", from = ConfigType)]` - `from` is optional; when
/// omitted, the adapter struct itself is the deserialisation target.
#[proc_macro_derive(ChannelFactory, attributes(channel))]
pub fn derive_channel_factory(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let adapter_name = &input.ident;
    let factory_name = quote::format_ident!("{}Factory", adapter_name);

    let attrs = parse_channel_attrs(&input.attrs)
        .unwrap_or_else(|err| panic!("#[channel(...)] attribute parse error: {err}"));
    let id = attrs.id;
    let manifest_rel_path = format!("/../../resources/channels/{id}.yaml");

    let build_adapter = match attrs.from {
        Some(config_ty) => quote! {
            let cfg: #config_ty = serde_json::from_value(config).map_err(|e| {
                frona::core::error::AppError::Validation(
                    format!("invalid {} config: {}", #id, e),
                )
            })?;
            Ok(Box::new(<#adapter_name as ::std::convert::From<#config_ty>>::from(cfg)))
        },
        None => quote! {
            let adapter: #adapter_name = serde_json::from_value(config).map_err(|e| {
                frona::core::error::AppError::Validation(
                    format!("invalid {} config: {}", #id, e),
                )
            })?;
            Ok(Box::new(adapter))
        },
    };

    let expanded = quote! {
        #[automatically_derived]
        #[doc = concat!("Generated factory for `", stringify!(#adapter_name),
            "`. Registers the adapter under provider id `", #id,
            "` and loads its manifest from `resources/channels/", #id, ".yaml`.")]
        pub struct #factory_name;

        #[automatically_derived]
        #[async_trait::async_trait]
        impl frona::chat::channel::models::ChannelFactory for #factory_name {
            fn manifest(&self) -> frona::chat::channel::models::ChannelManifest {
                static M: std::sync::OnceLock<frona::chat::channel::models::ChannelManifest>
                    = std::sync::OnceLock::new();
                M.get_or_init(|| {
                    serde_yaml::from_str(include_str!(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        #manifest_rel_path,
                    )))
                        .unwrap_or_else(|e| panic!(
                            "invalid manifest yaml for channel {}: {}", #id, e))
                }).clone()
            }

            fn create(
                &self,
                config: serde_json::Value,
            ) -> Result<
                Box<dyn frona::chat::channel::models::ChannelAdapter>,
                frona::core::error::AppError,
            > {
                #build_adapter
            }
        }
    };

    expanded.into()
}

struct ChannelAttrs {
    id: String,
    from: Option<syn::Type>,
}

fn parse_channel_attrs(attrs: &[syn::Attribute]) -> syn::Result<ChannelAttrs> {
    let attr = attrs
        .iter()
        .find(|a| a.path().is_ident("channel"))
        .ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "#[channel(id = \"...\")] attribute is required",
            )
        })?;

    let mut id: Option<String> = None;
    let mut from: Option<syn::Type> = None;

    attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("id") {
            let value: syn::LitStr = meta.value()?.parse()?;
            id = Some(value.value());
        } else if meta.path.is_ident("from") {
            let value: syn::Type = meta.value()?.parse()?;
            from = Some(value);
        } else {
            return Err(meta.error("unknown #[channel] argument; expected one of: id, from"));
        }
        Ok(())
    })?;

    Ok(ChannelAttrs {
        id: id.ok_or_else(|| {
            syn::Error::new(attr.span(), "#[channel] missing required `id = \"...\"`")
        })?,
        from,
    })
}

#[proc_macro_attribute]
pub fn migration(attr: TokenStream, item: TokenStream) -> TokenStream {
    migration::migration(attr, item)
}

#[proc_macro_derive(Entity, attributes(entity))]
pub fn derive_entity(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let table = input
        .attrs
        .iter()
        .find_map(|attr| {
            if !attr.path().is_ident("entity") {
                return None;
            }
            let nested: Meta = attr.parse_args().ok()?;
            if let Meta::NameValue(nv) = nested
                && nv.path.is_ident("table")
                && let Expr::Lit(lit) = &nv.value
                && let Lit::Str(s) = &lit.lit
            {
                return Some(s.value());
            }
            None
        })
        .expect("#[entity(table = \"...\")] attribute is required");

    let expanded = quote! {
        impl frona::core::repository::Entity for #name {
            fn table() -> &'static str {
                #table
            }

            fn id(&self) -> &str {
                &self.id
            }
        }
    };

    expanded.into()
}

struct AgentToolArgs {
    name: Option<String>,
    files: Option<Vec<String>>,
    /// Subdirectory under `tools/` holding the definition `.md`(s). Lets a tool
    /// keep its prompt in `tools/<dir>/<name>.md` without restating the filename.
    dir: Option<String>,
}

impl Parse for AgentToolArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut name = None;
        let mut files = None;
        let mut dir = None;

        while !input.is_empty() {
            let ident: syn::Ident = input.parse()?;
            match ident.to_string().as_str() {
                "name" => {
                    let _eq: Token![=] = input.parse()?;
                    let lit: syn::LitStr = input.parse()?;
                    name = Some(lit.value());
                }
                "dir" => {
                    let _eq: Token![=] = input.parse()?;
                    let lit: syn::LitStr = input.parse()?;
                    dir = Some(lit.value());
                }
                "files" => {
                    let content;
                    syn::parenthesized!(content in input);
                    let mut file_list = Vec::new();
                    while !content.is_empty() {
                        let lit: syn::LitStr = content.parse()?;
                        file_list.push(lit.value());
                        if !content.is_empty() {
                            let _comma: Token![,] = content.parse()?;
                        }
                    }
                    files = Some(file_list);
                }
                other => {
                    return Err(syn::Error::new(
                        ident.span(),
                        format!("unknown agent_tool argument: {other}"),
                    ));
                }
            }

            if !input.is_empty() {
                let _comma: Token![,] = input.parse()?;
            }
        }

        Ok(AgentToolArgs { name, files, dir })
    }
}

fn derive_tool_name(struct_name: &str) -> String {
    let base = struct_name.strip_suffix("Tool").unwrap_or(struct_name);
    base.to_snake_case()
}

#[proc_macro_attribute]
pub fn agent_tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as AgentToolArgs);
    let input = parse_macro_input!(item as ItemImpl);

    let struct_type = &input.self_ty;
    let struct_name = quote!(#struct_type).to_string();

    let tool_name = args.name.unwrap_or_else(|| derive_tool_name(&struct_name));

    let file_names = args.files.unwrap_or_else(|| vec![tool_name.clone()]);

    // Optional `tools/<dir>/` prefix; defaults to `tools/` (backwards-compatible).
    let dir_prefix = args
        .dir
        .map(|d| d.trim_matches('/').to_string())
        .filter(|d| !d.is_empty())
        .map(|d| format!("{d}/"))
        .unwrap_or_default();

    let definitions_body = if file_names.len() == 1 {
        let path = format!("tools/{dir_prefix}{}.md", file_names[0]);
        quote! {
            crate::tool::load_tool_definition_with_vars(&self.prompts, #path, &self.definition_vars())
                .into_iter()
                .collect()
        }
    } else {
        let stmts = file_names.iter().map(|f| {
            let path = format!("tools/{dir_prefix}{f}.md");
            quote! {
                if let Some(d) = crate::tool::load_tool_definition_with_vars(&self.prompts, #path, &self.definition_vars()) {
                    defs.push(d);
                }
            }
        });
        quote! {
            let mut defs = Vec::new();
            #(#stmts)*
            defs
        }
    };

    let user_items: Vec<&ImplItem> = input.items.iter().collect();

    let expanded = quote! {
        #[async_trait::async_trait]
        impl crate::tool::AgentTool for #struct_type {
            fn name(&self) -> &str {
                #tool_name
            }

            fn definitions(&self) -> Vec<crate::tool::ToolDefinition> {
                #definitions_body
            }

            #(#user_items)*
        }
    };

    expanded.into()
}
