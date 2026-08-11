//! Shared decoding for the integer enums openvpn3 puts on the bus.

/// A numeric value openvpn3 sent that this build does not know about.
///
/// Carried rather than panicked on: a newer openvpn3 introducing a code should
/// degrade to "here is what I did not understand", not take the panel applet
/// down with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind} value from openvpn3: {value}")]
pub struct UnknownWireValue {
    pub kind: &'static str,
    pub value: u32,
}

/// Defines a wire enum once, so the variant list and the integer decoding
/// cannot drift apart. The discriminants are the contract — the names are not.
macro_rules! wire_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident: $repr:ty { $( $variant:ident = $value:literal ),* $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr($repr)]
        pub enum $name { $( $variant = $value ),* }

        impl TryFrom<$repr> for $name {
            type Error = $crate::wire::UnknownWireValue;

            fn try_from(value: $repr) -> ::core::result::Result<Self, Self::Error> {
                match value {
                    $( $value => Ok($name::$variant), )*
                    other => Err($crate::wire::UnknownWireValue {
                        kind: stringify!($name),
                        value: u32::from(other),
                    }),
                }
            }
        }

        impl From<$name> for $repr {
            fn from(value: $name) -> $repr {
                value as $repr
            }
        }
    };
}

pub(crate) use wire_enum;
