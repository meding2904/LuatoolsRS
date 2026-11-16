//! Authentication state shared by the backend and interface.
//!
//! The desktop client can run as a guest for every local Steam feature. Sign-in only adds a
//! display name and unlocks the online account surfaces that explicitly require it.

use serde::{Deserialize, Serialize};

/// A signed-in profile summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserProfile {
    /// Stable provider user id.
    pub id: String,
    /// Display name chosen by the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Account email, when the provider exposes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Avatar image URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
}

/// Current authentication state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum AuthState {
    /// No sign-in attempted, or the user is browsing as a guest.
    #[default]
    Guest,
    /// A browser sign-in is in progress.
    WaitingForBrowser {
        /// Local callback URL the browser must reach.
        callback_url: String,
    },
    /// Signed in.
    SignedIn {
        /// Profile summary.
        profile: UserProfile,
    },
    /// Sign-in failed.
    Failed {
        /// Localized reason.
        reason: String,
    },
}

impl AuthState {
    /// Returns the signed-in profile, when there is one.
    #[must_use]
    pub fn profile(&self) -> Option<&UserProfile> {
        match self {
            Self::SignedIn { profile } => Some(profile),
            Self::Guest | Self::WaitingForBrowser { .. } | Self::Failed { .. } => None,
        }
    }

    /// Returns true when a user is signed in.
    #[must_use]
    pub const fn is_signed_in(&self) -> bool {
        matches!(self, Self::SignedIn { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_signed_in_states_expose_a_profile() {
        let profile = UserProfile {
            id: "1".to_owned(),
            display_name: Some("Ada".to_owned()),
            email: None,
            avatar_url: None,
        };
        let state = AuthState::SignedIn { profile };
        assert!(state.is_signed_in());
        assert_eq!(
            state
                .profile()
                .and_then(|profile| profile.display_name.as_deref()),
            Some("Ada")
        );
        assert!(!AuthState::Guest.is_signed_in());
    }
}
