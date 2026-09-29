//! A track's audio read in order while it downloads, one bounded range at a time.
//!
//! The stream host serves a plain GET at a trickle and drops long connections halfway without
//! saying so, but it answers a bounded `Range` request at full speed. So the body is asked for
//! a mebibyte at a time, each read has a deadline, and a range that stalls or breaks is asked
//! for again from the byte it stopped at.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use bytes::Bytes;

use crate::models::AudioFormat;

/// How much one request asks for.
const CHUNK: u64 = 1024 * 1024;
/// How long a request or a read may go without a byte before the range is asked for again.
const STALL: Duration = Duration::from_secs(8);
/// How many times in a row the same byte may be asked for before the stream gives up.
const RETRIES: u32 = 3;

/// The body of one audio format, handed out chunk by chunk in file order.
pub struct AudioStream {
    http: reqwest::Client,
    url: String,
    agent: &'static str,
    total: Option<u64>,
    /// The next byte to hand out.
    offset: u64,
    /// The last byte the open request covers.
    end: u64,
    response: Option<reqwest::Response>,
    /// How many attempts in a row have failed at `offset`.
    failures: u32,
}

impl AudioStream {
    /// Sends the first range request and checks the stream host accepts it. A refusal here is
    /// the same answer a probe would have got, so a caller can fall back to another url.
    pub async fn open(http: reqwest::Client, format: &AudioFormat) -> Result<Self> {
        let mut stream = Self {
            http,
            url: format.url.clone(),
            agent: format.user_agent,
            total: format.content_length,
            offset: 0,
            end: 0,
            response: None,
            failures: 0,
        };
        stream.request().await?;
        Ok(stream)
    }

    /// The length of the body, when the player response gave one.
    pub fn total(&self) -> Option<u64> {
        self.total
    }

    /// The next part of the body, or `None` once all of it has been handed out. Fails only
    /// when the stream host refuses a range, or the same byte has stalled `RETRIES` times.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        loop {
            if self.total.is_some_and(|total| self.offset >= total) {
                return Ok(None);
            }
            if self.response.is_none() {
                match self.request().await {
                    Ok(()) => {}
                    Err(error) if error.is::<Refused>() => return Err(error),
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
                    if self.total.is_none() && self.offset <= self.end {
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
        let end = match self.total {
            Some(total) => (self.offset + CHUNK - 1).min(total.saturating_sub(1)),
            None => self.offset + CHUNK - 1,
        };
        let sent = request(&self.http, &self.url, self.agent, self.offset, end);
        let response = tokio::time::timeout(STALL, sent)
            .await
            .context("the stream host did not answer")??;
        self.end = end;
        self.response = Some(response);
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
    let response = http
        .get(url)
        .header("Range", format!("bytes={start}-{end}"))
        .header("User-Agent", agent)
        .header("Accept-Encoding", "identity")
        .header("Origin", "https://www.youtube.com")
        .header("Referer", "https://www.youtube.com/")
        .send()
        .await
        .context("cannot reach stream host")?;
    let status = response.status();
    if !status.is_success() {
        bail!(Refused(status));
    }
    Ok(response)
}
