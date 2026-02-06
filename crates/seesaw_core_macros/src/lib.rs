use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote, ToTokens};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{
    parse_macro_input, parse_quote, Attribute, Expr, FnArg, GenericArgument, Ident, Item, ItemFn,
    ItemMod, Lit, Meta, MetaList, MetaNameValue, Pat, Path, PathArguments, ReturnType, Signature,
    Token, Type, TypePath,
};

#[proc_macro_attribute]
pub fn effect(attr: TokenStream, item: TokenStream) -> TokenStream {
    let metas = parse_macro_input!(attr with Punctuated::<Meta, Token![,]>::parse_terminated);
    let input_fn = parse_macro_input!(item as ItemFn);

    match expand_effect(parse_effect_args(&metas), input_fn) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn effects(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut module = parse_macro_input!(item as ItemMod);
    match expand_effects_module(&mut module) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[derive(Clone)]
enum OnSpec {
    EventType(Path),
    Variants(Vec<Path>),
}

impl OnSpec {
    fn event_type(&self) -> syn::Result<Path> {
        match self {
            OnSpec::EventType(path) => Ok(path.clone()),
            OnSpec::Variants(variants) => {
                let Some(first) = variants.first() else {
                    return Err(syn::Error::new(
                        proc_macro2::Span::call_site(),
                        "on = [] is not supported",
                    ));
                };

                let base = variant_base_path(first)?;
                let base_key = path_key(&base);

                for variant in variants.iter().skip(1) {
                    let candidate = variant_base_path(variant)?;
                    if path_key(&candidate) != base_key {
                        return Err(syn::Error::new_spanned(
                            variant,
                            "all variants in on = [...] must belong to the same enum",
                        ));
                    }
                }

                Ok(base)
            }
        }
    }
}

#[derive(Default)]
struct EffectArgs {
    on: Option<OnSpec>,
    extract: Vec<Ident>,
    join: bool,
    queued: bool,
    id: Option<String>,
    dlq_terminal: Option<Path>,
    retry: Option<u32>,
    timeout_secs: Option<u64>,
    timeout_ms: Option<u64>,
    delay_secs: Option<u64>,
    delay_ms: Option<u64>,
    priority: Option<i32>,
    group: Option<String>,
}

struct ParamInfo {
    ident: Ident,
    ty: Type,
}

fn expand_effect(args: syn::Result<EffectArgs>, input_fn: ItemFn) -> syn::Result<TokenStream2> {
    let args = args?;
    let fn_ident = input_fn.sig.ident.clone();
    let wrapper_ident = format_ident!("__seesaw_effect_{}", fn_ident);
    let output_event_ty = effect_output_event_type(&input_fn.sig)?;

    if input_fn.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            &input_fn.sig.fn_token,
            "#[effect] requires an async function",
        ));
    }
    if !input_fn.sig.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input_fn.sig.generics,
            "#[effect] does not support generic functions",
        ));
    }

    if args.timeout_secs.is_some() && args.timeout_ms.is_some() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "cannot specify both timeout_secs and timeout_ms",
        ));
    }
    if args.delay_secs.is_some() && args.delay_ms.is_some() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "cannot specify both delay_secs and delay_ms",
        ));
    }
    if effect_requires_stable_id(&args) && args.id.is_none() && args.group.is_none() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "queued/durable #[effect] requires an explicit id = \"...\" (or group = \"...\")",
        ));
    }

    let on = args.on.clone().ok_or_else(|| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[effect] requires on = EventType or on = [Enum::Variant, ...]",
        )
    })?;
    let on_event_type = on.event_type()?;

    let (ctx_idx, deps_ty) = find_effect_context(&input_fn.sig)?;
    let params = collect_params(&input_fn.sig)?;
    let non_ctx_params: Vec<ParamInfo> = params
        .into_iter()
        .enumerate()
        .filter_map(|(idx, param)| if idx == ctx_idx { None } else { Some(param) })
        .collect();

    let input_builder = quote! { ::seesaw_core::effect::on::<#on_event_type>() };
    let builder = apply_effect_config(input_builder, &args, &fn_ident);

    let chain = if args.join {
        if !args.extract.is_empty() {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "join and extract(...) cannot be used together",
            ));
        }
        if !matches!(on, OnSpec::EventType(_)) {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "join requires on = EventType (not on = [...])",
            ));
        }
        if non_ctx_params.len() != 1 {
            return Err(syn::Error::new_spanned(
                &input_fn.sig.inputs,
                "join requires exactly one non-context parameter of type Vec<T>",
            ));
        }

        let batch_param = &non_ctx_params[0];
        let batch_ty = vec_inner_type(&batch_param.ty).ok_or_else(|| {
            syn::Error::new_spanned(
                &batch_param.ty,
                "join requires first parameter to be Vec<T> where on = T",
            )
        })?;

        if type_key(batch_ty) != path_key(&on_event_type) {
            return Err(syn::Error::new_spanned(
                &batch_param.ty,
                "join requires first parameter type Vec<T> to match on = T",
            ));
        }

        let batch_ident = &batch_param.ident;
        quote! {
            #builder
                .join()
                .same_batch()
                .then::<#deps_ty, _, _, _, #output_event_ty>(|#batch_ident, __seesaw_ctx| #fn_ident(#batch_ident, __seesaw_ctx))
        }
    } else if !args.extract.is_empty() {
        if non_ctx_params.len() != args.extract.len() {
            return Err(syn::Error::new_spanned(
                &input_fn.sig.inputs,
                "effect function parameters must match extract(...) fields",
            ));
        }

        for (param, extracted) in non_ctx_params.iter().zip(args.extract.iter()) {
            if param.ident != *extracted {
                return Err(syn::Error::new_spanned(
                    &param.ident,
                    format!(
                        "parameter `{}` must match extracted field `{}`",
                        param.ident, extracted
                    ),
                ));
            }
        }

        let fields = &args.extract;
        let extract_call = match &on {
            OnSpec::EventType(_) => {
                if fields.len() == 1 {
                    let field = &fields[0];
                    quote! {
                        .extract(|__seesaw_event| Some(__seesaw_event.#field.clone()))
                    }
                } else {
                    quote! {
                        .extract(|__seesaw_event| Some((#(__seesaw_event.#fields.clone()),*)))
                    }
                }
            }
            OnSpec::Variants(variants) => {
                let match_arms = variants.iter().map(|variant| {
                    if fields.len() == 1 {
                        let field = &fields[0];
                        quote! {
                            #variant { #field, .. } => Some(#field.clone()),
                        }
                    } else {
                        quote! {
                            #variant { #(#fields),*, .. } => Some((#(#fields.clone()),*)),
                        }
                    }
                });

                quote! {
                    .extract(|__seesaw_event| match __seesaw_event {
                        #(#match_arms)*
                        #[allow(unreachable_patterns)]
                        _ => None,
                    })
                }
            }
        };

        if fields.len() == 1 {
            let field = &fields[0];
            let extracted_ty = &non_ctx_params[0].ty;
            quote! {
                #builder
                    #extract_call
                    .then::<#deps_ty, #extracted_ty, _, _, _, #output_event_ty>(|#field, __seesaw_ctx| #fn_ident(#field, __seesaw_ctx))
            }
        } else {
            let extracted_tys: Vec<&Type> = non_ctx_params.iter().map(|param| &param.ty).collect();
            quote! {
                #builder
                    #extract_call
                    .then::<#deps_ty, (#(#extracted_tys),*), _, _, _, #output_event_ty>(|(#(#fields),*), __seesaw_ctx| #fn_ident(#(#fields),*, __seesaw_ctx))
            }
        }
    } else {
        if non_ctx_params.len() != 1 {
            return Err(syn::Error::new_spanned(
                &input_fn.sig.inputs,
                "#[effect] requires exactly one event parameter plus EffectContext when extract(...) is not used",
            ));
        }
        if !matches!(on, OnSpec::EventType(_)) {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "on = [...] requires extract(...)",
            ));
        }

        let event_param = &non_ctx_params[0];
        if type_key(&event_param.ty) != path_key(&on_event_type) {
            return Err(syn::Error::new_spanned(
                &event_param.ty,
                "event parameter type must match on = EventType",
            ));
        }

        let event_ident = &event_param.ident;
        quote! {
            #builder
                .then::<#deps_ty, ::std::sync::Arc<#on_event_type>, _, _, _, #output_event_ty>(|#event_ident, __seesaw_ctx| #fn_ident((#event_ident).as_ref().clone(), __seesaw_ctx))
        }
    };

    Ok(quote! {
        #input_fn

        #[doc(hidden)]
        pub fn #wrapper_ident() -> ::seesaw_core::Effect<#deps_ty> {
            #chain
        }
    })
}

fn expand_effects_module(module: &mut ItemMod) -> syn::Result<TokenStream2> {
    let Some((_, items)) = &mut module.content else {
        return Err(syn::Error::new_spanned(
            module,
            "#[effects] requires an inline module",
        ));
    };

    let mut wrappers = Vec::new();
    let mut deps_ty: Option<Type> = None;

    for item in items.iter() {
        let Item::Fn(item_fn) = item else {
            continue;
        };
        if !has_attr(&item_fn.attrs, "effect") {
            continue;
        }

        let wrapper_ident = format_ident!("__seesaw_effect_{}", item_fn.sig.ident);
        wrappers.push(wrapper_ident);

        let (_, deps) = find_effect_context(&item_fn.sig)?;
        match &deps_ty {
            None => deps_ty = Some(deps),
            Some(existing_deps) => {
                if type_key(existing_deps) != type_key(&deps) {
                    return Err(syn::Error::new_spanned(
                        &item_fn.sig,
                        "all #[effect] handlers in an #[effects] module must use the same EffectContext<Deps>",
                    ));
                }
            }
        }
    }

    if wrappers.is_empty() {
        return Err(syn::Error::new_spanned(
            module,
            "#[effects] module must contain at least one #[effect] function",
        ));
    }

    let deps_ty = deps_ty.expect("checked above");
    let registration_fn: ItemFn = parse_quote! {
        pub fn effects() -> ::std::vec::Vec<::seesaw_core::Effect<#deps_ty>> {
            ::std::vec![#(#wrappers()),*]
        }
    };
    items.push(Item::Fn(registration_fn));

    Ok(quote! { #module })
}

fn parse_effect_args(metas: &Punctuated<Meta, Token![,]>) -> syn::Result<EffectArgs> {
    let mut args = EffectArgs::default();

    for meta in metas {
        match meta {
            Meta::NameValue(nv) if nv.path.is_ident("on") => {
                ensure_unset(&args.on, nv, "on")?;
                args.on = Some(parse_on_expr(&nv.value)?);
            }
            Meta::List(list) if list.path.is_ident("extract") => {
                if !args.extract.is_empty() {
                    return Err(syn::Error::new_spanned(
                        list,
                        "extract(...) specified more than once",
                    ));
                }
                args.extract = parse_extract_fields(list)?;
            }
            Meta::Path(path) if path.is_ident("join") => {
                if args.join {
                    return Err(syn::Error::new_spanned(
                        path,
                        "join specified more than once",
                    ));
                }
                args.join = true;
            }
            Meta::Path(path) if path.is_ident("queued") => {
                if args.queued {
                    return Err(syn::Error::new_spanned(
                        path,
                        "queued specified more than once",
                    ));
                }
                args.queued = true;
            }
            Meta::NameValue(nv) if nv.path.is_ident("id") => {
                ensure_unset(&args.id, nv, "id")?;
                args.id = Some(parse_string_lit(&nv.value)?);
            }
            Meta::NameValue(nv) if nv.path.is_ident("dlq_terminal") => {
                ensure_unset(&args.dlq_terminal, nv, "dlq_terminal")?;
                args.dlq_terminal = Some(parse_path_expr(&nv.value, "dlq_terminal")?);
            }
            Meta::NameValue(nv) if nv.path.is_ident("retry") => {
                ensure_unset(&args.retry, nv, "retry")?;
                args.retry = Some(parse_int_lit::<u32>(&nv.value, "retry")?);
            }
            Meta::NameValue(nv) if nv.path.is_ident("timeout_secs") => {
                ensure_unset(&args.timeout_secs, nv, "timeout_secs")?;
                args.timeout_secs = Some(parse_int_lit::<u64>(&nv.value, "timeout_secs")?);
            }
            Meta::NameValue(nv) if nv.path.is_ident("timeout_ms") => {
                ensure_unset(&args.timeout_ms, nv, "timeout_ms")?;
                args.timeout_ms = Some(parse_int_lit::<u64>(&nv.value, "timeout_ms")?);
            }
            Meta::NameValue(nv) if nv.path.is_ident("delay_secs") => {
                ensure_unset(&args.delay_secs, nv, "delay_secs")?;
                args.delay_secs = Some(parse_int_lit::<u64>(&nv.value, "delay_secs")?);
            }
            Meta::NameValue(nv) if nv.path.is_ident("delay_ms") => {
                ensure_unset(&args.delay_ms, nv, "delay_ms")?;
                args.delay_ms = Some(parse_int_lit::<u64>(&nv.value, "delay_ms")?);
            }
            Meta::NameValue(nv) if nv.path.is_ident("priority") => {
                ensure_unset(&args.priority, nv, "priority")?;
                args.priority = Some(parse_int_lit::<i32>(&nv.value, "priority")?);
            }
            Meta::NameValue(nv) if nv.path.is_ident("group") => {
                ensure_unset(&args.group, nv, "group")?;
                args.group = Some(parse_string_lit(&nv.value)?);
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    meta,
                    "unsupported #[effect] option",
                ));
            }
        }
    }

    Ok(args)
}

fn parse_on_expr(expr: &Expr) -> syn::Result<OnSpec> {
    let expr = strip_expr_groups(expr);
    match expr {
        Expr::Path(expr_path) => Ok(OnSpec::EventType(expr_path.path.clone())),
        Expr::Array(expr_array) => Ok(OnSpec::Variants(parse_path_array(expr_array)?)),
        Expr::Binary(binary) if matches!(binary.op, syn::BinOp::BitOr(_)) => {
            let left = parse_on_expr(&binary.left)?;
            let right = parse_on_expr(&binary.right)?;

            match (left, right) {
                (OnSpec::EventType(_), OnSpec::Variants(variants))
                | (OnSpec::Variants(variants), OnSpec::EventType(_)) => {
                    Ok(OnSpec::Variants(variants))
                }
                _ => Err(syn::Error::new_spanned(
                    binary,
                    "expected on = EventType | [Enum::Variant, ...]",
                )),
            }
        }
        _ => Err(syn::Error::new_spanned(
            expr,
            "expected on = EventType or on = [Enum::Variant, ...]",
        )),
    }
}

fn parse_path_array(array: &syn::ExprArray) -> syn::Result<Vec<Path>> {
    let mut variants = Vec::new();
    for element in &array.elems {
        let element = strip_expr_groups(element);
        match element {
            Expr::Path(path) => variants.push(path.path.clone()),
            _ => {
                return Err(syn::Error::new_spanned(
                    element,
                    "on = [...] expects paths like Enum::Variant",
                ));
            }
        }
    }
    if variants.is_empty() {
        return Err(syn::Error::new_spanned(array, "on = [] is not supported"));
    }
    Ok(variants)
}

fn parse_extract_fields(list: &MetaList) -> syn::Result<Vec<Ident>> {
    let parser = Punctuated::<Ident, Token![,]>::parse_terminated;
    let idents = parser.parse2(list.tokens.clone())?;
    if idents.is_empty() {
        return Err(syn::Error::new_spanned(
            list,
            "extract(...) requires at least one field",
        ));
    }
    Ok(idents.into_iter().collect())
}

fn parse_string_lit(expr: &Expr) -> syn::Result<String> {
    match strip_expr_groups(expr) {
        Expr::Lit(expr_lit) => match &expr_lit.lit {
            Lit::Str(value) => Ok(value.value()),
            _ => Err(syn::Error::new_spanned(expr, "expected a string literal")),
        },
        _ => Err(syn::Error::new_spanned(expr, "expected a string literal")),
    }
}

fn parse_int_lit<T>(expr: &Expr, label: &str) -> syn::Result<T>
where
    T: std::str::FromStr,
    <T as std::str::FromStr>::Err: std::fmt::Display,
{
    match strip_expr_groups(expr) {
        Expr::Lit(expr_lit) => match &expr_lit.lit {
            Lit::Int(value) => value
                .base10_parse::<T>()
                .map_err(|err| syn::Error::new_spanned(expr, format!("invalid {label}: {err}"))),
            _ => Err(syn::Error::new_spanned(
                expr,
                format!("expected integer for {label}"),
            )),
        },
        _ => Err(syn::Error::new_spanned(
            expr,
            format!("expected integer for {label}"),
        )),
    }
}

fn parse_path_expr(expr: &Expr, label: &str) -> syn::Result<Path> {
    match strip_expr_groups(expr) {
        Expr::Path(expr_path) => Ok(expr_path.path.clone()),
        _ => Err(syn::Error::new_spanned(
            expr,
            format!("expected path for {label}, e.g. module::handler"),
        )),
    }
}

fn find_effect_context(sig: &Signature) -> syn::Result<(usize, Type)> {
    let mut found: Option<(usize, Type)> = None;

    for (index, input) in sig.inputs.iter().enumerate() {
        let FnArg::Typed(typed) = input else {
            continue;
        };

        if let Some(deps) = parse_effect_context_type(&typed.ty) {
            if found.is_some() {
                return Err(syn::Error::new_spanned(
                    &typed.ty,
                    "multiple EffectContext parameters are not supported",
                ));
            }
            found = Some((index, deps));
        }
    }

    found.ok_or_else(|| {
        syn::Error::new_spanned(
            &sig.inputs,
            "effect handler must include ctx: EffectContext<Deps>",
        )
    })
}

fn parse_effect_context_type(ty: &Type) -> Option<Type> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return None;
    };
    let last = path.segments.last()?;
    if last.ident != "EffectContext" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };

    let mut types = Vec::new();
    for arg in &args.args {
        if let GenericArgument::Type(ty) = arg {
            types.push(ty.clone());
        }
    }
    if types.len() != 1 {
        return None;
    }
    Some(types[0].clone())
}

