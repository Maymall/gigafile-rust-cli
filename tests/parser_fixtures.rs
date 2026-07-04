// SPDX-License-Identifier: MIT

use rgfile::{
    error::GfileError,
    parser::download::{
        PageKind, PageState, classify_page, parse_download_page, parse_single_file_page,
    },
    parser::landing::parse_landing_server,
};

const FILE_ID: &str = "0123abcd-000000example";

#[test]
fn parse_single_basic_fixture_extracts_file_info() {
    let page = parse_download_page(include_str!("fixtures/single_basic.html"), FILE_ID).unwrap();

    assert_eq!(page.kind, PageKind::Single);
    assert_eq!(page.files.len(), 1);
    let file = &page.files[0];
    assert_eq!(file.file_id, FILE_ID);
    assert_eq!(file.raw_name, "example file.bin");
    assert_eq!(file.display_size.as_deref(), Some("10KB"));
    assert_eq!(file.approx_bytes, Some(10 * 1024));
}

#[test]
fn parse_single_japanese_fixture_preserves_name_bytes() {
    let page = parse_download_page(include_str!("fixtures/single_japanese.html"), FILE_ID).unwrap();

    assert_eq!(
        page.files[0].raw_name.as_bytes(),
        "テスト資料_2026.zip".as_bytes()
    );
}

#[test]
fn parse_matomete_two_files_fixture_extracts_files() {
    let page =
        parse_download_page(include_str!("fixtures/matomete_two_files.html"), FILE_ID).unwrap();

    assert_eq!(page.kind, PageKind::Matomete);
    assert_eq!(page.files.len(), 2);
    assert_eq!(page.files[0].file_id, FILE_ID);
    assert_eq!(page.files[0].raw_name, "******.bin");
    assert_eq!(page.files[0].display_size.as_deref(), Some("10KB"));
    assert_eq!(page.files[1].file_id, "0123abcd-000000example-2");
    assert_eq!(page.files[1].raw_name, "******.bin");
    assert_eq!(page.files[1].approx_bytes, Some(20 * 1024));
}

#[test]
fn parse_matomete_unicode_fixture_preserves_name_bytes() {
    let page =
        parse_download_page(include_str!("fixtures/matomete_unicode.html"), FILE_ID).unwrap();

    assert_eq!(
        page.files[0].raw_name.as_bytes(),
        "テスト資料_2026.zip".as_bytes()
    );
    assert_eq!(
        page.files[1].raw_name.as_bytes(),
        "测试文档_🎉.pdf".as_bytes()
    );
}

#[test]
fn parse_matomete_empty_fixture_reports_parse_error() {
    let error = parse_download_page(include_str!("fixtures/matomete_empty.html"), FILE_ID)
        .expect_err("empty matomete fixture should fail");

    match error {
        GfileError::Parse { what, .. } => assert!(what.contains(".matomete_file"), "{what}"),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn classify_page_fixtures_report_download_states() {
    assert_eq!(
        classify_page(include_str!("fixtures/single_basic.html"), 200),
        PageState::Ok
    );
    assert_eq!(
        classify_page(include_str!("fixtures/single_disabled_key.html"), 200),
        PageState::Ok
    );
    assert_eq!(
        classify_page(include_str!("fixtures/page_needs_key.html"), 200),
        PageState::NeedsKey
    );
    assert_eq!(
        classify_page(include_str!("fixtures/page_wrong_key.html"), 200),
        PageState::WrongKey
    );
    assert_eq!(
        classify_page(include_str!("fixtures/page_notfound.html"), 404),
        PageState::NotFoundOrExpired
    );
    assert_eq!(
        classify_page(include_str!("fixtures/page_expired.html"), 200),
        PageState::NotFoundOrExpired
    );
    assert_eq!(
        classify_page(include_str!("fixtures/page_blocked.html"), 200),
        PageState::NotFoundOrExpired
    );
}

#[test]
fn parse_single_broken_fixture_reports_missing_dl() {
    let error = parse_single_file_page(include_str!("fixtures/single_broken.html"), FILE_ID)
        .expect_err("broken fixture should fail");

    match error {
        GfileError::Parse { what, .. } => assert!(what.contains("#dl"), "{what}"),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn parse_landing_fixture_extracts_upload_server() {
    let server = parse_landing_server(include_str!("fixtures/landing_server.html")).unwrap();

    assert_eq!(server, "99.gigafile.nu");
}
