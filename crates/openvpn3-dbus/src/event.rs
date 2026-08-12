//! The session manager's lifecycle signal.

use crate::wire::wire_enum;

wire_enum! {
    /// `SessionManager::EventType`, from openvpn3-linux
    /// `src/sessionmgr/sessionmgr-events.hpp`.
    ///
    /// Transcribed rather than guessed for a reason: this value is how the
    /// applet learns a session is gone, and getting it wrong deadlocks the
    /// transition machine — it waits forever for a teardown it never sees.
    pub enum SessionEventType: u16 {
        Unset = 0,
        SessionCreated = 1,
        SessionDestroyed = 2,
    }
}

impl SessionEventType {
    pub fn is_destroyed(self) -> bool {
        matches!(self, Self::SessionDestroyed)
    }
}

#[cfg(test)]
mod tests {
    use super::SessionEventType;

    #[test]
    fn discriminants_match_openvpn3_constants() {
        assert_eq!(SessionEventType::try_from(0u16).unwrap(), SessionEventType::Unset);
        assert_eq!(
            SessionEventType::try_from(1u16).unwrap(),
            SessionEventType::SessionCreated
        );
        assert_eq!(
            SessionEventType::try_from(2u16).unwrap(),
            SessionEventType::SessionDestroyed
        );
    }

    /// The regression this module exists to prevent.
    #[test]
    fn only_two_means_destroyed() {
        assert!(SessionEventType::SessionDestroyed.is_destroyed());
        assert!(!SessionEventType::SessionCreated.is_destroyed());
        assert!(!SessionEventType::Unset.is_destroyed());
        assert!(
            SessionEventType::try_from(3u16).is_err(),
            "3 is not a valid event type; treating it as destruction is what \
             wedged the transition machine"
        );
    }
}
