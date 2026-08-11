//! U6 — the KTD8 attention routing table.
//!
//! The security-relevant property here is the *default*. An attention request
//! this applet cannot service must fail loudly: ignoring it parks the session
//! forever behind a "connecting" icon with no way for the user to learn why.

use openvpn3_dbus::attention::{
    AttentionRouting, ClientAttentionGroup as Group, ClientAttentionType as Type, InputRequest,
    PromptKind, route_attention,
};

#[track_caller]
fn assert_prompt(type_: Type, group: Group, expected: PromptKind) {
    match route_attention(type_, group) {
        AttentionRouting::Prompt(actual) => assert_eq!(
            actual, expected,
            "{type_:?}/{group:?} should prompt as {expected:?}, got {actual:?}"
        ),
        AttentionRouting::Unsupported { reason } => {
            panic!("{type_:?}/{group:?} should prompt as {expected:?}, got Unsupported({reason})")
        }
    }
}

#[track_caller]
fn assert_unsupported(type_: Type, group: Group) {
    match route_attention(type_, group) {
        AttentionRouting::Unsupported { reason } => {
            assert!(
                !reason.trim().is_empty(),
                "{type_:?}/{group:?} must carry a reason the user can read"
            );
        }
        AttentionRouting::Prompt(kind) => {
            panic!("{type_:?}/{group:?} is out of scope but routed to a {kind:?} prompt")
        }
    }
}

// ------------------------------------------------------------- Supported (R8)

#[test]
fn username_and_password_prompts() {
    assert_prompt(Type::Credentials, Group::UserPassword, PromptKind::UserPassword);
}

#[test]
fn private_key_passphrase_prompts() {
    assert_prompt(
        Type::Credentials,
        Group::PkPassphrase,
        PromptKind::PrivateKeyPassphrase,
    );
}

#[test]
fn static_and_dynamic_challenges_prompt() {
    assert_prompt(
        Type::Credentials,
        Group::ChallengeStatic,
        PromptKind::Challenge,
    );
    assert_prompt(
        Type::Credentials,
        Group::ChallengeDynamic,
        PromptKind::Challenge,
    );
}

// ----------------------------------------------------------- Unsupported (KTD8)

#[test]
fn browser_auth_is_unsupported_and_says_so() {
    match route_attention(Type::Credentials, Group::OpenUrl) {
        AttentionRouting::Unsupported { reason } => assert!(
            reason.to_lowercase().contains("browser") || reason.to_lowercase().contains("web"),
            "the reason should name browser auth so the log window explains itself, got: {reason}"
        ),
        other => panic!("browser auth must be unsupported, got {other:?}"),
    }
}

#[test]
fn smartcard_requests_are_unsupported() {
    assert_unsupported(Type::Pkcs11, Group::Pkcs11Sign);
    assert_unsupported(Type::Pkcs11, Group::Pkcs11Decrypt);
}

#[test]
fn proxy_credentials_and_pending_challenges_are_unsupported() {
    assert_unsupported(Type::Credentials, Group::HttpProxyCreds);
    assert_unsupported(Type::Credentials, Group::ChallengeAuthPending);
}

/// ACCESS_PERM is unsupported regardless of which group accompanies it.
#[test]
fn access_permission_requests_are_unsupported_for_every_group() {
    for group in Group::ALL {
        assert_unsupported(Type::AccessPerm, group);
    }
}

/// The table must be total. Any pair the implementation has not thought about
/// falls to unsupported — never to a prompt that cannot be answered correctly.
#[test]
fn routing_is_total_and_never_panics() {
    for type_ in Type::ALL {
        for group in Group::ALL {
            let _ = route_attention(type_, group);
        }
    }
}

// ------------------------------------------------------------- Prompt fields

#[test]
fn hidden_queue_items_produce_masked_fields() {
    let secret = InputRequest {
        type_: Type::Credentials,
        group: Group::UserPassword,
        id: 1,
        name: "password".into(),
        description: "Password".into(),
        hidden_input: true,
    };
    let plain = InputRequest {
        type_: Type::Credentials,
        group: Group::UserPassword,
        id: 0,
        name: "username".into(),
        description: "Username".into(),
        hidden_input: false,
    };

    assert!(secret.to_field().masked, "hidden_input must mask the field");
    assert!(
        !plain.to_field().masked,
        "a non-hidden item must not be masked"
    );
}

/// The label comes from the queue item's description — that is the text
/// openvpn3 wrote for a human, and for a dynamic challenge it *is* the question
/// being asked.
#[test]
fn field_label_comes_from_the_description() {
    let req = InputRequest {
        type_: Type::Credentials,
        group: Group::ChallengeDynamic,
        id: 0,
        name: "static_challenge".into(),
        description: "Enter your 6-digit token".into(),
        hidden_input: true,
    };
    assert_eq!(req.to_field().label, "Enter your 6-digit token");
}

/// Falling back to the machine name is better than rendering an empty label.
#[test]
fn field_label_falls_back_to_name_when_description_is_empty() {
    let req = InputRequest {
        type_: Type::Credentials,
        group: Group::UserPassword,
        id: 0,
        name: "username".into(),
        description: String::new(),
        hidden_input: false,
    };
    assert_eq!(req.to_field().label, "username");
}

// -------------------------------------------------------------- Wire decoding

#[test]
fn wire_discriminants_match_openvpn3_constants() {
    assert_eq!(Type::try_from(1u8).unwrap(), Type::Credentials);
    assert_eq!(Type::try_from(2u8).unwrap(), Type::Pkcs11);
    assert_eq!(Type::try_from(3u8).unwrap(), Type::AccessPerm);
    assert_eq!(Group::try_from(1u8).unwrap(), Group::UserPassword);
    assert_eq!(Group::try_from(3u8).unwrap(), Group::PkPassphrase);
    assert_eq!(Group::try_from(4u8).unwrap(), Group::ChallengeStatic);
    assert_eq!(Group::try_from(5u8).unwrap(), Group::ChallengeDynamic);
    assert_eq!(Group::try_from(9u8).unwrap(), Group::OpenUrl);
}

#[test]
fn unknown_attention_values_are_rejected() {
    assert!(Type::try_from(77u8).is_err());
    assert!(Group::try_from(77u8).is_err());
}
