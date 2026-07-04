# rgfile

[![CI](https://github.com/Maymall/gigafile-rust-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/Maymall/gigafile-rust-cli/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/rgfile.svg)](https://crates.io/crates/rgfile)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](../LICENSE)

[GigaFile便](https://gigafile.nu)（ギガファイル便）のコマンドラインクライアント。

[English](../README.md) | [简体中文](README.zh.md) | 日本語

## インストール

```bash
curl -fsSL https://raw.githubusercontent.com/Maymall/gigafile-rust-cli/main/install.sh | sh   # Linux / macOS
cargo install rgfile                                                                          # Rust 1.85+
brew install Maymall/tap/rgfile                                                               # Homebrew
```

Windows：`irm https://raw.githubusercontent.com/Maymall/gigafile-rust-cli/main/install.ps1 | iex`

ビルド済みアーカイブと Debian パッケージは
[リリースページ](https://github.com/Maymall/gigafile-rust-cli/releases/latest)にあります。
リリース版バイナリは `rgfile self-update` で自己更新できます。

## 使い方

```bash
rgfile ul file.bin                   # アップロード。URL・削除キー・期限を表示
rgfile ul file.bin --lifetime 7      # 保持期間 7 日（3–100）

rgfile dl <url>                      # ダウンロード。中断しても再実行で再開
rgfile dl <url> --threads 8          # 複数コネクションの分割ダウンロード
rgfile dl <url> --select 1,3-5       # まとめてページから選んで取得

rgfile info <url>                    # ダウンロードせずにページ情報を確認
rgfile delete <url>                  # 削除キーでアップロードを取り下げ
rgfile parts list                    # 途中まで落とした .part の一覧
rgfile parts clean --older-than 7    # 古い残骸を削除。実行中のものには触れない

rgfile config init                   # 対話式で設定ファイルを作成
rgfile history list                  # ローカル履歴（デフォルト無効）
rgfile completions zsh               # シェル補完
```

すべてのコマンドで `--json` が使えます。詳細は `rgfile <コマンド> --help` へ。

## 設定

任意の TOML ファイル。場所は `~/.config/rgfile/config.toml`
（macOS：`~/Library/Application Support/rgfile/`、Windows：`%APPDATA%\rgfile\`）。
コマンドライン引数が設定より優先されます。

```toml
[download]
dir = "/home/alice/Downloads"
threads = 8                    # ファイルあたりの接続数、1–16

[upload]
lifetime = 7                   # 日数：3/5/7/14/30/60/100
threads = 4                    # 先読みウィンドウ、1–16

[history]
enabled = true                 # デフォルトは無効
store_delete_keys = false      # 平文保存のため明示的に有効化
```

## 挙動

- ダウンロードは中断地点から再開します。ページ上のファイル名がマスクされていても
  問題ありません。完了はアトミックで、サイズ検証つき。ファイル名は
  `Content-Disposition` から取得するので日本語名もそのまま残ります。
- 分割ダウンロードは同時接続を少数に保ち、サーバーに拒否されたら
  自動的に間隔を空けます。
- Ctrl-C で中断すると、どこまで保存できたかと再開方法を表示します。
  削除キーとダウンロードパスワードはログに一切出ません。
- アップロードはストリーミングでチャンクごとに再試行。チャンクの完了順は
  厳密に維持します（順不同だとサーバー側でデータが欠落することを実測で確認済み）。
- rgfile は GigaFile の制限回避・パスワード推測・リンク収集を行いません。

## 終了コード

| コード | 意味 |
|---:|---|
| 0 | 成功 |
| 2 | 引数エラー |
| 10 | GigaFile の URL ではない |
| 11 | ネットワーク失敗（リトライ上限） |
| 12 | 想定外の HTTP ステータス |
| 13 | ページを解析できない |
| 14 | 存在しない・期限切れ |
| 15 / 16 | ダウンロードキーが必要 / 不一致 |
| 17 | サイズ不一致 |
| 18 | ファイルシステムエラー、または出力先が既に存在（`--force` で上書き） |
| 19 | アップロード拒否 |
| 20 | 検証失敗 |
| 21 | 別の rgfile プロセスがロック中 |
| 22 | 削除拒否 |
| 130 | 中断。残った `.part` は再実行で再開 |

変更履歴：[CHANGELOG.md](CHANGELOG.md)

## ライセンス

MIT — [LICENSE](../LICENSE) を参照。

GigaFile.nu のプロトコルの流れは
[`Sraq-Zit/gfile`](https://github.com/Sraq-Zit/gfile) と
そのフォーク [`fireattack/gfile`](https://github.com/fireattack/gfile)
を参考にさせていただきました。
