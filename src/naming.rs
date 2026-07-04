// SPDX-License-Identifier: MIT

use std::path::Path;

use tracing::debug;

const MAX_FILENAME_BYTES: usize = 240;
const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

pub fn sanitize_server_filename(raw_name: &str, file_id: &str) -> String {
    let replaced: String = raw_name
        .chars()
        .map(|ch| {
            if is_forbidden(ch) || ch.is_control() {
                '_'
            } else {
                ch
            }
        })
        .collect();
    let trimmed = replaced.trim_end_matches(['.', ' ']).to_owned();
    let mut name = if trimmed.is_empty() || trimmed.chars().all(|ch| ch == '_') {
        format!("gigafile_{file_id}")
    } else {
        trimmed
    };

    name = avoid_windows_reserved_name(&name);
    truncate_utf8_filename(&name, MAX_FILENAME_BYTES)
}

pub fn log_name_diagnostics(raw_name: &str, sanitized_name: &str, final_path: &Path) {
    debug!("raw_name={raw_name:?}");
    debug!("sanitized_name={sanitized_name}");
    debug!("final_path={}", final_path.display());
}

fn is_forbidden(ch: char) -> bool {
    matches!(ch, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
}

fn avoid_windows_reserved_name(name: &str) -> String {
    let (stem, suffix) = split_first_extension(name);
    if RESERVED_NAMES
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        format!("{stem}_{suffix}")
    } else {
        name.to_owned()
    }
}

fn split_first_extension(name: &str) -> (&str, &str) {
    match name.find('.') {
        Some(index) if index > 0 => (&name[..index], &name[index..]),
        _ => (name, ""),
    }
}

fn split_last_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(index) if index > 0 => (&name[..index], &name[index..]),
        _ => (name, ""),
    }
}

fn truncate_utf8_filename(name: &str, max_bytes: usize) -> String {
    if name.len() <= max_bytes {
        return name.to_owned();
    }

    let (stem, extension) = split_last_extension(name);
    let extension_len = extension.len();
    if extension_len >= max_bytes {
        return truncate_to_boundary(name, max_bytes);
    }

    let stem_budget = max_bytes - extension_len;
    format!("{}{}", truncate_to_boundary(stem, stem_budget), extension)
}

fn truncate_to_boundary(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }

    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    input[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE_ID: &str = "0123abcd-000000example";

    #[test]
    fn sanitize_japanese_name_preserved() {
        assert_eq!(
            sanitize_server_filename("テスト資料_2026.zip", FILE_ID),
            "テスト資料_2026.zip"
        );
    }

    #[test]
    fn sanitize_chinese_name_preserved() {
        assert_eq!(
            sanitize_server_filename("测试文档.pdf", FILE_ID),
            "测试文档.pdf"
        );
    }

    #[test]
    fn sanitize_emoji_name_preserved() {
        assert_eq!(
            sanitize_server_filename("🎉report.txt", FILE_ID),
            "🎉report.txt"
        );
    }

    #[test]
    fn sanitize_windows_forbidden_characters_replaced() {
        assert_eq!(
            sanitize_server_filename("a:b*c?.txt", FILE_ID),
            "a_b_c_.txt"
        );
    }

    #[test]
    fn sanitize_path_separators_replaced() {
        assert_eq!(sanitize_server_filename("a/b\\c.txt", FILE_ID), "a_b_c.txt");
    }

    #[test]
    fn sanitize_control_characters_replaced() {
        let input: String = (0_u8..=31).map(char::from).collect();
        let expected = "gigafile_0123abcd-000000example";
        assert_eq!(sanitize_server_filename(&input, FILE_ID), expected);
    }

    #[test]
    fn sanitize_windows_reserved_names_get_suffix() {
        assert_eq!(sanitize_server_filename("CON", FILE_ID), "CON_");
        assert_eq!(sanitize_server_filename("con.txt", FILE_ID), "con_.txt");
        assert_eq!(
            sanitize_server_filename("COM1.tar.gz", FILE_ID),
            "COM1_.tar.gz"
        );
    }

    #[test]
    fn sanitize_trailing_dots_and_spaces_removed() {
        assert_eq!(sanitize_server_filename("name. ", FILE_ID), "name");
        assert_eq!(sanitize_server_filename("name...", FILE_ID), "name");
    }

    #[test]
    fn sanitize_all_illegal_or_empty_falls_back_to_file_id() {
        assert_eq!(
            sanitize_server_filename("???", FILE_ID),
            "gigafile_0123abcd-000000example"
        );
        assert_eq!(
            sanitize_server_filename("", FILE_ID),
            "gigafile_0123abcd-000000example"
        );
    }

    #[test]
    fn sanitize_overlong_name_truncates_on_utf8_boundary_and_preserves_extension() {
        let raw = format!("{}{}", "資".repeat(120), ".zip");
        let sanitized = sanitize_server_filename(&raw, FILE_ID);

        assert!(sanitized.len() <= MAX_FILENAME_BYTES);
        assert!(sanitized.ends_with(".zip"));
        assert!(sanitized.is_char_boundary(sanitized.len()));
    }

    #[test]
    fn sanitize_mixed_forbidden_and_non_ascii_preserves_non_ascii() {
        assert_eq!(
            sanitize_server_filename("テスト:資料?.zip", FILE_ID),
            "テスト_資料_.zip"
        );
    }
}
