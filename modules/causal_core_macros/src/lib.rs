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
    /// Default: use `Event::subject_id`. Emitted when `#[aggregator]`
    /// has no arguments (or `#[aggregators]` has no module-level
    /// default). Factory uses the plain `Aggregator::for_type::<A,F>()`
    /// path which delegates to `Event::subject_id` internally.
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
    // `Event::subject_id`. 0.4.5 restores the v0.3 semantics: events
    // can fold into aggregators keyed by a field/method/singleton
    // that differs from their natural subject_id.
    // User id_fn methods may return either `Uuid` or `Option<Uuid>` —
    // the `AggregatorIdValue` trait lifts both into `Option<Uuid>` so
    // either signature compiles. Field access lifts a bare `Uuid`
    // field; singleton always returns `Some(Uuid::nil())`. The default
    // (no attribute) path uses `Aggregator::for_type` which delegates
    // to `Event::subject_id` internally — exactly what most run-scoped
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

    // No module args → default to `Event::subject_id` for bare functions.
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

/// Marks a struct as a causal fact, generating its `causal::Event` impl.
///
/// Every fact declares its identities; machinery is derived:
///
/// ```ignore
/// // what it's CALLED        about which THING     (workflow_id: which WORK — 0.10 §3)
/// #[event(name = "job_opened", subject_id = "job_id")]
/// #[derive(Clone, Serialize, Deserialize)]
/// pub struct JobOpened { pub job_id: Uuid, pub occurred_at: DateTime<Utc> }
///
/// // co-location: several fact families, one subject history
/// #[event(name = "job_billed", subject_id = "job_id", subject = "job")]
/// pub struct JobBilled { pub job_id: Uuid, pub cents: u64 }
///
/// // provably subject-less (no Uuid fields): omission is legal
/// #[event(name = "tick", ephemeral)]
/// pub struct TickRecorded { pub n: u64 }
///
/// // reference-carrying subject-less: explicit opt-out
/// #[event(name = "cache_purged", no_subject)]
/// pub struct CachePurged { pub requested_by: Uuid }
/// ```
///
/// `name` is REQUIRED and never derived from the type name: it is the
/// wire `event_type`, matched by equality — a type rename must not
/// silently re-vocabulary the log. Enums are retracted (one fact = one
/// struct); see the error for the variant-poison argument.
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
    /// `name = "job_opened"` — the fact's kind, the exact `event_type`
    /// on the wire, matched by consumers by equality. REQUIRED and
    /// never derived from the type name: it is a wire format, and a
    /// type rename must not silently re-vocabulary the log.
    name: Option<String>,
    ephemeral: bool,
    /// v0.3 Event: name of the field carrying the stream id. Must be
    /// present on every variant. Type must be `Uuid`.
    subject_id: Option<String>,
    /// v0.3 Event: name of the field carrying the logical occurrence
    /// time. Defaults to `"occurred_at"`. Must be present on every
    /// variant when generating Event. Type must be `DateTime<Utc>`.
    occurred_at_field: Option<String>,
    /// `subject = "job"` — the subject KIND whose history this fact
    /// joins (`Event::SUBJECT`, the storage stream's left half). Set it
    /// to co-locate several distinct fact families in one subject
    /// history (the anti-god-enum valve; durable restore reads exactly
    /// this history). Defaults to `CATEGORY`.
    subject: Option<String>,
    /// v0.9: explicit opt-in to the streamless category-singleton shape
    /// (`subject_id()` = `Uuid::nil()`, every value sharing one
    /// `{category}-nil` stream). Before 0.9 this was the SILENT default
    /// when `subject_id` was omitted — the trap that mass-produced
    /// fan-in aggregates no per-stream read can serve. Now one of
    /// `subject_id = "..."` / `no_subject` must be written out.
    no_subject: bool,
}