fn collect_params(sig: &Signature) -> syn::Result<Vec<ParamInfo>> {
    let mut params = Vec::new();

    for input in &sig.inputs {
        let FnArg::Typed(typed) = input else {
            return Err(syn::Error::new_spanned(
                input,
                "methods with self are not supported",
            ));
        };

        let Pat::Ident(pat_ident) = typed.pat.as_ref() else {
            return Err(syn::Error::new_spanned(
                &typed.pat,
                "parameter patterns are not supported; use simple identifiers",
            ));
        };

        params.push(ParamInfo {
            ident: pat_ident.ident.clone(),
            ty: (*typed.ty).clone(),
        });
    }

    Ok(params)
}

fn vec_inner_type(ty: &Type) -> Option<&Type> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    let last = type_path.path.segments.last()?;
    if last.ident != "Vec" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    for arg in &args.args {
        if let GenericArgument::Type(inner) = arg {
            return Some(inner);
        }
    }
    None
}

fn variant_base_path(path: &Path) -> syn::Result<Path> {
    if path.segments.len() < 2 {
        return Err(syn::Error::new_spanned(
            path,
            "expected variant path like Enum::Variant",
        ));
    }
    let mut base = path.clone();
    let mut segments = Punctuated::new();
    for segment in path.segments.iter().take(path.segments.len() - 1) {
        segments.push(segment.clone());
    }
    base.segments = segments;
    Ok(base)
}

