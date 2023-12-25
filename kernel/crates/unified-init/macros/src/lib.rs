extern crate alloc;

extern crate quote;
use proc_macro::TokenStream;
use syn::{
    __private::ToTokens,
    parse::{self, Parse, ParseStream},
    spanned::Spanned,
    ItemFn, Path,
};

#[proc_macro_attribute]
pub fn unified_init(args: TokenStream, input: TokenStream) -> TokenStream {
    do_unified_init(args, input)
        .unwrap_or_else(|e| e.to_compile_error().into())
        .into()
}

fn do_unified_init(args: TokenStream, input: TokenStream) -> syn::Result<proc_macro2::TokenStream> {
    // 解析属性数
    let attr_arg = syn::parse::<UnifiedInitArg>(args)?;
    // 在当前函数上方添加#[::linkme::distributed_slice(attr_args.initializer_instance)]属性

    // 获取当前函数
    let mut function = syn::parse::<ItemFn>(input)?;

    check_function_signature(&function)?;

    // 添加#[::linkme::distributed_slice(attr_args.initializer_instance)]属性
    let target_slice = attr_arg.initializer_instance.get_ident().unwrap();

    function.attrs.push(syn::parse_quote! {
        #[::linkme::distributed_slice(#target_slice)]
    });
    // 重新构造函数

    // let result_new_function = generate_proxy_function(&function, attr_args)?;

    // 生成函数
    return Ok(function.into_token_stream());
}

/// 检查函数签名是否满足要求
/// 函数签名应该为
///
/// ```rust
/// fn() -> Result<(), SystemError>
/// ```
fn check_function_signature(function: &ItemFn) -> syn::Result<()> {
    // 检查函数签名
    if function.sig.inputs.len() != 0 {
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
                            if tuple.elems.len() != 0 {
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
