use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use tokio::sync::RwLock;

use crate::models::{Track, TrackKind};

pub struct ResolveCache {
    entries: RwLock<HashMap<String, Option<Track>>>,
    path: Option<PathBuf>,
}

impl ResolveCache {
    pub fn memory() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            path: None,
        }
    }

    pub fn disk(path: PathBuf) -> Self {
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default();
        Self {
            entries: RwLock::new(entries),
            path: Some(path),
        }
    }

    pub async fn get(&self, video_id: &str) -> Option<Option<Track>> {
        self.entries.read().await.get(video_id).cloned()
    }

    pub async fn put(&self, video_id: String, resolved: Option<Track>) {
        self.entries.write().await.insert(video_id, resolved);
    }

    pub async fn flush(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let entries = self.entries.read().await;
        if let Ok(data) = serde_json::to_vec(&*entries) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, data);
        }
    }
}

const NOISE: &[&str] = &[
    "official video",
    "official music video",
    "official audio",
    "official lyric video",
    "official lyrics video",
    "lyric video",
    "lyrics video",
    "music video",
    "visualizer",
    "audio",
    "mv",
];

pub fn collapse(tracks: Vec<Track>) -> Vec<Track> {
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut best: HashMap<String, Track> = HashMap::new();
    for track in tracks {
        if let Some(id) = &track.video_id
            && !seen_ids.insert(id.clone())
        {
            continue;
        }
        let key = group_key(&track);
        match best.get(&key) {
            None => {
                order.push(key.clone());
                best.insert(key, track);
            }
            Some(existing) => {
                if prefers(&track, existing) {
                    best.insert(key, track);
                }
            }
        }
    }
    order
        .into_iter()
        .filter_map(|key| best.remove(&key))
        .collect()
}

pub fn search_query(track: &Track) -> String {
    let artists = track
        .artists
        .iter()
        .map(|artist| artist.name.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    format!("{} {}", normalize_title(&track.title), artists)
        .trim()
        .to_string()
}

pub fn best_song_match(video: &Track, candidates: Vec<Track>) -> Option<Track> {
    let target = group_key(video);
    candidates
        .into_iter()
        .find(|candidate| !candidate.is_video() && group_key(candidate) == target)
}

fn prefers(candidate: &Track, current: &Track) -> bool {
    rank(candidate) > rank(current)
}

fn rank(track: &Track) -> u8 {
    let song = u8::from(matches!(track.kind, TrackKind::Song));
    let album = u8::from(track.album.is_some());
    song * 2 + album
}

fn group_key(track: &Track) -> String {
    let title = normalize_title(&track.title);
    let mut artists: Vec<String> = track
        .artists
        .iter()
        .map(|artist| normalize(&artist.name))
        .filter(|name| !name.is_empty())
        .collect();
    artists.sort();
    artists.dedup();
    format!("{title}|{}", artists.join(","))
}

fn normalize_title(title: &str) -> String {
    let mut cleaned = String::with_capacity(title.len());
    let mut depth = 0u32;
    let mut segment = String::new();
    for ch in title.chars() {
        match ch {
            '(' | '[' => {
                depth += 1;
                segment.clear();
            }
            ')' | ']' => {
                depth = depth.saturating_sub(1);
                let lowered = segment.trim().to_lowercase();
                if !NOISE.iter().any(|noise| lowered.contains(noise)) && !segment.trim().is_empty()
                {
                    cleaned.push(' ');
                    cleaned.push_str(segment.trim());
                }
                segment.clear();
            }
            _ if depth > 0 => segment.push(ch),
            _ => cleaned.push(ch),
        }
    }
    normalize(&cleaned)
}

fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            space = false;
        } else if !out.is_empty() && !space {
            out.push(' ');
            space = true;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AlbumRef, ArtistRef};

    fn track(id: &str, title: &str, artist: &str, kind: TrackKind, album: bool) -> Track {
        Track {
            video_id: Some(id.to_string()),
            title: title.to_string(),
            artists: vec![ArtistRef {
                name: artist.to_string(),
                id: None,
                channel: false,
            }],
            album: album.then(|| AlbumRef {
                name: "Album".to_string(),
                id: Some("MPREb".to_string()),
            }),
            kind,
            ..Track::default()
        }
    }

    #[test]
    fn strips_video_noise_from_title() {
        assert_eq!(normalize_title("Get Lucky (Official Video)"), "get lucky");
        assert_eq!(normalize_title("Song [Official Music Video]"), "song");
        assert_eq!(normalize_title("Track (Remix)"), "track remix");
    }

    #[test]
    fn prefers_song_over_video() {
        let song = track("a", "Get Lucky", "Daft Punk", TrackKind::Song, true);
        let video = track(
            "b",
            "Get Lucky (Official Video)",
            "Daft Punk",
            TrackKind::Video,
            false,
        );
        let collapsed = collapse(vec![video, song.clone()]);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].video_id.as_deref(), Some("a"));
    }

    #[test]
    fn drops_exact_id_duplicates() {
        let a = track("x", "Song", "Artist", TrackKind::Song, true);
        let collapsed = collapse(vec![a.clone(), a.clone()]);
        assert_eq!(collapsed.len(), 1);
    }

    #[test]
    fn keeps_distinct_songs() {
        let a = track("a", "One", "Artist", TrackKind::Song, true);
        let b = track("b", "Two", "Artist", TrackKind::Song, true);
        assert_eq!(collapse(vec![a, b]).len(), 2);
    }

    #[test]
    fn keeps_remix_separate() {
        let original = track("a", "Song", "Artist", TrackKind::Song, true);
        let remix = track("b", "Song (Remix)", "Artist", TrackKind::Song, true);
        assert_eq!(collapse(vec![original, remix]).len(), 2);
    }

    #[test]
    fn matches_video_to_song() {
        let video = track(
            "v",
            "Get Lucky (Official Video)",
            "Daft Punk",
            TrackKind::Video,
            false,
        );
        let song = track("s", "Get Lucky", "Daft Punk", TrackKind::Song, true);
        let other = track("o", "Something Else", "Daft Punk", TrackKind::Song, true);
        let matched = best_song_match(&video, vec![other, song]);
        assert_eq!(matched.unwrap().video_id.as_deref(), Some("s"));
    }
}
