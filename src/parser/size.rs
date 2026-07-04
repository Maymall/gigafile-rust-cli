// SPDX-License-Identifier: MIT

use regex::Regex;

// gfile.py@4c45392 lines 21-37 use 1024 as the display-size unit divisor.
const UNITS: &[&str] = &["B", "K", "M", "G", "T", "P", "E", "Z", "Y"];

pub fn parse_display_size(input: &str) -> Option<u64> {
    let re = Regex::new(r"(?i)^\s*(?P<num>\d+(?:\.\d+)?)\s*(?P<unit>[KMGTPEZY]?)(?:I?B)?\s*$")
        .expect("valid size regex");
    let caps = re.captures(input)?;
    let number = caps.name("num")?.as_str().parse::<f64>().ok()?;
    let unit = caps
        .name("unit")
        .map(|m| m.as_str().to_ascii_uppercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "B".to_owned());
    let exponent = UNITS.iter().position(|candidate| *candidate == unit)?;

    Some((number * 1024_u64.pow(exponent as u32) as f64).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_display_size_accepts_common_units() {
        assert_eq!(parse_display_size("1.5GB"), Some(1_610_612_736));
        assert_eq!(parse_display_size("500MB"), Some(524_288_000));
        assert_eq!(parse_display_size("3KB"), Some(3072));
        assert_eq!(parse_display_size("999B"), Some(999));
        assert_eq!(parse_display_size(" 2 mb "), Some(2_097_152));
        assert_eq!(parse_display_size("4MiB"), Some(4_194_304));
    }

    #[test]
    fn parse_display_size_rejects_invalid_input() {
        assert_eq!(parse_display_size(""), None);
        assert_eq!(parse_display_size("unknown"), None);
        assert_eq!(parse_display_size("1.2.3MB"), None);
    }
}
