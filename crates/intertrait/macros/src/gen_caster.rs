use core::str::from_utf8_unchecked;

use proc_macro2::TokenStream;
use uuid::adapter::Simple;
use uuid::Uuid;

use quote::format_ident;
use quote::quote;
use quote::ToTokens;

pub fn generate_caster(ty: &impl ToTokens, trait_: &impl ToTokens, sync: bool) -> TokenStream {
    let mut fn_buf = [0u8; FN_BUF_LEN];
    let fn_ident = format_ident!("{}", new_fn_name(&mut fn_buf));
    // 生成从dyn trait转换为具体类型结构体ty的caster
    let new_caster = if sync {
        quote! {
            ::intertrait::Caster::<dyn #trait_>::new_sync(
                |from| from.downcast_ref::<#ty>().unwrap(),
                |from| from.downcast_mut::<#ty>().unwrap(),
                |from| from.downcast::<#ty>().unwrap(),
                |from| from.downcast::<#ty>().unwrap(),
                |from| from.downcast::<#ty>().unwrap()
            )
        }
    } else {
        quote! {
            ::intertrait::Caster::<dyn #trait_>::new(
                |from| from.downcast_ref::<#ty>().unwrap(),
                |from| from.downcast_mut::<#ty>().unwrap(),
                |from| from.downcast::<#ty>().unwrap(),
                |from| from.downcast::<#ty>().unwrap(),
            )
        }
    };

    // 由于过程宏是在预编译期执行的，这里的target_os是linux。
    // 编译完成的proc macro会交给下一阶段进行编译，因此，#[cfg(target_os)]会在下一阶段生效。
    // 我们必须在预处理阶段把两种代码的token stream都生成出来，然后在下一阶段选择性地使用其中一种。
    quote! {

        #[cfg(not(target_os = "none"))]
        #[::linkme::distributed_slice(::intertrait::CASTERS)]
        fn #fn_ident() -> (::std::any::TypeId, ::intertrait::BoxedCaster) {
            (::std::any::TypeId::of::<#ty>(), Box::new(#new_caster))
        }

        #[cfg(target_os = "none")]
        #[::linkme::distributed_slice(::intertrait::CASTERS)]
        fn #fn_ident() -> (::core::any::TypeId, ::intertrait::BoxedCaster) {
            (::core::any::TypeId::of::<#ty>(), alloc::boxed::Box::new(#new_caster))
        }
    }
}

const FN_PREFIX: &[u8] = b"__";
const FN_BUF_LEN: usize = FN_PREFIX.len() + Simple::LENGTH;

fn new_fn_name(buf: &mut [u8]) -> &str {
    buf[..FN_PREFIX.len()].copy_from_slice(FN_PREFIX);
    Uuid::new_v4()
        .to_simple()
        .encode_lower(&mut buf[FN_PREFIX.len()..]);
    unsafe { from_utf8_unchecked(&buf[..FN_BUF_LEN]) }
}
