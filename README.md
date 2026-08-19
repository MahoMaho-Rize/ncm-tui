# ncm-tui

一个运行在终端里的网易云音乐客户端，使用 Rust、Ratatui 和 Rodio 编写。它把发现音乐、账号歌单、在线播放、同步歌词、本地音乐和下载管理放进同一个键盘友好的 TUI 中。

## 功能

- 网易云音乐 App 扫码登录，也支持通过 `MUSIC_U` Cookie 登录
- 每日推荐、推荐歌单和歌曲/专辑/歌手/歌单搜索
- 浏览我喜欢的音乐、创建及收藏的歌单、收藏的歌手和专辑、听歌排行
- 在线流式播放、同步歌词、播放队列、播放模式、进度与音量控制
- 启动后在后台预热常用歌单和推荐内容，减少首次打开时的等待
- 持久化播放缓存：边播边缓存，默认上限 4 GiB，重启后仍可复用，并按 LRU 自动淘汰
- 下载歌曲、专辑或歌单，支持指定范围和并发下载
- 本地音乐扫描、搜索、下载去重、收藏与最近播放记录
- 鼠标操作和紧凑终端布局

> [!NOTE]
> 本项目是非官方客户端，与网易云音乐及其关联公司无关。部分内容需要登录或有效的音乐服务权益；请遵守所在地法律法规及服务条款。

## 安装

### 一键安装

```bash
curl -fsSL https://mahomaho-rize.com/ncm-tui/install.sh | sh
```

Windows（PowerShell）：

```powershell
irm https://mahomaho-rize.com/ncm-tui/install.ps1 | iex
```

默认安装最新版本。可以指定版本和安装目录：

```bash
curl -fsSL https://mahomaho-rize.com/ncm-tui/install.sh | \
  NCM_TUI_VERSION=0.1.3 NCM_TUI_INSTALL_DIR="$HOME/.local/bin" sh
```

预编译包支持 Apple Silicon macOS、64 位 Windows，以及 x86_64/aarch64 Linux。请确保安装目录已经加入 `PATH`。

### 从源码构建

需要 Rust 2024 edition 兼容工具链和系统音频支持。

```bash
git clone https://github.com/MahoMaho-Rize/ncm-tui.git
cd ncm-tui
cargo build --release
./target/release/ncm-tui
```

Linux 上如果音频依赖无法编译，请先安装 ALSA 开发包。例如 Debian/Ubuntu：

```bash
sudo apt install pkg-config libasound2-dev
```

## 快速开始

`ncm-tui` 从当前工作目录读取 `config.toml`。仓库已经提供了一份带注释的默认配置；使用预编译版本时，可以在运行目录创建最小配置：

```toml
[download]
dir = "./downloads"
max_workers = 4
timeout = 30
api_qps = 0.0

[playback_cache]
dir = "./.ncm-cache/playback"
max_bytes = 4294967296

[library]
dirs = []
scan_before_download = false
```

随后启动：

```bash
ncm-tui
```

按 `L` 打开登录页面，用网易云音乐 App 扫码并确认。登录会话默认保存在当前目录的 `.ncm_session`，后续启动会自动恢复。你也可以在 `config.toml` 的 `[auth]` 中填写 `music_u` Cookie。

配置中的相对路径均以启动程序时的当前目录为基准。完整配置及每个选项的说明见 [`config.toml`](config.toml)。

## 常用操作

按 `?` 可以随时在程序内查看完整快捷键。常用按键如下：

| 按键 | 操作 |
| --- | --- |
| `↑` / `↓` 或 `j` / `k` | 移动选择 |
| `←` / `→` 或 `Tab` / `Shift+Tab` | 切换栏目 |
| `Enter` | 打开所选内容或播放歌曲 |
| `Esc` | 返回、收起栏目或取消输入 |
| `/` | 搜索 |
| `Space` | 播放/暂停 |
| `p` / `n` | 上一首/下一首 |
| `[` / `]` | 快退/快进 |
| `+` / `-` | 调高/调低音量 |
| `m` | 切换播放模式 |
| `l` | 聚焦或收起歌词 |
| `h` | 彻底关闭或打开歌词栏 |
| `a` / `d` | 加入/移出播放队列 |
| `f` | 收藏或取消收藏 |
| `D` | 新建下载任务 |
| `I` | 导入本地音乐 |
| `Ctrl+P` | 命令面板 |
| `L` | 登录 |
| `r` | 刷新当前内容 |
| `q` | 退出 |

在“账号与登录”页面按 `s` 可以修改播放缓存上限，按 `X` 两次可以清除缓存；正在播放的文件不会因此中断。

## 下载

按 `D` 后输入下载目标。支持单曲、专辑和歌单：

```text
track 347230
album 32311
playlist 3778678
playlist 3778678 10-20
```

最后一种写法只下载歌单中的第 10 至 20 首。下载文件默认写入 `./downloads`，成功下载的歌曲会自动进入本地音乐库。

## 数据文件

默认情况下，程序会在运行目录维护以下文件：

| 路径 | 用途 |
| --- | --- |
| `.ncm_session` | 登录会话，文件权限会设为 `0600` |
| `.ncm-cache/playback/` | 跨进程播放缓存 |
| `downloads/` | 下载的音乐文件 |
| `downloads/.ncm-tui/library.sqlite3` | 本地音乐库索引 |

播放缓存只会把完整下载的音频登记为可复用条目。容量超限时会优先清理最久未使用且当前没有播放或下载的文件。

## 开发

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

需要真实访问网易云接口的测试默认不会运行。如需手动执行：

```bash
cargo test --test live_api -- --ignored --nocapture
```

## 许可证

本项目采用 [MIT License](LICENSE)。
