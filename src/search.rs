use anyhow::Result;
use serde_json::json;

use crate::client::YtMusic;
use crate::context::Client;
use crate::models::{Album, Playlist, Track};
use crate::nav::Nav as _;
use crate::parse;

pub const SONGS: &str = "EgWKAQIIAQ%3D%3D";
pub const ALBUMS: &str = "EgWKAQIYAQ%3D%3D";
pub const ARTISTS: &str = "EgWKAQIgAQ%3D%3D";
pub const PLAYLISTS: &str = "EgWKAQIoAQ%3D%3D";

impl YtMusic {
    pub async fn search_songs(&self, query: &str) -> Result<Vec<Track>> {
        self.search_songs_with(query, true).await
    }

    pub async fn search_songs_with(&self, query: &str, use_auth: bool) -> Result<Vec<Track>> {
        let response = self
            .execute_with(
                "search",
                Client::Music,
                json!({ "query": query, "params": SONGS }),
                use_auth,
            )
            .await?;
        let mut tracks = Vec::new();
        for shelf in parse::find_renderers(&response, "musicShelfRenderer") {
            for item in shelf.items(&["contents"]) {
                if let Some(track) = parse::list_item_track(item)
                    && track.video_id.is_some()
                {
                    tracks.push(track);
                }
            }
        }
        Ok(tracks)
    }

    pub async fn search_albums(&self, query: &str) -> Result<Vec<Album>> {
        self.search_albums_with(query, true).await
    }

    pub async fn search_albums_with(&self, query: &str, use_auth: bool) -> Result<Vec<Album>> {
        let response = self
            .execute_with(
                "search",
                Client::Music,
                json!({ "query": query, "params": ALBUMS }),
                use_auth,
            )
            .await?;
        let mut albums = Vec::new();
        for shelf in parse::find_renderers(&response, "musicShelfRenderer") {
            for item in shelf.items(&["contents"]) {
                if let Some(album) = parse::list_item_album(item) {
                    albums.push(album);
                }
            }
        }
        Ok(albums)
    }

    pub async fn search_playlists(&self, query: &str) -> Result<Vec<Playlist>> {
        self.search_playlists_with(query, true).await
    }

    pub async fn search_playlists_with(
        &self,
        query: &str,
        use_auth: bool,
    ) -> Result<Vec<Playlist>> {
        let response = self
            .execute_with(
                "search",
                Client::Music,
                json!({ "query": query, "params": PLAYLISTS }),
                use_auth,
            )
            .await?;
        let mut playlists = Vec::new();
        for shelf in parse::find_renderers(&response, "musicShelfRenderer") {
            for item in shelf.items(&["contents"]) {
                if let Some(playlist) = parse::list_item_playlist(item) {
                    playlists.push(playlist);
                }
            }
        }
        Ok(playlists)
    }

    pub async fn search_suggestions(&self, input: &str) -> Result<Vec<String>> {
        let response = self
            .execute(
                "music/get_search_suggestions",
                Client::Music,
                json!({ "input": input }),
            )
            .await?;
        Ok(parse::find_renderers(&response, "searchSuggestionRenderer")
            .into_iter()
            .filter_map(|suggestion| suggestion.run_text(&["suggestion"]))
            .collect())
    }
}
