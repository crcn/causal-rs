//! Core traits for the seesaw event-driven architecture.
//!
//! # Overview
//!
//! Seesaw is built on events - facts about what happened.
//! Events flow through effects, which can emit new events.
//!
//! # Correlation
//!
//! Events can be tagged with a [`CorrelationId`] to track
//! related work across the system. This enables:
//! - Awaiting completion of all work triggered by an API request
//! - Distributed tracing
//! - Error propagation back to the caller

use std::any::{Any, TypeId};
use std::fmt;
use std::sync::Arc;

use uuid::Uuid;

/// Correlation ID for tracking related events.
///
/// Each `emit_and_await` call generates a unique correlation ID that propagates
/// through the system, allowing the caller to wait for all related inline work.
///
/// Use `CorrelationId::NONE` for uncorrelated events, or `CorrelationId::new()`
/// to generate a fresh ID.
///
/// # Example
///
/// ```ignore
/// use seesaw::CorrelationId;
///
/// // Create a new random correlation ID
/// let cid = CorrelationId::new();
///
/// // Use NONE for uncorrelated events
/// let uncorrelated = CorrelationId::NONE;
/// assert!(uncorrelated.is_none());
///
/// // Convert from existing Uuid
/// let cid = CorrelationId::from(my_uuid);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CorrelationId(Uuid);

impl CorrelationId {
    /// Sentinel value for uncorrelated events.
    ///
    /// Uses nil UUID (`00000000-0000-0000-0000-000000000000`).
    pub const NONE: Self = Self(Uuid::nil());

    /// Create a new random correlation ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Check if this is the NONE sentinel value.
    pub fn is_none(&self) -> bool {
        self.0.is_nil()
    }

    /// Check if this is a real correlation ID (not NONE).
    pub fn is_some(&self) -> bool {
        !self.is_none()
    }

    /// Get the inner UUID value.
    pub fn into_inner(self) -> Uuid {
        self.0
    }

    /// Get a reference to the inner UUID.
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for CorrelationId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Uuid> for CorrelationId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl From<CorrelationId> for Uuid {
    fn from(cid: CorrelationId) -> Uuid {
        cid.0
    }
}

impl From<Option<Uuid>> for CorrelationId {
    fn from(opt: Option<Uuid>) -> Self {
        match opt {
            Some(uuid) => Self(uuid),
            None => Self::NONE,
        }
    }
}

impl fmt::Display for CorrelationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_none() {
            write!(f, "NONE")
        } else {
            write!(f, "{}", self.0)
        }
    }
}

/// Envelope wrapping an event with correlation metadata.
///
/// `EventEnvelope` is the internal transport format for events. It carries:
/// - The correlation ID for tracking related work
/// - The type ID for filtering by effects
/// - The event payload
///
/// Domain event enums remain clean - correlation is transport-level metadata.
#[derive(Clone)]
pub struct EventEnvelope {
    /// Correlation ID for tracking related work
    pub cid: CorrelationId,
    /// Type ID of the payload event
    pub type_id: TypeId,
    /// The actual event payload
    pub payload: Arc<dyn Any + Send + Sync>,
}

impl EventEnvelope {
    /// Create a new event envelope.
    pub fn new<E: Any + Send + Sync + 'static>(cid: CorrelationId, event: E) -> Self {
        Self {
            cid,
            type_id: TypeId::of::<E>(),
            payload: Arc::new(event),
        }
    }

    /// Create a new event envelope from a raw UUID (for internal use).
    pub(crate) fn new_with_uuid<E: Any + Send + Sync + 'static>(cid: Uuid, event: E) -> Self {
        Self {
            cid: CorrelationId::from(cid),
            type_id: TypeId::of::<E>(),
            payload: Arc::new(event),
        }
    }

    /// Create an envelope with a new random correlation ID.
    pub fn new_random<E: Any + Send + Sync + 'static>(event: E) -> Self {
        Self::new(CorrelationId::new(), event)
    }

    /// Downcast the payload to a concrete event type.
    pub fn downcast_ref<E: Any>(&self) -> Option<&E> {
        self.payload.downcast_ref()
    }
}

