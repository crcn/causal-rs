use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{
    parse_macro_input, parse_quote, Attribute, Data, DeriveInput, Expr, Fields, FnArg, Ident,
    Item, ItemFn, ItemMod, Lit, Meta, MetaNameValue, Pat, Signature, Token, Type,
};

// The legacy `#[reactor]` / `#[reactors]` / `#[projection]` attribute
// macros and the `DistributedSafe` derive were removed in the 2026-06-10
// audit remediation (Phase 1): they generated calls to APIs that no
// longer exist (`.timeout()`, `.priority()`, `IntoEvents`, …) and had no
// consumers. Implement `Reactor` / `Projector` by hand — the traits are
// two items each.

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
/// Use `id = "field"` for struct events (field access) or `id_fn = "method"` for enum
/// events (method call).
///
/// ```ignore
/// #[aggregators]
/// mod order_aggregators {
///     #[aggregator(id = "order_id")]
///     fn on_placed(order: &mut Order, event: OrderPlaced) {
///         order.status = Status::Placed;
///     }
///
///     // For enum events, use id_fn to call a method instead of accessing a field:
///     #[aggregator(id_fn = "order_id")]
///     fn on_status_changed(order: &mut Order, event: OrderEvent) {
///         // event.order_id() is called to extract the aggregate ID
///     }
/// }
///
/// // Usage: engine.with_aggregators(order_aggregators::aggregators())
/// ```
#[proc_macro_attribute]
pub fn aggregators(attr: TokenStream, item: TokenStream) -> TokenStream {
    let module_metas =
        parse_macro_input!(attr with Punctuated::<Meta, Token![,]>::parse_terminated);
    let mut module = parse_macro_input!(item as ItemMod);
    match expand_aggregators_module(&module_metas, &mut module) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn has_attr(attrs: &[Attribute], name: &str) -> bool {
    attrs.iter().any(|attr| attr.path().is_ident(name))
}

fn has_attr_any(attrs: &[Attribute], names: &[&str]) -> bool {
    names.iter().any(|name| has_attr(attrs, name))
}

/// How an `#[aggregator]` extracts the aggregate id from its event.
enum IdAccess {
    /// `e.field` — for struct events with a public field.
    Field(Ident),
    /// `e.method()` — for enum events with an accessor method.
    Method(Ident),
    /// Singleton — uses `Uuid::nil()` as a constant ID.
    Singleton,
    /// Default: use `Event::stream_id`. Emitted when `#[aggregator]`
    /// has no arguments (or `#[aggregators]` has no module-level
    /// default). Factory uses the plain `Aggregator::for_type::<A,F>()`
    /// path which delegates to `Event::stream_id` internally.
    FactStreamId,
}

fn parse_aggregator_id_access(metas: &Punctuated<Meta, Token![,]>) -> syn::Result<IdAccess> {
    for meta in metas {
        if let Meta::Path(path) = meta {
            if path.is_ident("singleton") {
                return Ok(IdAccess::Singleton);
            }
        }
        if let Meta::NameValue(nv) = meta {
            if nv.path.is_ident("id") {
                if let Expr::Lit(expr_lit) = &nv.value {
                    if let Lit::Str(lit_str) = &expr_lit.lit {
                        return Ok(IdAccess::Field(Ident::new(
                            &lit_str.value(),
                            lit_str.span(),
                        )));
                    }
                }
                return Err(syn::Error::new_spanned(
                    &nv.value,
                    "expected string literal for `id`, e.g. id = \"order_id\"",
                ));
            }
            if nv.path.is_ident("id_fn") {
                if let Expr::Lit(expr_lit) = &nv.value {
                    if let Lit::Str(lit_str) = &expr_lit.lit {
                        return Ok(IdAccess::Method(Ident::new(
                            &lit_str.value(),
                            lit_str.span(),
                        )));
                    }
                }
                return Err(syn::Error::new_spanned(
                    &nv.value,
                    "expected string literal for `id_fn`, e.g. id_fn = \"run_id\"",
                ));
            }
        }
    }
    Ok(IdAccess::FactStreamId)
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

/// Expand `#[aggregator(id = "field")]` or `#[aggregator(id_fn = "method")]` on a function.
fn expand_aggregator(
    metas: &Punctuated<Meta, Token![,]>,
    input_fn: ItemFn,
) -> syn::Result<TokenStream2> {
    let id_access = parse_aggregator_id_access(metas)?;
    expand_aggregator_with_id(&id_access, &input_fn)
}

/// Expand an aggregator function with a pre-resolved `IdAccess`.
fn expand_aggregator_with_id(
    id_access: &IdAccess,
    input_fn: &ItemFn,
) -> syn::Result<TokenStream2> {
    let (agg_ty, agg_ident, event_ty, event_ident) = parse_aggregator_params(&input_fn.sig)?;
    let fn_name = &input_fn.sig.ident;
    let body = &input_fn.block;
    let factory_name = format_ident!("__causal_aggregator_{}", fn_name);

    // Build the id-extraction expression for the aggregator factory.
    // `e` binds to a `&#event_ty` inside the closure passed to
    // `for_type_with_id_fn`; the closure must return `Option<Uuid>`
    // so we wrap user-supplied Uuid-returning fields/methods in
    // `Some(...)`. The singleton case returns `Some(Uuid::nil())`.
    //
    // 0.4.0–0.4.4 silently ignored this attribute and hard-coded
    // `Event::stream_id`. 0.4.5 restores the v0.3 semantics: events
    // can fold into aggregators keyed by a field/method/singleton
    // that differs from their natural stream_id.
    // User id_fn methods may return either `Uuid` or `Option<Uuid>` —
    // the `AggregatorIdValue` trait lifts both into `Option<Uuid>` so
    // either signature compiles. Field access lifts a bare `Uuid`
    // field; singleton always returns `Some(Uuid::nil())`. The default
    // (no attribute) path uses `Aggregator::for_type` which delegates
    // to `Event::stream_id` internally — exactly what most run-scoped
    // facts want.
    let factory_body = match id_access {
        IdAccess::FactStreamId => quote! {
            ::causal::Aggregator::for_type::<#agg_ty, #event_ty>()
        },
        IdAccess::Field(field_ident) => quote! {
            ::causal::Aggregator::for_type_with_id_fn::<#agg_ty, #event_ty, _>(
                |e: &#event_ty| {
                    use ::causal::aggregator::AggregatorIdValue;
                    e.#field_ident.into_aggregator_id()
                }
            )
        },
        IdAccess::Method(method_ident) => quote! {
            ::causal::Aggregator::for_type_with_id_fn::<#agg_ty, #event_ty, _>(
                |e: &#event_ty| {
                    use ::causal::aggregator::AggregatorIdValue;
                    e.#method_ident().into_aggregator_id()
                }
            )
        },
        IdAccess::Singleton => quote! {
            ::causal::Aggregator::for_type_with_id_fn::<#agg_ty, #event_ty, _>(
                |_: &#event_ty| Some(::uuid::Uuid::nil())
            )
        },
    };

    // v0.4 Apply<F>::apply takes `&mut self, fact: &F` (borrow).
    // User authors aggregator fns in v0.3 owned style
    // (`fn on_x(state, event: F)`); the macro adapts by binding
    // `#event_ident` to a `&F` and shadowing it with a cheap clone
    // inside the body so the user's owned-value semantics stand.
    // Event types are Clone by trait bound, so this is uniformly safe.
    Ok(quote! {
        impl ::causal::Apply<#event_ty> for #agg_ty {
            fn apply(&mut self, #event_ident: &#event_ty) {
                let #agg_ident = self;
                // Shadow `event` as owned so v0.3-style bodies that
                // pattern-match or pass-by-value compile unchanged.
                // Clone is bound by Event: Clone.
                let #event_ident: #event_ty = #event_ident.clone();
                #body
            }
        }

        fn #factory_name() -> ::causal::Aggregator {
            #factory_body
        }
    })
}

