use std::{
    path::{Path, PathBuf},
    sync::{Arc, MutexGuard},
};

use anyhow::{Context as _, Result, bail};
use reqwest::header::SET_COOKIE;
use reqwest_cookie_store::{CookieStore, CookieStoreMutex, RawCookie};
use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::context::Client;

const API_BASE: &str = "https://www.youtube.com/youtubei/v1/";
const MUSIC_API_BASE: &str = "https://music.youtube.com/youtubei/v1/";
const VISITOR_URL: &str = "https://www.youtube.com/sw.js_data";
/// Where the signed-in account's data sync id is published.
const HOME_URL: &str = "https://www.youtube.com/";
/// The url a pasted cookie is stored against. A `youtube.com` domain cookie stored here
/// also matches `www.youtube.com`.
const COOKIE_ORIGIN: &str = "https://music.youtube.com/";
/// Attributes added to each pasted cookie. A header value has none, and the store does
/// not write a cookie without an expiry to disk.
const SEEDED: &str = "Domain=youtube.com; Path=/; Secure; Max-Age=34560000";
/// Attributes for a `__Host-` cookie, which cannot have a domain.
const SEEDED_HOST_ONLY: &str = "Path=/; Secure; Max-Age=34560000";
const SAPISID: [&str; 2] = ["SAPISID", "__Secure-3PAPISID"];

pub struct YtMusic {
    pub(crate) http: reqwest::Client,
    visitor: RwLock<Option<String>>,
    solver: RwLock<Option<std::sync::Arc<crate::deobf::Solver>>>,
    player_cache: Option<PathBuf>,
    authed: Option<Authed>,
    authuser: usize,
    page_id: Option<String>,
    pub(crate) resolve_cache: crate::dedup::ResolveCache,
    pub(crate) po_tokens: crate::potoken::PoTokens,
    /// The account's data sync id, once it has been looked for. The inner `None` records a
    /// look that found nothing, so a signed-in run asks Google for it exactly once.
    data_sync: RwLock<Option<Option<String>>>,
    hl: String,
    gl: String,
}

/// The cookies of a signed-in account, the http client that sends them and stores the
/// ones Google sets in response, and the file they are saved to. Guest requests use the
/// plain client and send no cookies.
struct Authed {
    http: reqwest::Client,
    cookies: Arc<CookieStoreMutex>,
    file: Option<PathBuf>,
}

impl YtMusic {
    /// Signs in with the value of a `Cookie` request header: `name=value` pairs joined by
    /// `;`. The caller removes anything else first.
    pub fn with_cookies(cookies: impl Into<String>) -> Self {
        let store = Arc::new(CookieStoreMutex::new(seed(&cookies.into())));
        let http = reqwest::Client::builder()
            .cookie_provider(store.clone())
            .build()
            .expect("reqwest client");
        Self {
            authed: Some(Authed {
                http,
                cookies: store,
                file: None,
            }),
            ..Self::anonymous()
        }
    }

    pub fn anonymous() -> Self {
        Self {
            http: reqwest::Client::new(),
            visitor: RwLock::new(None),
            solver: RwLock::new(None),
            player_cache: None,
            authed: None,
            authuser: 0,
            page_id: None,
            resolve_cache: crate::dedup::ResolveCache::memory(),
            po_tokens: crate::potoken::PoTokens::default(),
            data_sync: RwLock::new(None),
            hl: "en".to_string(),
            gl: "US".to_string(),
        }
    }

    pub fn as_user(mut self, authuser: usize) -> Self {
        self.authuser = authuser;
        self
    }

    pub fn as_page(mut self, page_id: impl Into<String>) -> Self {
        self.page_id = Some(page_id.into());
        self
    }

    pub fn cache_resolutions(mut self, path: PathBuf) -> Self {
        self.resolve_cache = crate::dedup::ResolveCache::disk(path);
        self
    }

    pub fn cache_player(mut self, path: PathBuf) -> Self {
        self.player_cache = Some(path);
        self
    }

    /// Mints proof-of-origin tokens through `minter`, a host that has a browser to run
    /// BotGuard in. Without one the client still sends cold start tokens, which only cover
    /// the start of a stream.
    pub fn mint_po_tokens(mut self, minter: Arc<dyn crate::potoken::Minter>) -> Self {
        self.po_tokens = crate::potoken::PoTokens::new(Some(minter));
        self
    }

