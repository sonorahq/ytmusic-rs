use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::client::YtMusic;
use crate::context::Client;
use crate::models::{
    Album, AlbumDetail, AlbumKind, AlbumRef, Artist, ArtistRef, Playlist, PlaylistDetail,
    PlaylistPage, Track,
};
use crate::nav::Nav as _;
use crate::parse;
use crate::util::parse_year;

const MAX_PAGES: usize = 50;

impl YtMusic {
    pub async fn album(&self, browse_id: &str) -> Result<AlbumDetail> {
        let response = self
            .execute("browse", Client::Music, json!({ "browseId": browse_id }))
            .await?;
        let header = parse::find_renderer(&response, "musicResponsiveHeaderRenderer")
            .or_else(|| parse::find_renderer(&response, "musicDetailHeaderRenderer"))
            .context("album response has no header")?;
        let title = header.run_text(&["title"]).unwrap_or_default();
        let subtitle_runs = header.runs(&["subtitle"]);
        let kind = subtitle_runs
            .iter()
            .filter_map(|run| run.str_at(&["text"]))
            .find_map(parse::album_kind)
            .unwrap_or(AlbumKind::Album);
        let year = subtitle_runs
            .iter()
            .chain(header.runs(&["secondSubtitle"]).iter())
            .filter_map(|run| run.str_at(&["text"]))
            .find_map(parse_year);
        let mut artists = parse::artist_runs(header.runs(&["straplineTextOne"]));
        artists.extend(parse::artist_runs(subtitle_runs));
        if artists.is_empty()
            && let Some(text) = header.run_text(&["straplineTextOne"])
        {
            artists.push(ArtistRef {
                name: text,
                id: None,
            });
        }
        let track_count = header
            .runs(&["secondSubtitle"])
            .iter()
            .filter_map(|run| run.str_at(&["text"]))
            .find_map(parse::count_from_text);
        let thumbnails = parse::thumbnails(header);
        let playlist_id = playlist_id_of(header);
        let description = parse::find_renderer(header, "musicDescriptionShelfRenderer")
            .and_then(|shelf| shelf.run_text(&["description"]))
            .or_else(|| header.run_text(&["description"]));

        let mut tracks = Vec::new();
        for shelf in parse::find_renderers(&response, "musicShelfRenderer")
            .into_iter()
            .chain(parse::find_renderers(
                &response,
                "musicPlaylistShelfRenderer",
            ))
        {
            for item in shelf.items(&["contents"]) {
                if let Some(track) = parse::list_item_track(item) {
                    tracks.push(track);
                }
            }
        }
        let album_ref = AlbumRef {
            name: title.clone(),
            id: Some(browse_id.to_string()),
        };
        for track in &mut tracks {
            if track.album.is_none() {
                track.album = Some(album_ref.clone());
            }
            if track.thumbnails.is_empty() {
                track.thumbnails = thumbnails.clone();
            }
            if track.artists.is_empty() {
                track.artists = artists.clone();
            }
        }
        Ok(AlbumDetail {
            album: Album {
                browse_id: browse_id.to_string(),
                playlist_id,
                title,
                artists,
                kind,
                year,
                track_count: track_count.or(Some(tracks.len() as u32)),
                thumbnails,
            },
            description,
            duration_text: None,
            tracks,
        })
    }

