//! Legacy Store module.
//!
//! The monolithic `Store` trait has been replaced by two focused traits:
//! - [`EventLog`](crate::event_log::EventLog) — durable event history
//! - [`HandlerQueue`](crate::handler_queue::HandlerQueue) — work distribution
//!
//! This module is preserved for backward compatibility but contains no types.
//! Use `EventLog` and `HandlerQueue` directly.
