use std::{collections::HashSet, future::Future};

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::client::YtMusic;
use crate::context::Client;
use crate::dedup;
use crate::models::{Album, Identity, Playlist, Profile, Track, TrackKind};
use crate::nav::{Nav as _, find_all};
use crate::parse;

pub const LIKED_SONGS: &str = "LM";
const LIBRARY_ALBUMS: &str = "FEmusic_liked_albums";
const LIBRARY_PLAYLISTS: &str = "FEmusic_liked_playlists";
const RESOLVE_CONCURRENCY: usize = 6;

impl YtMusic {
    pub async fn liked_songs(&self) -> Result<Vec<Track>> {
        let detail = self.playlist(LIKED_SONGS).await?;
        Ok(dedup::collapse(detail.tracks))
    }

    pub async fn track_duration(&self, video_id: &str) -> Option<std::time::Duration> {
        let response = self
            .execute("next", Client::Music, json!({ "videoId": video_id }))
            .await
            .ok()?;
        parse::find_renderers(&response, "playlistPanelVideoRenderer")
            .into_iter()
            .find_map(|renderer| renderer.run_text(&["lengthText"]))
            .as_deref()
            .and_then(crate::util::parse_clock)
    }

    pub async fn resolve_song(&self, track: &Track) -> Result<Option<Track>> {
        if !track.is_video() {
            return Ok(None);
        }
        let query = dedup::search_query(track);
        let candidates = self.search_songs(&query).await?;
        Ok(dedup::best_song_match(track, candidates))
    }

    pub async fn liked_songs_resolved(self: &std::sync::Arc<Self>) -> Result<Vec<Track>> {
        let raw = self.liked_songs().await?;
        let resolved = self.resolve_videos(raw).await;
        Ok(dedup::collapse(resolved))
    }

    pub async fn swap_playable(self: &std::sync::Arc<Self>, tracks: Vec<Track>) -> Vec<Track> {
        use tokio::sync::Semaphore;
        let limit = std::sync::Arc::new(Semaphore::new(RESOLVE_CONCURRENCY));
        let mut tasks = tokio::task::JoinSet::new();
        for (index, mut track) in tracks.into_iter().enumerate() {
            let api = self.clone();
            let limit = limit.clone();
            tasks.spawn(async move {
                let Some(video_id) = track.video_id.clone().filter(|_| track.is_video()) else {
                    return (index, track);
                };
                let song = match api.resolve_cache.get(&video_id).await {
                    Some(cached) => cached,
                    None => {
                        let _permit = limit.acquire().await;
                        let resolved = api.resolve_song(&track).await.ok().flatten();
                        api.resolve_cache.put(video_id, resolved.clone()).await;
                        resolved
                    }
                };
                if let Some(song) = song {
                    track.video_id = song.video_id;
                    if song.duration.is_some() {
                        track.duration = song.duration;
                    }
                    track.kind = TrackKind::Song;
                    if track.album.is_none() {
                        track.album = song.album;
                    }
                }
                (index, track)
            });
        }
        let mut resolved: Vec<(usize, Track)> = Vec::new();
        while let Some(result) = tasks.join_next().await {
            if let Ok(pair) = result {
                resolved.push(pair);
            }
        }
        self.resolve_cache.flush().await;
        resolved.sort_by_key(|(index, _)| *index);
        resolved.into_iter().map(|(_, track)| track).collect()
    }

    pub async fn resolve_videos(self: &std::sync::Arc<Self>, tracks: Vec<Track>) -> Vec<Track> {
        use tokio::sync::Semaphore;
        let limit = std::sync::Arc::new(Semaphore::new(RESOLVE_CONCURRENCY));
        let mut tasks = tokio::task::JoinSet::new();
        for (index, track) in tracks.into_iter().enumerate() {
            let api = self.clone();
            let limit = limit.clone();
            tasks.spawn(async move {
                if !track.is_video() {
                    return (index, track);
                }
                let Some(video_id) = track.video_id.clone() else {
                    return (index, track);
                };
                if let Some(cached) = api.resolve_cache.get(&video_id).await {
                    return (index, cached.unwrap_or(track));
                }
                let _permit = limit.acquire().await;
                let resolved = api.resolve_song(&track).await.ok().flatten();
                api.resolve_cache.put(video_id, resolved.clone()).await;
                (index, resolved.unwrap_or(track))
            });
        }
        let mut resolved: Vec<(usize, Track)> = Vec::new();
        while let Some(result) = tasks.join_next().await {
            if let Ok(pair) = result {
                resolved.push(pair);
            }
        }
        self.resolve_cache.flush().await;
        resolved.sort_by_key(|(index, _)| *index);
        resolved.into_iter().map(|(_, track)| track).collect()
    }

