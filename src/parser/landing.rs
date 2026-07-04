// SPDX-License-Identifier: MIT

use regex::Regex;

use crate::error::GfileError;

// gfile.py@4c45392 line 181 extracts the upload server with
// `var server = "..."` from https://gigafile.nu/.
const SERVER_ASSIGNMENT_RE: &str = r#"var\s+server\s*=\s*"([^"]+)""#;

pub fn parse_landing_server(html: &str) -> Result<String, GfileError> {
    let re = Regex::new(SERVER_ASSIGNMENT_RE).expect("valid upload server regex");
    let server = re
        .captures(html)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| GfileError::Parse {
            what: "missing upload server assignment".to_owned(),
            hint: "Page structure may have changed; rerun with --dump-page and -vv.".to_owned(),
        })?;

    Ok(server)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_landing_server_extracts_server_assignment() {
        let html = r#"<script>var server = "99.gigafile.nu";</script>"#;

        assert_eq!(parse_landing_server(html).unwrap(), "99.gigafile.nu");
    }

    #[test]
    fn parse_landing_server_reports_parse_error() {
        let error = parse_landing_server("<html></html>").unwrap_err();

        assert!(matches!(error, GfileError::Parse { .. }));
    }
}
