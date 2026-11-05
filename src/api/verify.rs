//! Verification of sessions by comparing seven emojis with the other session.

use std::time::Duration;

use futures_util::StreamExt;
use matrix_sdk::encryption::verification::{
    SasState, SasVerification, Verification, VerificationRequest, VerificationRequestState,
};
use matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent;
use matrix_sdk::ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent};
use tokio::sync::mpsc::UnboundedSender;

use super::{failed, user_id, Client, Error};
use crate::api::types::UserId;

const PATIENCE: Duration = Duration::from_secs(120);

/// A verification that waits for the reader to say whether the emojis match.
#[derive(Debug, Clone)]
pub struct Check {
    sas: SasVerification,
    /// The symbol and the name of each of the seven emojis to compare.
    pub emojis: Vec<(String, String)>,
}

impl Check {
    /// Tells the other session that the emojis match, and waits for it to agree.
    ///
    /// # Errors
    /// Fails when the other session refuses or the exchange breaks.
    pub async fn confirm(&self) -> Result<(), Error> {
        self.sas.confirm().await.map_err(failed)?;
        let mut states = self.sas.changes();
        while let Some(state) = states.next().await {
            match state {
                SasState::Done { .. } => return Ok(()),
                SasState::Cancelled(info) => return Err(failed(info.reason())),
                _ => {}
            }
        }
        Err(failed("Verification stopped"))
    }

    /// Tells the other session that the emojis differ.
    ///
    /// # Errors
    /// Fails when the exchange breaks.
    pub async fn refuse(&self) -> Result<(), Error> {
        self.sas.mismatch().await.map_err(failed)
    }
}

async fn emojis_of(sas: SasVerification) -> Result<Check, Error> {
    let mut states = sas.changes();
    if !sas.we_started() {
        sas.accept().await.map_err(failed)?;
    }
    while sas.emoji().is_none() {
        match states.next().await {
            Some(SasState::Cancelled(info)) => return Err(failed(info.reason())),
            Some(SasState::Done { .. }) | None => return Err(failed("Verification stopped")),
            Some(_) => {}
        }
    }
    let emojis = sas
        .emoji()
        .ok_or_else(|| failed("The other session cannot show emojis"))?
        .iter()
        .map(|e| (e.symbol.to_owned(), e.description.to_owned()))
        .collect();
    Ok(Check { sas, emojis })
}

async fn present(request: VerificationRequest) -> Result<Check, Error> {
    let mut states = request.changes();
    if !request.we_started() {
        request.accept().await.map_err(failed)?;
    }
    while let Some(state) = states.next().await {
        match state {
            VerificationRequestState::Ready { .. } if request.we_started() => {
                let sas = request.start_sas().await.map_err(failed)?;
                let sas = sas.ok_or_else(|| failed("The other session cannot compare emojis"))?;
                return emojis_of(sas).await;
            }
            VerificationRequestState::Transitioned {
                verification: Verification::SasV1(sas),
            } => return emojis_of(sas).await,
            VerificationRequestState::Cancelled(info) => return Err(failed(info.reason())),
            VerificationRequestState::Done => return Err(failed("Verification stopped")),
            _ => {}
        }
    }
    Err(failed("Verification stopped"))
}

async fn wait_for(request: VerificationRequest) -> Result<Check, Error> {
    tokio::time::timeout(PATIENCE, present(request))
        .await
        .map_err(|_| failed("Timed out waiting for the other session"))?
}

impl Client {
    /// Asks the other session that was active last to verify this one.
    ///
    /// # Errors
    /// Fails when the account has no cross-signing keys or no other session, or on network errors.
    pub async fn verify(&self) -> Result<Check, Error> {
        let sdk = self.sdk();
        let me = sdk.user_id().ok_or_else(|| failed("Not logged in"))?;
        let encryption = sdk.encryption();
        encryption
            .request_user_identity(me)
            .await
            .map_err(failed)?
            .ok_or_else(|| {
                failed("This account has no cross-signing keys; set them up in another client")
            })?;
        let last = sdk
            .devices()
            .await
            .map_err(failed)?
            .devices
            .into_iter()
            .filter(|d| Some(d.device_id.as_ref()) != sdk.device_id())
            .max_by_key(|d| d.last_seen_ts)
            .ok_or_else(|| failed("This account has no other session to verify against"))?;
        let device = encryption
            .get_device(me, &last.device_id)
            .await
            .map_err(failed)?
            .ok_or_else(|| failed("The keys of the other session are not known yet"))?;
        wait_for(device.request_verification().await.map_err(failed)?).await
    }

    /// Asks another user, in a direct room, to verify their keys with this session.
    ///
    /// # Errors
    /// Fails when the user has no cross-signing keys, or on network errors.
    pub async fn verify_user(&self, user: &UserId) -> Result<Check, Error> {
        let identity = self
            .sdk()
            .encryption()
            .request_user_identity(&user_id(user)?)
            .await
            .map_err(failed)?
            .ok_or_else(|| failed("This user has no cross-signing keys"))?;
        wait_for(identity.request_verification().await.map_err(failed)?).await
    }

    /// Reports each verification that another session or another user asks for.
    pub fn watch(&self, found: UnboundedSender<Check>) {
        let sdk = self.sdk();
        let encryption = sdk.encryption();
        let (devices, people) = (found.clone(), found);
        let seen = encryption.clone();
        sdk.add_event_handler(move |event: ToDeviceKeyVerificationRequestEvent| {
            let encryption = seen.clone();
            let found = devices.clone();
            async move {
                let id = &event.content.transaction_id;
                if let Some(request) = encryption.get_verification_request(&event.sender, id).await
                {
                    offered(request, found);
                }
            }
        });
        sdk.add_event_handler(move |event: OriginalSyncRoomMessageEvent| {
            let encryption = encryption.clone();
            let found = people.clone();
            async move {
                if !matches!(event.content.msgtype, MessageType::VerificationRequest(_)) {
                    return;
                }
                let request = encryption
                    .get_verification_request(&event.sender, &event.event_id)
                    .await;
                if let Some(request) = request {
                    offered(request, found);
                }
            }
        });
    }
}

fn offered(request: VerificationRequest, found: UnboundedSender<Check>) {
    tokio::spawn(async move {
        if let Ok(check) = wait_for(request).await {
            let _ = found.send(check);
        }
    });
}
