use serde::{Serialize, Serializer};

/// Public state shared by operation response headers, job JSON and problems.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum OperationPublicState {
    Running,
    Succeeded,
    Failed,
    Rejected,
    Unknown,
}

impl OperationPublicState {
    pub(super) const fn wire_name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
            Self::Unknown => "unknown",
        }
    }

    #[cfg(test)]
    pub(super) const fn from_wire_name(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"running" => Some(Self::Running),
            b"succeeded" => Some(Self::Succeeded),
            b"failed" => Some(Self::Failed),
            b"rejected" => Some(Self::Rejected),
            b"unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

impl Serialize for OperationPublicState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.wire_name())
    }
}

/// Public state shared by upload response headers and problem extensions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum UploadPublicState {
    Running,
    AwaitingConfirmation,
    Committed,
    Rejected,
    NotSeen,
    NotStarted,
    Unknown,
}

impl UploadPublicState {
    pub(super) const fn wire_name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::AwaitingConfirmation => "awaiting-confirmation",
            Self::Committed => "committed",
            Self::Rejected => "rejected",
            Self::NotSeen => "not-seen",
            Self::NotStarted => "not-started",
            Self::Unknown => "unknown",
        }
    }

    #[cfg(test)]
    pub(super) const fn from_wire_name(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"running" => Some(Self::Running),
            b"awaiting-confirmation" => Some(Self::AwaitingConfirmation),
            b"committed" => Some(Self::Committed),
            b"rejected" => Some(Self::Rejected),
            b"not-seen" => Some(Self::NotSeen),
            b"not-started" => Some(Self::NotStarted),
            b"unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

impl Serialize for UploadPublicState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.wire_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_states_round_trip_the_exact_wire_vocabulary() {
        for state in [
            OperationPublicState::Running,
            OperationPublicState::Succeeded,
            OperationPublicState::Failed,
            OperationPublicState::Rejected,
            OperationPublicState::Unknown,
        ] {
            assert_eq!(
                OperationPublicState::from_wire_name(state.wire_name()),
                Some(state)
            );
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{}\"", state.wire_name())
            );
        }
        for state in [
            UploadPublicState::Running,
            UploadPublicState::AwaitingConfirmation,
            UploadPublicState::Committed,
            UploadPublicState::Rejected,
            UploadPublicState::NotSeen,
            UploadPublicState::NotStarted,
            UploadPublicState::Unknown,
        ] {
            assert_eq!(
                UploadPublicState::from_wire_name(state.wire_name()),
                Some(state)
            );
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{}\"", state.wire_name())
            );
        }
    }

    #[test]
    fn unknown_wire_states_are_rejected() {
        assert_eq!(OperationPublicState::from_wire_name("conflict"), None);
        assert_eq!(UploadPublicState::from_wire_name("complete"), None);
    }
}