    pub async fn library_albums(&self) -> Result<Vec<Album>> {
        let response = self
            .execute(
                "browse",
                Client::Music,
                json!({ "browseId": LIBRARY_ALBUMS }),
            )
            .await?;
        Ok(parse::find_renderers(&response, "musicTwoRowItemRenderer")
            .into_iter()
            .filter_map(parse::two_row_album)
            .collect())
    }

    pub async fn library_playlists(&self) -> Result<Vec<Playlist>> {
        library_playlists_with(|payload| self.execute("browse", Client::Music, payload)).await
    }

    pub async fn profile(&self) -> Result<Profile> {
        match self.is_cookie_auth() {
            true => self.profile_from_menu().await,
            false => self.profile_from_accounts().await,
        }
    }

    async fn profile_from_menu(&self) -> Result<Profile> {
        let response = self
            .execute("account/account_menu", Client::Music, json!({}))
            .await?;
        let Some(account) = parse::find_renderer(&response, "activeAccountHeaderRenderer") else {
            log::debug!(
                "profile: account_menu has no active account, response: {}",
                snippet(&response)
            );
            anyhow::bail!("account menu has no active account");
        };
        log::debug!("profile: activeAccountHeaderRenderer: {account}");
        Ok(profile(account))
    }
    pub async fn identities(&self) -> Result<Vec<Identity>> {
        let response = self
            .execute(
                "account/accounts_list",
                Client::Web,
                json!({ "requestType": "ACCOUNTS_LIST_REQUEST_TYPE_ACCOUNT_SWITCHER" }),
            )
            .await;
        let found: Vec<Identity> = match &response {
            Ok(response) => parse::find_renderers(response, "accountItem")
                .into_iter()
                .filter(|item| item.at(&["isDisabled"]).and_then(Value::as_bool) != Some(true))
                .map(identity)
                .collect(),
            Err(error) => {
                log::debug!("identities: the account switcher did not answer: {error:#}");
                Vec::new()
            }
        };
        log::debug!(
            "identities: authuser {} sees {} in the switcher",
            self.authuser(),
            found.len()
        );
        if !found.is_empty() {
            return Ok(found);
        }
        if let Ok(response) = &response {
            log::debug!(
                "identities: the switcher named no account, response: {}",
                snippet(response)
            );
        }
        Ok(vec![Identity {
            profile: self.profile().await?,
            page_id: None,
        }])
    }

    async fn profile_from_accounts(&self) -> Result<Profile> {
        let response = self
            .execute("account/accounts_list", Client::Tv, json!({}))
            .await?;
        let Some(account) = parse::find_renderer(&response, "accountItem") else {
            log::debug!(
                "profile: accounts_list has no accountItem, response: {}",
                snippet(&response)
            );
            anyhow::bail!("accounts list has no account");
        };
        log::debug!("profile: accountItem: {account}");
        Ok(profile(account))
    }
}

/// Collects library pages in server order without returning a silently truncated library.
async fn library_playlists_with<F, Fut>(mut fetch: F) -> Result<Vec<Playlist>>
where
    F: FnMut(Value) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let mut payload = json!({ "browseId": LIBRARY_PLAYLISTS });
    let mut playlists = Vec::new();
    let mut ids = HashSet::new();
    let mut tokens = HashSet::new();
    loop {
        let response = fetch(payload).await?;
        let (items, continuation) = library_playlist_page(&response)?;
        for playlist in items {
            if playlist.id != LIKED_SONGS && ids.insert(playlist.id.clone()) {
                playlists.push(playlist);
            }
        }
        let Some(token) = continuation else {
            return Ok(playlists);
        };
        anyhow::ensure!(
            tokens.insert(token.clone()),
            "library playlists repeated a continuation"
        );
        payload = json!({ "continuation": token });
    }
}

