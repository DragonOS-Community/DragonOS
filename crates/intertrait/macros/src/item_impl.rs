use hashbrown::HashSet;
use proc_macro2::TokenStream;
use quote::{quote, quote_spanned, ToTokens};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::Token;
use syn::{
    AngleBracketedGenericArguments, Binding, GenericArgument, ImplItem, ItemImpl, Path,
    PathArguments,
};
use PathArguments::AngleBracketed;

use crate::args::Flag;
use crate::gen_caster::generate_caster;

pub fn process(flags: &HashSet<Flag>, input: ItemImpl) -> TokenStream {
    let ItemImpl {
        ref self_ty,
        ref trait_,
        ref items,
        ..
    } = input;

    let generated = match trait_ {
        None => quote_spanned! {
            self_ty.span() => compile_error!("#[cast_to] should only be on an impl of a trait");
        },
        Some(trait_) => match trait_ {
            (Some(bang), _, _) => quote_spanned! {
                bang.span() => compile_error!("#[cast_to] is not for !Trait impl");
            },
            (None, path, _) => {
                let path = fully_bound_trait(path, items);
                generate_caster(self_ty, &path, flags.contains(&Flag::Sync))
            }
        },
    };

    quote! {
        #input
        #generated
    }
}

fn fully_bound_trait(path: &Path, items: &[ImplItem]) -> impl ToTokens {
    let bindings = items
        .iter()
        .filter_map(|item| {
            if let ImplItem::Type(assoc_ty) = item {
                Some(GenericArgument::Binding(Binding {
                    ident: assoc_ty.ident.to_owned(),
                    eq_token: Default::default(),
                    ty: assoc_ty.ty.to_owned(),
                }))
            } else {
                None
            }
        })
        .collect::<Punctuated<_, Token![,]>>();

    let mut path = path.clone();

    if bindings.is_empty() {
        return path;
    }

    if let Some(last) = path.segments.last_mut() {
        match &mut last.arguments {
            PathArguments::None => {
                last.arguments = AngleBracketed(AngleBracketedGenericArguments {
                    colon2_token: None,
                    lt_token: Default::default(),
                    args: bindings,
                    gt_token: Default::default(),
                })
            }
            AngleBracketed(args) => args.args.extend(bindings),
            _ => {}
        }
    }
    path
}