    pub async fn artist(&self, browse_id: &str) -> Result<Artist> {
        let response = self
            .execute("browse", Client::Music, json!({ "browseId": browse_id }))
            .await?;
        let header = parse::find_renderer(&response, "musicImmersiveHeaderRenderer")
            .or_else(|| parse::find_renderer(&response, "musicVisualHeaderRenderer"))
            .or_else(|| parse::find_renderer(&response, "musicHeaderRenderer"))
            .context("artist response has no header")?;
        let name = header.run_text(&["title"]).unwrap_or_default();
        let description = header
            .run_text(&["description"])
            .or_else(|| {
                parse::find_renderer(&response, "musicDescriptionShelfRenderer")
                    .and_then(|shelf| shelf.run_text(&["description"]))
            })
            .filter(|text| !text.is_empty());
        let subscribers = header
            .run_text(&[
                "subscriptionButton",
                "subscribeButtonRenderer",
                "subscriberCountText",
            ])
            .or_else(|| {
                header
                    .at(&["subscriptionButton", "subscribeButtonRenderer"])
                    .and_then(|button| button.run_text(&["subscriberCountText"]))
            });
        let mut top_tracks = Vec::new();
        if let Some(shelf) = parse::find_renderer(&response, "musicShelfRenderer") {
            for item in shelf.items(&["contents"]) {
                if let Some(track) = parse::list_item_track(item) {
                    top_tracks.push(track);
                }
            }
        }
        let mut albums = Vec::new();
        let mut singles = Vec::new();
        for carousel in parse::find_renderers(&response, "musicCarouselShelfRenderer") {
            let label = carousel
                .run_text(&["header", "musicCarouselShelfBasicHeaderRenderer", "title"])
                .unwrap_or_default();
            let bucket = match label.as_str() {
                "Albums" => &mut albums,
                "Singles" | "Singles & EPs" | "Singles and EPs" => &mut singles,
                _ => continue,
            };
            for item in carousel.items(&["contents"]) {
                if let Some(album) = parse::two_row_album(item) {
                    bucket.push(album);
                }
            }
        }
        Ok(Artist {
            browse_id: browse_id.to_string(),
            name,
            description,
            subscribers,
            thumbnails: parse::thumbnails(header),
            top_tracks,
            albums,
            singles,
        })
    }

    /// Every release on an artist's discography page, newest first. The artist page carousels
    /// stop at ten albums and ten singles, so this is the only way to the full list. Pages past
    /// the first are fetched until the grid runs out or `MAX_PAGES` is reached.
    pub async fn discography(&self, channel_id: &str) -> Result<Vec<Album>> {
        let mut response = self
            .execute(
                "browse",
                Client::Music,
                json!({ "browseId": format!("MPAD{channel_id}") }),
            )
            .await?;
        let mut albums = Vec::new();
        let mut pages = 0;
        loop {
            let grids = parse::find_renderers(&response, "gridRenderer")
                .into_iter()
                .chain(parse::find_renderers(&response, "gridContinuation"));
            let mut token = None;
            for grid in grids {
                albums.extend(
                    grid.items(&["items"])
                        .iter()
                        .filter_map(parse::two_row_album),
                );
                token = token.or_else(|| parse::shelf_continuation(grid));
            }
            pages += 1;
            let Some(token) = token.filter(|_| pages < MAX_PAGES) else {
                break;
            };
            response = self
                .execute("browse", Client::Music, json!({ "continuation": token }))
                .await?;
        }
        Ok(albums)
    }

    pub async fn playlist(&self, playlist_id: &str) -> Result<PlaylistDetail> {
        let mut detail = self.playlist_page(playlist_id).await?;
        let mut pages = 0;
        while let Some(token) = detail.continuation.take() {
            pages += 1;
            if pages > MAX_PAGES {
                break;
            }
            let page = self.playlist_continuation(&token).await?;
            detail.tracks.extend(page.tracks);
            detail.continuation = page.continuation;
        }
        Ok(detail)
    }