impl std::fmt::Debug for EventEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventEnvelope")
            .field("cid", &self.cid)
            .field("type_id", &self.type_id)
            .finish_non_exhaustive()
    }
}

/// The role of an event in the system.
///
/// Events flow through a single bus but serve different purposes:
/// - **Input**: Edge-originated events (user requests, API calls)
/// - **Fact**: Effect-produced events (ground truth, what actually happened)
/// - **Signal**: Ephemeral UI notifications (typing indicators, progress)
///
/// # Design Principle
///
/// One bus, one thing that flows, but events have roles.
/// This avoids the ceremony of separate Message/Event types while
/// maintaining clear semantics.
///
/// # Example
///
/// ```ignore
/// enum OrderEvent {
///     // Input - "a request was made" (edge-originated)
///     PlaceRequested { user_id: Uuid, items: Vec<Item> },
///
///     // Fact - "this happened" (effect-produced)
///     Placed { order_id: Uuid, total: Decimal },
///
///     // Signal would use ctx.signal() instead
/// }
///
/// impl OrderEvent {
///     pub fn role(&self) -> EventRole {
///         match self {
///             Self::PlaceRequested { .. } => EventRole::Input,
///             Self::Placed { .. } => EventRole::Fact,
///             // ...
///         }
///     }
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventRole {
    /// Edge-originated events (user requests, API calls).
    Input,
    /// Effect-produced events (ground truth).
    Fact,
    /// Ephemeral UI notifications (typing indicators, progress).
    Signal,
}

impl EventRole {
    /// Returns true if this is an input event.
    pub fn is_input(&self) -> bool {
        matches!(self, EventRole::Input)
    }

    /// Returns true if this is a fact event.
    pub fn is_fact(&self) -> bool {
        matches!(self, EventRole::Fact)
    }

    /// Returns true if this is a signal event.
    pub fn is_signal(&self) -> bool {
        matches!(self, EventRole::Signal)
    }

    /// Returns true if this event should be processed by effects.
    /// Signals are typically filtered out.
    pub fn is_actionable(&self) -> bool {
        !self.is_signal()
    }
}

/// Events are immutable descriptions of what occurred.
///
/// **Note**: This trait is automatically implemented for any type that is
/// `Clone + Send + Sync + 'static`. You don't need to implement it manually.
///
/// # Event Roles
///
/// Events have roles (see [`EventRole`]):
/// - **Input**: Requests from edges (e.g., `CreateRequested`)
/// - **Fact**: Results from effects (e.g., `Created`)
/// - **Signal**: Ephemeral UI updates (via `ctx.signal()`)
///
/// # Example
///
/// ```ignore
/// #[derive(Debug, Clone)]
/// enum UserEvent {
///     // Input
///     CreateRequested { email: String },
///     // Fact
///     Created { user_id: Uuid, email: String },
///     Deleted { user_id: Uuid },
/// }
/// // Event is automatically implemented!
/// ```
pub trait Event: Any + Send + Sync + 'static {
    /// Downcast to &dyn Any for type checking
    fn as_any(&self) -> &dyn Any;
}

// Blanket implementation for any type that meets the requirements
impl<T: Clone + Send + Sync + 'static> Event for T {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// EnvelopeMatch - Ergonomic event matching
// ─────────────────────────────────────────────────────────────────────────────

