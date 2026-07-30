// SPDX-License-Identifier: MIT

use std::{
    io,
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use regex::Regex;
use reqwest::{Client, Response, Url, redirect::Policy};
use tokio::time::sleep;
use tracing::warn;

use crate::error::{GfileError, boxed, usage};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REDIRECT_LIMIT: usize = 5;
pub const PAGE_BODY_LIMIT: u64 = 8 * 1024 * 1024;
pub const API_BODY_LIMIT: u64 = 1024 * 1024;
pub const UPDATE_ARCHIVE_LIMIT: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedirectScope {
    AnyHost,
    GigaFile,
}

pub fn default_user_agent() -> String {
    format!(
        "rgfile/{} (+https://github.com/Maymall/gigafile-rust-cli)",
        env!("CARGO_PKG_VERSION")
    )
}

pub fn build_client(user_agent: Option<&str>) -> Result<Client, GfileError> {
    build_client_with_redirect_scope(user_agent, RedirectScope::AnyHost)
}

pub(crate) fn build_gigafile_client(
    user_agent: Option<&str>,
    allow_any_host: bool,
) -> Result<Client, GfileError> {
    let scope = if allow_any_host {
        RedirectScope::AnyHost
    } else {
        RedirectScope::GigaFile
    };
    build_client_with_redirect_scope(user_agent, scope)
}

fn build_client_with_redirect_scope(
    user_agent: Option<&str>,
    redirect_scope: RedirectScope,
) -> Result<Client, GfileError> {
    let user_agent = user_agent
        .map(str::to_owned)
        .unwrap_or_else(default_user_agent);
    validate_user_agent(&user_agent)?;
    Client::builder()
        .user_agent(user_agent)
        .cookie_store(true)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(redirect_policy(redirect_scope))
        .referer(false)
        .gzip(true)
        // Live 2026-07-03: GigaFile matches header names case-sensitively and
        // ignores hyper's lowercase `range:`, answering 200 instead of 206.
        // Title-cased HTTP/1.1 headers make Range (resume + segmented
        // downloads) actually work against the real server.
        .http1_title_case_headers()
        .build()
        .map_err(|source| GfileError::Network {
            source: boxed(source),
            context: "building HTTP client".to_owned(),
        })
}

fn redirect_policy(scope: RedirectScope) -> Policy {
    Policy::custom(move |attempt| {
        if let Some(reason) = redirect_rejection(scope, attempt.url(), attempt.previous()) {
            attempt.error(reason)
        } else {
            attempt.follow()
        }
    })
}

fn redirect_rejection(scope: RedirectScope, next: &Url, previous: &[Url]) -> Option<&'static str> {
    if previous.len() > REDIRECT_LIMIT {
        return Some("too many redirects");
    }
    if previous.iter().any(|current| current == next) {
        return Some("redirect loop detected");
    }
    if !matches!(next.scheme(), "http" | "https") {
        return Some("redirect target must use HTTP or HTTPS");
    }
    if !next.username().is_empty() || next.password().is_some() {
        return Some("redirect target must not contain user information");
    }
    if previous
        .last()
        .is_some_and(|current| current.scheme() == "https" && next.scheme() != "https")
    {
        return Some("HTTPS redirects must not downgrade to HTTP");
    }
    if scope == RedirectScope::GigaFile && !is_trusted_gigafile_url(next) {
        return Some("GigaFile requests must not redirect to an untrusted host");
    }
    None
}

fn is_trusted_gigafile_url(url: &Url) -> bool {
    url.scheme() == "https"
        && !url.port().is_some_and(|port| port != 443)
        && url.host_str().is_some_and(|host| {
            host == "gigafile.nu"
                || host.strip_suffix(".gigafile.nu").is_some_and(|prefix| {
                    !prefix.is_empty() && prefix.bytes().all(|byte| byte.is_ascii_digit())
                })
        })
}

pub fn validate_user_agent(value: &str) -> Result<(), GfileError> {
    if value.trim().is_empty() || value.parse::<reqwest::header::HeaderValue>().is_err() {
        Err(usage(
            "user agent must be a non-empty HTTP header value without control characters",
        ))
    } else {
        Ok(())
    }
}

pub async fn get_with_retries(
    client: &Client,
    url: &str,
    retries: u32,
    context: &str,
) -> Result<Response, GfileError> {
    get_with_retries_and_timeout(client, url, retries, context, None).await
}

