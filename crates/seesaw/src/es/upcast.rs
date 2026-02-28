use anyhow::Result;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

/// Schema evolution for events. Upcasts old event versions to the current schema.
pub trait EventUpcast: Sized + DeserializeOwned + Serialize {
    /// The current schema version of this event type.
    fn current_version() -> u32 {
        1
    }

    /// Upcast from an older version. Default implementation only handles current version.
    fn upcast_from(version: u32, data: Value) -> Result<Self> {
        if version == Self::current_version() {
            Ok(serde_json::from_value(data)?)
        } else {
            anyhow::bail!(
                "no upcaster for {} version {version} (current: {})",
                std::any::type_name::<Self>(),
                Self::current_version()
            )
        }
    }
}

/// Blanket implementation: implement `EventUpcast` with default version 1
/// for any type that is Serialize + DeserializeOwned.
///
/// Users can override by implementing the trait explicitly with custom
/// `current_version()` and `upcast_from()`.
#[macro_export]
macro_rules! impl_upcast {
    ($ty:ty) => {
        impl $crate::es::EventUpcast for $ty {}
    };
    ($ty:ty, version = $ver:expr, upcast = $upcast_fn:expr) => {
        impl $crate::es::EventUpcast for $ty {
            fn current_version() -> u32 {
                $ver
            }

            fn upcast_from(
                version: u32,
                data: ::serde_json::Value,
            ) -> ::anyhow::Result<Self> {
                let upcast: fn(u32, ::serde_json::Value) -> ::anyhow::Result<Self> = $upcast_fn;
                upcast(version, data)
            }
        }
    };
}
