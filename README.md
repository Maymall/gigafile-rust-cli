# gfile-rust

`gfile-rust` is a Rust command line tool for automating public GigaFile web
upload and download flows. Download support is implemented for single-file and
matomete pages; upload support is still planned.

## Download Usage

Download a public file page into the current directory:

```bash
gfile download https://23.gigafile.nu/0123abcd-000000example
```

Choose an output directory or, for a single-file page, an explicit filename:

```bash
gfile download https://23.gigafile.nu/0123abcd-000000example -o ./downloads
gfile download https://23.gigafile.nu/0123abcd-000000example -o "./example file.bin"
```

Download with a key:

```bash
gfile download https://23.gigafile.nu/0123abcd-000000example --key EXAMPLE-KEY-0000
```

If a page requires a key and `--key` is not provided, an interactive terminal
will prompt once without echoing input. Non-interactive runs exit with code 15.

Resume is enabled by default when a matching `.part` and `.part.json` sidecar
exist. Use `--no-resume` to ignore partial state and start from zero.

For scripts, use `--json` to print one final JSON object to stdout and suppress
progress output:

```bash
gfile download --json https://23.gigafile.nu/0123abcd-000000example
```

Matomete pages are downloaded sequentially. If one file fails, later files are
still attempted and the final process exit code is the first failure code.

When GigaFile's page masks the displayed filename, `gfile-rust` prefers the
`Content-Disposition` filename from the actual file response, including UTF-8
`filename*=` values.

## From Python gfile

| Python gfile | gfile-rust | Notes |
|---|---|---|
| `gfile download URL` | `gfile download URL` | Same basic download shape. |
| `--key` / `--password` | `--key` / `--password` / `-k` | Password value is sent as `dlkey`. |
| output filename | `-o <PATH>` | For matomete, `-o` must be an existing directory. |
| built-in sequential download | built-in sequential download | Matomete files are intentionally not downloaded in parallel. |
| `--aria2` | not implemented | Multi-connection aria2 integration is planned only as a backlog item. |
| JSON output | `--json` | Rust version provides a stable final JSON object. |

## Security

Download keys are never written to the resume sidecar; the sidecar stores only
whether a key was used. Cookies are kept in memory and are not persisted.

Passing `--key EXAMPLE-KEY-0000` can expose the value through shell history or
process listings such as `ps`. Prefer the interactive prompt when that matters.
Do not publish `--dump-page` output without reviewing it; it may contain private
filenames or page details.

## License

This project is licensed under GPL-3.0-only. See [LICENSE](LICENSE).

## Attribution

This project is a Rust rewrite derived from and substantially informed by
`Sraq-Zit/gfile` and `fireattack/gfile`, both GPL-3.0 projects. The pinned
reference commit is `4c45392d2cc99903b38653b34e1dd07706c9c65a`.

See [NOTICE.md](NOTICE.md) for details.

## Disclaimer

This is an unofficial tool. Users are responsible for complying with GigaFile's
official terms and acceptable-use rules, including
https://gigafile.nu/privacy.php.

## Behavior Boundaries

- No password guessing, dictionary attacks, link scanning, or enumeration.
- No high-concurrency stress or load-testing mode.
- No bypass of download pages, advertising, membership restrictions, or other
  service controls.
- No persistence of cookies, passwords, tokens, or download keys to disk.
- No third-party email notification features.
- No browser impersonation for bypass purposes.
