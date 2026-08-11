//! Mid-connect input requests, and which of them this applet will service.
//!
//! Discriminants transcribed from openvpn3-linux `src/dbus/constants.hpp`.

use crate::wire::wire_enum;

wire_enum! {
    /// `ClientAttentionType` — the broad category of an `AttentionRequired`.
    pub enum ClientAttentionType: u8 {
        Unset = 0,
        Credentials = 1,
        Pkcs11 = 2,
        AccessPerm = 3,
    }
}

wire_enum! {
    /// `ClientAttentionGroup` — what specifically is being asked for.
    pub enum ClientAttentionGroup: u8 {
        Unset = 0,
        UserPassword = 1,
        HttpProxyCreds = 2,
        PkPassphrase = 3,
        ChallengeStatic = 4,
        ChallengeDynamic = 5,
        ChallengeAuthPending = 6,
        Pkcs11Sign = 7,
        Pkcs11Decrypt = 8,
        OpenUrl = 9,
    }
}

impl ClientAttentionType {
    pub const ALL: [Self; 4] = [Self::Unset, Self::Credentials, Self::Pkcs11, Self::AccessPerm];
}

impl ClientAttentionGroup {
    pub const ALL: [Self; 10] = [
        Self::Unset,
        Self::UserPassword,
        Self::HttpProxyCreds,
        Self::PkPassphrase,
        Self::ChallengeStatic,
        Self::ChallengeDynamic,
        Self::ChallengeAuthPending,
        Self::Pkcs11Sign,
        Self::Pkcs11Decrypt,
        Self::OpenUrl,
    ];
}

/// The shape of prompt to present. R8 names exactly these three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    UserPassword,
    PrivateKeyPassphrase,
    Challenge,
}

/// What to do about an `AttentionRequired`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttentionRouting {
    Prompt(PromptKind),
    /// Out of scope. The caller disconnects the session and surfaces `reason`,
    /// because a request left unanswered parks the session forever behind a
    /// "connecting" icon with no way for the user to find out why.
    Unsupported { reason: String },
}

/// The KTD8 routing table. Total by construction — the fallback arm means a
/// combination nobody anticipated fails loudly rather than reaching a prompt
/// that cannot answer it correctly.
pub fn route_attention(
    type_: ClientAttentionType,
    group: ClientAttentionGroup,
) -> AttentionRouting {
    use AttentionRouting::{Prompt, Unsupported};
    use ClientAttentionGroup as G;
    use ClientAttentionType as T;

    let unsupported = |reason: &str| Unsupported {
        reason: reason.to_owned(),
    };

    match (type_, group) {
        (T::Credentials, G::UserPassword) => Prompt(PromptKind::UserPassword),
        (T::Credentials, G::PkPassphrase) => Prompt(PromptKind::PrivateKeyPassphrase),
        (T::Credentials, G::ChallengeStatic | G::ChallengeDynamic) => Prompt(PromptKind::Challenge),

        (T::Credentials, G::OpenUrl) => unsupported(
            "This profile uses browser-based (web) authentication, which this applet does not \
             support. Start it from a terminal with `openvpn3 session-start`.",
        ),
        (T::Credentials, G::HttpProxyCreds) => unsupported(
            "This profile asked for HTTP proxy credentials, which this applet does not support.",
        ),
        (T::Credentials, G::ChallengeAuthPending) => unsupported(
            "This profile requires out-of-band approval to continue, which this applet cannot \
             wait on. Start it from a terminal with `openvpn3 session-start`.",
        ),
        (T::Pkcs11, _) => unsupported(
            "This profile requires a smartcard or PKCS#11 token, which this applet does not \
             support.",
        ),
        (T::AccessPerm, _) => unsupported(
            "openvpn3 asked for an access-permission decision, which this applet does not handle.",
        ),

        _ => Unsupported {
            reason: format!("Unsupported openvpn3 request: {type_:?}/{group:?}."),
        },
    }
}

/// One item drained from a session's user-input queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputRequest {
    pub type_: ClientAttentionType,
    pub group: ClientAttentionGroup,
    pub id: u32,
    /// Machine name, e.g. `username`.
    pub name: String,
    /// Human-facing text. For a dynamic challenge this *is* the question.
    pub description: String,
    pub hidden_input: bool,
}

/// A single field to render in the prompt window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptField {
    pub id: u32,
    pub name: String,
    pub label: String,
    pub masked: bool,
}

impl InputRequest {
    pub fn to_field(&self) -> PromptField {
        // openvpn3 does not always populate the description; falling back to the
        // machine name beats rendering an unlabelled box.
        let label = if self.description.trim().is_empty() {
            self.name.clone()
        } else {
            self.description.clone()
        };

        PromptField {
            id: self.id,
            name: self.name.clone(),
            label,
            masked: self.hidden_input,
        }
    }
}