    /// Saves the signed-in cookies to `path`. An existing file replaces the pasted cookies,
    /// and every cookie Google sets afterwards is written back. Does nothing for a guest.
    pub fn persist_cookies(mut self, path: PathBuf) -> Self {
        let Some(authed) = self.authed.as_mut() else {
            return self;
        };
        if let Some(loaded) = load(&path) {
            *authed.store() = loaded;
        }
        authed.file = Some(path);
        self
    }

    pub async fn execute(&self, endpoint: &str, client: Client, payload: Value) -> Result<Value> {
        self.execute_with(endpoint, client, payload, true).await
    }

    pub async fn execute_with(
        &self,
        endpoint: &str,
        client: Client,
        payload: Value,
        use_auth: bool,
    ) -> Result<Value> {
        self.execute_visiting(endpoint, client, payload, use_auth, None)
            .await
    }

    pub(crate) async fn execute_visiting(
        &self,
        endpoint: &str,
        client: Client,
        payload: Value,
        use_auth: bool,
        guest: Option<&str>,
    ) -> Result<Value> {
        let authed = self.authed.as_ref().filter(|_| use_auth);
        let authenticated = authed.is_some();
        let held = match authenticated {
            true => String::new(),
            false => match guest {
                Some(guest) => guest.to_string(),
                None => self.visitor().await,
            },
        };
        let visitor = held.as_str();
        let mut body = payload;
        let context = client.context(visitor, &self.hl, &self.gl);
        body.as_object_mut()
            .context("payload must be an object")?
            .insert("context".to_string(), context);
        if client == Client::Music {
            body["isAudioOnly"] = json!(true);
        }
        let (base, origin) = match client {
            Client::Music => (MUSIC_API_BASE, "https://music.youtube.com"),
            _ => (API_BASE, "https://www.youtube.com"),
        };
        let url = format!("{base}{endpoint}?prettyPrint=false&alt=json");
        let http = authed.map_or(&self.http, |authed| &authed.http);
        let mut request = http
            .post(&url)
            .header("Accept", "*/*")
            .header("Accept-Language", "*")
            .header("Content-Type", "application/json")
            .header("Origin", origin)
            .header("User-Agent", client.user_agent())
            .header("X-Youtube-Client-Name", client.id().to_string())
            .header("X-Youtube-Client-Version", client.version())
            .json(&body);
        match authed {
            Some(authed) => {
                let authorization = authed
                    .authorization(origin)
                    .context("cookies have no SAPISID")?;
                request = request
                    .header("Authorization", authorization)
                    .header("X-Origin", origin)
                    .header("X-Goog-AuthUser", self.authuser.to_string());
                if let Some(page) = &self.page_id {
                    request = request.header("X-Goog-PageId", page);
                }
            }
            None => request = request.header("X-Goog-Visitor-Id", visitor),
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("cannot reach {endpoint}"))?;
        let status = response.status();
        if let Some(authed) = authed
            && response.headers().contains_key(SET_COOKIE)
        {
            authed.save();
        }
        let body = response
            .bytes()
            .await
            .with_context(|| format!("cannot read {endpoint} response"))?;
        log::debug!(
            "{endpoint} via {}: {status}, {} bytes",
            client.name(),
            body.len()
        );
        let Ok(value) = serde_json::from_slice::<Value>(&body) else {
            log::warn!(
                "{endpoint} non-json body: {}",
                String::from_utf8_lossy(&body[..body.len().min(600)])
            );
            bail!("{endpoint} returned non-json response with status {status}");
        };
        if let Some(error) = value.get("error") {
            log::warn!("{endpoint} error body: {error}");
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("{endpoint} failed ({status}): {message}");
        }
        if !status.is_success() {
            bail!("{endpoint} failed with status {status}");
        }
        Ok(value)
    }

    pub fn is_cookie_auth(&self) -> bool {
        self.authed.is_some()
    }

    pub fn is_authenticated(&self) -> bool {
        self.authed.is_some()
    }

    pub fn authuser(&self) -> usize {
        self.authuser
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.http
    }