/// Reads the library grid and its continuation, excluding unrelated response renderers.
fn library_playlist_page(response: &Value) -> Result<(Vec<Playlist>, Option<String>)> {
    let grid = parse::find_renderer(response, "gridRenderer")
        .or_else(|| parse::find_renderer(response, "gridContinuation"));
    let (items, mut continuation) = if let Some(grid) = grid {
        let items = grid
            .at(&["items"])
            .and_then(Value::as_array)
            .context("library playlists grid has no items")?;
        let continuation = parse::shelf_continuation(grid);
        anyhow::ensure!(
            grid.items(&["continuations"]).is_empty() || continuation.is_some(),
            "library playlists grid has an unknown continuation"
        );
        (items.as_slice(), continuation)
    } else {
        let action = parse::find_renderer(response, "appendContinuationItemsAction")
            .context("library playlists response has no grid")?;
        let items = action
            .at(&["continuationItems"])
            .and_then(Value::as_array)
            .context("library playlists continuation has no items")?;
        (items.as_slice(), None)
    };
    let mut playlists = Vec::new();
    for item in items {
        if let Some(playlist) = item
            .at(&["musicTwoRowItemRenderer"])
            .and_then(parse::two_row_playlist)
        {
            playlists.push(playlist);
        }
        if continuation.is_none()
            && let Some(endpoint) = item.at(&["continuationItemRenderer", "continuationEndpoint"])
        {
            continuation = endpoint
                .str_at(&["continuationCommand", "token"])
                .or_else(|| {
                    endpoint
                        .items(&["commandExecutorCommand", "commands"])
                        .iter()
                        .find_map(|command| {
                            (command.str_at(&["continuationCommand", "request"])
                                == Some("CONTINUATION_REQUEST_TYPE_BROWSE"))
                            .then(|| command.str_at(&["continuationCommand", "token"]))
                            .flatten()
                        })
                })
                .filter(|token| !token.is_empty())
                .map(str::to_string);
            anyhow::ensure!(
                continuation.is_some(),
                "library playlists item has an unknown continuation"
            );
        }
    }
    Ok((playlists, continuation))
}

fn identity(item: &Value) -> Identity {
    Identity {
        profile: profile(item),
        page_id: page_id(item),
    }
}

fn profile(node: &Value) -> Profile {
    let email = node
        .run_text(&["channelHandle"])
        .or_else(|| node.run_text(&["email"]))
        .or_else(|| node.run_text(&["accountByline"]));
    let name = node
        .run_text(&["accountName"])
        .or_else(|| email.clone())
        .unwrap_or_else(|| "YouTube Music".to_string());
    Profile {
        name,
        email,
        thumbnails: parse::thumbnails(node),
    }
}

fn page_id(item: &Value) -> Option<String> {
    let mut found = Vec::new();
    find_all(item, "pageIdToken", &mut found);
    found
        .into_iter()
        .find_map(|token| token.str_at(&["pageId"]).map(str::to_string))
}

