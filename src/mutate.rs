use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::client::YtMusic;
use crate::context::Client;

impl YtMusic {
    pub async fn rate_track(&self, video_id: &str, liked: bool) -> Result<()> {
        let endpoint = match liked {
            true => "like/like",
            false => "like/removelike",
        };
        self.execute(
            endpoint,
            Client::Music,
            json!({ "target": { "videoId": video_id } }),
        )
        .await?;
        Ok(())
    }

    pub async fn rate_playlist(&self, playlist_id: &str, saved: bool) -> Result<()> {
        let endpoint = match saved {
            true => "like/like",
            false => "like/removelike",
        };
        self.execute(
            endpoint,
            Client::Music,
            json!({ "target": { "playlistId": playlist_id } }),
        )
        .await?;
        Ok(())
    }

    pub async fn create_playlist(&self, title: &str) -> Result<String> {
        let response = self
            .execute(
                "playlist/create",
                Client::Music,
                json!({ "title": title, "privacyStatus": "PRIVATE" }),
            )
            .await?;
        response
            .get("playlistId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("create response has no playlistId")
    }

    pub async fn delete_playlist(&self, playlist_id: &str) -> Result<()> {
        self.execute(
            "playlist/delete",
            Client::Music,
            json!({ "playlistId": strip_vl(playlist_id) }),
        )
        .await?;
        Ok(())
    }

    pub async fn rename_playlist(&self, playlist_id: &str, title: &str) -> Result<()> {
        self.edit_playlist(
            playlist_id,
            json!([{ "action": "ACTION_SET_PLAYLIST_NAME", "playlistName": title }]),
        )
        .await
    }

    pub async fn set_playlist_privacy(&self, playlist_id: &str, public: bool) -> Result<()> {
        let privacy = match public {
            true => "PUBLIC",
            false => "PRIVATE",
        };
        self.edit_playlist(
            playlist_id,
            json!([{ "action": "ACTION_SET_PLAYLIST_PRIVACY", "playlistPrivacy": privacy }]),
        )
        .await
    }

    /// Uploads `image` and makes it the playlist's thumbnail. YouTube crops it to a square, so
    /// a square picture shows as it is. An account that has not verified a phone number gets
    /// the upload through and the attach refused.
    pub async fn set_playlist_thumbnail(&self, playlist_id: &str, image: Vec<u8>) -> Result<()> {
        let blob = self.upload_playlist_image(image).await?;
        let reply = self
            .execute(
                "browse/edit_playlist",
                Client::Music,
                json!({
                    "playlistId": strip_vl(playlist_id),
                    "actions": [{
                        "action": "ACTION_SET_CUSTOM_THUMBNAIL",
                        "addedCustomThumbnail": {
                            "imageKey": thumbnail_key(),
                            "playlistScottyEncryptedBlobId": blob,
                        },
                    }],
                }),
            )
            .await
            .context("cannot attach the thumbnail")?;
        succeeded(&reply)
    }

    /// Takes the playlist's own thumbnail off, so YouTube shows its collage again.
    pub async fn remove_playlist_thumbnail(&self, playlist_id: &str) -> Result<()> {
        let reply = self
            .execute(
                "browse/edit_playlist",
                Client::Music,
                json!({
                    "playlistId": strip_vl(playlist_id),
                    "actions": [{
                        "action": "ACTION_REMOVE_CUSTOM_THUMBNAIL",
                        "deletedCustomThumbnail": thumbnail_key(),
                    }],
                }),
            )
            .await?;
        succeeded(&reply)
    }

    pub async fn add_playlist_track(&self, playlist_id: &str, video_id: &str) -> Result<()> {
        self.edit_playlist(
            playlist_id,
            json!([{
                "action": "ACTION_ADD_VIDEO",
                "addedVideoId": video_id,
                "dedupeOption": "DEDUPE_OPTION_SKIP",
            }]),
        )
        .await
    }

    pub async fn remove_playlist_track(
        &self,
        playlist_id: &str,
        video_id: &str,
        set_video_id: &str,
    ) -> Result<()> {
        self.edit_playlist(
            playlist_id,
            json!([{
                "action": "ACTION_REMOVE_VIDEO",
                "setVideoId": set_video_id,
                "removedVideoId": video_id,
            }]),
        )
        .await
    }

    async fn edit_playlist(&self, playlist_id: &str, actions: Value) -> Result<()> {
        self.execute(
            "browse/edit_playlist",
            Client::Music,
            json!({ "playlistId": strip_vl(playlist_id), "actions": actions }),
        )
        .await?;
        Ok(())
    }
}

/// Which picture slot of a playlist the custom thumbnail goes in.
fn thumbnail_key() -> Value {
    json!({ "name": "studio_square_thumbnail", "type": "PLAYLIST_IMAGE_TYPE_CUSTOM_THUMBNAIL" })
}

/// Fails unless an `edit_playlist` reply reports success. YouTube answers 200 with a failed
/// status for an edit it turned down.
fn succeeded(reply: &Value) -> Result<()> {
    match reply["status"].as_str() {
        Some("STATUS_SUCCEEDED") => Ok(()),
        status => anyhow::bail!(
            "YouTube refused the edit: {}",
            status.unwrap_or("no status")
        ),
    }
}

fn strip_vl(playlist_id: &str) -> &str {
    playlist_id.trim_start_matches("VL")
}