/// Ergonomic wrapper for matching events in an envelope.
///
/// Provides a cleaner API for downcasting event envelopes without
/// verbose `downcast_ref` calls scattered throughout match logic.
///
/// # Example
///
/// ```ignore
/// use seesaw::EnvelopeMatch;
///
/// let result = EnvelopeMatch::new(&envelope)
///     .try_match(|e: &UserEvent| match e {
///         UserEvent::Created { user } => Some(Ok(user.clone())),
///         _ => None,
///     })
///     .or_try(|denied: &AuthorizationDenied| {
///         Some(Err(anyhow!("Permission denied: {}", denied.reason)))
///     })
///     .result();
/// ```
pub struct EnvelopeMatch<'a> {
    env: &'a EventEnvelope,
}

impl<'a> EnvelopeMatch<'a> {
    /// Create a new envelope matcher.
    pub fn new(env: &'a EventEnvelope) -> Self {
        Self { env }
    }

    /// Try to downcast to a specific event type.
    pub fn event<E: 'static>(&self) -> Option<&E> {
        self.env.downcast_ref::<E>()
    }

    /// Check if envelope contains this event type.
    pub fn is<E: 'static>(&self) -> bool {
        self.env.type_id == TypeId::of::<E>()
    }

    /// Try to extract and map an event type.
    pub fn map<E: 'static, T>(&self, f: impl FnOnce(&E) -> T) -> Option<T> {
        self.event::<E>().map(f)
    }

    /// Try to extract and flat_map an event type.
    pub fn and_then<E: 'static, T>(&self, f: impl FnOnce(&E) -> Option<T>) -> Option<T> {
        self.event::<E>().and_then(f)
    }

    /// Start a match chain with the first event type to try.
    pub fn try_match<E: 'static, T>(&self, f: impl FnOnce(&E) -> Option<T>) -> MatchChain<'a, T> {
        MatchChain {
            env: self.env,
            result: self.event::<E>().and_then(f),
        }
    }
}

/// A chain of event type matches.
///
/// Created by [`EnvelopeMatch::try_match`] and extended with [`MatchChain::or_try`].
pub struct MatchChain<'a, T> {
    env: &'a EventEnvelope,
    result: Option<T>,
}

