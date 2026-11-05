//! Spaces: creating one and moving rooms in and out of it.

use matrix_sdk::ruma::api::client::room::create_room;
use matrix_sdk::ruma::serde::Raw;
use serde_json::{json, Value};

use super::{failed, Client, Error, HOME};
use crate::api::types::{ChannelId, TeamId};

impl Client {
    /// Creates a space that only invited users join.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn create_space(&self, name: &str) -> Result<(), Error> {
        let mut request = create_room::v3::Request::new();
        request.name = Some(name.to_owned());
        request.preset = Some(create_room::v3::RoomPreset::PrivateChat);
        let kind = Raw::new(&json!({"type": "m.space"})).map_err(failed)?;
        request.creation_content = Some(kind.cast_unchecked());
        self.sdk().create_room(request).await?;
        Ok(())
    }

    /// Puts a room in a space.
    ///
    /// # Errors
    /// Fails on network or server errors, and when the reader may not edit the space.
    pub async fn space_add(&self, team: &TeamId, channel: &ChannelId) -> Result<(), Error> {
        let via = self
            .sdk()
            .user_id()
            .map(|me| me.server_name().to_string())
            .ok_or_else(|| failed("Not logged in"))?;
        self.child(team, channel, json!({"via": [via]})).await
    }

    /// Takes a room out of a space.
    ///
    /// # Errors
    /// Fails on network or server errors, on Home, and when the reader may not edit the space.
    pub async fn space_remove(&self, team: &TeamId, channel: &ChannelId) -> Result<(), Error> {
        self.child(team, channel, json!({})).await
    }

    async fn child(&self, team: &TeamId, channel: &ChannelId, content: Value) -> Result<(), Error> {
        if team.0 == HOME {
            return Err(failed("Home is not a space"));
        }
        let space = self.room(&ChannelId(team.0.clone()))?;
        space
            .send_state_event_raw("m.space.child", &channel.0, content)
            .await?;
        Ok(())
    }
}
