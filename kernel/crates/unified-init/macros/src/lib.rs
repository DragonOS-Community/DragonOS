extern crate alloc;

extern crate quote;
use proc_macro::TokenStream;
use quote::quote;
use syn::{
    __private::ToTokens,
    parse::{self, Parse, ParseStream},
    spanned::Spanned,
    ItemFn, Path,
};
use uuid::Uuid;

/// 统一初始化宏,
/// 用于将函数注册到统一初始化列表中
///
/// ## 用法
///
/// ```rust
/// use system_error::SystemError;
/// use unified_init::define_unified_initializer_slice;
/// use unified_init_macros::unified_init;
///
/// /// 初始化函数都将会被放到这个列表中
/// define_unified_initializer_slice!(INITIALIZER_LIST);
///
/// #[unified_init(INITIALIZER_LIST)]
/// fn init1() -> Result<(), SystemError> {
///    Ok(())
/// }
///
/// #[unified_init(INITIALIZER_LIST)]
/// fn init2() -> Result<(), SystemError> {
///    Ok(())
/// }
///
/// fn main() {
///     assert_eq!(INITIALIZER_LIST.len(), 2);
/// }
///
/// ```
#[proc_macro_attribute]
pub fn unified_init(args: TokenStream, input: TokenStream) -> TokenStream {
    do_unified_init(args, input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

fn do_unified_init(args: TokenStream, input: TokenStream) -> syn::Result<proc_macro2::TokenStream> {
    // 解析属性数
    let attr_arg = syn::parse::<UnifiedInitArg>(args)?;
    // 获取当前函数
    let function = syn::parse::<ItemFn>(input)?;
    // 检查函数签名
    check_function_signature(&function)?;

    // 添加#[::linkme::distributed_slice(attr_args.initializer_instance)]属性
    let target_slice = attr_arg.initializer_instance.get_ident().unwrap();

    // 在旁边添加一个UnifiedInitializer
    let initializer =
        generate_unified_initializer(&function, target_slice, function.sig.ident.to_string())?;

    // 拼接
    let mut output = proc_macro2::TokenStream::new();
    output.extend(function.into_token_stream());
    output.extend(initializer);

    Ok(output)
}

/// 检查函数签名是否满足要求
/// 函数签名应该为
///
/// ```rust
/// use system_error::SystemError;
/// fn xxx() -> Result<(), SystemError> {
///     Ok(())
/// }
/// ```
fn check_function_signature(function: &ItemFn) -> syn::Result<()> {
    // 检查函数签名
    if !function.sig.inputs.is_empty() {
        return Err(syn::Error::new(
            function.sig.inputs.span(),
            "Expected no arguments",
        ));
    }

    if let syn::ReturnType::Type(_, ty) = &function.sig.output {
        // 确认返回类型为 Result<(), SystemError>
        // 解析类型

        let output_type: syn::Type = syn::parse2(ty.clone().into_token_stream())?;

        // 检查类型是否为 Result<(), SystemError>
        if let syn::Type::Path(type_path) = output_type {
            if type_path.path.segments.last().unwrap().ident == "Result" {
                // 检查泛型参数，看看是否满足 Result<(), SystemError>
                if let syn::PathArguments::AngleBracketed(generic_args) =
                    type_path.path.segments.last().unwrap().arguments.clone()
                {
                    if generic_args.args.len() != 2 {
                        return Err(syn::Error::new(
                            generic_args.span(),
                            "Expected two generic arguments",
                        ));
                    }

                    // 检查第一个泛型参数是否为()
                    if let syn::GenericArgument::Type(type_arg) = generic_args.args.first().unwrap()
                    {
                        if let syn::Type::Tuple(tuple) = type_arg {
                            if !tuple.elems.is_empty() {
                                return Err(syn::Error::new(tuple.span(), "Expected empty tuple"));
                            }
                        } else {
                            return Err(syn::Error::new(type_arg.span(), "Expected empty tuple"));
                        }
                    } else {
                        return Err(syn::Error::new(
                            generic_args.span(),
                            "Expected first generic argument to be a type",
                        ));
                    }

                    // 检查第二个泛型参数是否为SystemError
                    if let syn::GenericArgument::Type(type_arg) = generic_args.args.last().unwrap()
                    {
                        if let syn::Type::Path(type_path) = type_arg {
                            if type_path.path.segments.last().unwrap().ident == "SystemError" {
                                // 类型匹配，返回 Ok
                                return Ok(());
                            }
                        }
                    } else {
                        return Err(syn::Error::new(
                            generic_args.span(),
                            "Expected second generic argument to be a type",
                        ));
                    }

                    return Err(syn::Error::new(
                        generic_args.span(),
                        "Expected second generic argument to be SystemError",
                    ));
                }

                return Ok(());
            }
        }
    }

    Err(syn::Error::new(
        function.sig.output.span(),
        "Expected -> Result<(), SystemError>",
    ))
}

/// 生成UnifiedInitializer全局变量
fn generate_unified_initializer(
    function: &ItemFn,
    target_slice: &syn::Ident,
    raw_initializer_name: String,
) -> syn::Result<proc_macro2::TokenStream> {
    let initializer_name = format!(
        "unified_initializer_{}_{}",
        raw_initializer_name,
        &Uuid::new_v4().to_simple().to_string().to_ascii_uppercase()[..8]
    )
    .to_ascii_uppercase();

    // 获取函数的全名
    let initializer_name_ident = syn::Ident::new(&initializer_name, function.sig.ident.span());

    let function_ident = &function.sig.ident;

    // 生成UnifiedInitializer
    let initializer = quote! {
        #[::linkme::distributed_slice(#target_slice)]
        static #initializer_name_ident: unified_init::UnifiedInitializer = ::unified_init::UnifiedInitializer::new(#raw_initializer_name, &(#function_ident as ::unified_init::UnifiedInitFunction));
    };

    Ok(initializer)
}

struct UnifiedInitArg {
    initializer_instance: Path,
}

impl Parse for UnifiedInitArg {
    fn parse(input: ParseStream) -> parse::Result<Self> {
        let mut initializer_instance = None;

        while !input.is_empty() {
            if initializer_instance.is_some() {
                return Err(parse::Error::new(
                    input.span(),
                    "Expected exactly one initializer instance",
                ));
            }
            // 解析Ident
            let ident = input.parse::<syn::Ident>()?;

            // 将Ident转换为Path
            let initializer = syn::Path::from(ident);

            initializer_instance = Some(initializer);
        }

        if initializer_instance.is_none() {
            return Err(parse::Error::new(
                input.span(),
                "Expected exactly one initializer instance",
            ));
        }

        // 判断是否为标识符
        if initializer_instance.as_ref().unwrap().get_ident().is_none() {
            return Err(parse::Error::new(
                initializer_instance.span(),
                "Expected identifier",
            ));
        }

        Ok(UnifiedInitArg {
            initializer_instance: initializer_instance.unwrap(),
        })
    }
}