impl<'a, T> MatchChain<'a, T> {
    /// Try another event type if previous didn't match.
    pub fn or_try<E: 'static>(self, f: impl FnOnce(&E) -> Option<T>) -> Self {
        if self.result.is_some() {
            return self;
        }
        Self {
            env: self.env,
            result: self.env.downcast_ref::<E>().and_then(f),
        }
    }

    /// Get the match result.
    pub fn result(self) -> Option<T> {
        self.result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone)]
    struct TestEvent {
        value: i32,
    }

    // =========================================================================
    // CorrelationId Tests
    // =========================================================================

    #[test]
    fn test_correlation_id_new() {
        let cid1 = CorrelationId::new();
        let cid2 = CorrelationId::new();

        assert!(cid1.is_some());
        assert!(cid2.is_some());
        assert_ne!(cid1, cid2);
    }

    #[test]
    fn test_correlation_id_none() {
        let cid = CorrelationId::NONE;

        assert!(cid.is_none());
        assert!(!cid.is_some());
        assert_eq!(cid.into_inner(), Uuid::nil());
    }

    #[test]
    fn test_correlation_id_from_uuid() {
        let uuid = Uuid::new_v4();
        let cid = CorrelationId::from(uuid);

        assert_eq!(cid.into_inner(), uuid);
        assert!(cid.is_some());
    }

    #[test]
    fn test_correlation_id_from_option_some() {
        let uuid = Uuid::new_v4();
        let cid = CorrelationId::from(Some(uuid));

        assert_eq!(cid.into_inner(), uuid);
    }

    #[test]
    fn test_correlation_id_from_option_none() {
        let cid = CorrelationId::from(None);

        assert!(cid.is_none());
        assert_eq!(cid, CorrelationId::NONE);
    }

    #[test]
    fn test_correlation_id_display_none() {
        let cid = CorrelationId::NONE;
        assert_eq!(format!("{}", cid), "NONE");
    }

    #[test]
    fn test_correlation_id_display_some() {
        let uuid = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let cid = CorrelationId::from(uuid);
        assert_eq!(format!("{}", cid), "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn test_correlation_id_as_uuid() {
        let uuid = Uuid::new_v4();
        let cid = CorrelationId::from(uuid);

        assert_eq!(cid.as_uuid(), &uuid);
    }

    #[test]
    fn test_correlation_id_into_uuid() {
        let uuid = Uuid::new_v4();
        let cid = CorrelationId::from(uuid);
        let back: Uuid = cid.into();

        assert_eq!(back, uuid);
    }

    #[test]
    fn test_correlation_id_ordering() {
        let uuid1 = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let uuid2 = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let cid1 = CorrelationId::from(uuid1);
        let cid2 = CorrelationId::from(uuid2);

        assert!(cid1 < cid2);
    }

    #[test]
    fn test_correlation_id_hash() {
        use std::collections::HashSet;

        let cid1 = CorrelationId::new();
        let cid2 = CorrelationId::new();

        let mut set = HashSet::new();
        set.insert(cid1);
        set.insert(cid2);
        set.insert(cid1); // Duplicate

        assert_eq!(set.len(), 2);
    }

    // =========================================================================
    // EventRole Tests
    // =========================================================================

    #[test]
    fn test_event_role_is_input() {
        assert!(EventRole::Input.is_input());
        assert!(!EventRole::Fact.is_input());
        assert!(!EventRole::Signal.is_input());
    }

    #[test]
    fn test_event_role_is_fact() {
        assert!(!EventRole::Input.is_fact());
        assert!(EventRole::Fact.is_fact());
        assert!(!EventRole::Signal.is_fact());
    }

    #[test]
    fn test_event_role_is_signal() {
        assert!(!EventRole::Input.is_signal());
        assert!(!EventRole::Fact.is_signal());
        assert!(EventRole::Signal.is_signal());
    }

    #[test]
    fn test_event_role_is_actionable() {
        assert!(EventRole::Input.is_actionable());
        assert!(EventRole::Fact.is_actionable());
        assert!(!EventRole::Signal.is_actionable());
    }

    // =========================================================================
    // EnvelopeMatch Tests
    // =========================================================================

    #[derive(Debug, Clone, PartialEq)]
    struct UserCreated {
        user_id: Uuid,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct UserDeleted {
        user_id: Uuid,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct AuthDenied {
        reason: String,
    }

    #[test]
    fn test_envelope_match_event() {
        let user_id = Uuid::new_v4();
        let envelope = EventEnvelope::new(CorrelationId::new(), UserCreated { user_id });

        let matcher = EnvelopeMatch::new(&envelope);
        let event = matcher.event::<UserCreated>();

        assert!(event.is_some());
        assert_eq!(event.unwrap().user_id, user_id);
    }

    #[test]
    fn test_envelope_match_event_wrong_type() {
        let envelope = EventEnvelope::new(
            CorrelationId::new(),
            UserCreated {
                user_id: Uuid::new_v4(),
            },
        );

        let matcher = EnvelopeMatch::new(&envelope);
        let event = matcher.event::<UserDeleted>();

        assert!(event.is_none());
    }

    #[test]
    fn test_envelope_match_is() {
        let envelope = EventEnvelope::new(
            CorrelationId::new(),
            UserCreated {
                user_id: Uuid::new_v4(),
            },
        );

        let matcher = EnvelopeMatch::new(&envelope);

        assert!(matcher.is::<UserCreated>());
        assert!(!matcher.is::<UserDeleted>());
    }

    #[test]
    fn test_envelope_match_map() {
        let user_id = Uuid::new_v4();
        let envelope = EventEnvelope::new(CorrelationId::new(), UserCreated { user_id });

        let matcher = EnvelopeMatch::new(&envelope);
        let result = matcher.map(|e: &UserCreated| e.user_id);

        assert_eq!(result, Some(user_id));
    }

    #[test]
    fn test_envelope_match_and_then() {
        let user_id = Uuid::new_v4();
        let envelope = EventEnvelope::new(CorrelationId::new(), UserCreated { user_id });

        let matcher = EnvelopeMatch::new(&envelope);
        let result = matcher.and_then(|e: &UserCreated| {
            if e.user_id == user_id {
                Some("found")
            } else {
                None
            }
        });

        assert_eq!(result, Some("found"));
    }

    #[test]
    fn test_envelope_match_try_match_success() {
        let user_id = Uuid::new_v4();
        let envelope = EventEnvelope::new(CorrelationId::new(), UserCreated { user_id });

        let result = EnvelopeMatch::new(&envelope)
            .try_match(|e: &UserCreated| Some(e.user_id))
            .result();

        assert_eq!(result, Some(user_id));
    }

    #[test]
    fn test_envelope_match_try_match_or_try() {
        let user_id = Uuid::new_v4();
        let envelope = EventEnvelope::new(CorrelationId::new(), UserDeleted { user_id });

        let result: Option<Result<Uuid, &str>> = EnvelopeMatch::new(&envelope)
            .try_match(|e: &UserCreated| Some(Ok(e.user_id)))
            .or_try(|e: &UserDeleted| Some(Err("deleted")))
            .result();

        assert!(matches!(result, Some(Err("deleted"))));
    }

    #[test]
    fn test_envelope_match_chain_first_matches() {
        let user_id = Uuid::new_v4();
        let envelope = EventEnvelope::new(CorrelationId::new(), UserCreated { user_id });

        // First matcher should succeed, second should not be evaluated
        let result = EnvelopeMatch::new(&envelope)
            .try_match(|e: &UserCreated| Some(e.user_id))
            .or_try(|_: &UserDeleted| Some(Uuid::nil()))
            .result();

        assert_eq!(result, Some(user_id));
    }

    #[test]
    fn test_envelope_match_chain_no_match() {
        let envelope = EventEnvelope::new(
            CorrelationId::new(),
            AuthDenied {
                reason: "test".into(),
            },
        );

        let result: Option<Uuid> = EnvelopeMatch::new(&envelope)
            .try_match(|e: &UserCreated| Some(e.user_id))
            .or_try(|e: &UserDeleted| Some(e.user_id))
            .result();

        assert!(result.is_none());
    }

    // =========================================================================
    // EventEnvelope Tests
    // =========================================================================

    #[test]
    fn test_event_envelope_new() {
        let cid = CorrelationId::new();
        let event = UserCreated {
            user_id: Uuid::new_v4(),
        };
        let envelope = EventEnvelope::new(cid, event.clone());

        assert_eq!(envelope.cid, cid);
        assert_eq!(envelope.type_id, TypeId::of::<UserCreated>());
        assert_eq!(envelope.downcast_ref::<UserCreated>(), Some(&event));
    }

    #[test]
    fn test_event_envelope_new_random() {
        let event = UserCreated {
            user_id: Uuid::new_v4(),
        };
        let envelope = EventEnvelope::new_random(event);

        assert!(envelope.cid.is_some());
    }

    #[test]
    fn test_event_envelope_downcast_wrong_type() {
        let envelope = EventEnvelope::new(
            CorrelationId::new(),
            UserCreated {
                user_id: Uuid::new_v4(),
            },
        );

        assert!(envelope.downcast_ref::<UserDeleted>().is_none());
    }

    #[test]
    fn test_event_envelope_debug() {
        let cid = CorrelationId::new();
        let envelope = EventEnvelope::new(
            cid,
            UserCreated {
                user_id: Uuid::new_v4(),
            },
        );

        let debug = format!("{:?}", envelope);
        assert!(debug.contains("EventEnvelope"));
        assert!(debug.contains("cid"));
    }
}
