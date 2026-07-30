# rgfile

[![CI](https://github.com/Maymall/gigafile-rust-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/Maymall/gigafile-rust-cli/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/rgfile.svg)](https://crates.io/crates/rgfile)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](../LICENSE)

[GigaFile.nu](https://gigafile.nu) 的命令行客户端。

[English](../README.md) | 简体中文 | [日本語](README.ja.md)

## 安装

```bash
curl -fsSL https://raw.githubusercontent.com/Maymall/gigafile-rust-cli/main/install.sh | sh   # Linux / macOS
cargo install rgfile                                                                          # 需要 Rust 1.88+
brew install Maymall/tap/rgfile                                                               # Homebrew
```

Windows：`irm https://raw.githubusercontent.com/Maymall/gigafile-rust-cli/main/install.ps1 | iex`

两个一键安装脚本都会先用 release 中的 `SHA256SUMS` 校验所选压缩包，再解压安装。
`install.sh` 支持 Linux x86_64（静态 musl）及 macOS x86_64/arm64；
`install.ps1` 支持 Windows x86_64。
[releases 页面](https://github.com/Maymall/gigafile-rust-cli/releases/latest)
另有 Linux x86_64 glibc 压缩包和 x86_64 Debian 包。其他架构可从源码构建。
从 release 安装的二进制可以用 `rgfile self-update` 自行升级。

## 用法

```bash
rgfile ul file.bin                   # 上传；输出链接、删除密钥、有效期
rgfile ul file.bin --lifetime 7      # 保存 7 天（3/5/7/14/30/60/100）

rgfile dl <url>                      # 下载；中断后重跑即续传
rgfile dl <url> --threads 8          # 多连接分段下载
rgfile dl <url> --select 1,3-5       # 从多文件页面中选择下载

rgfile info <url>                    # 只看信息，不下载
rgfile delete <url>                  # 终端中安全提示删除密钥
rgfile delete <url> --delkey-file key.txt --yes
rgfile parts list                    # 列出未完成的下载
rgfile parts clean --older-than 7    # 清理旧残留；活跃下载绝不误删

rgfile config init                   # 交互式配置
rgfile history list                  # 本地历史（默认关闭）
rgfile completions zsh               # shell 补全
```

传输、信息、历史列表、parts 列表/清理、配置显示和 delete 命令支持
`--json`；JSON 清理也必须同时使用 `--yes`。其余见 `rgfile <命令> --help`。

## 配置

可选的 TOML 文件，位于 `~/.config/rgfile/config.toml`
（macOS：`~/Library/Application Support/rgfile/`，Windows：`%APPDATA%\rgfile\`）。
命令行参数优先于配置。

```toml
[download]
dir = "/home/alice/Downloads"
threads = 8                    # 每个文件的连接数，1–16

[upload]
lifetime = 7                   # 天数：3/5/7/14/30/60/100
threads = 4                    # 预读窗口，1–16

[network]
timeout = 60                   # 每次读取的空闲秒数，1–86400
retries = 3                    # 可重试失败次数，0–20
# user_agent = "rgfile/0.10.1"

[history]
enabled = true                 # 默认关闭
store_delete_keys = false      # 明文存储，须显式开启
```

## 行为

- 下载从中断处继续，页面显示掩码文件名时同样有效；完成是原子的并校验大小。
  文件名取自 `Content-Disposition`，UTF-8 / 日文名完整保留。
- 分段下载只保持少量活跃连接，收到服务器拒绝时自动退避。
- Ctrl-C 会打印已落盘的进度和续传方法。删除密钥和下载密码不会出现在任何日志里。
- 上传流式进行、按块重试；块严格按序完成——实测服务器会丢弃乱序到达的块。
- 上传预读内存约限制在 512 MiB；块较大时会自动降低窗口或改用流式上传。
- rgfile 不绕过 GigaFile 的限制、不猜密码、不批量抓链接。

## 退出码

| 码 | 含义 |
|---:|---|
| 0 | 成功 |
| 2 | 参数错误 |
| 10 | 不是 GigaFile 链接 |
| 11 | 网络失败，重试耗尽 |
| 12 | 意外的 HTTP 状态或控制响应过大 |
| 13 | 页面无法解析 |
| 14 | 不存在或已过期 |
| 15 / 16 | 需要下载密码 / 密码错误 |
| 17 | 大小不符 |
| 18 | 文件系统错误，或目标已存在（`--force` 覆盖） |
| 19 | 上传被拒 |
| 20 | 校验失败 |
| 21 | 目标被另一个 rgfile 进程锁定 |
| 22 | 删除被拒 |
| 130 | 已中断；保留的 `.part` 重跑即续传 |

更新日志：[CHANGELOG.md](CHANGELOG.md)

历史默认关闭。开启后，记录以 JSONL 保存在系统数据目录（Linux 默认
`~/.local/share/rgfile/`）。除非显式设置 `history.store_delete_keys = true`，
删除密钥不会保存；该选项会以明文保存在本机，应按凭据保护。
`history list --json` 不会输出已保存的删除密钥。

## 协议

MIT，见 [LICENSE](../LICENSE)。

GigaFile.nu 的协议流程最初参考了
[`Sraq-Zit/gfile`](https://github.com/Sraq-Zit/gfile)
及其 fork [`fireattack/gfile`](https://github.com/fireattack/gfile)，在此致谢。
