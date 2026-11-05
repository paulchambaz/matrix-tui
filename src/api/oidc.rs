//! Sign-in through OAuth 2.0, for servers that no longer accept a password.

use std::path::{Path, PathBuf};

use matrix_sdk::authentication::oauth::registration::{
    ApplicationType, ClientMetadata, Localized, OAuthGrantType,
};
use matrix_sdk::authentication::oauth::{
    ClientId, ClientRegistrationData, OAuthSession, UserSession,
};
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::store::RoomLoadSettings;
use matrix_sdk::utils::local_server::LocalServerBuilder;
use matrix_sdk::SessionChange;
use reqwest::Url;
use serde::{Deserialize, Serialize};

use super::{failed, write_private, Error, SSO_WAIT};

const HOME: &str = "https://github.com/paulchambaz/matrix-tui";

#[derive(Serialize, Deserialize)]
struct Saved {
    client_id: String,
    #[serde(flatten)]
    user: UserSession,
}

fn metadata(redirect: &Url) -> Result<Raw<ClientMetadata>, serde_json::Error> {
    let home = Url::parse(HOME).map_err(serde::de::Error::custom)?;
    let mut data = ClientMetadata::new(
        ApplicationType::Native,
        vec![OAuthGrantType::AuthorizationCode {
            redirect_uris: vec![redirect.clone()],
        }],
        Localized::new(home, []),
    );
    data.client_name = Some(Localized::new("mx".to_owned(), []));
    Raw::new(&data)
}

/// Signs in through the browser and waits for the server to send it back.
pub(super) async fn login(sdk: &matrix_sdk::Client, browser: &str) -> matrix_sdk::Result<()> {
    let (redirect, answer) = LocalServerBuilder::new().spawn().await?;
    let registration = ClientRegistrationData::new(metadata(&redirect)?);
    let oauth = sdk.oauth();
    let page = oauth
        .login(redirect, None, Some(registration), None)
        .build()
        .await?;
    let (failure, failed_to_open) = tokio::sync::oneshot::channel();
    let address = page.url.to_string();
    let hint = address.clone();
    crate::open::start(browser, &address, move |outcome| {
        if let Some(why) = outcome {
            let _ = failure.send(format!("{why}; open {hint} yourself"));
        }
    })?;
    let waited = tokio::time::timeout(SSO_WAIT, async {
        tokio::select! {
            query = answer => query.ok_or_else(|| matrix_sdk::Error::UnknownError("The browser sent nothing back".into())),
            Ok(why) = failed_to_open => Err(matrix_sdk::Error::UnknownError(why.into())),
        }
    })
    .await
    .unwrap_or_else(|_| {
        Err(matrix_sdk::Error::UnknownError(
            "Timed out waiting for the browser".into(),
        ))
    })?;
    oauth.finish_login(waited.into()).await
}

/// The session of a client signed in through OAuth, as it is kept on disk.
pub(super) fn saved(sdk: &matrix_sdk::Client) -> Result<Vec<u8>, Error> {
    let session = sdk
        .oauth()
        .full_session()
        .ok_or_else(|| failed("the server sent no session"))?;
    let saved = Saved {
        client_id: session.client_id.as_str().to_owned(),
        user: session.user,
    };
    Ok(serde_json::to_vec(&saved)?)
}

/// Signs in again with a session read from disk, when the bytes are one of this kind.
pub(super) async fn restore(
    sdk: &matrix_sdk::Client,
    bytes: &[u8],
) -> Option<matrix_sdk::Result<()>> {
    let saved: Saved = serde_json::from_slice(bytes).ok()?;
    let session = OAuthSession {
        client_id: ClientId::new(saved.client_id),
        user: saved.user,
    };
    Some(
        sdk.oauth()
            .restore_session(session, RoomLoadSettings::default())
            .await,
    )
}

/// Writes the session again each time the server renews its tokens.
pub(super) fn keep_saved(sdk: &matrix_sdk::Client, path: &Path) {
    let (sdk, path): (_, PathBuf) = (sdk.clone(), path.to_owned());
    let mut changes = sdk.subscribe_to_session_changes();
    tokio::spawn(async move {
        while let Ok(change) = changes.recv().await {
            if matches!(change, SessionChange::TokensRefreshed) {
                if let Ok(bytes) = saved(&sdk) {
                    let _ = write_private(&path, &bytes);
                }
            }
        }
    });
}
