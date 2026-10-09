<p align="center">
<img alt="iptv-rs" src="branding/banner.svg" width="560"/>
<br/>
<br/>
<a href="https://github.com/hsuyelin/iptv-rs"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-blue.svg"/></a>
<a href="https://github.com/hsuyelin/iptv-rs/stargazers"><img alt="Stars" src="https://img.shields.io/github/stars/hsuyelin/iptv-rs.svg"/></a>
<a href="https://github.com/hsuyelin/iptv-rs/commits/main"><img alt="Last Commit" src="https://img.shields.io/github/last-commit/hsuyelin/iptv-rs.svg"/></a>
<a href="https://github.com/hsuyelin/iptv-rs/actions/workflows/ci.yaml"><img alt="CI" src="https://github.com/hsuyelin/iptv-rs/actions/workflows/ci.yaml/badge.svg"/></a>
<br/>
<img alt="Rust" src="https://img.shields.io/badge/Rust-1.96+-DEA584?logo=rust&logoColor=white"/>
<img alt="Docker" src="https://img.shields.io/badge/Docker-ready-2496ED?logo=docker&logoColor=white"/>
</p>

---

iptv-rs is a small, fast IPTV relay written in Rust. It signs upstream requests, decrypts and remuxes HLS segments, and serves standard `.m3u` and `.m3u8` endpoints to any player. It is one half of [iptv-vod](https://github.com/hsuyelin/iptv-vod); the other half is the web console, [iptv-web](https://github.com/hsuyelin/iptv-web).

---

## Endpoints

| Route | Purpose |
|---|---|
| `GET /list.m3u` | Playlist for players |
| `GET /live/{ch}.m3u8` | Live HLS playlist |
| `GET /segment/{ch}/{id}.ts` | Remuxed MPEG-TS segment |
| `GET /channels` | Channel list as JSON |
| `GET /health` | Relay state and counters |

## Options

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--host`, `--port` | | `127.0.0.1`, `8787` | Listen address |
| `--channels` | | `/app/channels.yaml` | Channel list, reloaded on change |
| `--assets-dir` | `IPTV_ASSETS_DIR` | `./assets` | WASM assets, SHA-256 verified at start |
| `--web-dir` | `IPTV_WEB_DIR` | off | Serve a built console from this directory |
| `-v`, `-vv` | `RUST_LOG` | `info` | Log detail |
| | `IPTV_ADMIN_KEY` | generated | Administrator key |

## Acknowledgements

Thanks to the community and the authors of the original relay project, whose work on the upstream protocol this code builds on. Join the discussion in the Telegram group: <http://t.me/iptvorganization>.

Thanks also to the maintainers of axum, tokio, wasmtime and the other open-source projects used here.