fn apply_effect_config(base: TokenStream2, args: &EffectArgs, fn_ident: &Ident) -> TokenStream2 {
    let mut builder = base;

    if let Some(id) = args.id.as_ref().cloned().or_else(|| {
        args.group
            .as_ref()
            .map(|group| format!("{group}::{}", fn_ident))
    }) {
        let id_lit = syn::LitStr::new(&id, fn_ident.span());
        builder = quote! { #builder .id(#id_lit) };
    }

    if let Some(dlq_terminal) = &args.dlq_terminal {
        builder = quote! {
            #builder
                .dlq_terminal(|__seesaw_source, __seesaw_info| {
                    #dlq_terminal((__seesaw_source).as_ref().clone(), __seesaw_info)
                })
        };
    }

    if args.queued {
        builder = quote! { #builder .queued() };
    }

    if let Some(retry) = args.retry {
        builder = quote! { #builder .retry(#retry) };
    }
    if let Some(timeout_secs) = args.timeout_secs {
        builder = quote! { #builder .timeout(::std::time::Duration::from_secs(#timeout_secs)) };
    }
    if let Some(timeout_ms) = args.timeout_ms {
        builder = quote! { #builder .timeout(::std::time::Duration::from_millis(#timeout_ms)) };
    }
    if let Some(delay_secs) = args.delay_secs {
        builder = quote! { #builder .delayed(::std::time::Duration::from_secs(#delay_secs)) };
    }
    if let Some(delay_ms) = args.delay_ms {
        builder = quote! { #builder .delayed(::std::time::Duration::from_millis(#delay_ms)) };
    }
    if let Some(priority) = args.priority {
        builder = quote! { #builder .priority(#priority) };
    }

    builder
}

fn effect_requires_stable_id(args: &EffectArgs) -> bool {
    args.queued
        || args.join
        || args.delay_secs.is_some()
        || args.delay_ms.is_some()
        || args.timeout_secs.is_some()
        || args.timeout_ms.is_some()
        || args.priority.is_some()
        || args.retry.unwrap_or(1) > 1
}

fn has_attr(attrs: &[Attribute], name: &str) -> bool {
    attrs.iter().any(|attr| attr.path().is_ident(name))
}

fn path_key(path: &Path) -> String {
    path.to_token_stream().to_string()
}

fn type_key(ty: &Type) -> String {
    ty.to_token_stream().to_string()
}

fn strip_expr_groups(mut expr: &Expr) -> &Expr {
    loop {
        match expr {
            Expr::Group(group) => expr = &group.expr,
            _ => return expr,
        }
    }
}

fn ensure_unset<T>(existing: &Option<T>, meta: &MetaNameValue, name: &str) -> syn::Result<()> {
    if existing.is_some() {
        return Err(syn::Error::new_spanned(
            meta,
            format!("{name} specified more than once"),
        ));
    }
    Ok(())
}

fn effect_output_event_type(sig: &Signature) -> syn::Result<Type> {
    let ReturnType::Type(_, output_ty) = &sig.output else {
        return Err(syn::Error::new_spanned(
            &sig.output,
            "#[effect] must return Result<T>",
        ));
    };

    let result_ok_ty = result_ok_type(output_ty)?;
    if let Some(emit_inner) = emit_inner_type(&result_ok_ty) {
        Ok(emit_inner)
    } else {
        Ok(result_ok_ty)
    }
}

fn result_ok_type(ty: &Type) -> syn::Result<Type> {
    let Type::Path(type_path) = ty else {
        return Err(syn::Error::new_spanned(
            ty,
            "expected return type Result<T>",
        ));
    };
    let Some(last) = type_path.path.segments.last() else {
        return Err(syn::Error::new_spanned(
            ty,
            "expected return type Result<T>",
        ));
    };
    if last.ident != "Result" {
        return Err(syn::Error::new_spanned(
            ty,
            "expected return type Result<T>",
        ));
    }

    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return Err(syn::Error::new_spanned(
            ty,
            "expected return type Result<T>",
        ));
    };
    for arg in &args.args {
        if let GenericArgument::Type(inner) = arg {
            return Ok(inner.clone());
        }
    }

    Err(syn::Error::new_spanned(
        ty,
        "expected return type Result<T>",
    ))
}

fn emit_inner_type(ty: &Type) -> Option<Type> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    let last = type_path.path.segments.last()?;
    if last.ident != "Emit" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    for arg in &args.args {
        if let GenericArgument::Type(inner) = arg {
            return Some(inner.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;
    use syn::parse::Parser;

    fn parse_effect_meta_list(tokens: TokenStream2) -> Punctuated<Meta, Token![,]> {
        let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
        parser
            .parse2(tokens)
            .expect("effect meta list should parse")
    }

    #[test]
    fn parse_effect_args_supports_queued_flag() {
        let metas = parse_effect_meta_list(quote!(on = MyEvent, queued));
        let args = parse_effect_args(&metas).expect("queued should parse");
        assert!(args.queued);
        assert!(matches!(args.on, Some(OnSpec::EventType(_))));
    }

    #[test]
    fn parse_effect_args_rejects_duplicate_queued_flag() {
        let metas = parse_effect_meta_list(quote!(on = MyEvent, queued, queued));
        let error = parse_effect_args(&metas)
            .err()
            .expect("duplicate queued should fail");
        assert!(
            error
                .to_string()
                .contains("queued specified more than once"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn apply_effect_config_emits_queued_builder_call() {
        let args = EffectArgs {
            queued: true,
            ..EffectArgs::default()
        };
        let handler_ident: Ident = syn::parse_quote!(my_effect_handler);
        let configured = apply_effect_config(
            quote!(::seesaw_core::effect::on::<MyEvent>()),
            &args,
            &handler_ident,
        );
        let configured_text = configured.to_string();
        assert!(
            configured_text.contains(". queued ()"),
            "queued builder call should be emitted, got: {}",
            configured_text
        );
    }

    #[test]
    fn parse_effect_args_rejects_delivery_option() {
        let metas = parse_effect_meta_list(quote!(on = MyEvent, delivery = "durable"));
        let error = parse_effect_args(&metas)
            .err()
            .expect("delivery option should remain unsupported");
        assert!(
            error.to_string().contains("unsupported #[effect] option"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn stable_id_is_required_for_durable_effect_configs() {
        let durable = EffectArgs {
            retry: Some(3),
            ..EffectArgs::default()
        };
        assert!(effect_requires_stable_id(&durable));
    }
}
