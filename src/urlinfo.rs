// SPDX-License-Identifier: MIT

use reqwest::Url;

use crate::error::GfileError;

// gfile.py@4c45392 line 239 accepts `https?://<digits>.gigafile.nu/<id>`.
// The Rust CLI follows the project spec/test plan and requires HTTPS in normal mode.
// gfile.py@4c45392 line 286 constructs downloads as `<page-origin>/download.php?file=<id>`.
const DOWNLOAD_ENDPOINT_WITH_FILE_PARAM: &str = "/download.php?file=";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlInfo {
    pub page_url: String,
    pub origin: String,
    pub file_id: String,
}

impl UrlInfo {
    pub fn download_url(&self) -> String {
        self.download_url_for(&self.file_id, None)
    }

    pub fn download_url_for(&self, file_id: &str, key: Option<&str>) -> String {
        let mut url = Url::parse(&format!("{}/download.php", self.origin))
            .expect("validated origin must form a valid download endpoint");
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("file", file_id);
            if let Some(key) = key {
                query.append_pair("dlkey", key);
            }
        }
        url.to_string()
    }

    pub fn legacy_download_url_for(&self, file_id: &str) -> String {
        format!(
            "{}{}{}",
            self.origin, DOWNLOAD_ENDPOINT_WITH_FILE_PARAM, file_id
        )
    }
}

pub fn parse_download_url(input: &str, allow_any_host: bool) -> Result<UrlInfo, GfileError> {
    let url = Url::parse(input).map_err(|_| GfileError::InvalidUrl {
        url: input.to_owned(),
    })?;

    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid(input));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid(input));
    }

    let file_id = single_path_segment(&url).ok_or_else(|| invalid(input))?;
    if !valid_file_id(file_id) {
        return Err(invalid(input));
    }

    let host = url.host_str().ok_or_else(|| invalid(input))?;
    if allow_any_host {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(invalid(input));
        }
    } else {
        if url.scheme() != "https"
            || !numeric_gigafile_host(host)
            || url.port().is_some_and(|port| port != 443)
        {
            return Err(invalid(input));
        }
    }

    Ok(UrlInfo {
        page_url: url.as_str().to_owned(),
        origin: url.origin().ascii_serialization(),
        file_id: file_id.to_owned(),
    })
}

fn valid_file_id(file_id: &str) -> bool {
    file_id
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && file_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn numeric_gigafile_host(host: &str) -> bool {
    host.strip_suffix(".gigafile.nu").is_some_and(|prefix| {
        !prefix.is_empty() && prefix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn invalid(input: &str) -> GfileError {
    GfileError::InvalidUrl {
        url: input.to_owned(),
    }
}

fn single_path_segment(url: &Url) -> Option<&str> {
    let mut segments = url.path_segments()?;
    let first = segments.next()?;
    if first.is_empty() || segments.next().is_some() {
        return None;
    }
    Some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_download_url_valid_gigafile_url_extracts_origin_and_id() {
        let info =
            parse_download_url("https://23.gigafile.nu/0123abcd-000000example", false).unwrap();

        assert_eq!(info.origin, "https://23.gigafile.nu");
        assert_eq!(info.file_id, "0123abcd-000000example");
        assert_eq!(
            info.download_url(),
            "https://23.gigafile.nu/download.php?file=0123abcd-000000example"
        );
        assert_eq!(
            info.download_url_for("0123abcd-000000example-2", Some("EXAMPLE-KEY-0000")),
            "https://23.gigafile.nu/download.php?file=0123abcd-000000example-2&dlkey=EXAMPLE-KEY-0000"
        );
    }

    #[test]
    fn parse_download_url_rejects_invalid_matrix() {
        for url in [
            "http://23.gigafile.nu/0123abcd-000000example",
            "https://example.com/0123abcd-000000example",
            "https://abc.gigafile.nu/0123abcd-000000example",
            "https://23.gigafile.nu/",
            "https://23.gigafile.nu/0123abcd-000000example?x=1",
            "https://23.gigafile.nu/0123abcd-000000example#frag",
            "https://user:secret@23.gigafile.nu/0123abcd-000000example",
            "https://23.gigafile.nu:444/0123abcd-000000example",
            "https://23.gigafile.nu/-bad",
            "not a url",
        ] {
            assert!(parse_download_url(url, false).is_err(), "{url}");
        }
    }

    #[test]
    fn parse_download_url_allows_localhost_when_test_hook_enabled() {
        let info = parse_download_url("http://127.0.0.1:8080/abc", true).unwrap();

        assert_eq!(info.origin, "http://127.0.0.1:8080");
        assert_eq!(info.file_id, "abc");
    }

    #[test]
    fn parse_download_url_preserves_bracketed_ipv6_origin_with_port() {
        let info = parse_download_url("http://[::1]:8080/abc", true).unwrap();

        assert_eq!(info.origin, "http://[::1]:8080");
        assert_eq!(
            info.download_url(),
            "http://[::1]:8080/download.php?file=abc"
        );
    }

    #[test]
    fn parse_download_url_preserves_bracketed_ipv6_origin_without_port() {
        let info = parse_download_url("https://[2001:db8::1]/abc", true).unwrap();

        assert_eq!(info.origin, "https://[2001:db8::1]");
        assert_eq!(
            info.download_url_for("abc-2", Some("KEY")),
            "https://[2001:db8::1]/download.php?file=abc-2&dlkey=KEY"
        );
    }
}