fn parse_event_args(tokens: TokenStream2) -> EventArgs {
    let mut name_arg = None;
    let mut ephemeral = false;
    let mut subject_id = None;
    let mut occurred_at_field = None;
    let mut subject = None;
    let mut no_subject = false;

    if tokens.is_empty() {
        return EventArgs {
            name: name_arg,
            ephemeral,
            subject_id,
            occurred_at_field,
            subject,
            no_subject,
        };
    }

    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    let metas = match parser.parse2(tokens) {
        Ok(m) => m,
        Err(_) => {
            return EventArgs {
                name: name_arg,
                ephemeral,
                subject_id,
                occurred_at_field,
                subject,
                no_subject,
            };
        }
    };

    for meta in &metas {
        match meta {
            Meta::NameValue(MetaNameValue { path, value, .. }) if path.is_ident("name") => {
                if let Expr::Lit(expr_lit) = value {
                    if let Lit::Str(lit) = &expr_lit.lit {
                        name_arg = Some(lit.value());
                    }
                }
            }
            Meta::Path(path) if path.is_ident("ephemeral") => {
                ephemeral = true;
            }
            Meta::Path(path) if path.is_ident("no_subject") => {
                no_subject = true;
            }
            Meta::NameValue(MetaNameValue { path, value, .. })
                if path.is_ident("subject_id") =>
            {
                if let Expr::Lit(expr_lit) = value {
                    if let Lit::Str(lit) = &expr_lit.lit {
                        subject_id = Some(lit.value());
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
            Meta::NameValue(MetaNameValue { path, value, .. }) if path.is_ident("subject") => {
                if let Expr::Lit(expr_lit) = value {
                    if let Lit::Str(lit) = &expr_lit.lit {
                        subject = Some(lit.value());
                    }
                }
            }
            _ => {}
        }
    }

    EventArgs {
        name: name_arg,
        ephemeral,
        subject_id,
        occurred_at_field,
        subject,
        no_subject,
    }
}

/// Stream identity must be explicit. Before 0.9, omitting `subject_id`
/// silently defaulted `subject_id()` to `Uuid::nil()` — every value of
/// every entity landing in one `{category}-nil` stream. That default
/// mass-produced fan-in aggregates that no per-stream read can serve
/// (an aggregate's stream is the id its facts fold by), and the
/// failure surfaced far from here, at registration or read time.
/// Called AFTER each shape's structural checks (an enum missing its
/// prefix gets THAT error first — no stair-stepping).
fn require_subject_identity(args: &EventArgs, name: &Ident, input: &DeriveInput) -> Result<(), syn::Error> {
    if args.subject_id.is_some() && args.no_subject {
        return Err(syn::Error::new_spanned(
            name,
            "#[event]: `subject_id` and `no_subject` are contradictory — \
             pick one. `subject_id = \"<field>\"` keys each value by that \
             Uuid field; `no_subject` puts every value in the single \
             shared subject-less history.",
        ));
    }
    if args.subject_id.is_none() && !args.no_subject {
        // Shape-gated omission: inference is allowed only where being
        // wrong is impossible. A fact with NO scalar Uuid fields cannot
        // name a subject — omission is unambiguous and legal. A fact
        // that carries candidate ids and declares nothing is almost
        // always a forgotten declaration: teaching error.
        let candidates = candidate_subject_fields(input);
        if candidates.is_empty() {
            return Ok(()); // provably subject-less
        }
        return Err(syn::Error::new_spanned(
            name,
            format!(
                "#[event]: this fact carries Uuid field(s) {} but declares \
                 no subject. If it is ABOUT one of them, name it: \
                 `subject_id = \"{}\"` — the id you fold its state by. If \
                 those ids are only references, opt out explicitly with \
                 `no_subject`: every value then shares one subject-less \
                 history, which per-subject state reads cannot serve.",
                candidates
                    .iter()
                    .map(|c| format!("`{c}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                candidates[0],
            ),
        ));
    }
    Ok(())
}

/// Scalar `Uuid`-typed named fields — the candidate subjects — across
/// the struct or every enum variant. `Vec<Uuid>`/`Option<Uuid>` are not
/// candidates: a subject is one id, named by one scalar field.
fn candidate_subject_fields(input: &DeriveInput) -> Vec<String> {
    fn is_uuid(ty: &Type) -> bool {
        match ty {
            Type::Path(p) => p
                .path
                .segments
                .last()
                .map(|seg| seg.ident == "Uuid" && matches!(seg.arguments, syn::PathArguments::None))
                .unwrap_or(false),
            _ => false,
        }
    }
    let mut out: Vec<String> = Vec::new();
    let mut push_fields = |fields: &Fields| {
        if let Fields::Named(named) = fields {
            for f in &named.named {
                if is_uuid(&f.ty) {
                    if let Some(id) = &f.ident {
                        let n = id.to_string();
                        if !out.contains(&n) {
                            out.push(n);
                        }
                    }
                }
            }
        }
    };
    match &input.data {
        Data::Struct(d) => push_fields(&d.fields),
        Data::Enum(e) => {
            for v in &e.variants {
                push_fields(&v.fields);
            }
        }
        Data::Union(_) => {}
    }
    out
}

fn expand_event(args: EventArgs, input: DeriveInput) -> Result<TokenStream2, syn::Error> {
    let name = &input.ident;

    match &input.data {
        Data::Enum(_) => Err(syn::Error::new_spanned(
            name,
            "#[event]: the enum fact form was retracted (no-lying-defaults \
             \u{a7}1). One fact = one struct. A family enum's trigger \
             deserializes by serde tag, so ADDING A VARIANT poisons every \
             deployed consumer of the family (unknown variant \u{2192} \
             deserialization failure \u{2192} terminal failure) \u{2014} \
             vocabulary growth as a breaking change. Split each variant \
             into its own struct fact; to share one subject history, put \
             `subject = \"<kind>\"` on each.",
        )),
        Data::Struct(_) => expand_event_struct(args, &input),
        Data::Union(_) => Err(syn::Error::new_spanned(
            name,
            "#[event] cannot be applied to unions",
        )),
    }
}

/// Non-empty check for the declared kind, at expansion time.
fn crate_validate_kind(kind: &str, span: &Ident) -> Result<(), syn::Error> {
    if kind.is_empty() {
        return Err(syn::Error::new_spanned(
            span,
            "#[event]: `name` is empty — the kind is a wire identity",
        ));
    }
    Ok(())
}

fn expand_event_struct(
    args: EventArgs,
    input: &DeriveInput,
) -> Result<TokenStream2, syn::Error> {
    let name = &input.ident;
    require_subject_identity(&args, name, input)?;
    let ephemeral = args.ephemeral;

    // `name` is REQUIRED: the wire event_type is a format you pick
    // once. Deriving it from the type name would turn a rename
    // refactor into a silent vocabulary change (new emits stop
    // matching deployed consumers; old events stop matching new code).
    let Some(kind) = args.name.clone() else {
        return Err(syn::Error::new_spanned(
            name,
            "#[event] needs a `name` — the fact's kind, written verbatim \
             to the wire and matched by consumers by equality (e.g. \
             `#[event(name = \"job_opened\", subject_id = \"job_id\")]`). \
             It is never derived from the type name: renaming a type must \
             not silently re-vocabulary the log.",
        ));
    };
    crate_validate_kind(&kind, name)?;
    // Optional `subject = "..."` → `const SUBJECT` (subject-history
    // placement, distinct from the routing CATEGORY). Omitted = trait default.
    let stream_const = match &args.subject {
        Some(s) => quote! { const SUBJECT: &'static str = #s; },
        None => quote! {},
    };
    let fact_impl = if let Some(id_field) = args.subject_id.as_ref() {
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
                            "#[event(subject_id = \"{id_field}\")] requires a named-fields \
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
                    "#[event(subject_id = \"{id_field}\")]: this struct has no \
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
                const NAME: &'static str = #kind;
                #stream_const
                fn subject_id(&self) -> ::uuid::Uuid {
                    self.#id_field_ident
                }
                #occurred_impl
            }
        }
    } else {
        // `no_subject` opt-in. occurred_at() follows the same
        // presence-conditional rule as the subject_id shape — a
        // no_subject event with a timestamp must not silently lose it.
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
                const NAME: &'static str = #kind;
                #stream_const
                fn subject_id(&self) -> ::uuid::Uuid {
                    ::uuid::Uuid::nil()
                }
                #occurred_impl
            }
        }
    };

    let _ = ephemeral;
    Ok(quote! {
        #input

        #fact_impl
    })
}
