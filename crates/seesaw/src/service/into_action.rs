//! Trait for converting action functions into a form usable with `Service::run`.
//!
//! # Example
//!
//! ```ignore
//! use seesaw::IntoAction;
//!
//! service.run(state, get_container.with(opts)).await
//! ```

/// Wrapper struct that holds an action function and its options.
pub struct ActionWithOpts<F, Opts> {
    pub action: F,
    pub opts: Opts,
}

/// Trait to convert an action function into an `ActionWithOpts` for use with `Service::run`.
///
/// This trait is automatically implemented for any function.
pub trait IntoAction<Opts>: Sized {
    /// Convert this action function into an `ActionWithOpts`.
    fn with(self, opts: Opts) -> ActionWithOpts<Self, Opts>;
}

impl<F, Opts> IntoAction<Opts> for F {
    fn with(self, opts: Opts) -> ActionWithOpts<Self, Opts> {
        ActionWithOpts { action: self, opts }
    }
}
