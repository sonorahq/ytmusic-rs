//! A track's audio read in order while it downloads, one bounded range at a time.
//!
//! The stream host serves a plain GET at a trickle and drops long connections halfway without
//! saying so, but it answers a bounded `Range` request at full speed. So the body is asked for
//! a mebibyte at a time, each read has a deadline, and a range that stalls or breaks is asked
//! for again from the byte it stopped at. A url the host refuses partway through, which is what
//! a cold start token gets after its first megabyte or so, is renewed through the client that
//! handed it out, with a minted token when that client needs one, and read on from the same
//! byte.
//!
//! A [`Locator`] names the same file to anyone who reads it with requests of their own, such as
//! a player starting a second download at a seek's target. It shares the url with the stream,
//! so a url either of them renews is the one the other asks with next.

use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use bytes::Bytes;

use crate::client::YtMusic;
use crate::context::Client;
use crate::models::AudioFormat;

/// How much one request asks for. The stream host answers a range this long at full speed.
pub const CHUNK: u64 = 1024 * 1024;
/// How long a request or a read may go without a byte before the range is asked for again.
const STALL: Duration = Duration::from_secs(8);
/// How many times in a row the same byte may be asked for before the stream gives up.
const RETRIES: u32 = 3;
/// The headers every request to the stream host carries besides the client's `User-Agent`.
const HEADERS: [(&str, &str); 3] = [
    ("Accept-Encoding", "identity"),
    ("Origin", "https://www.youtube.com"),
    ("Referer", "https://www.youtube.com/"),
];

/// The body of one audio format, handed out chunk by chunk in file order.
pub struct AudioStream {
    http: reqwest::Client,
    locator: Locator,
    /// The next byte to hand out.
    offset: u64,
    /// The last byte the open request covers.
    end: u64,
    response: Option<reqwest::Response>,
    /// How many attempts in a row have failed at `offset`.
    failures: u32,
    /// The byte the last renewal was made at. A renewed url refused at that same byte ends the
    /// stream instead of renewing forever.
    renewed_at: Option<u64>,
}

/// Where the file of one audio stream is fetched from and how, for reading it from any byte
/// with requests of one's own. Clones share the url, so a renewal through one is seen by all.
#[derive(Clone)]
pub struct Locator {
    link: Arc<Mutex<Link>>,
    total: Option<u64>,
    /// How to ask for a fresh url once the host refuses this one. `None` for a stream opened
    /// without a client, which then ends at the first refusal.
    renewal: Option<Arc<Renewal>>,
}

/// The url a file is asked for at, and the user agent of the client that handed it out, which
/// the host expects to go with it.
struct Link {
    url: String,
    agent: &'static str,
}

/// What a stream needs to ask the player for a fresh url of the same file.
struct Renewal {
    client: Weak<YtMusic>,
    video_id: String,
    itag: u32,
    /// The client that handed out the url.
    from: Client,
}

impl AudioStream {
    /// Sends the first range request and checks the stream host accepts it. A refusal here is
    /// the same answer a probe would have got, so a caller can fall back to another url.
    pub async fn open(http: reqwest::Client, format: &AudioFormat) -> Result<Self> {
        let mut stream = Self {
            http,
            locator: Locator {
                link: Arc::new(Mutex::new(Link {
                    url: format.url.clone(),
                    agent: format.user_agent,
                })),
                total: format.content_length,
                renewal: None,
            },
            offset: 0,
            end: 0,
            response: None,
            failures: 0,
            renewed_at: None,
        };
        stream.request().await?;
        Ok(stream)
    }

    /// Lets the stream renew its url through `client` when the host refuses it partway through.
    /// `from` is the client that handed the url out, which decides how a new one is asked for.
    pub(crate) fn renewing(
        mut self,
        client: &Arc<YtMusic>,
        video_id: &str,
        itag: u32,
        from: Client,
    ) -> Self {
        self.locator.renewal = Some(Arc::new(Renewal {
            client: Arc::downgrade(client),
            video_id: video_id.to_string(),
            itag,
            from,
        }));
        self
    }

    /// The length of the body, when the player response gave one.
    pub fn total(&self) -> Option<u64> {
        self.locator.total
    }

    /// Where this stream's file is fetched from, for reading it from another byte. It shares
    /// the url with this stream, renewals included.
    pub fn locator(&self) -> Locator {
        self.locator.clone()
    }

