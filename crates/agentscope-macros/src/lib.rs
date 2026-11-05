//! `#[tool]` — 把一个普通函数声明成 agent 工具（对齐 Java `@Tool`）。
//!
//! 被标注的函数保持原样（仍然可以直接调用），宏另外生成一个构造函数
//! `{函数名}_tool()`，返回 [`agentscope_core::tool::FunctionTool`]：
//!
//! ```rust
//! use agentscope_macros::tool;
//!
//! /// 把两个数相加
//! #[tool]
//! fn add(a: i64, b: i64) -> String {
//!     format!("{}", a + b)
//! }
//!
//! // 生成的签名等价于：
//! // fn add_tool() -> agentscope_core::tool::FunctionTool
//! ```
//!
//! # 参数 → JSON Schema
//!
//! | Rust 类型 | JSON Schema |
//! | --- | --- |
//! | `String` / `&str` | `{"type":"string"}` |
//! | `i8`..`i64` / `u8`..`u64` / `usize` / `isize` | `{"type":"integer"}` |
//! | `f32` / `f64` | `{"type":"number"}` |
//! | `bool` | `{"type":"boolean"}` |
//! | `Vec<T>` | `{"type":"array","items": <T>}` |
//! | `Option<T>` | `<T>` 的 schema，且**不**进 `required` |
//! | `serde_json::Value` | `{}`（任意 JSON 值） |
//!
//! 其它类型会在编译期报错（中文提示）。
//!
//! # 属性
//!
//! - `#[tool(name = "...")]`：工具名，缺省用函数名。
//! - `#[tool(description = "...")]`：工具说明，缺省取函数 `///` 文档的第一段，
//!   再缺省用函数名。
//! - `#[tool(read_only)]`：无副作用（映射到 `FunctionTool::read_only`）。
//! - `#[tool(strict)]`：strict schema（递归注入 `additionalProperties: false`）。
//! - `#[tool(concurrency_safe = false)]`：禁止与本工具的其他调用并发执行。
//! - 参数上的 `#[tool(description = "...")]`：写进该参数的 `description`。
//!
//! # 返回值
//!
//! 支持 `String` / `&str`（按成功结果返回文本）以及 `Result<String, E>`
//! （`Err` 转成错误结果，模型可据此重试）。`FunctionTool` 的 handler 是同步闭包，
//! 因此**不支持 `async fn`**（会在编译期报错）。
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{
    parse_macro_input, FnArg, GenericArgument, ItemFn, LitStr, Pat, PathArguments, ReturnType, Type,
};

#[derive(Default)]
struct ToolConfig {
    name: Option<String>,
    description: Option<String>,
    read_only: bool,
    strict: bool,
    concurrency_safe: Option<bool>,
}

impl ToolConfig {
    fn parse(&mut self, meta: syn::meta::ParseNestedMeta<'_>) -> syn::Result<()> {
        if meta.path.is_ident("name") {
            self.name = Some(meta.value()?.parse::<LitStr>()?.value());
        } else if meta.path.is_ident("description") {
            self.description = Some(meta.value()?.parse::<LitStr>()?.value());
        } else if meta.path.is_ident("read_only") {
            self.read_only = true;
        } else if meta.path.is_ident("strict") {
            self.strict = true;
        } else if meta.path.is_ident("concurrency_safe") {
            self.concurrency_safe = Some(meta.value()?.parse::<syn::LitBool>()?.value());
        } else {
            return Err(meta.error(
                "unknown #[tool] option; expected name / description / read_only / strict / concurrency_safe",
            ));
        }
        Ok(())
    }
}

/// 参数类型分类结果：`(ParamKind 表达式, 是否 Option, Option 的 T)`。
struct ParamType {
    kind: TokenStream2,
    optional: bool,
    inner: Option<Type>,
    /// 声明类型是 `&str`：借用类型无法直接反序列化（`DeserializeOwned` 不允许借用），
    /// 需要先取出 `String` 再借用。
    borrowed: bool,
}

/// 判断类型是否为 `&str`。
fn is_borrowed_str(ty: &Type) -> bool {
    match ty {
        Type::Reference(reference) => matches!(&*reference.elem, Type::Path(p)
            if p.path.segments.last().is_some_and(|s| s.ident == "str")),
        _ => false,
    }
}

