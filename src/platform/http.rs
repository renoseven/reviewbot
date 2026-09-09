//! Blocking HTTP for GitLab and GitHub. Same retry policy as the model
//! protocol: timeout / reset / 5xx / 429, honour `Retry-After`. 401 / 403 /
//! 400 / 422 fail on the first try. 422 is returned as a distinct error so
//! a caller can retry once as a file-level comment; that is not a network
//! retry.

use std::error::Error as _;
use std::io::ErrorKind;
use std::time::{Duration, SystemTime};

use crate::config::Backoff;
use crate::security::Redactor;

use super::PlatformError;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const ERROR_SNIPPET: usize = 160;
/// GitHub answers 403 to any REST call without one, whatever the token can do.
pub const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

pub struct HttpClient {
    client: reqwest::blocking::Client,
    base_url: String,
    host: String,
    backoff: Backoff,
    redactor: Redactor,
}

pub struct HttpResponse {
    pub body: String,
    pub headers: reqwest::header::HeaderMap,
}

struct SendFailure {
    error: PlatformError,
    retry_after: Option<Duration>,
    transient: bool,
}

impl HttpClient {
    pub fn new(base_url: String, host: String, token: &str, backoff: Backoff) -> Self {
        let mut redactor = Redactor::new();
        redactor.hide_value(token);
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .expect("reqwest TLS client");
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            host,
            backoff,
            redactor,
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// Join path segments onto `base_url` without collapsing `%2F`. A
    /// GitLab project path `group/sub` must travel as one segment.
    pub fn url(&self, segments: &[&str]) -> Result<reqwest::Url, PlatformError> {
        let mut url = reqwest::Url::parse(&self.base_url).map_err(|error| {
            self.request_error("building a request URL", error.to_string(), None)
        })?;
        {
            let mut path = url.path_segments_mut().map_err(|_| {
                self.request_error(
                    "building a request URL",
                    "base_url cannot take path segments".to_string(),
                    None,
                )
            })?;
            for segment in segments {
                path.push(segment);
            }
        }
        Ok(url)
    }

    pub fn get(&self, url: reqwest::Url) -> reqwest::blocking::RequestBuilder {
        self.client.get(url)
    }

    pub fn post(&self, url: reqwest::Url) -> reqwest::blocking::RequestBuilder {
        self.client.post(url)
    }

    pub fn send(
        &self,
        operation: &'static str,
        build: impl Fn() -> reqwest::blocking::RequestBuilder,
    ) -> Result<HttpResponse, PlatformError> {
        let attempts = self.backoff.attempts();
        let mut last = None;
        for attempt in 0..attempts {
            match self.send_once(operation, build()) {
                Ok(response) => return Ok(response),
                Err(failure) if failure.transient && attempt + 1 < attempts => {
                    let delay = match failure.retry_after {
                        Some(after) => after,
                        None => Duration::from_millis(
                            self.backoff
                                .delay_with_jitter_ms(attempt, jitter_fraction()),
                        ),
                    };
                    tracing::warn!(
                        operation,
                        attempt = attempt + 1,
                        attempts,
                        delay_ms = delay.as_millis() as u64,
                        reason = %failure.error,
                        "retrying platform request"
                    );
                    std::thread::sleep(delay);
                    last = Some(failure.error);
                }
                Err(failure) => return Err(failure.error),
            }
        }
        Err(last.unwrap_or_else(|| {
            self.request_error(operation, "retries exhausted".to_string(), None)
        }))
    }

    /// Follow `Link: rel=next` (and GitLab's `X-Next-Page`) until the
    /// collection is complete. Each page must be a JSON array.
    pub fn get_pages(
        &self,
        operation: &'static str,
        first: reqwest::Url,
        headers: &[(&str, String)],
    ) -> Result<Vec<serde_json::Value>, PlatformError> {
        let mut url = Some(first);
        let mut items = Vec::new();
        while let Some(current) = url.take() {
            let page_url = current.clone();
            let response = self.send(operation, || {
                let mut request = self.client.get(page_url.clone());
                for (name, value) in headers {
                    request = request.header(*name, value);
                }
                request
            })?;
            let page: serde_json::Value =
                serde_json::from_str(&response.body).map_err(|error| {
                    self.request_error(
                        operation,
                        format!(
                            "response is not JSON: {error}; {}",
                            snippet(&response.body, &self.redactor)
                        ),
                        None,
                    )
                })?;
            match page {
                serde_json::Value::Array(rows) => items.extend(rows),
                other => items.push(other),
            }
            url = next_page(&response.headers, &current);
        }
        Ok(items)
    }

    pub fn json<T: serde::de::DeserializeOwned>(
        &self,
        operation: &'static str,
        body: &str,
    ) -> Result<T, PlatformError> {
        serde_json::from_str(body).map_err(|error| {
            self.request_error(
                operation,
                format!(
                    "response is not JSON: {error}; {}",
                    snippet(body, &self.redactor)
                ),
                None,
            )
        })
    }

    fn send_once(
        &self,
        operation: &'static str,
        request: reqwest::blocking::RequestBuilder,
    ) -> Result<HttpResponse, SendFailure> {
        let request = request
            .build()
            .map_err(|error| self.transport_error(operation, error))?;
        let url = request.url().clone();
        tracing::debug!(operation, method = %request.method(), url = %url, "platform request");
        let response = self
            .client
            .execute(request)
            .map_err(|error| self.transport_error(operation, error))?;
        let status = response.status();
        let retry_after = retry_after_delay(response.headers());
        let headers = response.headers().clone();
        let body = response
            .text()
            .map_err(|error| self.transport_error(operation, error))?;
        let code = status.as_u16();

        if status.is_success() {
            return Ok(HttpResponse { body, headers });
        }
        Err(self.status_error(operation, code, &body, retry_after, &url))
    }

    fn transport_error(&self, operation: &'static str, error: reqwest::Error) -> SendFailure {
        let reason = self.redactor.redact(&error.to_string());
        let transient = error.is_timeout()
            || error.is_connect()
            || error.is_request()
            || is_connection_reset(&error);
        SendFailure {
            error: self.request_error(operation, reason, error.status().map(|s| s.as_u16())),
            retry_after: None,
            transient,
        }
    }

    fn status_error(
        &self,
        operation: &'static str,
        status: u16,
        body: &str,
        retry_after: Option<Duration>,
        url: &reqwest::Url,
    ) -> SendFailure {
        let said = snippet(body, &self.redactor);
        let reason = format!("HTTP {status}: {said}");
        if status == 401 || status == 403 {
            return SendFailure {
                error: PlatformError::Permission {
                    operation,
                    host: self.host.clone(),
                    status,
                    url: url.to_string(),
                    said,
                },
                retry_after: None,
                transient: false,
            };
        }
        if status == 422 {
            return SendFailure {
                error: PlatformError::Unprocessable {
                    operation,
                    host: self.host.clone(),
                    reason,
                },
                retry_after: None,
                transient: false,
            };
        }
        SendFailure {
            error: self.request_error(operation, reason, Some(status)),
            retry_after,
            transient: is_transient_status(status),
        }
    }

    fn request_error(
        &self,
        operation: &'static str,
        reason: String,
        _status: Option<u16>,
    ) -> PlatformError {
        PlatformError::Request {
            operation,
            host: self.host.clone(),
            reason,
        }
    }
}

/// Hidden HTML comments of the form `<!-- reviewbot:{run_id}:{id} -->`.
pub fn markers_in(body: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("<!-- reviewbot:") {
        let from = &rest[start..];
        match from.find("-->") {
            Some(end) => {
                found.push(from[..=end + 2].trim().to_string());
                rest = &from[end + 3..];
            }
            None => break,
        }
    }
    found
}

fn is_transient_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
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

fn retry_after_delay(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
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

fn next_page(headers: &reqwest::header::HeaderMap, current: &reqwest::Url) -> Option<reqwest::Url> {
    if let Some(link) = headers
        .get(reqwest::header::LINK)
        .and_then(|v| v.to_str().ok())
    {
        for part in link.split(',') {
            let part = part.trim();
            let is_next = part.contains("rel=\"next\"") || part.contains("rel=next");
            if !is_next {
                continue;
            }
            let start = part.find('<')?;
            let end = part.find('>')?;
            return reqwest::Url::parse(part[start + 1..end].trim()).ok();
        }
    }
    let next = headers.get("x-next-page")?.to_str().ok()?.trim();
    if next.is_empty() {
        return None;
    }
    let mut url = current.clone();
    url.query_pairs_mut().clear();
    for (key, value) in current.query_pairs() {
        if key != "page" {
            url.query_pairs_mut().append_pair(&key, &value);
        }
    }
    url.query_pairs_mut().append_pair("page", next);
    Some(url)
}

fn snippet(text: &str, redactor: &Redactor) -> String {
    let redacted = redactor.redact(text);
    redacted.chars().take(ERROR_SNIPPET).collect()
}

fn jitter_fraction() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos) / 1_000_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_pulled_out_of_a_body() {
        let body = "hello\n<!-- reviewbot:abc:trace-1 -->\nmore\n<!-- reviewbot:abc:summary -->";
        assert_eq!(
            markers_in(body),
            vec![
                "<!-- reviewbot:abc:trace-1 -->".to_string(),
                "<!-- reviewbot:abc:summary -->".to_string(),
            ]
        );
    }

    #[test]
    fn a_body_without_a_marker_is_empty() {
        assert!(markers_in("no marker here").is_empty());
    }
}