    pub(crate) async fn solver(&self) -> Result<std::sync::Arc<crate::deobf::Solver>> {
        if let Some(ready) = self.solver.read().await.clone() {
            return Ok(ready);
        }
        let mut slot = self.solver.write().await;
        if let Some(ready) = slot.clone() {
            return Ok(ready);
        }
        let cache = self.player_cache.clone();
        let script = crate::deobf::fetch(&self.http, cache.as_deref()).await?;
        let id = script.id.clone();
        let started = std::time::Instant::now();
        let solver =
            tokio::task::spawn_blocking(move || crate::deobf::Solver::start(script, cache))
                .await
                .context("the deobfuscator did not start")??;
        log::debug!("deobf: player {id} ready in {:?}", started.elapsed());
        let solver = std::sync::Arc::new(solver);
        *slot = Some(solver.clone());
        Ok(solver)
    }

    pub(crate) async fn visitor(&self) -> String {
        if let Some(ready) = self.visitor.read().await.clone() {
            return ready;
        }
        let mut slot = self.visitor.write().await;
        if let Some(ready) = slot.clone() {
            return ready;
        }
        match fetch_visitor(&self.http).await {
            Ok(issued) => {
                *slot = Some(issued.clone());
                issued
            }
            Err(error) => {
                log::warn!("ytmusic: cannot fetch a visitor id, asking again next time: {error:#}");
                String::new()
            }
        }
    }

    pub(crate) async fn adopt_visitor(&self, issued: String) {
        *self.visitor.write().await = Some(issued);
    }

    /// What a proof-of-origin token for this session is bound to: the account's data sync id
    /// when the cookies name one, and the visitor id otherwise. YouTube refuses a token bound
    /// to anything else, so a signed-in run that cannot read its data sync id sends nothing
    /// rather than a token for the wrong identity.
    pub(crate) async fn session_binding(&self) -> Option<String> {
        match self.is_authenticated() {
            true => self.data_sync_id().await,
            false => Some(self.visitor().await).filter(|visitor| !visitor.is_empty()),
        }
    }

    /// The account's data sync id, read once out of the signed-in home page and kept for the
    /// life of the client. It changes only when the account does, and a client signs in once.
    async fn data_sync_id(&self) -> Option<String> {
        if let Some(ready) = self.data_sync.read().await.clone() {
            return ready;
        }
        let mut slot = self.data_sync.write().await;
        if let Some(ready) = slot.clone() {
            return ready;
        }
        let found = match fetch_data_sync_id(self).await {
            Ok(found) => Some(found),
            Err(error) => {
                log::warn!("ytmusic: cannot read the data sync id: {error:#}");
                None
            }
        };
        *slot = Some(found.clone());
        found
    }

    pub fn lang(&self) -> &str {
        &self.hl
    }

    pub fn region(&self) -> &str {
        &self.gl
    }
}

