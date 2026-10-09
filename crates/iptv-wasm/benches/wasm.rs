#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use criterion::{criterion_group, criterion_main, Criterion};
use iptv_wasm::{AssetBundle, KeygenInput, TicketRequest};

fn wasm_hosts(c: &mut Criterion) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
    c.bench_function("assets/load_and_compile", |b| {
        b.iter(|| AssetBundle::load(&dir).unwrap());
    });
    let bundle = AssetBundle::load(&dir).unwrap();

    c.bench_function("cmg/new_runtime", |b| {
        b.iter(|| {
            bundle
                .new_cmg_runtime("https://www.yangshipin.cn/tv/home?pid=600001859")
                .unwrap()
        });
    });

    let ticket = bundle.ticket_signer();
    c.bench_function("ticket/sign", |b| {
        b.iter(|| {
            ticket
                .sign(&TicketRequest {
                    pid: "600099502",
                    auth_ts: "1778337597",
                    cnlid: "2027249301",
                    guid: "moygaemw_oj9xhxuw53",
                    app_id: "519748109",
                    app_ver: "V1.0.0",
                })
                .unwrap()
        });
    });

    let keygen = bundle.keygen_signer();
    c.bench_function("keygen/signature", |b| {
        b.iter(|| {
            keygen
                .signature_hex(KeygenInput {
                    guid: "moxfxpzd_dto2apb3j9j".to_string(),
                    token: "372067b35cb4378ef8c86aab94a23a52".to_string(),
                    app_id: "519748109".to_string(),
                    input: "a9f1e4f4aa672cbed61eec91fcade54e-moxfxpzd_dto2apb3j9j-1-r"
                        .to_string(),
                    ts: "1778337551617".to_string(),
                    version: "v1".to_string(),
                    host: "www.yangshipin.cn".to_string(),
                    protocol: "https:".to_string(),
                })
                .unwrap()
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = wasm_hosts
}
criterion_main!(benches);
