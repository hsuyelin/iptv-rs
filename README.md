# iptv-rs

IPTV relay: signs upstream requests, decrypts and remuxes HLS segments, and serves
`/list.m3u`, `/live/{ch}.m3u8`, `/segment/{ch}/{id}.ts`, `/channels` and `/health`.
It serves no UI of its own; the web console lives in `iptv-web`.

Part of [iptv-vod](https://github.com/hsuyelin/iptv-vod), which holds build and deployment instructions.

```sh
cargo run --release -p iptv-server -- --channels /path/to/channels.yaml --assets-dir ./assets
just all   # fmt, clippy, test, doc, deps, names, cargo-deny
```

Crates: `iptv-media` (pure TS/HLS logic), `iptv-wasm` (WASM hosts, asset loader),
`iptv-upstream` (signing, flow control, segment pipeline), `iptv-server` (axum, `iptv-rs`
binary). `assets/` is read at run time and verified against `manifest.json`.
