use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, DeriveInput, Expr, Lit, Meta};

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
            if let Meta::NameValue(nv) = nested {
                if nv.path.is_ident("table") {
                    if let Expr::Lit(lit) = &nv.value {
                        if let Lit::Str(s) = &lit.lit {
                            return Some(s.value());
                        }
                    }
                }
            }
            None
        })
        .expect("#[entity(table = \"...\")] attribute is required");

    let expanded = quote! {
        impl frona::repository::Entity for #name {
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