fn classify(ty: &Type) -> syn::Result<ParamType> {
    let unsupported = || {
        syn::Error::new(
            ty.span(),
            "不支持的工具参数类型：请使用 String / &str / 整数 / 浮点数 / bool / \
             Vec<T> / Option<T> / serde_json::Value",
        )
    };

    // `&str`：schema 与 `String` 相同，但取值走「先取 String 再借用」。
    // 注意它不是 `Type::Path`，必须在下面的路径匹配之前判断。
    if is_borrowed_str(ty) {
        return Ok(ParamType {
            kind: quote!(::agentscope_core::tool::macros::ParamKind::String),
            optional: false,
            inner: None,
            borrowed: true,
        });
    }

    let Type::Path(path) = ty else {
        return Err(unsupported());
    };
    let Some(segment) = path.path.segments.last() else {
        return Err(unsupported());
    };
    let name = segment.ident.to_string();

    if name == "Option" {
        let PathArguments::AngleBracketed(args) = &segment.arguments else {
            return Err(unsupported());
        };
        let Some(GenericArgument::Type(inner)) = args.args.first() else {
            return Err(unsupported());
        };
        let inner_class = classify(inner)?;
        if inner_class.optional {
            return Err(syn::Error::new(
                inner.span(),
                "不支持嵌套的 Option<T>：工具参数最多一层可选",
            ));
        }
        if inner_class.borrowed {
            return Err(syn::Error::new(
                inner.span(),
                "不支持 Option<&str>：可选参数请改用 Option<String>",
            ));
        }
        return Ok(ParamType {
            kind: inner_class.kind,
            optional: true,
            inner: Some(inner.clone()),
            borrowed: false,
        });
    }

    if name == "Vec" {
        let PathArguments::AngleBracketed(args) = &segment.arguments else {
            return Err(unsupported());
        };
        let Some(GenericArgument::Type(inner)) = args.args.first() else {
            return Err(unsupported());
        };
        let inner_class = classify(inner)?;
        if inner_class.optional {
            return Err(syn::Error::new(
                inner.span(),
                "不支持 Vec<Option<T>>：工具参数最多一层可选",
            ));
        }
        if inner_class.borrowed {
            return Err(syn::Error::new(
                inner.span(),
                "不支持 Vec<&str>：数组元素请改用 Vec<String>",
            ));
        }
        let inner_kind = inner_class.kind;
        return Ok(ParamType {
            kind: quote!(::agentscope_core::tool::macros::ParamKind::Array(
                ::std::boxed::Box::new(#inner_kind)
            )),
            optional: false,
            inner: None,
            borrowed: false,
        });
    }

    let kind = match name.as_str() {
        "String" => quote!(::agentscope_core::tool::macros::ParamKind::String),
        "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "usize" | "isize" => {
            quote!(::agentscope_core::tool::macros::ParamKind::Integer)
        }
        "f32" | "f64" => quote!(::agentscope_core::tool::macros::ParamKind::Number),
        "bool" => quote!(::agentscope_core::tool::macros::ParamKind::Boolean),
        // `serde_json::Value`：任意 JSON 值（路径可能带 crate 前缀，按尾段识别）。
        "Value" => quote!(::agentscope_core::tool::macros::ParamKind::Any),
        _ => return Err(unsupported()),
    };
    Ok(ParamType {
        kind,
        optional: false,
        inner: None,
        borrowed: false,
    })
}

/// 取 `Result<T, E>` 的 `T`；不是 `Result` 时返回 `None`。
fn result_ok_type(ty: &Type) -> Option<Type> {
    let Type::Path(path) = ty else { return None };
    let segment = path.path.segments.last()?;
    if segment.ident != "Result" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    match args.args.first() {
        Some(GenericArgument::Type(ok)) => Some(ok.clone()),
        _ => None,
    }
}

fn is_string(ty: &Type) -> bool {
    matches!(ty, Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "String"))
}

/// 取函数文档注释的第一段（遇到空行即停）。
fn doc_summary(attrs: &[syn::Attribute]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        let syn::Meta::NameValue(name_value) = &attr.meta else {
            continue;
        };
        let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(text),
            ..
        }) = &name_value.value
        else {
            continue;
        };
        let line = text.value();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        lines.push(trimmed.to_string());
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join(" "))
    }
}