/// Expand `#[aggregators]` on a module.
///
/// When `#[aggregators(singleton)]` (or `id = "..."` / `id_fn = "..."`) is provided,
/// all `fn` items in the module are treated as aggregator functions — no per-function
/// `#[aggregator]` attribute needed. Functions with their own `#[aggregator(...)]`
/// override the module-level default.
fn expand_aggregators_module(
    module_metas: &Punctuated<Meta, Token![,]>,
    module: &mut ItemMod,
) -> syn::Result<TokenStream2> {
    let Some((_, items)) = &mut module.content else {
        return Err(syn::Error::new_spanned(
            module,
            "#[aggregators] requires an inline module",
        ));
    };

    // No module args → default to `Event::stream_id` for bare functions.
    // Pre-0.4.5, no-args `#[aggregators]` skipped bare functions; now
    // they expand with the documented default.
    let module_id_access = if module_metas.is_empty() {
        Some(IdAccess::FactStreamId)
    } else {
        Some(parse_aggregator_id_access(module_metas)?)
    };

    let mut factory_names = Vec::new();
    let mut expanded_fns = Vec::new();
    let mut expanded_fn_names = Vec::new();

    for item in items.iter() {
        let Item::Fn(item_fn) = item else {
            continue;
        };

        let has_aggregator_attr = has_attr_any(&item_fn.attrs, &["aggregator"]);

        if has_aggregator_attr {
            // Has per-function #[aggregator(...)]: standalone proc macro handles it
            let factory_name = format_ident!("__causal_aggregator_{}", item_fn.sig.ident);
            factory_names.push(factory_name);
        } else if let Some(ref default_id) = module_id_access {
            // No per-function attr, but module-level default exists: expand inline
            let factory_name = format_ident!("__causal_aggregator_{}", item_fn.sig.ident);
            factory_names.push(factory_name.clone());
            expanded_fn_names.push(item_fn.sig.ident.to_string());
            expanded_fns.push(expand_aggregator_with_id(default_id, item_fn)?);
        }
    }

    if factory_names.is_empty() {
        let msg = if module_id_access.is_some() {
            "#[aggregators] module must contain at least one function"
        } else {
            "#[aggregators] module must contain at least one #[aggregator] function"
        };
        return Err(syn::Error::new_spanned(module, msg));
    }

    // Remove functions that were expanded via module-level default
    // (they live outside the module now as impl + factory)
    items.retain(|item| {
        if let Item::Fn(item_fn) = item {
            !expanded_fn_names.contains(&item_fn.sig.ident.to_string())
        } else {
            true
        }
    });

    let aggregators_fn: ItemFn = parse_quote! {
        pub fn aggregators() -> ::std::vec::Vec<::causal::Aggregator> {
            ::std::vec![#(#factory_names()),*]
        }
    };
    items.push(Item::Fn(aggregators_fn));

    let expanded = quote! { #module };
    Ok(quote! {
        #expanded
        #(#expanded_fns)*
    })
}

// ── #[event] proc macro ─────────────────────────────────────────────

/// Marks a type as a causal Event, generating a `causal::Event` impl.
///
/// # Usage
///
/// Every event names its stream identity: `stream_id = "<field>"` (the
/// Uuid field its aggregates fold by — present on every variant), or the
/// explicit `nil_stream` opt-in for genuinely streamless facts
/// (telemetry, ops counters), which routes every value into the single
/// `{category}-nil` stream. Omitting both is a compile error — the old
/// silent nil default produced fan-in aggregates no per-stream read
/// could serve.
///
/// ```ignore
/// // Enum with domain prefix (requires #[serde(tag = "...")])
/// #[event(prefix = "scrape", stream_id = "source_id")]
/// #[derive(Clone, Serialize, Deserialize)]
/// #[serde(tag = "type", rename_all = "snake_case")]
/// pub enum ScrapeEvent {
///     WebScrapeCompleted { source_id: Uuid, occurred_at: DateTime<Utc>, urls_scraped: usize },
///     SourcesResolved { source_id: Uuid, occurred_at: DateTime<Utc> },
/// }
///
/// // Streamless telemetry — explicit opt-in
/// #[event(prefix = "synthesis", ephemeral, nil_stream)]
/// // ...
///
/// // Struct (no prefix needed — snake_case of struct name)
/// #[event(stream_id = "order_id")]
/// #[derive(Clone, Serialize, Deserialize)]
/// pub struct OrderPlaced { pub order_id: Uuid, pub occurred_at: DateTime<Utc> }
/// ```
#[proc_macro_attribute]
pub fn event(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    let args = parse_event_args(attr.into());

    match expand_event(args, input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Parsed arguments for #[event(...)].
struct EventArgs {
    prefix: Option<String>,
    ephemeral: bool,
    /// v0.3 Event: stream category. When both `stream_category` and
    /// `stream_id` are set, the macro additionally generates
    /// `impl ::causal::Event` with `stream()` returning a `StreamRef`
    /// built from the named field on each variant.
    stream_category: Option<String>,
    /// v0.3 Event: name of the field carrying the stream id. Must be
    /// present on every variant. Type must be `Uuid`.
    stream_id: Option<String>,
    /// v0.3 Event: name of the field carrying the logical occurrence
    /// time. Defaults to `"occurred_at"`. Must be present on every
    /// variant when generating Event. Type must be `DateTime<Utc>`.
    occurred_at_field: Option<String>,
    /// v0.7 Event: physical stream this event is *stored* in
    /// (`Event::STREAM_CATEGORY`) — distinct from `prefix`/`CATEGORY`,
    /// which stays the routing key. Set it to co-locate several distinct
    /// event types in one stream (for durable aggregate restore). When
    /// omitted, `STREAM_CATEGORY` defaults to `CATEGORY` (unchanged).
    stream: Option<String>,
    /// v0.9: explicit opt-in to the streamless category-singleton shape
    /// (`stream_id()` = `Uuid::nil()`, every value sharing one
    /// `{category}-nil` stream). Before 0.9 this was the SILENT default
    /// when `stream_id` was omitted — the trap that mass-produced
    /// fan-in aggregates no per-stream read can serve. Now one of
    /// `stream_id = "..."` / `nil_stream` must be written out.
    nil_stream: bool,
}

fn parse_event_args(tokens: TokenStream2) -> EventArgs {
    let mut prefix = None;
    let mut ephemeral = false;
    let mut stream_category = None;
    let mut stream_id = None;
    let mut occurred_at_field = None;
    let mut stream = None;
    let mut nil_stream = false;

    if tokens.is_empty() {
        return EventArgs {
            prefix,
            ephemeral,
            stream_category,
            stream_id,
            occurred_at_field,
            stream,
            nil_stream,
        };
    }

    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    let metas = match parser.parse2(tokens) {
        Ok(m) => m,
        Err(_) => {
            return EventArgs {
                prefix,
                ephemeral,
                stream_category,
                stream_id,
                occurred_at_field,
                stream,
                nil_stream,
            };
        }
    };

    for meta in &metas {
        match meta {
            Meta::NameValue(MetaNameValue { path, value, .. }) if path.is_ident("prefix") => {
                if let Expr::Lit(expr_lit) = value {
                    if let Lit::Str(lit) = &expr_lit.lit {
                        prefix = Some(lit.value());
                    }
                }
            }
            Meta::Path(path) if path.is_ident("ephemeral") => {
                ephemeral = true;
            }
            Meta::Path(path) if path.is_ident("nil_stream") => {
                nil_stream = true;
            }
            Meta::NameValue(MetaNameValue { path, value, .. })
                if path.is_ident("stream_category") =>
            {
                if let Expr::Lit(expr_lit) = value {
                    if let Lit::Str(lit) = &expr_lit.lit {
                        stream_category = Some(lit.value());
                    }
                }
            }
            Meta::NameValue(MetaNameValue { path, value, .. })
                if path.is_ident("stream_id") =>
            {
                if let Expr::Lit(expr_lit) = value {
                    if let Lit::Str(lit) = &expr_lit.lit {
                        stream_id = Some(lit.value());
                    }
                }
            }
            Meta::NameValue(MetaNameValue { path, value, .. })
                if path.is_ident("occurred_at_field") =>
            {
                if let Expr::Lit(expr_lit) = value {
                    if let Lit::Str(lit) = &expr_lit.lit {
                        occurred_at_field = Some(lit.value());
                    }
                }
            }
            Meta::NameValue(MetaNameValue { path, value, .. }) if path.is_ident("stream") => {
                if let Expr::Lit(expr_lit) = value {
                    if let Lit::Str(lit) = &expr_lit.lit {
                        stream = Some(lit.value());
                    }
                }
            }
            _ => {}
        }
    }

    EventArgs {
        prefix,
        ephemeral,
        stream_category,
        stream_id,
        occurred_at_field,
        stream,
        nil_stream,
    }
}

/// Stream identity must be explicit. Before 0.9, omitting `stream_id`
/// silently defaulted `stream_id()` to `Uuid::nil()` — every value of
/// every entity landing in one `{category}-nil` stream. That default
/// mass-produced fan-in aggregates that no per-stream read can serve
/// (an aggregate's stream is the id its facts fold by), and the
/// failure surfaced far from here, at registration or read time.
/// Called AFTER each shape's structural checks (an enum missing its
/// prefix gets THAT error first — no stair-stepping).
fn require_stream_identity(args: &EventArgs, name: &Ident) -> Result<(), syn::Error> {
    if args.stream_id.is_some() && args.nil_stream {
        return Err(syn::Error::new_spanned(
            name,
            "#[event]: `stream_id` and `nil_stream` are contradictory — \
             pick one. `stream_id = \"<field>\"` streams each value by \
             that Uuid field; `nil_stream` puts every value in the single \
             `{category}-nil` stream.",
        ));
    }
    if args.stream_id.is_none() && !args.nil_stream {
        return Err(syn::Error::new_spanned(
            name,
            "#[event] needs a stream identity. Add `stream_id = \"<field>\"` \
             naming the Uuid field this event streams by — the id you fold \
             its aggregate by (e.g. `stream_id = \"signal_id\"`). For a \
             genuinely streamless fact (telemetry, ops counters) opt in \
             explicitly with `nil_stream`: every value then shares the one \
             `{category}-nil` stream, which per-stream aggregates cannot \
             fold and `ctx`-side reads cannot serve.",
        ));
    }
    Ok(())
}

fn expand_event(args: EventArgs, input: DeriveInput) -> Result<TokenStream2, syn::Error> {
    let name = &input.ident;

    match &input.data {
        Data::Enum(data_enum) => expand_event_enum(args, &input, data_enum),
        Data::Struct(_) => expand_event_struct(args, &input),
        Data::Union(_) => Err(syn::Error::new_spanned(
            name,
            "#[event] cannot be applied to unions",
        )),
    }
}

/// Compute the generated `occurred_at()` impl for an enum (both the
/// `stream_id` and `nil_stream` shapes use this, so the rules cannot
/// drift):
///
/// - field on ALL named-fields variants → `Some(*field)` arms; unit /
///   tuple variants (which cannot carry fields) get honest `None` arms;
/// - field on NO variant → no impl (trait default `None`);
/// - MIXED presence among named variants → error naming the odd
///   variant out — that is almost always a typo, and silently
///   returning `None` for one variant makes its events sort by the
///   `created_at` fallback, a quiet data bug;
/// - an explicit `occurred_at_field = "..."` states intent: every
///   named variant must carry it, and at least one variant must be
///   named-fields.
fn enum_occurred_impl(
    name: &Ident,
    data_enum: &syn::DataEnum,
    occurred_field: &str,
    explicit: bool,
) -> Result<TokenStream2, syn::Error> {
    let field_ident = format_ident!("{}", occurred_field);
    let mut have: Vec<&syn::Ident> = Vec::new();
    let mut lack: Vec<&syn::Ident> = Vec::new();
    let mut fieldless: Vec<&syn::Variant> = Vec::new();
    for v in &data_enum.variants {
        match &v.fields {
            Fields::Named(f)
                if f.named.iter().any(|f| f.ident.as_ref() == Some(&field_ident)) =>
            {
                have.push(&v.ident)
            }
            Fields::Named(_) => lack.push(&v.ident),
            Fields::Unnamed(_) | Fields::Unit => fieldless.push(v),
        }
    }
    if explicit {
        if let Some(missing) = lack.first() {
            return Err(syn::Error::new_spanned(
                missing,
                format!(
                    "#[event(occurred_at_field = \"{occurred_field}\")] requires every \
                     named-fields variant to have a `{occurred_field}` field — this \
                     variant lacks it",
                ),
            ));
        }
        if have.is_empty() {
            return Err(syn::Error::new_spanned(
                name,
                format!(
                    "#[event(occurred_at_field = \"{occurred_field}\")]: no variant \
                     carries a `{occurred_field}` field",
                ),
            ));
        }
    }
    if !have.is_empty() && !lack.is_empty() {
        return Err(syn::Error::new_spanned(
            lack[0],
            format!(
                "#[event]: some variants have an `{occurred_field}` field and some \
                 don't — occurred_at() is generated only when ALL named-fields \
                 variants carry it, so this is almost always a typo on the odd \
                 variant out. Add `{occurred_field}` to this variant, or remove it \
                 everywhere for the trait default (None), or name a different field \
                 with `occurred_at_field = \"...\"`.",
            ),
        ));
    }
    if have.is_empty() {
        return Ok(quote! {});
    }
    let some_arms = have.iter().map(|vn| {
        quote! { #name::#vn { #field_ident, .. } => ::core::option::Option::Some(*#field_ident) }
    });
    let none_arms = fieldless.iter().map(|v| {
        let vn = &v.ident;
        match &v.fields {
            Fields::Unnamed(_) => quote! { #name::#vn(..) => ::core::option::Option::None },
            _ => quote! { #name::#vn => ::core::option::Option::None },
        }
    });
    Ok(quote! {
        fn occurred_at(&self) -> ::core::option::Option<::chrono::DateTime<::chrono::Utc>> {
            match self {
                #(#some_arms,)*
                #(#none_arms,)*
            }
        }
    })
}

fn expand_event_enum(
    args: EventArgs,
    input: &DeriveInput,
    data_enum: &syn::DataEnum,
) -> Result<TokenStream2, syn::Error> {
    let name = &input.ident;

    // Require a prefix for enums
    let prefix = args.prefix.clone().ok_or_else(|| {
        syn::Error::new_spanned(
            name,
            "#[event] on enums requires a prefix: #[event(prefix = \"...\")]",
        )
    })?;
    require_stream_identity(&args, name)?;

    // Optional `stream = "..."` → `const STREAM_CATEGORY` (physical stream
    // placement, distinct from the routing CATEGORY). Omitted = trait default.
    let stream_const = match &args.stream {
        Some(s) => quote! { const STREAM_CATEGORY: &'static str = #s; },
        None => quote! {},
    };

    // Parse serde attributes to find tag and rename_all
    let serde_info = parse_serde_attrs(&input.attrs)?;

    // Require #[serde(tag = "...")] for enums
    if serde_info.tag.is_none() && !serde_info.untagged {
        return Err(syn::Error::new_spanned(
            name,
            "#[event] on enums requires #[serde(tag = \"...\")] for variant discrimination",
        ));
    }

    if serde_info.untagged {
        return Err(syn::Error::new_spanned(
            name,
            "#[event] cannot be applied to #[serde(untagged)] enums — untagged enums have no stable variant discriminator",
        ));
    }

    let ephemeral = args.ephemeral;

    // Build match arms for durable_name
    let mut match_arms = Vec::new();
    let mut name_arms = Vec::new();
    for variant in &data_enum.variants {
        let variant_name = &variant.ident;

        // Check for per-variant #[serde(rename = "...")]
        let renamed = get_serde_rename(&variant.attrs);

        let variant_str = if let Some(rename) = renamed {
            rename
        } else {
            apply_rename_rule(&variant_name.to_string(), serde_info.rename_all.as_deref())
        };

        let durable = format!("{}:{}", prefix, variant_str);
        let bare = variant_str.clone();

        let pattern = match &variant.fields {
            Fields::Named(_) => quote! { #name::#variant_name { .. } },
            Fields::Unnamed(_) => quote! { #name::#variant_name(..) },
            Fields::Unit => quote! { #name::#variant_name },
        };

        match_arms.push(quote! { #pattern => #durable });
        name_arms.push(quote! { #pattern => #bare });
    }

    // ─── v0.4 Event impl ──
    //
    // Emit a Event impl in all cases. CATEGORY comes from
    // `stream_category` if supplied, otherwise from `prefix`.
    // stream_id() uses the variant field named by `stream_id` if
    // supplied, otherwise defaults to `Uuid::nil()` (acceptable for
    // category-singleton facts like telemetry where every variant
    // shares one logical "stream"). occurred_at() uses the
    // `occurred_at_field` when supplied, otherwise returns None.
    let fact_impl = if args.stream_id.is_some() {
        // Per-variant stream_id + occurred_at extraction. Each
        // variant MUST have the stream_id field; occurred_at is
        // optional (variants without it get None).
        let category = args.stream_category.as_ref().unwrap_or(&prefix);
        let id_field = args.stream_id.as_ref().unwrap();
        let id_field_ident = format_ident!("{}", id_field);
        let occurred_field = args
            .occurred_at_field
            .clone()
            .unwrap_or_else(|| "occurred_at".to_string());

        // Per-variant stream_id arms. Every variant MUST carry the
        // stream-identity field (a stream-keyed event with a keyless
        // variant has nowhere to land). occurred_at() generation is the
        // shared all/none/mixed rule — see `enum_occurred_impl`.
        let mut stream_arms = Vec::new();
        for variant in &data_enum.variants {
            let variant_name = &variant.ident;
            match &variant.fields {
                Fields::Named(fields) => {
                    let has_id = fields
                        .named
                        .iter()
                        .any(|f| f.ident.as_ref().map(|i| i == &id_field_ident).unwrap_or(false));
                    if !has_id {
                        return Err(syn::Error::new_spanned(
                            variant_name,
                            format!(
                                "#[event(stream_id = \"{}\")] requires every variant to have a `{}` field",
                                id_field, id_field
                            ),
                        ));
                    }
                    stream_arms.push(quote! {
                        #name::#variant_name { #id_field_ident, .. } => *#id_field_ident
                    });
                }
                Fields::Unnamed(_) | Fields::Unit => {
                    return Err(syn::Error::new_spanned(
                        variant_name,
                        "#[event] Event generation requires named-fields variants when stream_id/occurred_at_field are used",
                    ));
                }
            }
        }
        let occurred_impl = enum_occurred_impl(
            name,
            data_enum,
            &occurred_field,
            args.occurred_at_field.is_some(),
        )?;

        quote! {
            impl ::causal::Event for #name {
                const CATEGORY: &'static str = #category;
                #stream_const
                fn event_type(&self) -> &str {
                    match self {
                        #(#name_arms,)*
                    }
                }
                fn stream_id(&self) -> ::uuid::Uuid {
                    match self {
                        #(#stream_arms,)*
                    }
                }
                #occurred_impl
            }
        }
    } else {
        // Explicit `nil_stream` opt-in (`require_stream_identity`
        // rejects plain omission): emit a Event impl with the prefix as
        // CATEGORY, bare variant name as `name()`, and `Uuid::nil()` as
        // stream_id (category-singleton). For operational/telemetry
        // events that genuinely aren't per-aggregate. occurred_at()
        // follows the same shared rule as the stream_id shape — a
        // nil_stream event with timestamps must not silently lose them.
        let occurred_field = args
            .occurred_at_field
            .clone()
            .unwrap_or_else(|| "occurred_at".to_string());
        let occurred_impl = enum_occurred_impl(
            name,
            data_enum,
            &occurred_field,
            args.occurred_at_field.is_some(),
        )?;
        quote! {
            impl ::causal::Event for #name {
                const CATEGORY: &'static str = #prefix;
                #stream_const
                fn event_type(&self) -> &str {
                    match self {
                        #(#name_arms,)*
                    }
                }
                fn stream_id(&self) -> ::uuid::Uuid {
                    ::uuid::Uuid::nil()
                }
                #occurred_impl
            }
        }
    };

    // Legacy `Event` impl emission removed in P11.d — only `Event`
    // is generated now.
    let _ = (match_arms, ephemeral);
    Ok(quote! {
        #input

        #fact_impl
    })
}

fn expand_event_struct(
    args: EventArgs,
    input: &DeriveInput,
) -> Result<TokenStream2, syn::Error> {
    let name = &input.ident;
    require_stream_identity(&args, name)?;
    let ephemeral = args.ephemeral;

    // For structs, the durable name is the snake_case of the struct name
    // OR the prefix if provided
    let durable = if let Some(ref prefix) = args.prefix {
        prefix.clone()
    } else {
        to_snake_case(&name.to_string())
    };

    let prefix_str = durable.clone();

    // v0.9 Event impl for structs. CATEGORY = `stream_category` if
    // supplied, else `prefix` (or the snake-cased struct name).
    // stream_id reads the field named by the (required) `stream_id`
    // arg; `nil_stream` is the explicit opt-in for the streamless
    // shape. occurred_at() is generated when the `occurred_at` field
    // (or the `occurred_at_field` override) is present.
    let bare_name = prefix_str.clone();
    // Optional `stream = "..."` → `const STREAM_CATEGORY` (physical stream
    // placement, distinct from the routing CATEGORY). Omitted = trait default.
    let stream_const = match &args.stream {
        Some(s) => quote! { const STREAM_CATEGORY: &'static str = #s; },
        None => quote! {},
    };
    let fact_impl = if let Some(id_field) = args.stream_id.as_ref() {
        let category = args.stream_category.as_ref().unwrap_or(&prefix_str);
        let id_field_ident = format_ident!("{}", id_field);
        let occurred_field = args
            .occurred_at_field
            .clone()
            .unwrap_or_else(|| "occurred_at".to_string());
        let occurred_field_ident = format_ident!("{}", occurred_field);

        // Struct fields are in view at expansion time, so a missing
        // field must be a teaching error HERE — not a raw rustc
        // "no field" error pointing into generated code.
        let named_fields: Vec<&Ident> = match &input.data {
            Data::Struct(d) => match &d.fields {
                Fields::Named(f) => f.named.iter().filter_map(|f| f.ident.as_ref()).collect(),
                _ => {
                    return Err(syn::Error::new_spanned(
                        name,
                        format!(
                            "#[event(stream_id = \"{id_field}\")] requires a named-fields \
                             struct (the macro reads `self.{id_field}`)",
                        ),
                    ));
                }
            },
            _ => unreachable!("expand_event_struct only receives structs"),
        };
        if !named_fields.iter().any(|f| **f == id_field_ident) {
            return Err(syn::Error::new_spanned(
                name,
                format!(
                    "#[event(stream_id = \"{id_field}\")]: this struct has no \
                     `{id_field}` field — name the Uuid field this event streams by",
                ),
            ));
        }
        let has_occurred = named_fields.iter().any(|f| **f == occurred_field_ident);
        if args.occurred_at_field.is_some() && !has_occurred {
            return Err(syn::Error::new_spanned(
                name,
                format!(
                    "#[event(occurred_at_field = \"{occurred_field}\")]: this struct \
                     has no `{occurred_field}` field",
                ),
            ));
        }
        // occurred_at() is generated only when the field exists —
        // absent = the trait's default `None` (timeless facts are
        // legitimate; requiring a timestamp to satisfy the macro
        // would be ceremony).
        let occurred_impl = if has_occurred {
            quote! {
                fn occurred_at(&self) -> ::core::option::Option<::chrono::DateTime<::chrono::Utc>> {
                    ::core::option::Option::Some(self.#occurred_field_ident)
                }
            }
        } else {
            quote! {}
        };
        quote! {
            impl ::causal::Event for #name {
                const CATEGORY: &'static str = #category;
                #stream_const
                fn event_type(&self) -> &str { #bare_name }
                fn stream_id(&self) -> ::uuid::Uuid {
                    self.#id_field_ident
                }
                #occurred_impl
            }
        }
    } else {
        // `nil_stream` opt-in. occurred_at() follows the same
        // presence-conditional rule as the stream_id shape — a
        // nil_stream event with a timestamp must not silently lose it.
        let category = args.stream_category.as_ref().unwrap_or(&prefix_str);
        let occurred_field = args
            .occurred_at_field
            .clone()
            .unwrap_or_else(|| "occurred_at".to_string());
        let occurred_field_ident = format_ident!("{}", occurred_field);
        let has_occurred = match &input.data {
            Data::Struct(d) => match &d.fields {
                Fields::Named(f) => f
                    .named
                    .iter()
                    .any(|f| f.ident.as_ref() == Some(&occurred_field_ident)),
                _ => false, // unit/tuple struct — no named fields to read
            },
            _ => unreachable!("expand_event_struct only receives structs"),
        };
        if args.occurred_at_field.is_some() && !has_occurred {
            return Err(syn::Error::new_spanned(
                name,
                format!(
                    "#[event(occurred_at_field = \"{occurred_field}\")]: this struct \
                     has no `{occurred_field}` field",
                ),
            ));
        }
        let occurred_impl = if has_occurred {
            quote! {
                fn occurred_at(&self) -> ::core::option::Option<::chrono::DateTime<::chrono::Utc>> {
                    ::core::option::Option::Some(self.#occurred_field_ident)
                }
            }
        } else {
            quote! {}
        };
        quote! {
            impl ::causal::Event for #name {
                const CATEGORY: &'static str = #category;
                #stream_const
                fn event_type(&self) -> &str { #bare_name }
                fn stream_id(&self) -> ::uuid::Uuid {
                    ::uuid::Uuid::nil()
                }
                #occurred_impl
            }
        }
    };

    // Legacy `Event` impl emission removed in P11.d — see the enum
    // path above for rationale.
    let _ = (durable, prefix_str, ephemeral);
    Ok(quote! {
        #input

        #fact_impl
    })
}

/// Parsed serde attributes relevant to event macro.
struct SerdeInfo {
    tag: Option<String>,
    rename_all: Option<String>,
    untagged: bool,
}

fn parse_serde_attrs(attrs: &[Attribute]) -> Result<SerdeInfo, syn::Error> {
    let mut info = SerdeInfo {
        tag: None,
        rename_all: None,
        untagged: false,
    };

    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }

        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("tag") {
                let value = meta.value()?;
                let lit: Lit = value.parse()?;
                if let Lit::Str(s) = lit {
                    info.tag = Some(s.value());
                }
            } else if meta.path.is_ident("rename_all") {
                let value = meta.value()?;
                let lit: Lit = value.parse()?;
                if let Lit::Str(s) = lit {
                    info.rename_all = Some(s.value());
                }
            } else if meta.path.is_ident("untagged") {
                info.untagged = true;
            } else if meta.input.peek(Token![=]) {
                // Consume `= "value"` for unknown key=value attrs (e.g. content = "data")
                let _: Token![=] = meta.input.parse()?;
                let _: Lit = meta.input.parse()?;
            } else {
                // Skip unknown flag attrs
            }
            Ok(())
        })?;
    }

    Ok(info)
}

/// Get #[serde(rename = "...")] from variant attributes.
fn get_serde_rename(attrs: &[Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }

        let mut rename = None;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let value = meta.value()?;
                let lit: Lit = value.parse()?;
                if let Lit::Str(s) = lit {
                    rename = Some(s.value());
                }
            }
            Ok(())
        });
        if rename.is_some() {
            return rename;
        }
    }
    None
}

/// Apply a serde rename_all rule to a variant name.
fn apply_rename_rule(name: &str, rule: Option<&str>) -> String {
    match rule {
        Some("snake_case") => to_snake_case(name),
        Some("camelCase") => to_camel_case(name),
        Some("PascalCase") => name.to_string(),
        Some("SCREAMING_SNAKE_CASE") => to_snake_case(name).to_uppercase(),
        Some("kebab-case") => to_snake_case(name).replace('_', "-"),
        _ => name.to_string(), // No rename_all or unknown rule — use as-is
    }
}

/// Convert PascalCase to snake_case.
fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    let mut prev_was_upper = false;
    let mut prev_was_underscore = true; // treat start as underscore

    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() {
            // Insert underscore before uppercase if:
            // - not at start
            // - previous char was NOT uppercase (camelCase boundary)
            // - OR next char is lowercase (acronym end like "HTTPServer" → "http_server")
            if i > 0 && !prev_was_underscore {
                if !prev_was_upper {
                    result.push('_');
                } else {
                    // Check if next char is lowercase (end of acronym)
                    let next_is_lower = s.chars().nth(i + 1).map_or(false, |c| c.is_lowercase());
                    if next_is_lower {
                        result.push('_');
                    }
                }
            }
            result.push(ch.to_lowercase().next().unwrap());
            prev_was_upper = true;
            prev_was_underscore = false;
        } else if ch == '_' {
            result.push('_');
            prev_was_upper = false;
            prev_was_underscore = true;
        } else {
            result.push(ch);
            prev_was_upper = false;
            prev_was_underscore = false;
        }
    }

    result
}

/// Convert PascalCase to camelCase.
fn to_camel_case(s: &str) -> String {
    let mut result = String::new();
    let mut first = true;

    for ch in s.chars() {
        if first && ch.is_uppercase() {
            result.push(ch.to_lowercase().next().unwrap());
            first = false;
        } else {
            result.push(ch);
            first = false;
        }
    }

    result
}
