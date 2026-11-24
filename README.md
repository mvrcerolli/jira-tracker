# jt — minimal Jira tracker

A fast terminal UI for Jira Cloud: projects on the left, issues on the right, nothing else.

## Setup

1. Create an API token: https://id.atlassian.com/manage-profile/security/api-tokens
2. Create the config (`~/.config/jt/config.toml` on Linux, `%APPDATA%\jt\config.toml` on Windows; jt creates a template on first run):

   ```toml
   base_url  = "https://your-site.atlassian.net"
   email     = "you@example.com"
   api_token = "..."            # or set JIRA_API_TOKEN
   # projects = ["ABC", "XYZ"]  # optional: only show these projects
   ```

3. Install and run:

   ```sh
   cargo install --path .
   jt
   ```

## Keys

Press `?` inside jt for the full list.

| Key | Action |
|---|---|
| `j`/`k`, arrows, `Home`/`End` | move |
| `Tab`, `h`/`l` | switch pane |
| `1`–`9` | jump to pinned project |
| `*` | pin / unpin project |
| `Enter` | show / hide issue detail |
| `J`/`K` | scroll detail |
| `o` | open issue in browser |
| `/` | text filter |
| `f` | filters (status, assignee, priority, type, JQL), saved per project |
| `t` | sort by column |
| `Space`, `Ctrl+a` | mark issue / all |
| `s` / `a` / `p` | change status / assignee / priority (marked issues or selected) |
| `z` | show / hide projects |
| `r` / `R` | refresh project / everything |
| `Ctrl+c` | quit |

Pins and filters are stored next to the config (`pinned.txt`, `filters.toml`).

## Windows build (from Linux)

```sh
rustup target add x86_64-pc-windows-gnu
cargo install cargo-zigbuild   # needs zig on PATH (e.g. `mise install zig@0.13`)
PATH="$(mise where zig@0.13):$PATH" cargo zigbuild --release --target x86_64-pc-windows-gnu
# -> target/x86_64-pc-windows-gnu/release/jt.exe
```
