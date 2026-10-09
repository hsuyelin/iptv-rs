<h1 align="center">iptv-rs</h1>
<h3 align="center">The IPTV relay: parse and stream</h3>

---

<p align="center">
<img alt="iptv-rs" src="branding/banner.svg" width="560"/>
<br/>
<br/>
<a href="https://github.com/hsuyelin/iptv-rs"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-blue.svg"/></a>
<a href="https://github.com/hsuyelin/iptv-rs/stargazers"><img alt="Stars" src="https://img.shields.io/github/stars/hsuyelin/iptv-rs.svg"/></a>
<a href="https://github.com/hsuyelin/iptv-rs/commits/main"><img alt="Last Commit" src="https://img.shields.io/github/last-commit/hsuyelin/iptv-rs.svg"/></a>
<a href="http://t.me/iptvorganization"><img alt="Chat on Telegram" src="https://img.shields.io/badge/chat-telegram-26A5E4?logo=telegram&logoColor=white"/></a>
<br/>
<img alt="Rust" src="https://img.shields.io/badge/Rust-1.96+-DEA584?logo=rust&logoColor=white"/>
<img alt="Tokio" src="https://img.shields.io/badge/Tokio-async-4A6CF7"/>
<img alt="axum" src="https://img.shields.io/badge/axum-0.8-7C3AED"/>
<img alt="WebAssembly" src="https://img.shields.io/badge/WebAssembly-wasmtime-654FF0?logo=webassembly&logoColor=white"/>
<img alt="HLS" src="https://img.shields.io/badge/HLS-m3u8-E5484D"/>
<img alt="Docker" src="https://img.shields.io/badge/Docker-ready-2496ED?logo=docker&logoColor=white"/>
</p>

---

iptv-rs is a small, fast IPTV relay written in Rust. It signs upstream requests, decrypts and remuxes HLS segments, and serves standard `.m3u` and `.m3u8` endpoints to any player. It parses and streams, nothing else: there is no UI in the binary, and the WASM assets are loaded and verified at run time, never embedded.

It is one half of [iptv-vod](https://github.com/hsuyelin/iptv-vod); the other half is the web console, [iptv-web](https://github.com/hsuyelin/iptv-web).

<strong>Want to get started?</strong><br/>
Follow the deployment guide in <a href="https://github.com/hsuyelin/iptv-vod#readme">iptv-vod</a>, or <a href="#running-the-relay">run it from source</a>.<br/>

<strong>Something not working right?</strong><br/>
Open an <a href="https://github.com/hsuyelin/iptv-rs/issues">Issue</a> on GitHub.<br/>

<strong>Want to contribute?</strong><br/>
Read <a href="#development">Development</a>, then open a pull request. Commits follow <a href="https://www.conventionalcommits.org">Conventional Commits</a>.<br/>

<strong>Questions or ideas?</strong><br/>
Join the community on <a href="http://t.me/iptvorganization">Telegram</a>.<br/>

---

## Relay Endpoints

| Route | Purpose |
|---|---|
| `GET /list.m3u` | Playlist for players |
| `GET /live/{ch}.m3u8` | Live HLS playlist |
| `GET /segment/{ch}/{id}.ts` | Remuxed MPEG-TS segment |
| `GET /channels` | Channel list as JSON |
| `GET /health` | Relay state and counters |

## Development

### Prerequisites

- Rust 1.96 or newer
- [just](https://github.com/casey/just) and [cargo-deny](https://github.com/EmbarkStudios/cargo-deny)

### Cloning the Repository

```bash
git clone https://github.com/hsuyelin/iptv-rs.git
cd iptv-rs
```

### Running the Relay

```bash
cargo run --release -p iptv-server -- \
  --channels /path/to/channels.yaml \
  --assets-dir ./assets
```

Add `--help` to list every option. Use `-v` or `-vv` for debug and trace logs, or set `RUST_LOG`. Set `IPTV_ADMIN_KEY` to choose the administrator key; otherwise one is generated and printed at start.

### Verifying Changes

```bash
just all      # fmt, clippy, test, doc, deps, names, cargo-deny
just bench    # criterion benchmarks
```

## Acknowledgements

Thanks to the community and the authors of the original relay project, whose work on the upstream protocol this code builds on. Join the discussion in the Telegram group: <http://t.me/iptvorganization>.

Thanks also to the maintainers of axum, tokio, wasmtime and the other open-source projects used here.