async fn fetch_visitor(http: &reqwest::Client) -> Result<String> {
    let body = http
        .get(VISITOR_URL)
        .header("User-Agent", Client::VisionOs.user_agent())
        .send()
        .await
        .context("cannot reach the visitor endpoint")?
        .text()
        .await
        .context("cannot read the visitor response")?;
    let issued = regex_lite::Regex::new(r#""(Cg[A-Za-z0-9%_+=-]{20,})""#)?
        .captures(&body)
        .and_then(|found| found.get(1))
        .map(|found| found.as_str().to_string())
        .context("the visitor response carries no visitor id")?;
    log::debug!("ytmusic: adopted a server-issued visitor id");
    Ok(issued)
}

/// Reads `DATASYNC_ID` off the signed-in home page, which is the only place Google publishes
/// it. A brand account carries it as `delegated||user`; both halves belong to the binding, so
/// the value is taken whole.
async fn fetch_data_sync_id(api: &YtMusic) -> Result<String> {
    let authed = api
        .authed
        .as_ref()
        .context("the session is not signed in")?;
    let body = authed
        .http
        .get(HOME_URL)
        .header("User-Agent", Client::Music.user_agent())
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await
        .context("cannot reach the home page")?
        .text()
        .await
        .context("cannot read the home page")?;
    regex_lite::Regex::new(r#""DATASYNC_ID"\s*:\s*"([^"]+)""#)?
        .captures(&body)
        .and_then(|found| found.get(1))
        .map(|found| found.as_str().to_string())
        .context("the home page carries no data sync id")
}

impl Authed {
    fn store(&self) -> MutexGuard<'_, CookieStore> {
        self.cookies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The `SAPISIDHASH` authorization for `origin`, computed from the SAPISID cookie in
    /// the store.
    fn authorization(&self, origin: &str) -> Option<String> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        let sapisid = sapisid(&self.store())?;
        Some(format!(
            "SAPISIDHASH {}",
            sid_hash(timestamp, &sapisid, origin)
        ))
    }

    /// Writes the store to its file, if one is set. On failure the error is logged and the
    /// cookies stay in memory.
    fn save(&self) {
        let Some(path) = &self.file else {
            return;
        };
        let mut body = Vec::new();
        if let Err(error) = cookie_store::serde::json::save(&self.store(), &mut body) {
            log::warn!("ytmusic: cannot encode the cookies: {error}");
            return;
        }
        if let Err(error) = write_private(path, &body) {
            log::warn!("ytmusic: cannot save the cookies: {error:#}");
        }
    }
}

/// Builds a store from a `Cookie` header value. Each pair becomes a secure `youtube.com`
/// cookie. A pair that does not parse is logged and skipped.
fn seed(header: &str) -> CookieStore {
    let origin = origin();
    let mut store = CookieStore::new();
    for pair in header.split(';').map(str::trim) {
        if !pair.contains('=') {
            continue;
        }
        let attributes = match pair.starts_with("__Host-") {
            true => SEEDED_HOST_ONLY,
            false => SEEDED,
        };
        let inserted = RawCookie::parse(format!("{pair}; {attributes}"))
            .map_err(anyhow::Error::from)
            .and_then(|cookie| {
                store
                    .insert_raw(&cookie, &origin)
                    .map_err(anyhow::Error::from)
            });
        if let Err(error) = inserted {
            log::warn!("ytmusic: cannot parse a pasted cookie, skipping it: {error}");
        }
    }
    store
}

fn sapisid(store: &CookieStore) -> Option<String> {
    let origin = origin();
    SAPISID.iter().find_map(|name| {
        store
            .get_request_values(&origin)
            .find(|(found, _)| found == name)
            .map(|(_, value)| value.to_string())
    })
}

fn origin() -> reqwest::Url {
    reqwest::Url::parse(COOKIE_ORIGIN).expect("a valid origin")
}

fn load(path: &Path) -> Option<CookieStore> {
    let file = std::fs::File::open(path).ok()?;
    match cookie_store::serde::json::load(std::io::BufReader::new(file)) {
        Ok(store) => Some(store),
        Err(error) => {
            log::warn!("ytmusic: cannot read {}: {error}", path.display());
            None
        }
    }
}

/// Replaces `path` with a file readable by its owner only, through a temp file and a
/// rename.
fn write_private(path: &Path, body: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("cannot create the cookie dir")?;
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temp, body).with_context(|| format!("cannot write {}", temp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot restrict {}", temp.display()))?;
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error).with_context(|| format!("cannot replace {}", path.display()));
    }
    Ok(())
}

fn sid_hash(timestamp: u64, secret: &str, origin: &str) -> String {
    use sha1::{Digest as _, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(format!("{timestamp} {secret} {origin}"));
    let hash = hasher.finalize();
    let hex: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{timestamp}_{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_a_header_value() {
        let store = seed("VISITOR_INFO1_LIVE=abc;   SAPISID=xyz/123; __Secure-3PAPISID=xyz/123  ");
        assert_eq!(sapisid(&store), Some("xyz/123".to_string()));
        let www = reqwest::Url::parse("https://www.youtube.com/youtubei/v1/browse").unwrap();
        assert_eq!(store.get_request_values(&www).count(), 3);
    }

    #[test]
    fn falls_back_to_secure_sapisid() {
        let store = seed("__Secure-3PAPISID=only/456");
        assert_eq!(sapisid(&store), Some("only/456".to_string()));
    }

    #[test]
    fn seeded_cookies_survive_a_round_trip() {
        let mut body = Vec::new();
        cookie_store::serde::json::save(&seed("SAPISID=abc; SID=def"), &mut body).unwrap();
        let store = cookie_store::serde::json::load(body.as_slice()).unwrap();
        assert_eq!(sapisid(&store), Some("abc".to_string()));
    }

    #[test]
    fn sid_hash_shape() {
        let auth = format!(
            "SAPISIDHASH {}",
            sid_hash(1, "abc", "https://music.youtube.com")
        );
        assert!(auth.starts_with("SAPISIDHASH "));
        assert_eq!(auth.split('_').nth(1).map(str::len), Some(40));
    }
}