    pub async fn playlist_page(&self, playlist_id: &str) -> Result<PlaylistDetail> {
        let browse_id = match playlist_id.starts_with("VL") {
            true => playlist_id.to_string(),
            false => format!("VL{playlist_id}"),
        };
        let response = self
            .execute("browse", Client::Music, json!({ "browseId": browse_id }))
            .await?;
        let editable =
            parse::find_renderer(&response, "musicEditablePlaylistDetailHeaderRenderer").is_some();
        let privacy = privacy_of(&response);
        let header = parse::find_renderer(&response, "musicResponsiveHeaderRenderer")
            .or_else(|| parse::find_renderer(&response, "musicDetailHeaderRenderer"))
            .context("playlist response has no header")?;
        let title = header.run_text(&["title"]).unwrap_or_default();
        let author = match editable {
            true => None,
            false => header
                .runs(&["straplineTextOne"])
                .iter()
                .chain(header.runs(&["subtitle"]).iter())
                .filter_map(|run| run.str_at(&["text"]))
                .find(|text| {
                    !matches!(*text, " • " | ", ")
                        && !text.starts_with("Playlist")
                        && !text.starts_with("Album")
                        && parse_year(text).is_none()
                })
                .map(str::to_string),
        };
        let track_count = header
            .runs(&["secondSubtitle"])
            .iter()
            .filter_map(|run| run.str_at(&["text"]))
            .find_map(parse::count_from_text);
        let thumbnails = parse::thumbnails(header);

        let mut tracks = Vec::new();
        collect_list_tracks(&response, &mut tracks);
        Ok(PlaylistDetail {
            playlist: Playlist {
                id: browse_id.trim_start_matches("VL").to_string(),
                title,
                author,
                owned: editable,
                public: privacy.as_deref().map(|status| status == "PUBLIC"),
                track_count: track_count.or(Some(tracks.len() as u32)),
                thumbnails,
            },
            public: privacy.as_deref() == Some("PUBLIC"),
            tracks,
            continuation: next_continuation(&response),
        })
    }

    pub async fn playlist_continuation(&self, token: &str) -> Result<PlaylistPage> {
        let response = self
            .execute("browse", Client::Music, json!({ "continuation": token }))
            .await?;
        let mut tracks = Vec::new();
        collect_list_tracks(&response, &mut tracks);
        Ok(PlaylistPage {
            tracks,
            continuation: next_continuation(&response),
        })
    }
}

fn collect_list_tracks(response: &Value, tracks: &mut Vec<Track>) {
    let mut items: Vec<&Value> = Vec::new();
    for shelf in parse::find_renderers(response, "musicPlaylistShelfRenderer") {
        items.extend(shelf.items(&["contents"]));
    }
    if items.is_empty() {
        for shelf in parse::find_renderers(response, "musicShelfRenderer") {
            items.extend(shelf.items(&["contents"]));
        }
    }
    if items.is_empty() {
        for action in parse::find_renderers(response, "appendContinuationItemsAction") {
            items.extend(action.items(&["continuationItems"]));
        }
        for continuation in parse::find_renderers(response, "musicPlaylistShelfContinuation")
            .into_iter()
            .chain(parse::find_renderers(response, "musicShelfContinuation"))
        {
            items.extend(continuation.items(&["contents"]));
        }
    }
    for item in items {
        if let Some(track) = parse::list_item_track(item) {
            tracks.push(track);
        }
    }
}

fn next_continuation(response: &Value) -> Option<String> {
    for shelf in parse::find_renderers(response, "musicPlaylistShelfRenderer")
        .into_iter()
        .chain(parse::find_renderers(response, "musicShelfRenderer"))
        .chain(parse::find_renderers(
            response,
            "musicPlaylistShelfContinuation",
        ))
        .chain(parse::find_renderers(response, "musicShelfContinuation"))
    {
        if let Some(token) = parse::shelf_continuation(shelf) {
            return Some(token);
        }
    }
    parse::find_renderers(response, "continuationItemRenderer")
        .into_iter()
        .find_map(|item| {
            item.str_at(&["continuationEndpoint", "continuationCommand", "token"])
                .map(str::to_string)
        })
}

fn playlist_id_of(response: &Value) -> Option<String> {
    parse::find_renderers(response, "watchPlaylistEndpoint")
        .into_iter()
        .find_map(|endpoint| endpoint.str_at(&["playlistId"]).map(str::to_string))
        .or_else(|| {
            parse::find_renderers(response, "watchEndpoint")
                .into_iter()
                .find_map(|endpoint| endpoint.str_at(&["playlistId"]).map(str::to_string))
        })
}

fn privacy_of(response: &Value) -> Option<String> {
    let mut found = Vec::new();
    crate::nav::find_all(response, "privacy", &mut found);
    found
        .into_iter()
        .find_map(|value| value.as_str().map(str::to_string))
}