pub async fn get_with_retries_and_timeout(
    client: &Client,
    url: &str,
    retries: u32,
    context: &str,
    send_timeout: Option<Duration>,
) -> Result<Response, GfileError> {
    let mut attempt = 0;
    loop {
        match get_once(client, url, context, send_timeout).await {
            Ok(response) if is_retryable_status(response.status()) && attempt < retries => {
                let status = response.status().as_u16();
                let delay = retry_after(response.headers()).unwrap_or_else(|| retry_delay(attempt));
                drop(response);
                warn!(
                    status,
                    url = ?redact_url(url),
                    context = ?context,
                    "retrying request after retryable HTTP response"
                );
                sleep(delay).await;
                attempt += 1;
            }
            Ok(response) if !response.status().is_success() => {
                return Err(GfileError::HttpStatus {
                    status: response.status().as_u16(),
                    url_redacted: redact_url(url),
                });
            }
            Ok(response) => return Ok(response),
            Err(error) if is_retryable(&error) && attempt < retries => {
                warn!(
                    url = ?redact_url(url),
                    context = ?context,
                    error = ?error.user_message(),
                    "retrying request after retryable error"
                );
                sleep(retry_delay(attempt)).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

pub async fn get_once(
    client: &Client,
    url: &str,
    context: &str,
    send_timeout: Option<Duration>,
) -> Result<Response, GfileError> {
    let request = client.get(url).send();
    let result = match send_timeout {
        Some(timeout) => tokio::time::timeout(timeout, request)
            .await
            .map_err(|_| timeout_error(context))?,
        None => request.await,
    };

    result.map_err(|source| GfileError::Network {
        source: boxed(source),
        context: context.to_owned(),
    })
}

pub fn status_error(status: reqwest::StatusCode, url: &str) -> GfileError {
    GfileError::HttpStatus {
        status: status.as_u16(),
        url_redacted: redact_url(url),
    }
}

pub async fn read_body_limited(
    mut response: Response,
    limit: u64,
    idle_timeout: Duration,
    context: &str,
) -> Result<Vec<u8>, GfileError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err(GfileError::ResponseTooLarge {
            context: context.to_owned(),
            limit,
        });
    }

    let initial_capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0);
    let mut body = Vec::with_capacity(initial_capacity);
    loop {
        let chunk = tokio::time::timeout(idle_timeout, response.chunk())
            .await
            .map_err(|_| timeout_error(context))?
            .map_err(|source| GfileError::Network {
                source: boxed(source),
                context: context.to_owned(),
            })?;
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        let next_len = (body.len() as u64).saturating_add(chunk.len() as u64);
        if next_len > limit {
            return Err(GfileError::ResponseTooLarge {
                context: context.to_owned(),
                limit,
            });
        }
        body.extend_from_slice(&chunk);
    }
}

pub fn is_retryable(error: &GfileError) -> bool {
    match error {
        GfileError::Network { .. } => true,
        GfileError::HttpStatus { status, .. } => is_retryable_status_code(*status),
        _ => false,
    }
}

pub(crate) fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    is_retryable_status_code(status.as_u16())
}

fn is_retryable_status_code(status: u16) -> bool {
    matches!(status, 408 | 425 | 429) || (500..=599).contains(&status)
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let seconds = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())?;
    Some(Duration::from_secs(seconds.min(60)))
}

pub fn retry_delay(attempt: u32) -> Duration {
    let base = 1_u64.checked_shl(attempt).unwrap_or(4).min(4);
    Duration::from_secs(base) + Duration::from_millis(jitter_millis())
}

pub fn redact_url(input: &str) -> String {
    static SECRET_QUERY_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(^|[?&])(dlkey|delkey|delete_key)=[^&#]*")
            .expect("valid key redaction regex")
    });
    SECRET_QUERY_RE
        .replace_all(input, |caps: &regex::Captures<'_>| {
            format!(
                "{}{}=***",
                caps.get(1).map_or("", |m| m.as_str()),
                caps.get(2).map_or("", |m| m.as_str())
            )
        })
        .into_owned()
}

fn timeout_error(context: &str) -> GfileError {
    GfileError::Network {
        source: boxed(io::Error::new(io::ErrorKind::TimedOut, "request timed out")),
        context: context.to_owned(),
    }
}

