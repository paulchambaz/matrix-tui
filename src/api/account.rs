//! Account actions: recovery key, sessions, password, deactivation and trust in other users.

use std::future::Future;

use matrix_sdk::encryption::recovery::RecoveryState;
use matrix_sdk::ruma::api::client::uiaa::{
    AuthData, MatrixUserIdentifier, Password, UserIdentifier,
};
use matrix_sdk::ruma::OwnedDeviceId;
use matrix_sdk::RoomMemberships;

use super::{failed, room_id, user_id, Client, Error};
use crate::api::types::{ChannelId, UserId};
use crate::config::Credentials;

/// A device logged in to the account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub ip: String,
    /// When it was last active, in milliseconds since the epoch.
    pub seen: i64,
    /// Whether this is the session of the running program.
    pub current: bool,
}

impl Client {
    async fn confirmed<T, F, Fut>(&self, call: F) -> Result<T, Error>
    where
        F: Fn(Option<AuthData>) -> Fut,
        Fut: Future<Output = Result<T, matrix_sdk::Error>>,
    {
        let first = match call(None).await {
            Ok(done) => return Ok(done),
            Err(e) => e,
        };
        let Some(info) = first.as_uiaa_response() else {
            return Err(failed(first));
        };
        let Credentials::Password { login, password } = &self.credentials else {
            return Err(failed(
                "This needs the password login: do it in your account page",
            ));
        };
        let who = UserIdentifier::Matrix(MatrixUserIdentifier::new(login.clone()));
        let mut auth = Password::new(who, password.clone());
        auth.session.clone_from(&info.session);
        call(Some(AuthData::Password(auth))).await.map_err(failed)
    }

    /// Lists the devices logged in to the account, the most recently active first.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn sessions(&self) -> Result<Vec<Session>, Error> {
        let sdk = self.sdk();
        let mine = sdk.device_id().map(ToOwned::to_owned);
        let mut sessions: Vec<Session> = sdk
            .devices()
            .await
            .map_err(failed)?
            .devices
            .into_iter()
            .map(|d| Session {
                current: Some(&d.device_id) == mine.as_ref(),
                id: d.device_id.to_string(),
                name: d.display_name.unwrap_or_default(),
                ip: d.last_seen_ip.unwrap_or_default(),
                seen: d.last_seen_ts.map_or(0, |t| i64::from(t.0)),
            })
            .collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.seen));
        Ok(sessions)
    }

    /// Logs another device out of the account.
    ///
    /// # Errors
    /// Fails when the device is the running one, on server errors, and when the password is refused.
    pub async fn end_session(&self, id: &str) -> Result<(), Error> {
        let sdk = self.sdk();
        if sdk.device_id().is_some_and(|d| d.as_str() == id) {
            return Err(failed("This is the running session: use :quit"));
        }
        let ids = [OwnedDeviceId::from(id)];
        self.confirmed(|auth| {
            let (sdk, ids) = (&sdk, &ids);
            async move { Ok(sdk.delete_devices(ids, auth).await.map(drop)?) }
        })
        .await
    }

    /// Changes the password of the account.
    ///
    /// # Errors
    /// Fails on server errors, when the server finds the password weak, and when the old one is refused.
    pub async fn change_password(&self, new: &str) -> Result<(), Error> {
        let sdk = self.sdk();
        self.confirmed(|auth| {
            let sdk = &sdk;
            async move { sdk.account().change_password(new, auth).await.map(drop) }
        })
        .await
    }

    /// Closes the account for good.
    ///
    /// # Errors
    /// Fails on server errors, and when the password is refused.
    pub async fn deactivate(&self) -> Result<(), Error> {
        let sdk = self.sdk();
        self.confirmed(|auth| {
            let sdk = &sdk;
            async move { sdk.account().deactivate(None, auth, false).await.map(drop) }
        })
        .await?;
        let _ = std::fs::remove_dir_all(&self.dir);
        Ok(())
    }

    /// Creates the recovery key, or replaces it when the account already has one.
    ///
    /// # Errors
    /// Fails on server errors, and when the password is refused.
    pub async fn recovery_key(&self) -> Result<String, Error> {
        let sdk = self.sdk();
        let recovery = sdk.encryption().recovery();
        let key = if recovery.state() == RecoveryState::Enabled {
            recovery.reset_key().await
        } else {
            self.confirmed(|auth| {
                let sdk = &sdk;
                async move {
                    sdk.encryption()
                        .bootstrap_cross_signing_if_needed(auth)
                        .await
                }
            })
            .await?;
            recovery.enable().await
        };
        key.map_err(failed)
    }

    /// The members of a room whose verified keys have changed since the reader trusted them.
    pub async fn changed_keys(&self, channel: &ChannelId) -> Vec<String> {
        let (Ok(id), sdk) = (room_id(channel), self.sdk()) else {
            return Vec::new();
        };
        let Some(room) = sdk.get_room(&id) else {
            return Vec::new();
        };
        let mut names = Vec::new();
        for member in room
            .members(RoomMemberships::ACTIVE)
            .await
            .unwrap_or_default()
        {
            if member.user_id().as_str() == self.me.0 {
                continue;
            }
            let identity = sdk.encryption().get_user_identity(member.user_id()).await;
            if identity
                .ok()
                .flatten()
                .is_some_and(|i| i.has_verification_violation())
            {
                names.push(member.name().to_owned());
            }
        }
        names
    }

    /// Accepts the current keys of a user whose identity changed.
    ///
    /// # Errors
    /// Fails when the user's keys are not known or the store fails.
    pub async fn trust(&self, user: &UserId) -> Result<(), Error> {
        let sdk = self.sdk();
        let identity = sdk
            .encryption()
            .request_user_identity(&user_id(user)?)
            .await
            .map_err(failed)?
            .ok_or_else(|| failed("This user has no keys yet"))?;
        if identity.has_verification_violation() {
            identity.withdraw_verification().await.map_err(failed)
        } else {
            identity.pin().await.map_err(failed)
        }
    }
}
