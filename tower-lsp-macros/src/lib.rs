//! Internal procedural macros for [`tower-lsp`](https://docs.rs/tower-lsp).
//!
//! This crate should not be used directly.

extern crate proc_macro;

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    parse_macro_input, AttributeArgs, FnArg, ItemTrait, Lit, Meta, MetaNameValue, NestedMeta,
    ReturnType, TraitItem,
};

/// Macro for generating LSP server implementation from [`lsp-types`](https://docs.rs/lsp-types).
///
/// This procedural macro annotates the `tower_lsp::LanguageServer` trait and generates a
/// corresponding `register_lsp_methods()` function which registers all the methods on that trait
/// as RPC handlers.
#[proc_macro_attribute]
pub fn rpc(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr_args = parse_macro_input!(attr as AttributeArgs);

    match attr_args.as_slice() {
        [] => {}
        [NestedMeta::Meta(meta)] if meta.path().is_ident("name") => return item,
        _ => panic!("unexpected attribute arguments"),
    }

    let lang_server_trait = parse_macro_input!(item as ItemTrait);
    let method_calls = parse_method_calls(&lang_server_trait);
    let req_types_and_router_fn = gen_server_router(&lang_server_trait.ident, &method_calls);

    let tokens = quote! {
        #lang_server_trait
        #req_types_and_router_fn
    };

    tokens.into()
}

struct MethodCall<'a> {
    rpc_name: String,
    handler_name: &'a syn::Ident,
    params: Option<&'a syn::Type>,
    result: Option<&'a syn::Type>,
}

fn parse_method_calls(lang_server_trait: &ItemTrait) -> Vec<MethodCall> {
    let mut calls = Vec::new();

    for item in &lang_server_trait.items {
        let method = match item {
            TraitItem::Method(m) => m,
            _ => continue,
        };

        let rpc_name = method
            .attrs
            .iter()
            .filter_map(|attr| attr.parse_args::<Meta>().ok())
            .filter(|meta| meta.path().is_ident("name"))
            .find_map(|meta| match meta {
                Meta::NameValue(MetaNameValue {
                    lit: Lit::Str(lit), ..
                }) => Some(lit.value().trim_matches('"').to_owned()),
                _ => panic!("expected string literal for `#[rpc(name = ???)]` attribute"),
            })
            .expect("expected `#[rpc(name = \"foo\")]` attribute");

        let params = method.sig.inputs.iter().nth(1).and_then(|arg| match arg {
            FnArg::Typed(pat) => Some(&*pat.ty),
            _ => None,
        });

        let result = match &method.sig.output {
            ReturnType::Default => None,
            ReturnType::Type(_, ty) => Some(&**ty),
        };

        calls.push(MethodCall {
            rpc_name,
            handler_name: &method.sig.ident,
            params,
            result,
        });
    }

    calls
}

fn gen_server_router(trait_name: &syn::Ident, methods: &[MethodCall]) -> proc_macro2::TokenStream {
    let route_registrations: proc_macro2::TokenStream = methods
        .iter()
        .map(|method| {
            let rpc_name = &method.rpc_name;
            let handler = &method.handler_name;

            // NOTE: In a perfect world, we could simply loop over each `MethodCall` and emit
            // `router.register_method(#rpc_name, S::#handler);` for each. While such an approach
            // works for inherent async functions and methods, it breaks with `async-trait` methods
            // due to this unfortunate `rustc` bug:
            //
            // https://github.com/rust-lang/rust/issues/64552
            //
            // As a workaround, we wrap each `async-trait` method in a regular `async fn` before
            // passing it to `register_method`, as documented in this GitHub issue:
            //
            // https://github.com/dtolnay/async-trait/issues/167
            match (method.params, method.result) {
                (Some(params), Some(result)) => quote! {
                    async fn #handler<S: LanguageServer>(server: &S, params: #params) -> #result {
                        server.#handler(params).await
                    }
                    router.register_method(#rpc_name, #handler);
                },
                (None, Some(result)) => quote! {
                    async fn #handler<S: LanguageServer>(server: &S) -> #result {
                        server.#handler().await
                    }
                    router.register_method(#rpc_name, #handler);
                },
                (Some(params), None) => quote! {
                    async fn #handler<S: LanguageServer>(server: &S, params: #params) {
                        server.#handler(params).await
                    }
                    router.register_method(#rpc_name, #handler);
                },
                (None, None) => quote! {
                    async fn #handler<S: LanguageServer>(server: &S) {
                        server.#handler().await
                    }
                    router.register_method(#rpc_name, #handler);
                },
            }
        })
        .collect();

    quote! {
        mod generated {
            use std::sync::Arc;

            use lsp_types::*;
            use lsp_types::notification::*;
            use lsp_types::request::*;
            use serde_json::Value;

            use super::#trait_name;
            use crate::jsonrpc::{Result, Router};

            pub(crate) fn register_lsp_methods<S>(mut router: Router<S>) -> Router<S>
            where
                S: #trait_name,
            {
                #route_registrations
                router.register_method("exit", |_: &S| std::future::ready(()));
                router
            }
        }
    }
}
