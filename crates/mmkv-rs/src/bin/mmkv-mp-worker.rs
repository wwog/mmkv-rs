//! Multi-process stress worker. Not a user-facing binary.
//!
//! Usage: `mmkv-mp-worker <root> <store-id> <key-prefix> <iterations> <value-len>`

use std::path::PathBuf;

use mmkv_rs::{InitOptions, Mode, StoreConfig};

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("root"));
    let id = args.next().expect("store id");
    let prefix = args.next().expect("key prefix");
    let iterations: usize = args.next().expect("iterations").parse().unwrap();
    let value_len: usize = args.next().expect("value length").parse().unwrap();

    mmkv_rs::init(InitOptions {
        root_dir: Some(&root),
        log_level: mmkv_rs::Level::Error,
        ..Default::default()
    })
    .expect("init");

    let store = mmkv_rs::open(id, &StoreConfig::new().mode(Mode::MULTI_PROCESS))
        .expect("open multi-process store");
    let value = "V".repeat(value_len);

    for index in 0..iterations {
        let key = format!("{prefix}-{index:05}");
        store.put_str(&key, &value).expect("put");
        if index % 16 == 0 {
            store.sync();
        }
        // Exercise content-change detection and partial reloads.
        if index % 64 == 0 {
            store.check_content_changed();
        }
    }
    store.sync();
}
