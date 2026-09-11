//! Platform-shaped HTTP: URL joining that keeps `%2F`, page following, and
//! mapping status codes onto `PlatformError`. The retry loop itself lives in
//! `common::http`.

use std::time::Duration;

use crate::common::Backoff;
use crate::common::http::{self, Client, Failure, Reply};
use crate::security::Redactor;

use super::PlatformError;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub struct HttpClient {
    http: Client,
    base_url: String,
    host: String,
    redactor: Redactor,
}

pub struct HttpResponse {
    pub body: String,
    pub headers: reqwest::header::HeaderMap,
}

impl HttpClient {
    pub fn new(
        base_url: String,
        host: String,
        token: &str,
        backoff: Backoff,
    ) -> Result<Self, PlatformError> {
        let redactor = Redactor::with_secrets([token]).map_err(|error| PlatformError::Request {
            operation: "building a redactor",
            host: host.clone(),
            reason: error.to_string(),
        })?;
        Ok(Self {
            http: Client::new(REQUEST_TIMEOUT, backoff).map_err(|error| {
                PlatformError::Request {
                    operation: "building an HTTP client",
                    host: host.clone(),
                    reason: error.to_string(),
                }
            })?,
            base_url: base_url.trim_end_matches('/').to_string(),
            host,
            redactor,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// Join path segments onto `base_url` without collapsing `%2F`. A
    /// GitLab project path `group/sub` must travel as one segment.
    pub fn url(&self, segments: &[&str]) -> Result<reqwest::Url, PlatformError> {
        let mut url = reqwest::Url::parse(&self.base_url)
            .map_err(|error| self.request_error("building a request URL", error.to_string()))?;
        {
            let mut path = url.path_segments_mut().map_err(|_| {
                self.request_error(
                    "building a request URL",
                    "base_url cannot take path segments".to_string(),
                )
            })?;
            for segment in segments {
                path.push(segment);
            }
        }
        Ok(url)
    }

    pub fn get(&self, url: reqwest::Url) -> reqwest::blocking::RequestBuilder {
        self.http.get(url)
    }

    pub fn post(&self, url: reqwest::Url) -> reqwest::blocking::RequestBuilder {
        self.http.post(url)
    }

    pub fn head(&self, url: reqwest::Url) -> reqwest::blocking::RequestBuilder {
        self.http.head(url)
    }

    pub fn send(
        &self,
        operation: &'static str,
        build: impl Fn() -> reqwest::blocking::RequestBuilder,
    ) -> Result<HttpResponse, PlatformError> {
        self.http
            .retry(operation, || match self.http.execute(operation, build()) {
                Ok(reply) if reply.status.is_success() => Ok(HttpResponse {
                    body: reply.body,
                    headers: reply.headers,
                }),
                Ok(reply) => Err(self.status_error(operation, &reply)),
                Err(error) => Err(self.transport_error(operation, error)),
            })
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
                let mut request = self.http.get(page_url.clone());
                for (name, value) in headers {
                    request = request.header(*name, value);
                }
                request
            })?;
            let page: serde_json::Value = self.json(operation, &response.body)?;
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
                    http::snippet(&self.redactor.redact(body))
                ),
            )
        })
    }

    fn transport_error(
        &self,
        operation: &'static str,
        error: reqwest::Error,
    ) -> Failure<PlatformError> {
        let reason = self.redactor.redact(&error.to_string());
        let mapped = self.request_error(operation, reason);
        match http::transport_is_transient(&error) {
            true => Failure::retry(mapped, None),
            false => Failure::fail(mapped),
        }
    }

    fn status_error(&self, operation: &'static str, reply: &Reply) -> Failure<PlatformError> {
        let status = reply.status.as_u16();
        let said = http::snippet(&self.redactor.redact(&reply.body));
        let reason = format!("HTTP {status}: {said}");
        if status == 401 || status == 403 {
            return Failure::fail(PlatformError::Permission {
                operation,
                host: self.host.clone(),
                status,
                url: reply.url.to_string(),
                said,
            });
        }
        if status == 422 {
            return Failure::fail(PlatformError::Unprocessable {
                operation,
                host: self.host.clone(),
                reason,
            });
        }
        let mapped = self.request_error(operation, reason);
        match http::status_is_transient(status) {
            true => Failure::retry(mapped, http::retry_after(&reply.headers)),
            false => Failure::fail(mapped),
        }
    }

    fn request_error(&self, operation: &'static str, reason: String) -> PlatformError {
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
