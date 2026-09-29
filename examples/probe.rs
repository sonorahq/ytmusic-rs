use std::path::PathBuf;

use ytmusic::YtMusic;

fn player_cache() -> PathBuf {
    std::env::var_os("YTMUSIC_PLAYER_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/ytmusic-player.json"))
}

fn client() -> YtMusic {
    session().cache_player(player_cache())
}

fn session() -> YtMusic {
    if let Some(path) = std::env::var_os("YTMUSIC_COOKIES")
        && let Ok(cookies) = std::fs::read_to_string(path)
    {
        eprintln!("probe: using cookie auth");
        return YtMusic::with_cookies(cookies.trim());
    }
    eprintln!("probe: anonymous session");
    YtMusic::anonymous()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let command = std::env::args().nth(1).unwrap_or_default();
    let argument = std::env::args().nth(2).unwrap_or_default();
    let api = client();
    match command.as_str() {
        "search" => {
            for track in api.search_songs(&argument).await? {
                println!(
                    "{:14} {:40} {:24} {:?}",
                    track.video_id.unwrap_or_default(),
                    truncate(&track.title, 40),
                    truncate(&artist_names(&track.artists), 24),
                    track.duration,
                );
            }
        }
        "searchalbums" => {
            for album in api.search_albums(&argument).await? {
                println!(
                    "{:20} {:40} {:24} {:?} {:?} playlist={:?}",
                    album.browse_id,
                    truncate(&album.title, 40),
                    truncate(&artist_names(&album.artists), 24),
                    album.kind,
                    album.year,
                    album.playlist_id,
                );
            }
        }
        "searchplaylists" => {
            for playlist in api.search_playlists(&argument).await? {
                println!(
                    "{:38} {:44} author={:?} tracks={:?}",
                    playlist.id,
                    truncate(&playlist.title, 44),
                    playlist.author,
                    playlist.track_count,
                );
            }
        }
        "album" => {
            let detail = api.album(&argument).await?;
            println!(
                "{} by {} ({:?}, {:?}) tracks={} playlist={:?}",
                detail.album.title,
                artist_names(&detail.album.artists),
                detail.album.kind,
                detail.album.year,
                detail.tracks.len(),
                detail.album.playlist_id,
            );
            for track in &detail.tracks {
                println!(
                    "  {:14} {:40} {:?}",
                    track.video_id.clone().unwrap_or_default(),
                    truncate(&track.title, 40),
                    track.duration
                );
            }
        }
        "artist" => {
            let artist = api.artist(&argument).await?;
            println!(
                "{} subs={:?} top={} albums={} singles={}",
                artist.name,
                artist.subscribers,
                artist.top_tracks.len(),
                artist.albums.len(),
                artist.singles.len(),
            );
        }
        "playlist" => {
            let detail = api.playlist(&argument).await?;
            println!(
                "{} by {:?} owned={} public={} tracks={}",
                detail.playlist.title,
                detail.playlist.author,
                detail.playlist.owned,
                detail.public,
                detail.tracks.len(),
            );
        }
        "radio" => {
            for track in api.track_radio(&argument).await? {
                println!(
                    "{:14} {}",
                    track.video_id.unwrap_or_default(),
                    truncate(&track.title, 60)
                );
            }
        }
        "stream" => {
            let mark = std::time::Instant::now();
            let (format, data) = api.load_audio(&argument).await?;
            println!(
                "itag={} codec={} bitrate={} loudness={:?} got {} bytes in {:?}",
                format.itag,
                format.codec,
                format.bitrate,
                format.loudness_db,
                data.len(),
                mark.elapsed()
            );
        }
        "open" => {
            let mark = std::time::Instant::now();
            let (format, mut stream) = api.open_audio(&argument).await?;
            let mut got = 0usize;
            let mut preroll = None;
            while let Some(chunk) = stream.chunk().await? {
                got += chunk.len();
                if preroll.is_none() && got >= 256 * 1024 {
                    preroll = Some(mark.elapsed());
                }
            }
            println!(
                "itag={} codec={} preroll in {:?}, {} of {:?} bytes in {:?}",
                format.itag,
                format.codec,
                preroll,
                got,
                stream.total(),
                mark.elapsed()
            );
        }
        "liked" => {
            for track in api.liked_songs().await? {
                println!(
                    "{:14} {}",
                    track.video_id.unwrap_or_default(),
                    truncate(&track.title, 60)
                );
            }
        }
        "albums" => {
            for album in api.library_albums().await? {
                println!("{:20} {}", album.browse_id, album.title);
            }
        }
        "playlists" => {
            for playlist in api.library_playlists().await? {
                println!(
                    "{:20} owned={:5} public={:?} {}",
                    playlist.id, playlist.owned, playlist.public, playlist.title
                );
            }
        }
        "profile" => {
            let profile = api.profile().await?;
            println!("{} ({:?})", profile.name, profile.email);
        }
        "artistdump" => {
            let response = api
                .execute(
                    "browse",
                    ytmusic::Client::Music,
                    serde_json::json!({"browseId": argument}),
                )
                .await?;
            let mut found = Vec::new();
            ytmusic::nav::find_all(&response, "musicResponsiveListItemRenderer", &mut found);
            if let Some(item) = found.first() {
                println!("{}", serde_json::to_string_pretty(item)?);
            }
        }
        "lmdump" => {
            let response = api
                .execute(
                    "browse",
                    ytmusic::Client::Music,
                    serde_json::json!({"browseId":"VLLM"}),
                )
                .await?;
            let mut found = Vec::new();
            ytmusic::nav::find_all(&response, "musicResponsiveListItemRenderer", &mut found);
            if let Some(item) = found.get(argument.parse::<usize>().unwrap_or(1)) {
                println!("{}", serde_json::to_string_pretty(item)?);
            }
        }
        "resolveall" => {
            let api = std::sync::Arc::new(client());
            let raw = api.liked_songs().await?;
            let videos_before = raw.iter().filter(|t| t.is_video()).count();
            let resolved = api.liked_songs_resolved().await?;
            let videos_after = resolved.iter().filter(|t| t.is_video()).count();
            let with_album = resolved.iter().filter(|t| t.album.is_some()).count();
            println!(
                "before: {} tracks, {} videos\nafter:  {} tracks, {} videos, {} with album",
                raw.len(),
                videos_before,
                resolved.len(),
                videos_after,
                with_album,
            );
        }
        "resolve" => {
            let raw = api.playlist("LM").await?.tracks;
            let videos: Vec<_> = raw.into_iter().filter(|t| t.is_video()).take(8).collect();
            for video in &videos {
                let resolved = api.resolve_song(video).await?;
                match resolved {
                    Some(song) => println!(
                        "VIDEO {} — {}\n  -> SONG {} — {} [{}]",
                        truncate(&video.title, 45),
                        artist_names(&video.artists),
                        truncate(&song.title, 45),
                        artist_names(&song.artists),
                        song.album.as_ref().map(|a| a.name.as_str()).unwrap_or("-"),
                    ),
                    None => println!(
                        "VIDEO {} — {}\n  -> no song match",
                        truncate(&video.title, 45),
                        artist_names(&video.artists),
                    ),
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
        "likedraw" => {
            let raw = api.playlist("LM").await?.tracks;
            let deduped = ytmusic::dedup::collapse(raw.clone());
            let videos = raw.iter().filter(|t| t.is_video()).count();
            println!(
                "raw={} deduped={} removed={} videos_in_raw={}",
                raw.len(),
                deduped.len(),
                raw.len() - deduped.len(),
                videos
            );
            for track in raw.iter().filter(|t| t.is_video()) {
                println!(
                    "  VIDEO {:14} {} — {}",
                    track.video_id.clone().unwrap_or_default(),
                    truncate(&track.title, 40),
                    artist_names(&track.artists),
                );
            }
        }
        "delete" => {
            api.delete_playlist(&argument).await?;
            println!("deleted {argument}");
        }
        "create" => {
            let id = api.create_playlist(&argument).await?;
            println!("created {id}");
        }
        "rename" => {
            let title = std::env::args().nth(3).unwrap_or_default();
            api.rename_playlist(&argument, &title).await?;
            println!("renamed {argument} to {title}");
        }
        "raw" => {
            let payload: serde_json::Value =
                serde_json::from_str(&std::env::args().nth(3).unwrap_or_else(|| "{}".into()))?;
            let client = match std::env::var("CLIENT").unwrap_or_default().as_str() {
                "tv" => ytmusic::Client::Tv,
                "visionos" => ytmusic::Client::VisionOs,
                _ => ytmusic::Client::Music,
            };
            let response = api.execute(&argument, client, payload).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
        "players" => {
            use ytmusic::Client;
            let clients = [
                ("music", Client::Music),
                ("tv", Client::Tv),
                ("visionos", Client::VisionOs),
            ];
            for (label, client) in clients {
                for auth in [true, false] {
                    let sts: u64 = std::env::var("STS")
                        .ok()
                        .and_then(|sts| sts.parse().ok())
                        .unwrap_or(0);
                    let payload = serde_json::json!({
                        "videoId": argument,
                        "contentCheckOk": true,
                        "racyCheckOk": true,
                        "playbackContext": {
                            "contentPlaybackContext": {
                                "html5Preference": "HTML5_PREF_WANTS",
                                "signatureTimestamp": sts,
                            }
                        },
                    });
                    let outcome = api
                        .execute_with("player", client, payload, auth)
                        .await
                        .map(|response| {
                            let status = response
                                .pointer("/playabilityStatus/status")
                                .and_then(|value| value.as_str())
                                .unwrap_or("NONE")
                                .to_string();
                            let reason = response
                                .pointer("/playabilityStatus/reason")
                                .and_then(|value| value.as_str())
                                .unwrap_or("")
                                .to_string();
                            let formats = response
                                .pointer("/streamingData/adaptiveFormats")
                                .and_then(|value| value.as_array().cloned())
                                .unwrap_or_default();
                            let audio: Vec<_> = formats
                                .iter()
                                .filter(|format| {
                                    format
                                        .get("mimeType")
                                        .and_then(|mime| mime.as_str())
                                        .is_some_and(|mime| mime.starts_with("audio/"))
                                })
                                .collect();
                            let direct = audio
                                .iter()
                                .filter(|format| format.get("url").is_some())
                                .count();
                            format!(
                                "{status:16} audio={:<3} direct={direct:<3} {reason}",
                                audio.len()
                            )
                        })
                        .unwrap_or_else(|error| format!("failed: {error:#}"));
                    println!("{label:14} auth={auth:<6} {outcome}");
                }
            }
        }
        "suite" => run_suite(&api).await,
        other => {
            eprintln!("unknown command: {other}");
            eprintln!(
                "usage: probe <search|searchalbums|searchplaylists|album|artist|playlist|radio|stream|liked|albums|playlists|profile|suite> [arg]"
            );
        }
    }
    Ok(())
}

async fn run_suite(api: &YtMusic) {
    let mut results: Vec<(&str, String)> = Vec::new();
    let pause = || tokio::time::sleep(std::time::Duration::from_secs(3));

    let search = api.search_songs("daft punk get lucky").await;
    let track_id = search
        .as_ref()
        .ok()
        .and_then(|tracks| tracks.first())
        .and_then(|track| track.video_id.clone());
    results.push(("search", verdict(search.as_ref().map(Vec::len))));
    pause().await;

    let album = api.album("MPREb_K8qWMWVqXGi").await;
    results.push((
        "album",
        verdict(album.as_ref().map(|detail| detail.tracks.len())),
    ));
    pause().await;

    let artist = api.artist("UCRr1xG_2WIDs18a6cIiCxeA").await;
    results.push((
        "artist",
        verdict(artist.as_ref().map(|artist| artist.top_tracks.len())),
    ));
    pause().await;

    let radio = api.track_radio("4D7u5KF7SP8").await;
    results.push(("radio", verdict(radio.as_ref().map(Vec::len))));
    pause().await;

    match api.best_audio("4D7u5KF7SP8").await {
        Ok(format) => {
            let data = api.download(&format).await;
            results.push(("stream+download", verdict(data.as_ref().map(Vec::len))));
        }
        Err(error) => results.push(("stream+download", format!("FAIL {error:#}"))),
    }
    pause().await;

    let profile = api.profile().await;
    results.push((
        "profile",
        verdict(profile.as_ref().map(|profile| profile.name.len())),
    ));
    pause().await;

    let liked = api.liked_songs().await;
    results.push(("liked songs", verdict(liked.as_ref().map(Vec::len))));
    pause().await;

    let albums = api.library_albums().await;
    results.push(("library albums", verdict(albums.as_ref().map(Vec::len))));
    pause().await;

    let playlists = api.library_playlists().await;
    results.push((
        "library playlists",
        verdict(playlists.as_ref().map(Vec::len)),
    ));
    pause().await;

    if let Ok(playlist_id) = api.create_playlist("sonora test suite").await {
        results.push(("playlist create", format!("OK {playlist_id}")));
        pause().await;
        let rename = api
            .rename_playlist(&playlist_id, "sonora test renamed")
            .await;
        results.push(("playlist rename", verdict(rename.as_ref().map(|_| 0))));
        pause().await;
        if let Some(track_id) = &track_id {
            let add = api.add_playlist_track(&playlist_id, track_id).await;
            results.push(("playlist add track", verdict(add.as_ref().map(|_| 0))));
            pause().await;
            let detail = api.playlist(&playlist_id).await;
            let set_video_id = detail
                .as_ref()
                .ok()
                .and_then(|detail| detail.tracks.first())
                .and_then(|track| track.set_video_id.clone());
            results.push((
                "playlist read back",
                verdict(detail.as_ref().map(|detail| detail.tracks.len())),
            ));
            pause().await;
            if let Some(set_video_id) = set_video_id {
                let removed = api
                    .remove_playlist_track(&playlist_id, track_id, &set_video_id)
                    .await;
                results.push((
                    "playlist remove track",
                    verdict(removed.as_ref().map(|_| 0)),
                ));
                pause().await;
            }
            let like = api.rate_track(track_id, true).await;
            results.push(("like track", verdict(like.as_ref().map(|_| 0))));
            pause().await;
            let unlike = api.rate_track(track_id, false).await;
            results.push(("unlike track", verdict(unlike.as_ref().map(|_| 0))));
            pause().await;
        }
        let deleted = api.delete_playlist(&playlist_id).await;
        results.push(("playlist delete", verdict(deleted.as_ref().map(|_| 0))));
    } else {
        results.push(("playlist create", "FAIL".to_string()));
    }

    println!();
    for (name, result) in &results {
        println!("{name:22} {result}");
    }
    let failed = results
        .iter()
        .filter(|(_, r)| r.starts_with("FAIL"))
        .count();
    println!("\n{} checks, {} failed", results.len(), failed);
}

fn artist_names(artists: &[ytmusic::ArtistRef]) -> String {
    artists
        .iter()
        .map(|artist| artist.name.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

fn verdict<T: std::fmt::Display, E: std::fmt::Display>(result: Result<T, &E>) -> String {
    match result {
        Ok(value) => format!("OK {value}"),
        Err(error) => format!("FAIL {error}"),
    }
}

fn truncate(text: &str, max: usize) -> String {
    match text.chars().count() > max {
        true => text.chars().take(max - 1).collect::<String>() + "…",
        false => text.to_string(),
    }
}
