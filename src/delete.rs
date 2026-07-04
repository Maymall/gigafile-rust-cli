// SPDX-License-Identifier: MIT

use std::time::Duration;

use reqwest::Url;
use serde::Deserialize;
use tracing::debug;

use crate::{
    error::GfileError,
    http,
    urlinfo::{UrlInfo, parse_download_url},
};

#[derive(Debug, Clone)]
pub struct DeleteOptions {
    pub url: String,
    pub delkey: String,
    pub timeout: Duration,
    pub retries: u32,
    pub user_agent: Option<String>,
    pub allow_any_host: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteReport {
    pub url: String,
}

#[derive(Debug, Deserialize)]
struct DeleteResponse {
    status: Option<i64>,
}

pub async fn delete(options: DeleteOptions) -> Result<DeleteReport, GfileError> {
    validate_delkey(&options.delkey)?;
    let info = parse_download_url(&options.url, options.allow_any_host)?;
    let remove_url = remove_url(&info, &options.delkey)?;
    let client = http::build_client(options.user_agent.as_deref())?;
    let response = http::get_with_retries_and_timeout(
        &client,
        &remove_url,
        options.retries,
        "deleting file",
        Some(options.timeout),
    )
    .await?;
    let parsed =
        response
            .json::<DeleteResponse>()
            .await
            .map_err(|source| GfileError::DeleteRejected {
                detail: format!("delete endpoint did not return valid JSON: {source}"),
                status: None,
            })?;
    debug!(status = ?parsed.status, "delete endpoint response");

    match parsed.status {
        Some(0) => Ok(DeleteReport { url: info.page_url }),
        Some(status) => Err(GfileError::DeleteRejected {
            detail: "delete endpoint returned a nonzero status".to_owned(),
            status: Some(status),
        }),
        None => Err(GfileError::DeleteRejected {
            detail: "delete endpoint response did not contain a status field".to_owned(),
            status: None,
        }),
    }
}

fn validate_delkey(delkey: &str) -> Result<(), GfileError> {
    if delkey.len() == 4 && delkey.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(GfileError::Usage {
            message: "delete key must be exactly 4 ASCII alphanumeric characters".to_owned(),
        })
    }
}

fn remove_url(info: &UrlInfo, delkey: &str) -> Result<String, GfileError> {
    let mut url = Url::parse(&format!("{}/remove.php", info.origin)).map_err(|source| {
        GfileError::DeleteRejected {
            detail: format!("could not build delete endpoint URL: {source}"),
            status: None,
        }
    })?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("file", &info.file_id);
        query.append_pair("delkey", delkey);
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_url_uses_same_origin_file_id_and_delkey() {
        let info =
            parse_download_url("https://23.gigafile.nu/0123abcd-000000example", false).unwrap();

        let url = remove_url(&info, "EXA1").unwrap();

        assert_eq!(
            url,
            "https://23.gigafile.nu/remove.php?file=0123abcd-000000example&delkey=EXA1"
        );
    }

    #[test]
    fn validate_delkey_rejects_invalid_shapes() {
        for delkey in ["", "abc", "abcde", "ab-c", "１２３４"] {
            assert!(validate_delkey(delkey).is_err(), "{delkey}");
        }
        validate_delkey("EXA1").unwrap();
    }
}
