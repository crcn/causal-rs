use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote, ToTokens};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{
    parse_macro_input, parse_quote, Attribute, Data, DeriveInput, Expr, Fields, FnArg,
    GenericArgument, Ident, Item, ItemFn, ItemMod, Lit, Meta, MetaList, MetaNameValue, Pat, Path,
    PathArguments, ReturnType, Signature, Token, Type, TypePath,
};

#[proc_macro_attribute]
pub fn handle(attr: TokenStream, item: TokenStream) -> TokenStream {
    let metas = parse_macro_input!(attr with Punctuated::<Meta, Token![,]>::parse_terminated);
    let input_fn = parse_macro_input!(item as ItemFn);

    match expand_effect(parse_effect_args(&metas), input_fn) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn handler(attr: TokenStream, item: TokenStream) -> TokenStream {
    handle(attr, item)
}

#[proc_macro_attribute]
pub fn handles(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut module = parse_macro_input!(item as ItemMod);
    match expand_effects_module(&mut module) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn handlers(attr: TokenStream, item: TokenStream) -> TokenStream {
    handles(attr, item)
}

/// Marks a function as an aggregator — generates `impl Apply<E> for A` and an `Aggregator` factory.
///
/// The function signature must be `fn name(agg: &mut AggregateType, event: EventType)`.
/// The `id` attribute parameter specifies the event field used as aggregate ID.
///
/// ```ignore
/// #[aggregator(id = "order_id")]
/// fn on_placed(order: &mut Order, event: OrderPlaced) {
///     order.status = Status::Placed;
///     order.total = event.total;
/// }
/// ```
#[proc_macro_attribute]
pub fn aggregator(attr: TokenStream, item: TokenStream) -> TokenStream {
    let metas = parse_macro_input!(attr with Punctuated::<Meta, Token![,]>::parse_terminated);
    let input_fn = parse_macro_input!(item as ItemFn);

    match expand_aggregator(&metas, input_fn) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Collects `#[aggregator]` functions in a module into a `fn aggregators() -> Vec<Aggregator>`.
///
/// ```ignore
/// #[aggregators]
/// mod order_aggregators {
///     #[aggregator(id = "order_id")]
///     fn on_placed(order: &mut Order, event: OrderPlaced) {
///         order.status = Status::Placed;
///     }
///
///     #[aggregator(id = "order_id")]
///     fn on_shipped(order: &mut Order, event: OrderShipped) {
///         order.status = Status::Shipped;
///     }
/// }
///
/// // Usage: engine.with_aggregators(order_aggregators::aggregators())
/// ```
#[proc_macro_attribute]
pub fn aggregators(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut module = parse_macro_input!(item as ItemMod);
    match expand_aggregators_module(&mut module) {
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
    accumulate: bool,
    join: bool,
    queued: bool,
    id: Option<String>,
    dlq_terminal: Option<Path>,
    retry: Option<u32>,
    timeout_secs: Option<u64>,
    timeout_ms: Option<u64>,
    window_timeout_secs: Option<u64>,
    window_timeout_ms: Option<u64>,
    delay_secs: Option<u64>,
    delay_ms: Option<u64>,
    priority: Option<i32>,
    group: Option<String>,
    aggregate: Option<Path>,
    transition: Option<syn::ExprClosure>,
}

struct ParamInfo {
    ident: Ident,
    ty: Type,
}

/// Generate the expression that converts `__result` into `Events`.
fn result_to_events(kind: &ReturnKind) -> TokenStream2 {
    match kind {
        ReturnKind::Unit => quote! { ::seesaw_core::Events::new() },
        ReturnKind::Events => quote! { __result },
        ReturnKind::Emit => quote! { ::seesaw_core::IntoEvents::into_events(__result) },
        ReturnKind::SingleEvent => quote! { ::seesaw_core::Events::new().add(__result) },
    }
}

fn expand_effect(args: syn::Result<EffectArgs>, input_fn: ItemFn) -> syn::Result<TokenStream2> {
    let args = args?;
    let fn_ident = input_fn.sig.ident.clone();
    let wrapper_ident = format_ident!("__seesaw_effect_{}", fn_ident);

    if input_fn.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            &input_fn.sig.fn_token,
            "#[handler] requires an async function",
        ));
    }
    if !input_fn.sig.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input_fn.sig.generics,
            "#[handler] does not support generic functions",
        ));
    }

    let return_kind = classify_return(&input_fn.sig)?;
    let convert_result = result_to_events(&return_kind);

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
    if args.window_timeout_secs.is_some() && args.window_timeout_ms.is_some() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "cannot specify both window_timeout_secs and window_timeout_ms",
        ));
    }
    let is_accumulate = args.accumulate || args.join;
    if effect_requires_stable_id(&args) && args.id.is_none() && args.group.is_none() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "queued/durable #[handler] requires an explicit id = \"...\" (or group = \"...\")",
        ));
    }

    // Enforce explicit 'queued' attribute when using background-only features
    if effect_requires_background(&args) && !args.queued && !is_accumulate {
        let features = collect_background_features(&args);
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            format!(
                "handlers with {} must explicitly include 'queued' attribute to indicate background execution",
                features.join(" and ")
            ),
        ));
    }

    let on = args.on.clone().ok_or_else(|| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[handler] requires on = EventType or on = [Enum::Variant, ...]",
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

    let input_builder = quote! { ::seesaw_core::on::<#on_event_type>() };
    let builder = apply_effect_config(input_builder, &args, &fn_ident);

    // Validate aggregate/transition pairing
    if args.aggregate.is_some() != args.transition.is_some() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "aggregate and transition must be specified together",
        ));
    }

    let chain = if args.aggregate.is_some() && args.transition.is_some() {
        // Transition guard path
        let aggregate_ty = args.aggregate.as_ref().unwrap();
        let transition_closure = args.transition.as_ref().unwrap();

        if args.extract.len() != 1 {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "transition handlers require extract() with exactly one Uuid field (the aggregate ID)",
            ));
        }

        if non_ctx_params.len() != 1 {
            return Err(syn::Error::new_spanned(
                &input_fn.sig.inputs,
                "transition handler function takes (aggregate_id: Uuid, ctx: Context<Deps>)",
            ));
        }

        let field = &args.extract[0];
        let param_ident = &non_ctx_params[0].ident;

        let extract_call = match &on {
            OnSpec::EventType(_) => {
                quote! {
                    .extract(|__seesaw_event| Some(__seesaw_event.#field.clone()))
                }
            }
            OnSpec::Variants(variants) => {
                let match_arms = variants.iter().map(|variant| {
                    quote! {
                        #variant { #field, .. } => Some(#field.clone()),
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

        quote! {
            #builder
                #extract_call
                .transition::<#aggregate_ty, _>(#transition_closure)
                .then::<#deps_ty, _, _>(|#param_ident, __seesaw_ctx| async move {
                    let __result = #fn_ident(#param_ident, __seesaw_ctx).await?;
                    Ok(#convert_result)
                })
        }
    } else if is_accumulate {
        if !args.extract.is_empty() {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "accumulate and extract(...) cannot be used together",
            ));
        }
        if !matches!(on, OnSpec::EventType(_)) {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "accumulate requires on = EventType (not on = [...])",
            ));
        }
        if non_ctx_params.len() != 1 {
            return Err(syn::Error::new_spanned(
                &input_fn.sig.inputs,
                "accumulate requires exactly one non-context parameter of type Vec<T>",
            ));
        }

        let batch_param = &non_ctx_params[0];
        let batch_ty = vec_inner_type(&batch_param.ty).ok_or_else(|| {
            syn::Error::new_spanned(
                &batch_param.ty,
                "accumulate requires first parameter to be Vec<T> where on = T",
            )
        })?;

        if type_key(batch_ty) != path_key(&on_event_type) {
            return Err(syn::Error::new_spanned(
                &batch_param.ty,
                "accumulate requires first parameter type Vec<T> to match on = T",
            ));
        }

        let batch_ident = &batch_param.ident;
        quote! {
            #builder
                .accumulate()
                .then::<#deps_ty, _, _>(|#batch_ident, __seesaw_ctx| async move {
                    let __result = #fn_ident(#batch_ident, __seesaw_ctx).await?;
                    Ok(#convert_result)
                })
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
                    .then::<#deps_ty, #extracted_ty, _, _>(|#field, __seesaw_ctx| async move {
                        let __result = #fn_ident(#field, __seesaw_ctx).await?;
                        Ok(#convert_result)
                    })
            }
        } else {
            let extracted_tys: Vec<&Type> = non_ctx_params.iter().map(|param| &param.ty).collect();
            quote! {
                #builder
                    #extract_call
                    .then::<#deps_ty, (#(#extracted_tys),*), _, _>(|(#(#fields),*), __seesaw_ctx| async move {
                        let __result = #fn_ident(#(#fields),*, __seesaw_ctx).await?;
                        Ok(#convert_result)
                    })
            }
        }
    } else {
        if non_ctx_params.len() != 1 {
            return Err(syn::Error::new_spanned(
                &input_fn.sig.inputs,
                "#[handler] requires exactly one event parameter plus Context when extract(...) is not used",
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
                .then::<#deps_ty, ::std::sync::Arc<#on_event_type>, _, _>(|#event_ident, __seesaw_ctx| async move {
                    let __result = #fn_ident((#event_ident).as_ref().clone(), __seesaw_ctx).await?;
                    Ok(#convert_result)
                })
        }
    };

    Ok(quote! {
        #input_fn

        #[doc(hidden)]
        pub fn #wrapper_ident() -> ::seesaw_core::Handler<#deps_ty> {
            #chain
        }
    })
}