/// 把一个函数声明成 agent 工具。
///
/// 见[模块文档](self)。
#[proc_macro_attribute]
pub fn tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut config = ToolConfig::default();
    let config_parser = syn::meta::parser(|meta| config.parse(meta));
    parse_macro_input!(attr with config_parser);

    let mut function = parse_macro_input!(item as ItemFn);

    if function.sig.asyncness.is_some() {
        return syn::Error::new(
            function.sig.span(),
            "`#[tool]` does not support `async fn`: the tool handler is a synchronous closure",
        )
        .to_compile_error()
        .into();
    }
    if !function.sig.generics.params.is_empty() {
        return syn::Error::new(
            function.sig.generics.span(),
            "`#[tool]` does not support generic functions",
        )
        .to_compile_error()
        .into();
    }
    if let Some(variadic) = &function.sig.variadic {
        return syn::Error::new(
            variadic.span(),
            "`#[tool]` does not support variadic functions",
        )
        .to_compile_error()
        .into();
    }

    let function_ident = function.sig.ident.clone();
    let constructor_ident = format_ident!("{}_tool", function_ident);
    let visibility = function.vis.clone();

    // 防御性地去掉函数级的 `#[tool]`：宏展开结果里不能再带上它，
    // 否则重新展开会再次触发本宏。
    function
        .attrs
        .retain(|attribute| !attribute.path().is_ident("tool"));

    // 每个参数：参数类型 → (schema spec, 取值语句, 实参名)。
    let mut specs = Vec::new();
    let mut extracts: Vec<TokenStream2> = Vec::new();
    let mut arguments = Vec::new();

    for input in function.sig.inputs.iter_mut() {
        let FnArg::Typed(pat_type) = input else {
            return syn::Error::new(input.span(), "`#[tool]` cannot be applied to methods")
                .to_compile_error()
                .into();
        };
        let Pat::Ident(pat_ident) = pat_type.pat.as_ref() else {
            return syn::Error::new(
                pat_type.pat.span(),
                "tool parameters must be plain identifiers",
            )
            .to_compile_error()
            .into();
        };
        let argument = pat_ident.ident.clone();
        let argument_type = (*pat_type.ty).clone();

        // 参数级 `#[tool(description = "...")]`：消费掉，不能留在函数签名上。
        let mut description: Option<String> = None;
        let mut kept = Vec::new();
        for attribute in &pat_type.attrs {
            if attribute.path().is_ident("tool") {
                let parsed = attribute.parse_nested_meta(|meta| {
                    if meta.path.is_ident("description") {
                        description = Some(meta.value()?.parse::<LitStr>()?.value());
                        Ok(())
                    } else {
                        Err(meta
                            .error("tool parameters only accept #[tool(description = \"...\")]"))
                    }
                });
                if let Err(error) = parsed {
                    return error.to_compile_error().into();
                }
            } else {
                kept.push(attribute.clone());
            }
        }
        pat_type.attrs = kept;

        let ParamType {
            kind,
            optional,
            inner,
            borrowed,
        } = match classify(&argument_type) {
            Ok(classified) => classified,
            Err(error) => return error.to_compile_error().into(),
        };

        let name = LitStr::new(&argument.to_string(), argument.span());
        let describe = description.map(|text| {
            let literal = LitStr::new(&text, argument.span());
            quote!(.describe(#literal))
        });
        let mark_optional = optional.then(|| quote!(.optional()));
        specs.push(quote! {
            ::agentscope_core::tool::macros::ParamSpec::new(#name, #kind)
                #describe
                #mark_optional
        });

        // `&str` 参数不能直接反序列化（`DeserializeOwned` 不允许借用）：
        // 先取出 `String` 再借出 `&str`；被借用的 String 活到闭包体结束，
        // 因此借用覆盖整个调用过程。
        let read = if borrowed {
            let owned = format_ident!("__tool_owned_{}", argument);
            quote! {
                let #owned: ::std::string::String =
                    match ::agentscope_core::tool::macros::required_arg::<
                        ::std::string::String,
                    >(&args, #name) {
                        ::std::result::Result::Ok(value) => value,
                        ::std::result::Result::Err(message) => {
                            return ::std::result::Result::Ok(
                                ::agentscope_core::tool::macros::error_result(message),
                            )
                        }
                    };
                let #argument: &::std::primitive::str = &#owned;
            }
        } else if let Some(inner) = inner {
            let inner = inner;
            quote! {
                let #argument: #argument_type =
                    match ::agentscope_core::tool::macros::optional_arg::<#inner>(&args, #name) {
                        ::std::result::Result::Ok(value) => value,
                        ::std::result::Result::Err(message) => {
                            return ::std::result::Result::Ok(
                                ::agentscope_core::tool::macros::error_result(message),
                            )
                        }
                    };
            }
        } else {
            quote! {
                let #argument: #argument_type =
                    match ::agentscope_core::tool::macros::required_arg::<#argument_type>(&args, #name) {
                        ::std::result::Result::Ok(value) => value,
                        ::std::result::Result::Err(message) => {
                            return ::std::result::Result::Ok(
                                ::agentscope_core::tool::macros::error_result(message),
                            )
                        }
                    };
            }
        };
        extracts.push(read);
        arguments.push(argument);
    }

    // 返回值：`String` / `&str` / `Result<String|&str, E>`。
    let body = match &function.sig.output {
        ReturnType::Default => {
            return syn::Error::new(
                function.sig.output.span(),
                "`#[tool]` functions must return `String` or `Result<String, E>`",
            )
            .to_compile_error()
            .into()
        }
        ReturnType::Type(_, ty) => {
            if is_string(ty) || is_borrowed_str(ty) {
                quote! {
                    ::std::result::Result::Ok(
                        ::agentscope_core::tool::macros::text_result(#function_ident(#(#arguments),*)),
                    )
                }
            } else if let Some(ok_type) = result_ok_type(ty) {
                if !is_string(&ok_type) && !is_borrowed_str(&ok_type) {
                    return syn::Error::new(
                        ok_type.span(),
                        "`#[tool]` only supports `Result<String, E>` / `Result<&str, E>`",
                    )
                    .to_compile_error()
                    .into();
                }
                quote! {
                    match #function_ident(#(#arguments),*) {
                        ::std::result::Result::Ok(value) => ::std::result::Result::Ok(
                            ::agentscope_core::tool::macros::text_result(value),
                        ),
                        ::std::result::Result::Err(error) => ::std::result::Result::Ok(
                            ::agentscope_core::tool::macros::error_result(::std::format!("{error}")),
                        ),
                    }
                }
            } else {
                return syn::Error::new(
                    ty.span(),
                    "`#[tool]` functions must return `String` or `Result<String, E>`",
                )
                .to_compile_error()
                .into();
            }
        }
    };

    let name = LitStr::new(
        &config.name.unwrap_or_else(|| function_ident.to_string()),
        function_ident.span(),
    );
    let description = LitStr::new(
        &config.description.unwrap_or_else(|| {
            doc_summary(&function.attrs).unwrap_or_else(|| function_ident.to_string())
        }),
        function_ident.span(),
    );

    let mut builders = TokenStream2::new();
    if config.read_only {
        builders.extend(quote!(.read_only(true)));
    }
    if config.strict {
        builders.extend(quote!(.strict(true)));
    }
    if config.concurrency_safe == Some(false) {
        builders.extend(quote!(.concurrency_safe(false)));
    }

    let expanded = quote! {
        #function

        #[allow(dead_code)]
        #visibility fn #constructor_ident() -> ::agentscope_core::tool::FunctionTool {
            ::agentscope_core::tool::FunctionTool::new(
                #name,
                #description,
                ::agentscope_core::tool::macros::object_schema(::std::vec![#(#specs),*]),
                ::std::boxed::Box::new(
                    move |args: ::agentscope_core::message::JsonMap,
                          _ctx: ::agentscope_core::tool::ToolExecutionContext|
                          -> ::agentscope_core::Result<::agentscope_core::message::ToolResultBlock> {
                        #(#extracts)*
                        #body
                    },
                ),
            )
            #builders
        }
    };
    expanded.into()
}