fn jitter_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::from(duration.subsec_millis() % 501))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    #[test]
    fn redact_url_hides_dlkey_parameter() {
        assert_eq!(
            redact_url("https://23.gigafile.nu/download.php?file=X&dlkey=EXAMPLE-KEY-0000"),
            "https://23.gigafile.nu/download.php?file=X&dlkey=***"
        );
        assert_eq!(
            redact_url("https://23.gigafile.nu/download.php?dlkey=EXAMPLE-KEY-0000&file=X"),
            "https://23.gigafile.nu/download.php?dlkey=***&file=X"
        );
        assert_eq!(
            redact_url("https://23.gigafile.nu/download.php?file=X"),
            "https://23.gigafile.nu/download.php?file=X"
        );
        assert_eq!(
            redact_url("https://23.gigafile.nu/remove.php?file=X&delkey=EXAMPLE-DELKEY-0000"),
            "https://23.gigafile.nu/remove.php?file=X&delkey=***"
        );
    }

    #[test]
    fn retry_policy_includes_rate_limits_and_caps_retry_after() {
        assert!(is_retryable_status_code(408));
        assert!(is_retryable_status_code(429));
        assert!(is_retryable_status_code(503));
        assert!(!is_retryable_status_code(404));

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3600".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(60)));
    }

    #[test]
    fn redirect_policy_rejects_downgrades_userinfo_and_untrusted_gigafile_hosts() {
        let https_source = Url::parse("https://23.gigafile.nu/source").unwrap();
        let previous = [https_source];

        assert!(
            redirect_rejection(
                RedirectScope::GigaFile,
                &Url::parse("https://24.gigafile.nu/target").unwrap(),
                &previous,
            )
            .is_none()
        );
        assert!(
            redirect_rejection(
                RedirectScope::GigaFile,
                &Url::parse("https://gigafile.nu/").unwrap(),
                &previous,
            )
            .is_none()
        );
        for target in [
            "http://24.gigafile.nu/target",
            "https://user:secret@24.gigafile.nu/target",
            "https://example.com/target",
            "https://24.gigafile.nu:444/target",
        ] {
            assert!(
                redirect_rejection(
                    RedirectScope::GigaFile,
                    &Url::parse(target).unwrap(),
                    &previous,
                )
                .is_some(),
                "{target}"
            );
        }
    }

    #[test]
    fn generic_redirect_policy_allows_https_cross_host_redirects() {
        let previous = [Url::parse("https://github.com/releases/download").unwrap()];
        let target = Url::parse("https://objects.githubusercontent.com/asset").unwrap();

        assert!(redirect_rejection(RedirectScope::AnyHost, &target, &previous).is_none());
    }

    #[test]
    fn redirect_policy_rejects_redirect_loops() {
        let source = Url::parse("https://23.gigafile.nu/source").unwrap();
        assert_eq!(
            redirect_rejection(
                RedirectScope::GigaFile,
                &source,
                std::slice::from_ref(&source)
            ),
            Some("redirect loop detected")
        );
    }

    #[test]
    fn redirect_policy_allows_exactly_the_configured_hop_limit() {
        let previous = (0..REDIRECT_LIMIT)
            .map(|index| Url::parse(&format!("https://23.gigafile.nu/{index}")).unwrap())
            .collect::<Vec<_>>();
        let allowed = Url::parse("https://23.gigafile.nu/allowed").unwrap();
        assert!(
            redirect_rejection(RedirectScope::GigaFile, &allowed, &previous).is_none(),
            "the fifth redirect must remain allowed"
        );

        let mut over_limit = previous;
        over_limit.push(allowed);
        let rejected = Url::parse("https://23.gigafile.nu/rejected").unwrap();
        assert_eq!(
            redirect_rejection(RedirectScope::GigaFile, &rejected, &over_limit),
            Some("too many redirects")
        );
    }

    #[tokio::test]
    async fn client_does_not_forward_referer_across_redirects() {
        let target = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/target"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&target)
            .await;
        let source = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/source"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/target", target.uri())),
            )
            .mount(&source)
            .await;

        let response = build_client(None)
            .unwrap()
            .get(format!("{}/source?dlkey=EXAMPLE-KEY-0000", source.uri()))
            .send()
            .await
            .unwrap();

        assert!(response.status().is_success());
        let requests = target.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].headers.contains_key(reqwest::header::REFERER));
    }

    #[tokio::test]
    async fn gigafile_client_stops_redirect_to_untrusted_host() {
        let target = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/target"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&target)
            .await;
        let source = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/source"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/target", target.uri())),
            )
            .mount(&source)
            .await;

        let result = build_gigafile_client(None, false)
            .unwrap()
            .get(format!("{}/source", source.uri()))
            .send()
            .await;

        assert!(result.is_err());
        assert!(target.received_requests().await.unwrap().is_empty());
    }
}