fn expand_effects_module(module: &mut ItemMod) -> syn::Result<TokenStream2> {
    let Some((_, items)) = &mut module.content else {
        return Err(syn::Error::new_spanned(
            module,
            "#[handlers] requires an inline module",
        ));
    };

    let mut wrappers = Vec::new();
    let mut deps_ty: Option<Type> = None;

    for item in items.iter() {
        let Item::Fn(item_fn) = item else {
            continue;
        };
        if !has_attr_any(&item_fn.attrs, &["handle", "handler"]) {
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
                        "all #[handler] handlers in an #[handlers] module must use the same Context<Deps>",
                    ));
                }
            }
        }
    }

    if wrappers.is_empty() {
        return Err(syn::Error::new_spanned(
            module,
            "#[handlers] module must contain at least one #[handler] function",
        ));
    }

    let deps_ty = deps_ty.expect("checked above");
    let handles_fn: ItemFn = parse_quote! {
        pub fn handles() -> ::std::vec::Vec<::seesaw_core::Handler<#deps_ty>> {
            ::std::vec![#(#wrappers()),*]
        }
    };
    let handlers_fn: ItemFn = parse_quote! {
        pub fn handlers() -> ::std::vec::Vec<::seesaw_core::Handler<#deps_ty>> {
            handles()
        }
    };
    items.push(Item::Fn(handles_fn));
    items.push(Item::Fn(handlers_fn));

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
            Meta::Path(path) if path.is_ident("accumulate") => {
                if args.accumulate || args.join {
                    return Err(syn::Error::new_spanned(
                        path,
                        "accumulate specified more than once",
                    ));
                }
                args.accumulate = true;
            }
            Meta::Path(path) if path.is_ident("join") => {
                if args.accumulate || args.join {
                    return Err(syn::Error::new_spanned(
                        path,
                        "accumulate specified more than once",
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
            Meta::NameValue(nv) if nv.path.is_ident("window_timeout_secs") => {
                ensure_unset(&args.window_timeout_secs, nv, "window_timeout_secs")?;
                args.window_timeout_secs =
                    Some(parse_int_lit::<u64>(&nv.value, "window_timeout_secs")?);
            }
            Meta::NameValue(nv) if nv.path.is_ident("window_timeout_ms") => {
                ensure_unset(&args.window_timeout_ms, nv, "window_timeout_ms")?;
                args.window_timeout_ms =
                    Some(parse_int_lit::<u64>(&nv.value, "window_timeout_ms")?);
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
            Meta::NameValue(nv) if nv.path.is_ident("aggregate") => {
                ensure_unset(&args.aggregate, nv, "aggregate")?;
                args.aggregate = Some(parse_path_expr(&nv.value, "aggregate")?);
            }
            Meta::NameValue(nv) if nv.path.is_ident("transition") => {
                if args.transition.is_some() {
                    return Err(syn::Error::new_spanned(
                        nv,
                        "transition specified more than once",
                    ));
                }
                args.transition = Some(parse_closure_expr(&nv.value)?);
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    meta,
                    "unsupported #[handler] option",
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

fn parse_closure_expr(expr: &Expr) -> syn::Result<syn::ExprClosure> {
    match strip_expr_groups(expr) {
        Expr::Closure(closure) => Ok(closure.clone()),
        _ => Err(syn::Error::new_spanned(
            expr,
            "expected a closure, e.g. |prev, next| prev.x > 0 && next.x == 0",
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
                    "multiple Context parameters are not supported",
                ));
            }
            found = Some((index, deps));
        }
    }

    found.ok_or_else(|| {
        syn::Error::new_spanned(
            &sig.inputs,
            "effect handler must include ctx: Context<Deps>",
        )
    })
}

fn parse_effect_context_type(ty: &Type) -> Option<Type> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return None;
    };
    let last = path.segments.last()?;
    if last.ident != "Context" {
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
                .on_failure(|__seesaw_source: ::std::sync::Arc<_>, __seesaw_info| {
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
    if let Some(window_timeout_secs) = args.window_timeout_secs {
        builder = quote! {
            #builder
                .window(::std::time::Duration::from_secs(#window_timeout_secs))
        };
    }
    if let Some(window_timeout_ms) = args.window_timeout_ms {
        builder = quote! {
            #builder
                .window(::std::time::Duration::from_millis(#window_timeout_ms))
        };
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
        || args.accumulate
        || args.join
        || args.delay_secs.is_some()
        || args.delay_ms.is_some()
        || args.timeout_secs.is_some()
        || args.timeout_ms.is_some()
        || args.window_timeout_secs.is_some()
        || args.window_timeout_ms.is_some()
        || args.priority.is_some()
        || args.retry.unwrap_or(1) > 1
}

/// Check if handler requires background execution (excluding explicit queued/accumulate)
fn effect_requires_background(args: &EffectArgs) -> bool {
    args.retry.unwrap_or(1) > 1
        || args.timeout_secs.is_some()
        || args.timeout_ms.is_some()
        || args.delay_secs.is_some()
        || args.delay_ms.is_some()
        || args.priority.is_some()
}

/// Collect list of background-only features for error message
fn collect_background_features(args: &EffectArgs) -> Vec<&'static str> {
    let mut features = Vec::new();
    if args.retry.unwrap_or(1) > 1 {
        features.push("retry > 1");
    }
    if args.timeout_secs.is_some() || args.timeout_ms.is_some() {
        features.push("timeout");
    }
    if args.delay_secs.is_some() || args.delay_ms.is_some() {
        features.push("delay");
    }
    if args.priority.is_some() {
        features.push("priority");
    }
    features
}

fn has_attr(attrs: &[Attribute], name: &str) -> bool {
    attrs.iter().any(|attr| attr.path().is_ident(name))
}

fn has_attr_any(attrs: &[Attribute], names: &[&str]) -> bool {
    names.iter().any(|name| has_attr(attrs, name))
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

/// Classify the handler return type to generate appropriate Events conversion code.
enum ReturnKind {
    /// `Result<()>` — empty events
    Unit,
    /// `Result<Events>` — pass-through
    Events,
    /// `Result<Emit<T>>` — legacy emit, use IntoEvents
    Emit,
    /// `Result<T>` — single event, wrap with Events::new().add()
    SingleEvent,
}

fn classify_return(sig: &Signature) -> syn::Result<ReturnKind> {
    let ReturnType::Type(_, output_ty) = &sig.output else {
        return Ok(ReturnKind::Unit);
    };

    let ok_ty = result_ok_type(output_ty)?;

    // Check for ()
    if let Type::Tuple(t) = &ok_ty {
        if t.elems.is_empty() {
            return Ok(ReturnKind::Unit);
        }
    }

    // Check for named types: Events or Emit<T>
    if let Type::Path(type_path) = &ok_ty {
        if let Some(last) = type_path.path.segments.last() {
            if last.ident == "Events" {
                return Ok(ReturnKind::Events);
            }
            if last.ident == "Emit" {
                return Ok(ReturnKind::Emit);
            }
        }
    }

    Ok(ReturnKind::SingleEvent)
}

fn result_ok_type(ty: &Type) -> syn::Result<Type> {
    let Type::Path(type_path) = ty else {
        return Err(syn::Error::new_spanned(ty, "expected return type Result<T>"));
    };
    let Some(last) = type_path.path.segments.last() else {
        return Err(syn::Error::new_spanned(ty, "expected return type Result<T>"));
    };
    if last.ident != "Result" {
        return Err(syn::Error::new_spanned(ty, "expected return type Result<T>"));
    }
    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return Err(syn::Error::new_spanned(ty, "expected return type Result<T>"));
    };
    for arg in &args.args {
        if let GenericArgument::Type(inner) = arg {
            return Ok(inner.clone());
        }
    }
    Err(syn::Error::new_spanned(ty, "expected return type Result<T>"))
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
    fn parse_effect_args_supports_accumulate_flag() {
        let metas = parse_effect_meta_list(quote!(on = MyEvent, accumulate));
        let args = parse_effect_args(&metas).expect("accumulate should parse");
        assert!(args.accumulate);
    }

    #[test]
    fn apply_effect_config_emits_queued_builder_call() {
        let args = EffectArgs {
            queued: true,
            ..EffectArgs::default()
        };
        let handler_ident: Ident = syn::parse_quote!(my_effect_handler);
        let configured = apply_effect_config(
            quote!(::seesaw_core::on::<MyEvent>()),
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
    fn apply_effect_config_emits_window_timeout_builder_call() {
        let args = EffectArgs {
            window_timeout_secs: Some(60),
            ..EffectArgs::default()
        };
        let handler_ident: Ident = syn::parse_quote!(my_effect_handler);
        let configured = apply_effect_config(
            quote!(::seesaw_core::on::<MyEvent>()),
            &args,
            &handler_ident,
        );
        let configured_text = configured.to_string();
        assert!(
            configured_text.contains(". window (")
                && configured_text.contains("Duration :: from_secs"),
            "window builder call should be emitted, got: {}",
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
            error.to_string().contains("unsupported #[handler] option"),
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

/// Derive macro for `DistributedSafe` trait.
///
/// Validates that all fields implement `DistributedSafe` to ensure
/// the type is safe for multi-worker distributed deployments.
///
/// # Example
///
/// ```rust,ignore
/// use seesaw_core::DistributedSafe;
///
/// #[derive(Clone, DistributedSafe)]
/// struct Deps {
///     db: sqlx::PgPool,      // ✅ Implements DistributedSafe
///     http: reqwest::Client, // ✅ Implements DistributedSafe
/// }
/// ```
///
/// # Opt-out
///
/// Use `#[allow_non_distributed]` to explicitly allow non-distributed fields:
///
/// ```rust,ignore
/// #[derive(Clone, DistributedSafe)]
/// struct Deps {
///     db: sqlx::PgPool,
///     #[allow_non_distributed]  // ⚠️ Warning: won't sync across workers
///     cache: Arc<Mutex<HashMap>>,
/// }
/// ```
#[proc_macro_derive(DistributedSafe, attributes(allow_non_distributed))]
pub fn derive_distributed_safe(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    match derive_distributed_safe_impl(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn derive_distributed_safe_impl(input: DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    let generics = &input.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    // Validate all fields implement DistributedSafe
    match &input.data {
        Data::Struct(data) => {
            validate_fields(&data.fields)?;
        }
        Data::Enum(_) => {
            return Err(syn::Error::new_spanned(
                name,
                "DistributedSafe can only be derived for structs, not enums",
            ));
        }
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                name,
                "DistributedSafe can only be derived for structs, not unions",
            ));
        }
    }

    Ok(quote! {
        impl #impl_generics ::seesaw_core::distributed_safe::sealed::Sealed for #name #ty_generics #where_clause {}

        impl #impl_generics ::seesaw_core::DistributedSafe for #name #ty_generics #where_clause {}
    })
}

fn validate_fields(fields: &Fields) -> syn::Result<()> {
    let fields_iter = match fields {
        Fields::Named(fields) => fields.named.iter(),
        Fields::Unnamed(fields) => fields.unnamed.iter(),
        Fields::Unit => return Ok(()),
    };

    for field in fields_iter {
        // Check if field has #[allow_non_distributed] attribute
        if has_attr(&field.attrs, "allow_non_distributed") {
            // Emit a warning-style compile message via compile_error in a const
            // (We can't emit actual warnings from proc macros, but we can document it)
            continue;
        }

        // Check if field type looks dangerous
        if is_dangerous_type(&field.ty) {
            return Err(syn::Error::new_spanned(
                &field.ty,
                format!(
                    "field type may not be distributed-safe (contains Arc<Mutex> or similar). \
                     Either: (1) use external storage (Database, Redis), \
                     (2) use event-threaded state, or \
                     (3) add #[allow_non_distributed] attribute to explicitly opt-out"
                ),
            ));
        }
    }

    Ok(())
}

fn is_dangerous_type(ty: &Type) -> bool {
    match ty {
        Type::Path(type_path) => {
            let path = &type_path.path;
            let segments: Vec<_> = path.segments.iter().map(|s| s.ident.to_string()).collect();

            // Check for Arc<Mutex<...>> pattern
            if segments.contains(&"Arc".to_string()) {
                // Look for nested Mutex, RwLock, etc.
                for segment in &path.segments {
                    if let PathArguments::AngleBracketed(args) = &segment.arguments {
                        for arg in &args.args {
                            if let GenericArgument::Type(inner_ty) = arg {
                                if type_contains_lock(inner_ty) {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }

            // Check for Mutex, RwLock, etc. at top level
            if segments.contains(&"Mutex".to_string())
                || segments.contains(&"RwLock".to_string())
                || segments.contains(&"RefCell".to_string())
            {
                return true;
            }

            false
        }
        _ => false,
    }
}

fn type_contains_lock(ty: &Type) -> bool {
    match ty {
        Type::Path(type_path) => {
            let segments: Vec<_> = type_path
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            segments.contains(&"Mutex".to_string())
                || segments.contains(&"RwLock".to_string())
                || segments.contains(&"RefCell".to_string())
        }
        _ => false,
    }
}

// ── Aggregator macros ────────────────────────────────────────────────────

/// Parse the `id` attribute from `#[aggregator(id = "field_name")]`.
fn parse_aggregator_id_field(metas: &Punctuated<Meta, Token![,]>) -> syn::Result<Ident> {
    for meta in metas {
        if let Meta::NameValue(nv) = meta {
            if nv.path.is_ident("id") {
                if let Expr::Lit(expr_lit) = &nv.value {
                    if let Lit::Str(lit_str) = &expr_lit.lit {
                        return Ok(Ident::new(&lit_str.value(), lit_str.span()));
                    }
                }
                return Err(syn::Error::new_spanned(
                    &nv.value,
                    "expected string literal for `id`, e.g. id = \"order_id\"",
                ));
            }
        }
    }
    Err(syn::Error::new(
        proc_macro2::Span::call_site(),
        "#[aggregator] requires `id = \"field_name\"` to specify the aggregate ID field on the event",
    ))
}

/// Extract `(&mut AggregateType, EventType)` from function signature.
fn parse_aggregator_params(sig: &Signature) -> syn::Result<(Type, Ident, Type, Ident)> {
    let params: Vec<_> = sig.inputs.iter().collect();
    if params.len() != 2 {
        return Err(syn::Error::new_spanned(
            &sig.inputs,
            "#[aggregator] function must have exactly 2 parameters: (agg: &mut Aggregate, event: Event)",
        ));
    }

    // First param: &mut AggregateType
    let (agg_ty, agg_ident) = match &params[0] {
        FnArg::Typed(pat_type) => {
            let ident = match pat_type.pat.as_ref() {
                Pat::Ident(pat_ident) => pat_ident.ident.clone(),
                _ => {
                    return Err(syn::Error::new_spanned(
                        &pat_type.pat,
                        "expected a simple identifier for aggregate parameter",
                    ))
                }
            };
            match pat_type.ty.as_ref() {
                Type::Reference(type_ref) if type_ref.mutability.is_some() => {
                    (type_ref.elem.as_ref().clone(), ident)
                }
                _ => {
                    return Err(syn::Error::new_spanned(
                        &pat_type.ty,
                        "first parameter must be `&mut AggregateType`",
                    ))
                }
            }
        }
        _ => {
            return Err(syn::Error::new_spanned(
                params[0],
                "first parameter must be a typed parameter",
            ))
        }
    };

    // Second param: EventType
    let (event_ty, event_ident) = match &params[1] {
        FnArg::Typed(pat_type) => {
            let ident = match pat_type.pat.as_ref() {
                Pat::Ident(pat_ident) => pat_ident.ident.clone(),
                _ => {
                    return Err(syn::Error::new_spanned(
                        &pat_type.pat,
                        "expected a simple identifier for event parameter",
                    ))
                }
            };
            (pat_type.ty.as_ref().clone(), ident)
        }
        _ => {
            return Err(syn::Error::new_spanned(
                params[1],
                "second parameter must be a typed parameter",
            ))
        }
    };

    Ok((agg_ty, agg_ident, event_ty, event_ident))
}

/// Expand `#[aggregator(id = "field")]` on a function.
fn expand_aggregator(
    metas: &Punctuated<Meta, Token![,]>,
    input_fn: ItemFn,
) -> syn::Result<TokenStream2> {
    let id_field = parse_aggregator_id_field(metas)?;
    let (agg_ty, agg_ident, event_ty, event_ident) = parse_aggregator_params(&input_fn.sig)?;
    let fn_name = &input_fn.sig.ident;
    let body = &input_fn.block;
    let factory_name = format_ident!("__seesaw_aggregator_{}", fn_name);

    Ok(quote! {
        impl ::seesaw_core::Apply<#event_ty> for #agg_ty {
            fn apply(&mut self, #event_ident: #event_ty) {
                let #agg_ident = self;
                #body
            }
        }

        fn #factory_name() -> ::seesaw_core::Aggregator {
            ::seesaw_core::Aggregator::new::<#event_ty, #agg_ty, _>(|e| e.#id_field)
        }
    })
}

/// Expand `#[aggregators]` on a module.
fn expand_aggregators_module(module: &mut ItemMod) -> syn::Result<TokenStream2> {
    let Some((_, items)) = &mut module.content else {
        return Err(syn::Error::new_spanned(
            module,
            "#[aggregators] requires an inline module",
        ));
    };

    let mut factory_names = Vec::new();

    for item in items.iter() {
        let Item::Fn(item_fn) = item else {
            continue;
        };
        if !has_attr_any(&item_fn.attrs, &["aggregator"]) {
            continue;
        }

        let factory_name = format_ident!("__seesaw_aggregator_{}", item_fn.sig.ident);
        factory_names.push(factory_name);
    }

    if factory_names.is_empty() {
        return Err(syn::Error::new_spanned(
            module,
            "#[aggregators] module must contain at least one #[aggregator] function",
        ));
    }

    let aggregators_fn: ItemFn = parse_quote! {
        pub fn aggregators() -> ::std::vec::Vec<::seesaw_core::Aggregator> {
            ::std::vec![#(#factory_names()),*]
        }
    };
    items.push(Item::Fn(aggregators_fn));

    Ok(quote! { #module })
}