fn snippet(value: &serde_json::Value) -> String {
    let mut text = value.to_string();
    text.truncate(4000);
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn playlist(id: &str) -> Value {
        json!({"musicTwoRowItemRenderer": {
            "navigationEndpoint": {"browseEndpoint": {"browseId": format!("VL{id}")}},
            "title": {"runs": [{"text": format!("Synthetic {id}")}]}
        }})
    }

    fn grid(items: Vec<Value>, token: Option<&str>, initial: bool) -> Value {
        let mut grid = json!({"items": items});
        if let Some(token) = token {
            grid["continuations"] = json!([{"nextContinuationData": {"continuation": token}}]);
        }
        if initial {
            json!({"contents": {"gridRenderer": grid}})
        } else {
            json!({"continuationContents": {"gridContinuation": grid}})
        }
    }

    async fn collect(pages: Vec<Value>) -> (Result<Vec<Playlist>>, Vec<Value>) {
        let mut pages: VecDeque<_> = pages.into();
        let mut requests = Vec::new();
        let result = library_playlists_with(|payload| {
            requests.push(payload);
            std::future::ready(pages.pop_front().context("unexpected extra request"))
        })
        .await;
        (result, requests)
    }

    #[tokio::test]
    async fn collects_twenty_four_plus_nine_playlists_in_order() {
        let first = (0..24).map(|i| playlist(&format!("PL{i}"))).collect();
        let second = (24..33).map(|i| playlist(&format!("PL{i}"))).collect();
        let (result, requests) = collect(vec![
            grid(first, Some("next-page"), true),
            grid(second, None, false),
        ])
        .await;
        let ids: Vec<_> = result.unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, (0..33).map(|i| format!("PL{i}")).collect::<Vec<_>>());
        assert_eq!(
            requests,
            vec![
                json!({"browseId": LIBRARY_PLAYLISTS}),
                json!({"continuation": "next-page"})
            ]
        );
    }

    #[tokio::test]
    async fn skips_special_tiles_and_deduplicates_by_id() {
        let new_playlist = json!({"musicTwoRowItemRenderer": {"navigationEndpoint": {"browseEndpoint": {"browseId": "SE"}}, "title": {"simpleText": "New playlist"}}});
        let (result, _) = collect(vec![
            grid(
                vec![playlist("LM"), new_playlist, playlist("PL1")],
                Some("next"),
                true,
            ),
            grid(vec![playlist("PL1"), playlist("PL2")], None, false),
        ])
        .await;
        assert_eq!(
            result
                .unwrap()
                .into_iter()
                .map(|p| p.id)
                .collect::<Vec<_>>(),
            ["PL1", "PL2"]
        );
    }

    #[tokio::test]
    async fn accepts_empty_and_single_pages_without_extra_requests() {
        for items in [vec![], vec![playlist("PL1")]] {
            let count = items.len();
            let (result, requests) = collect(vec![grid(items, None, true)]).await;
            assert_eq!(result.unwrap().len(), count);
            assert_eq!(requests.len(), 1);
        }
    }

    #[tokio::test]
    async fn continues_past_an_empty_page() {
        let (result, requests) = collect(vec![
            grid(vec![], Some("a"), true),
            grid(vec![], Some("b"), false),
            grid(vec![playlist("PL1")], None, false),
        ])
        .await;
        assert_eq!(result.unwrap().len(), 1);
        assert_eq!(requests.len(), 3);
    }

    #[tokio::test]
    async fn rejects_repeated_tokens_instead_of_hanging_or_returning_partial_data() {
        let (result, requests) = collect(vec![
            grid(vec![playlist("PL1")], Some("a"), true),
            grid(vec![playlist("PL2")], Some("b"), false),
            grid(vec![], Some("a"), false),
        ])
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("repeated a continuation")
        );
        assert_eq!(requests.len(), 3);
    }

    #[tokio::test]
    async fn propagates_fetch_errors() {
        let (result, requests) =
            collect(vec![grid(vec![playlist("PL1")], Some("missing"), true)]).await;
        assert_eq!(result.unwrap_err().to_string(), "unexpected extra request");
        assert_eq!(requests.len(), 2);
    }

    #[test]
    fn rejects_unrecognized_or_malformed_responses() {
        for response in [
            json!({}),
            json!({"gridRenderer": {}}),
            json!({"appendContinuationItemsAction": {}}),
        ] {
            assert!(library_playlist_page(&response).is_err());
        }
    }

    #[test]
    fn supports_append_actions_and_continuation_items() {
        let response = json!({"onResponseReceivedActions": [{"appendContinuationItemsAction": {"continuationItems": [playlist("PL1"), {"continuationItemRenderer": {"continuationEndpoint": {"continuationCommand": {"token": "next"}}}}]}}]});
        let (items, token) = library_playlist_page(&response).unwrap();
        assert_eq!(items[0].id, "PL1");
        assert_eq!(token.as_deref(), Some("next"));
    }
    #[test]
    fn reads_wrapped_browse_continuation_commands() {
        let response = grid(
            vec![
                json!({"continuationItemRenderer": {"continuationEndpoint": {"commandExecutorCommand": {"commands": [
                    {"unrelatedCommand": {}},
                    {"continuationCommand": {"request": "CONTINUATION_REQUEST_TYPE_BROWSE", "token": "wrapped"}}
                ]}}}}),
            ],
            None,
            true,
        );
        assert_eq!(
            library_playlist_page(&response).unwrap().1.as_deref(),
            Some("wrapped")
        );
    }

    #[test]
    fn rejects_unknown_continuations() {
        let mut response = grid(vec![], None, true);
        response["contents"]["gridRenderer"]["continuations"] =
            json!([{"unknownContinuation": {}}]);
        assert!(library_playlist_page(&response).is_err());
        let response = grid(
            vec![
                json!({"continuationItemRenderer": {"continuationEndpoint": {"unknownCommand": {}}}}),
            ],
            None,
            true,
        );
        assert!(library_playlist_page(&response).is_err());
    }
}
