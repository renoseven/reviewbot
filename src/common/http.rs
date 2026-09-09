//! Blocking HTTP with the process-wide retry policy.
//!
//! Timeout, connection reset, 5xx and 429 retry; 4xx other than 429 fail on
//! the first try. Callers map a completed exchange onto their own error type
//! and say whether that error is transient.

use std::error::Error as _;
use std::io::ErrorKind;
use std::time::{Duration, SystemTime};

use super::Backoff;

/// GitHub answers 403 to any REST call without one, whatever the token can do.
pub const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const ERROR_SNIPPET: usize = 160;

pub struct Client {
    inner: reqwest::blocking::Client,
    backoff: Backoff,
}

pub struct Reply {
    pub status: reqwest::StatusCode,
    pub body: String,
    pub headers: reqwest::header::HeaderMap,
    pub url: reqwest::Url,
}

/// What the retry loop does with one failed attempt.
pub struct Failure<E> {
    pub error: E,
    pub retry_after: Option<Duration>,
    pub transient: bool,
}

impl<E> Failure<E> {
    pub fn retry(error: E, retry_after: Option<Duration>) -> Self {
        Self {
            error,
            retry_after,
            transient: true,
        }
    }

    pub fn fail(error: E) -> Self {
        Self {
            error,
            retry_after: None,
            transient: false,
        }
    }
}

impl Client {
    pub fn new(timeout: Duration, backoff: Backoff) -> Self {
        let inner = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .expect("reqwest TLS client");
        Self { inner, backoff }
    }

    pub fn get(&self, url: reqwest::Url) -> reqwest::blocking::RequestBuilder {
        self.inner.get(url)
    }

    pub fn post(&self, url: reqwest::Url) -> reqwest::blocking::RequestBuilder {
        self.inner.post(url)
    }

    pub fn execute(
        &self,
        operation: &'static str,
        request: reqwest::blocking::RequestBuilder,
    ) -> Result<Reply, reqwest::Error> {
        let request = request.build()?;
        let url = request.url().clone();
        tracing::debug!(operation, method = %request.method(), url = %url, "http request");
        let response = self.inner.execute(request)?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.text()?;
        Ok(Reply {
            status,
            body,
            headers,
            url,
        })
    }

    /// Run `once` up to `backoff.attempts()` times. A transient failure sleeps
    /// the jittered curve (or `Retry-After`) and tries again; anything else
    /// returns on the first try.
    pub fn retry<T, E>(
        &self,
        operation: &'static str,
        mut once: impl FnMut() -> Result<T, Failure<E>>,
    ) -> Result<T, E>
    where
        E: std::fmt::Display,
    {
        let attempts = self.backoff.attempts();
        let mut last = None;
        for attempt in 0..attempts {
            match once() {
                Ok(value) => return Ok(value),
                Err(failure) if failure.transient && attempt + 1 < attempts => {
                    let delay = self.backoff.delay_now(attempt, failure.retry_after);
                    tracing::warn!(
                        operation,
                        attempt = attempt + 1,
                        attempts,
                        delay_ms = delay.as_millis() as u64,
                        reason = %failure.error,
                        "retrying request"
                    );
                    std::thread::sleep(delay);
                    last = Some(failure.error);
                }
                Err(failure) => return Err(failure.error),
            }
        }
        Err(last.expect("attempts is at least one"))
    }
}

/// Clip an already-redacted error body. Redaction stays with `security`.
pub fn snippet(text: &str) -> String {
    text.chars().take(ERROR_SNIPPET).collect()
}

pub fn transport_is_transient(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request() || is_connection_reset(error)
}

pub fn status_is_transient(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get("retry-after")?.to_str().ok()?.trim();
    if let Ok(seconds) = raw.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let when = httpdate::parse_http_date(raw).ok()?;
    Some(
        when.duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

fn is_connection_reset(error: &reqwest::Error) -> bool {
    let mut source = error.source();
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<std::io::Error>()
            && matches!(
                io.kind(),
                ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::ConnectionAborted
            )
        {
            return true;
        }
        source = err.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_429_and_5xx_are_transient_statuses() {
        assert!(status_is_transient(429));
        assert!(status_is_transient(500));
        assert!(status_is_transient(599));
        assert!(!status_is_transient(400));
        assert!(!status_is_transient(401));
        assert!(!status_is_transient(403));
        assert!(!status_is_transient(404));
        assert!(!status_is_transient(422));
        assert!(!status_is_transient(200));
    }

    #[test]
    fn retry_after_reads_delta_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "7".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(7)));
    }
}
