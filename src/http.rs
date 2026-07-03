// SPDX-License-Identifier: GPL-3.0-only

use std::{
    error::Error,
    io,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use regex::Regex;
use reqwest::{Client, Response, redirect::Policy};
use tokio::time::sleep;
use tracing::warn;

use crate::error::{BoxError, GfileError};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REDIRECT_LIMIT: usize = 5;

pub fn default_user_agent() -> String {
    format!(
        "rgfile/{} (+https://github.com/Maymall/gigafile-rust-cli)",
        env!("CARGO_PKG_VERSION")
    )
}

pub fn build_client(user_agent: Option<&str>) -> Result<Client, GfileError> {
    Client::builder()
        .user_agent(user_agent.unwrap_or(&default_user_agent()))
        .cookie_store(true)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(Policy::limited(REDIRECT_LIMIT))
        .gzip(true)
        .build()
        .map_err(|source| GfileError::Network {
            source: boxed(source),
            context: "building HTTP client".to_owned(),
        })
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
            Ok(response) if response.status().is_server_error() && attempt < retries => {
                let status = response.status().as_u16();
                warn!(
                    "retrying {context} after HTTP {status} from {}",
                    redact_url(url)
                );
                sleep(retry_delay(attempt)).await;
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
                    "retrying {context} after error from {}: {}",
                    redact_url(url),
                    error
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

pub fn is_retryable(error: &GfileError) -> bool {
    match error {
        GfileError::Network { .. } => true,
        GfileError::HttpStatus { status, .. } => (500..=599).contains(status),
        _ => false,
    }
}

pub fn retry_delay(attempt: u32) -> Duration {
    let base = 1_u64.checked_shl(attempt).unwrap_or(4).min(4);
    Duration::from_secs(base) + Duration::from_millis(jitter_millis())
}

pub fn redact_url(input: &str) -> String {
    let re = Regex::new(r"(?i)(^|[?&])dlkey=[^&#]*").expect("valid dlkey redaction regex");
    re.replace_all(input, |caps: &regex::Captures<'_>| {
        format!("{}dlkey=***", caps.get(1).map_or("", |m| m.as_str()))
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

fn boxed(error: impl Error + Send + Sync + 'static) -> BoxError {
    Box::new(error)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
