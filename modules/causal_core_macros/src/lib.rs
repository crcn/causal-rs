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
    /// 0.10 §3: `workflow_id = "field"` — declares this fact a
    /// workflow ROOT whose workflow is named BY the given payload
    /// field (macro-generated `declared_workflow_id()` accessor, the
    /// same machinery as `subject_id`). Omitted = chain member
    /// (inherits the emitter's workflow). Constitutional: a type is a
    /// root or a member at every emit site.
    workflow_id: Option<String>,
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
    let mut workflow_id = None;
    let mut no_subject = false;

    if tokens.is_empty() {
        return EventArgs {
            name: name_arg,
            ephemeral,
            subject_id,
            occurred_at_field,
            subject,
            workflow_id,
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
                workflow_id,
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
            Meta::NameValue(MetaNameValue { path, value, .. })
                if path.is_ident("workflow_id") =>
            {
                if let Expr::Lit(expr_lit) = value {
                    if let Lit::Str(lit) = &expr_lit.lit {
                        workflow_id = Some(lit.value());
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
        workflow_id,
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
    // Optional `workflow_id = "field"` → this fact is a workflow ROOT:
    // its envelope workflow is stamped FROM the named payload field
    // (one source of truth — payload and envelope cannot disagree).
    // Omitted = trait default `None` (chain member).
    let workflow_impl = match &args.workflow_id {
        Some(wf_field) => {
            let wf_ident = format_ident!("{}", wf_field);
            let has_field = match &input.data {
                Data::Struct(d) => match &d.fields {
                    Fields::Named(f) => f
                        .named
                        .iter()
                        .any(|f| f.ident.as_ref() == Some(&wf_ident)),
                    _ => false,
                },
                _ => unreachable!("expand_event_struct only receives structs"),
            };
            if !has_field {
                return Err(syn::Error::new_spanned(
                    name,
                    format!(
                        "#[event(workflow_id = \"{wf_field}\")]: this struct has no \
                         `{wf_field}` field — name the Uuid field that roots this \
                         fact's workflow (derive its value with ctx.derive_id; a \
                         new_v4() workflow id mints a phantom workflow on redelivery)",
                    ),
                ));
            }
            quote! {
                fn declared_workflow_id(&self) -> ::core::option::Option<::uuid::Uuid> {
                    ::core::option::Option::Some(self.#wf_ident)
                }
            }
        }
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
                #workflow_impl
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
                #workflow_impl
            }
        }
    };

    let _ = ephemeral;
    Ok(quote! {
        #input

        #fact_impl
    })
}

// ── #[reactors] / #[projectors] module macros (0.10 Primitive 7) ─────
//
// Revival of the macro deleted in the 2026-06-10 audit, with the
// lessons applied: the macro removes trait ceremony ONLY — it is
// forbidden from deciding anything load-bearing. Inference is allowed
// only where wrong inference fails loudly (the trigger type from the
// fn signature — a wrong type changes which events arrive, visibly);
// everything whose wrongness is silent stays explicit (`name` names a
// durable cursor; `kinds` entries are settle-latency couplings;
// `ordering` / `max_in_flight` are execution semantics that belong in
// the same diff as the body they govern).

/// What one `#[reactor]`/`#[projector]` fn parsed into.
struct ConsumerFn {
    fn_ident: Ident,
    struct_ident: Ident,
    /// The event/trigger parameter's type (`&T` → `T`).
    event_ty: Type,
    name: String,
    ordering: Option<Ident>,
    max_in_flight: Option<u64>,
    kinds: Option<Vec<String>>,
}

fn lit_str_of(value: &Expr) -> Option<String> {
    if let Expr::Lit(el) = value {
        if let Lit::Str(s) = &el.lit {
            return Some(s.value());
        }
    }
    None
}

/// Parse the module attr: optional `deps = SomeType`.
fn parse_module_deps(attr: TokenStream2, module: &ItemMod) -> syn::Result<Option<Type>> {
    if attr.is_empty() {
        return Ok(None);
    }
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    let metas = parser
        .parse2(attr)
        .map_err(|e| syn::Error::new_spanned(module, format!("bad module args: {e}")))?;
    for meta in &metas {
        if let Meta::NameValue(MetaNameValue { path, value, .. }) = meta {
            if path.is_ident("deps") {
                if let Expr::Path(p) = value {
                    return Ok(Some(Type::Path(syn::TypePath {
                        qself: None,
                        path: p.path.clone(),
                    })));
                }
                return Err(syn::Error::new_spanned(
                    value,
                    "deps takes a type, e.g. `deps = JobDeps`",
                ));
            }
        }
    }
    Ok(None)
}

/// Parse one consumer fn: validate the attr + signature, extract the
/// event type. `attr_name` is "reactor" or "projector"; `has_deps`
/// decides the expected arity.
fn parse_consumer_fn(
    attr_name: &str,
    metas: &Punctuated<Meta, Token![,]>,
    item_fn: &ItemFn,
    has_deps: bool,
) -> syn::Result<ConsumerFn> {
    let fn_ident = item_fn.sig.ident.clone();
    let mut name = None;
    let mut ordering = None;
    let mut max_in_flight = None;
    let mut kinds: Option<Vec<String>> = None;

    for meta in metas {
        match meta {
            Meta::NameValue(MetaNameValue { path, value, .. }) if path.is_ident("name") => {
                name = lit_str_of(value);
            }
            Meta::NameValue(MetaNameValue { path, value, .. })
                if path.is_ident("ordering") && attr_name == "reactor" =>
            {
                if let Expr::Path(p) = value {
                    if let Some(id) = p.path.get_ident() {
                        let canon = match id.to_string().as_str() {
                            "per_subject" => "PerSubject",
                            "per_workflow" => "PerWorkflow",
                            "none" => "None",
                            other => {
                                return Err(syn::Error::new_spanned(
                                    value,
                                    format!(
                                        "unknown ordering `{other}` — one of \
                                         per_subject | per_workflow | none",
                                    ),
                                ));
                            }
                        };
                        ordering = Some(format_ident!("{canon}"));
                    }
                }
            }
            Meta::NameValue(MetaNameValue { path, value, .. })
                if path.is_ident("max_in_flight") && attr_name == "reactor" =>
            {
                if let Expr::Lit(el) = value {
                    if let Lit::Int(i) = &el.lit {
                        max_in_flight = Some(i.base10_parse::<u64>()?);
                    }
                }
            }
            Meta::NameValue(MetaNameValue { path, value, .. })
                if path.is_ident("kinds") && attr_name == "projector" =>
            {
                if let Expr::Array(arr) = value {
                    let mut out = Vec::new();
                    for el in &arr.elems {
                        match lit_str_of(el) {
                            Some(s) => out.push(s),
                            None => {
                                return Err(syn::Error::new_spanned(
                                    el,
                                    "kinds entries are string literals, e.g. \
                                     `kinds = [\"job_opened\", \"job_billed\"]`",
                                ));
                            }
                        }
                    }
                    kinds = Some(out);
                }
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    format!("unknown #[{attr_name}] argument"),
                ));
            }
        }
    }

    // `name` is REQUIRED: it names a DURABLE CURSOR. A fn-name-derived
    // default would turn a rename refactor into an orphaned cursor
    // (reactors reseed at latest — backlog silently skipped; projectors
    // reseed at zero — full re-projection into live read models).
    let Some(name) = name else {
        return Err(syn::Error::new_spanned(
            &item_fn.sig.ident,
            format!(
                "#[{attr_name}] needs a `name` — it names this consumer's \
                 DURABLE CURSOR (checkpoint rows, failure records, the \
                 inspector). Deriving it from the fn name would turn a rename \
                 refactor into an orphaned cursor. e.g. \
                 `#[{attr_name}(name = \"job.enrich\")]`",
            ),
        ));
    };

    // Signature: (deps: &Deps,)? event: &E, ctx: Ctx<'_>
    let expected = if has_deps { 3 } else { 2 };
    let params: Vec<&FnArg> = item_fn.sig.inputs.iter().collect();
    if params.len() != expected {
        let shape = if has_deps {
            "(deps: &Deps, event: &E, ctx: Ctx<'_>)"
        } else {
            "(event: &E, ctx: Ctx<'_>)"
        };
        return Err(syn::Error::new_spanned(
            &item_fn.sig,
            format!(
                "#[{attr_name}] fn takes {shape} — this module {} `deps`",
                if has_deps { "declares" } else { "declares no" },
            ),
        ));
    }
    let event_param = params[if has_deps { 1 } else { 0 }];
    let FnArg::Typed(pt) = event_param else {
        return Err(syn::Error::new_spanned(event_param, "expected a typed parameter"));
    };
    let Type::Reference(r) = &*pt.ty else {
        return Err(syn::Error::new_spanned(
            &pt.ty,
            "the event parameter is taken by reference: `event: &E`",
        ));
    };
    let event_ty = (*r.elem).clone();

    // The attribute decides typed-vs-raw; the signature must agree.
    let is_recorded = matches!(
        &event_ty,
        Type::Path(tp) if tp.path.segments.last()
            .map(|s| s.ident == "RecordedEvent").unwrap_or(false)
    );
    if attr_name == "projector" {
        if kinds.is_some() && !is_recorded {
            return Err(syn::Error::new_spanned(
                &pt.ty,
                "#[projector(kinds = [...])] is the cross-kind shape — its fn \
                 takes the raw `event: &RecordedEvent` and routes by \
                 `event.event_type`. For a single typed event, drop `kinds`.",
            ));
        }
        if kinds.is_none() && is_recorded {
            return Err(syn::Error::new_spanned(
                &pt.ty,
                "a `&RecordedEvent` projector must declare \
                 `kinds = [\"...\"]` — each entry is a settle-latency \
                 coupling (settle waits on projector cursors), so the \
                 subscription is written out, never inferred.",
            ));
        }
    }

    Ok(ConsumerFn {
        struct_ident: format_ident!("__causal_{}_{}", attr_name, fn_ident),
        fn_ident,
        event_ty,
        name,
        ordering,
        max_in_flight,
        kinds,
    })
}

/// Shared driver for `#[reactors]` / `#[projectors]` on a module.
fn expand_consumers_module(
    attr: TokenStream2,
    mut module: ItemMod,
    attr_name: &str,
) -> syn::Result<TokenStream2> {
    let deps_ty = parse_module_deps(attr, &module)?;
    let Some((_, items)) = &mut module.content else {
        return Err(syn::Error::new_spanned(
            module,
            format!("#[{attr_name}s] requires an inline module"),
        ));
    };

    let mut parsed: Vec<ConsumerFn> = Vec::new();
    for item in items.iter_mut() {
        let Item::Fn(item_fn) = item else { continue };
        // Take the matching attr off the fn (it's ours, not rustc's).
        let Some(pos) = item_fn.attrs.iter().position(|a| a.path().is_ident(attr_name))
        else {
            continue; // bare fn — a helper, left untouched
        };
        let attr = item_fn.attrs.remove(pos);
        let metas = match &attr.meta {
            Meta::List(l) => {
                let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
                parser.parse2(l.tokens.clone())?
            }
            _ => Punctuated::new(),
        };
        parsed.push(parse_consumer_fn(attr_name, &metas, item_fn, deps_ty.is_some())?);
    }

    if parsed.is_empty() {
        return Err(syn::Error::new_spanned(
            &module.ident,
            format!(
                "#[{attr_name}s] module has no #[{attr_name}(name = \"...\")] \
                 functions",
            ),
        ));
    }

    // Generated struct + trait impl per fn, INSIDE the module so the
    // fn's own type paths resolve unchanged.
    let mut generated: Vec<Item> = Vec::new();
    for c in &parsed {
        let ConsumerFn { fn_ident, struct_ident, event_ty, name, ordering, max_in_flight, kinds } = c;
        let struct_def: Item = match &deps_ty {
            Some(d) => parse_quote! {
                #[allow(non_camel_case_types)]
                pub struct #struct_ident { deps: ::std::sync::Arc<#d> }
            },
            None => parse_quote! {
                #[allow(non_camel_case_types)]
                pub struct #struct_ident;
            },
        };
        generated.push(struct_def);
        // The body of the generated trait method: call the user's fn.
        let call = |evt: TokenStream2| -> TokenStream2 {
            match &deps_ty {
                Some(_) => quote! { #fn_ident(&self.deps, #evt, __ctx).await },
                None => quote! { #fn_ident(#evt, __ctx).await },
            }
        };

        let react_call = call(quote! { __trigger });
        let project_call = call(quote! { __event });
        let imp: Item = if attr_name == "reactor" {
            let ordering_const = ordering.as_ref().map(|o| {
                quote! { const ORDERING: ::causal::Ordering = ::causal::Ordering::#o; }
            });
            let mif_const = max_in_flight.map(|n| {
                let n = n as usize;
                quote! { const MAX_IN_FLIGHT: usize = #n; }
            });
            parse_quote! {
                #[::causal::async_trait]
                impl ::causal::Reactor for #struct_ident {
                    type Trigger = #event_ty;
                    const NAME: &'static str = #name;
                    #ordering_const
                    #mif_const
                    async fn react(
                        &self,
                        __trigger: &#event_ty,
                        __ctx: ::causal::Ctx<'_>,
                    ) -> ::anyhow::Result<::causal::Events> {
                        #react_call
                    }
                }
            }
        } else if let Some(kind_list) = kinds {
            parse_quote! {
                #[::causal::async_trait]
                impl ::causal::MultiProjector for #struct_ident {
                    const NAME: &'static str = #name;
                    const KINDS: &'static [&'static str] = &[#(#kind_list),*];
                    async fn project(
                        &self,
                        __event: &::causal::RecordedEvent,
                        __ctx: ::causal::Ctx<'_>,
                    ) -> ::anyhow::Result<()> {
                        #project_call
                    }
                }
            }
        } else {
            parse_quote! {
                #[::causal::async_trait]
                impl ::causal::Projector for #struct_ident {
                    type Event = #event_ty;
                    const NAME: &'static str = #name;
                    async fn project(
                        &self,
                        __event: &#event_ty,
                        __ctx: ::causal::Ctx<'_>,
                    ) -> ::anyhow::Result<()> {
                        #project_call
                    }
                }
            }
        };
        generated.push(imp);
    }

    // Registration fn(s). A reactors module gets `reactors()`; a
    // projectors module gets `projectors()` and/or `multi_projectors()`
    // (only the non-empty ones).
    let mk_registration = |fn_name: &str,
                           trait_path: TokenStream2,
                           members: &[&ConsumerFn]|
     -> Item {
        let fn_ident = format_ident!("{fn_name}");
        let boxes = members.iter().map(|c| {
            let s = &c.struct_ident;
            match &deps_ty {
                Some(_) => quote! {
                    ::std::boxed::Box::new(#s { deps: __deps.clone() })
                        as ::std::boxed::Box<dyn #trait_path>
                },
                None => quote! {
                    ::std::boxed::Box::new(#s) as ::std::boxed::Box<dyn #trait_path>
                },
            }
        });
        match &deps_ty {
            Some(d) => parse_quote! {
                pub fn #fn_ident(deps: #d)
                    -> ::std::vec::Vec<::std::boxed::Box<dyn #trait_path>>
                {
                    let __deps = ::std::sync::Arc::new(deps);
                    ::std::vec![#(#boxes),*]
                }
            },
            None => parse_quote! {
                pub fn #fn_ident()
                    -> ::std::vec::Vec<::std::boxed::Box<dyn #trait_path>>
                {
                    ::std::vec![#(#boxes),*]
                }
            },
        }
    };

    if attr_name == "reactor" {
        let all: Vec<&ConsumerFn> = parsed.iter().collect();
        generated.push(mk_registration(
            "reactors",
            quote! { ::causal::ReactorRegistration },
            &all,
        ));
    } else {
        let singles: Vec<&ConsumerFn> =
            parsed.iter().filter(|c| c.kinds.is_none()).collect();
        let multis: Vec<&ConsumerFn> =
            parsed.iter().filter(|c| c.kinds.is_some()).collect();
        if !singles.is_empty() {
            generated.push(mk_registration(
                "projectors",
                quote! { ::causal::ProjectorRegistration },
                &singles,
            ));
        }
        if !multis.is_empty() {
            generated.push(mk_registration(
                "multi_projectors",
                quote! { ::causal::MultiProjectorRegistration },
                &multis,
            ));
        }
    }

    items.extend(generated);
    Ok(quote! { #module })
}

/// Turns a module of plain async functions into registered [`Reactor`]s
/// — the fn body is the Primitive-2 body, untouched; the macro removes
/// trait ceremony only.
///
/// ```ignore
/// #[causal::reactors(deps = JobDeps)]
/// mod job_reactors {
///     pub struct JobDeps { pub whisper: Arc<WhisperClient> }
///
///     #[reactor(name = "job.enrich", max_in_flight = 4)]
///     async fn enrich(deps: &JobDeps, t: &JobOpened, ctx: Ctx<'_>) -> Result<Events> {
///         …
///     }
/// }
/// // EngineBuilder: .with_reactors(job_reactors::reactors(JobDeps { … }))
/// ```
///
/// Inference is allowed only where wrong inference fails loudly: the
/// trigger type comes from the fn signature (a wrong type changes which
/// events arrive — visibly). `name` is REQUIRED (it names a durable
/// cursor; a derived default turns a rename into an orphaned cursor),
/// and every execution-semantics knob is explicit: `ordering =
/// per_subject | per_workflow | none` (BLOCKING-4's declaration rides
/// here), `max_in_flight = N`.
#[proc_macro_attribute]
pub fn reactors(attr: TokenStream, item: TokenStream) -> TokenStream {
    let module = parse_macro_input!(item as ItemMod);
    match expand_consumers_module(attr.into(), module, "reactor") {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Turns a module of plain async functions into registered
/// [`Projector`]s / [`MultiProjector`]s — same contract as
/// [`macro@reactors`].
///
/// ```ignore
/// #[causal::projectors(deps = ReadModels)]
/// mod job_projectors {
///     #[projector(name = "job.read_model")]
///     async fn job_row(deps: &ReadModels, e: &JobOpened, ctx: Ctx<'_>) -> Result<()> { … }
///
///     // Cross-kind: `kinds` is explicit — each entry couples every
///     // workflow emitting that kind to this projector's write latency.
///     #[projector(name = "ops.audit", kinds = ["wf_job_opened", "wf_job_enriched"])]
///     async fn audit(deps: &ReadModels, e: &RecordedEvent, ctx: Ctx<'_>) -> Result<()> { … }
/// }
/// // .with_projectors(job_projectors::projectors(deps))
/// // .with_multi_projectors(job_projectors::multi_projectors(deps))
/// ```
#[proc_macro_attribute]
pub fn projectors(attr: TokenStream, item: TokenStream) -> TokenStream {
    let module = parse_macro_input!(item as ItemMod);
    match expand_consumers_module(attr.into(), module, "projector") {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}