    /// The next part of the body, or `None` once all of it has been handed out. Fails when the
    /// stream host refuses a range and the url cannot be renewed, or the same byte has stalled
    /// `RETRIES` times.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        loop {
            if self.locator.total.is_some_and(|total| self.offset >= total) {
                return Ok(None);
            }
            if self.response.is_none() {
                match self.request().await {
                    Ok(()) => {}
                    Err(error) if error.is::<Refused>() => {
                        self.renew(error).await?;
                        continue;
                    }
                    Err(error) => {
                        self.retry(error)?;
                        continue;
                    }
                }
            }
            let Some(response) = self.response.as_mut() else {
                continue;
            };
            match tokio::time::timeout(STALL, response.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    self.offset += chunk.len() as u64;
                    self.failures = 0;
                    return Ok(Some(chunk));
                }
                Ok(Ok(None)) => {
                    self.response = None;
                    // Without a length, a range that ended early is the end of the file.
                    if self.locator.total.is_none() && self.offset <= self.end {
                        return Ok(None);
                    }
                }
                Ok(Err(error)) => self.retry(anyhow::Error::new(error))?,
                Err(elapsed) => self.retry(anyhow::Error::new(elapsed))?,
            }
        }
    }

    /// Asks for the next range from `offset`.
    async fn request(&mut self) -> Result<()> {
        let end = match self.locator.total {
            Some(total) => (self.offset + CHUNK - 1).min(total.saturating_sub(1)),
            None => self.offset + CHUNK - 1,
        };
        let (url, agent) = self.locator.link();
        let sent = request(&self.http, &url, agent, self.offset, end);
        let response = tokio::time::timeout(STALL, sent)
            .await
            .context("the stream host did not answer")??;
        self.end = end;
        self.response = Some(response);
        Ok(())
    }

    /// Swaps in a fresh url for the same file after the host refused this one, or hands back
    /// `refused` when there is no way to renew or a renewed url was already refused here.
    async fn renew(&mut self, refused: anyhow::Error) -> Result<()> {
        if self.locator.renewal.is_none() || self.renewed_at == Some(self.offset) {
            return Err(refused);
        }
        self.renewed_at = Some(self.offset);
        log::debug!(
            "player: {} was refused at byte {}, renewing the url",
            self.locator.video_id(),
            self.offset
        );
        self.locator
            .renew()
            .await
            .map_err(|error| refused.context(format!("cannot renew the url: {error:#}")))?;
        Ok(())
    }

    /// Drops the broken range so the next read asks again from `offset`, or gives up.
    fn retry(&mut self, error: anyhow::Error) -> Result<()> {
        self.response = None;
        self.failures += 1;
        if self.failures > RETRIES {
            return Err(error.context(format!(
                "the stream stalled at byte {} {RETRIES} times",
                self.offset
            )));
        }
        log::debug!(
            "player: the stream broke at byte {}, asking again: {error:#}",
            self.offset
        );
        Ok(())
    }
}

impl Locator {
    /// The url to ask for the file at now and the headers every request for it has to carry,
    /// the client's `User-Agent` among them. Both are read at once, so a renewal never pairs
    /// one client's url with another client's headers.
    pub fn request(&self) -> (String, Vec<(&'static str, &'static str)>) {
        let (url, agent) = self.link();
        let headers = std::iter::once(("User-Agent", agent))
            .chain(HEADERS)
            .collect();
        (url, headers)
    }

    /// The length of the file, when the player response gave one.
    pub fn total(&self) -> Option<u64> {
        self.total
    }

    /// Asks the player for a fresh url of the same file after the host refused the current one,
    /// and makes it the url every holder of this locator asks with. It fails for a stream opened
    /// without a client, once the client is gone, or when the fresh url names a file of another
    /// length.
    pub async fn renew(&self) -> Result<String> {
        let Some(renewal) = &self.renewal else {
            bail!("the stream has no way to renew its url");
        };
        let Some(client) = renewal.client.upgrade() else {
            bail!("the client that opened the stream is gone");
        };
        let format = client
            .renewed_audio(&renewal.video_id, renewal.itag, renewal.from)
            .await?;
        if format.content_length != self.total {
            bail!(
                "the renewed url for {} is {:?} bytes long, not {:?}",
                renewal.video_id,
                format.content_length,
                self.total
            );
        }
        let mut link = self.link.lock().unwrap_or_else(PoisonError::into_inner);
        link.url.clone_from(&format.url);
        link.agent = format.user_agent;
        Ok(format.url)
    }

    /// The url and user agent to ask with now.
    fn link(&self) -> (String, &'static str) {
        let link = self.link.lock().unwrap_or_else(PoisonError::into_inner);
        (link.url.clone(), link.agent)
    }

    /// The video the file belongs to, for the log.
    fn video_id(&self) -> &str {
        self.renewal
            .as_ref()
            .map_or("the stream", |renewal| renewal.video_id.as_str())
    }
}

/// The stream host answered a range with an error status. Asking again gets the same answer.
#[derive(Debug)]
pub(crate) struct Refused(reqwest::StatusCode);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stream host refused the download with status {} \
             (the proof-of-origin token it carried was not accepted)",
            self.0
        )
    }
}

impl std::error::Error for Refused {}

/// Sends one range request and hands back the response once its status says the body follows.
pub(crate) async fn request(
    http: &reqwest::Client,
    url: &str,
    agent: &'static str,
    start: u64,
    end: u64,
) -> Result<reqwest::Response> {
    let mut sent = http
        .get(url)
        .header("Range", format!("bytes={start}-{end}"))
        .header("User-Agent", agent);
    for (name, value) in HEADERS {
        sent = sent.header(name, value);
    }
    let response = sent.send().await.context("cannot reach stream host")?;
    let status = response.status();
    if !status.is_success() {
        bail!(Refused(status));
    }
    Ok(response)
}
