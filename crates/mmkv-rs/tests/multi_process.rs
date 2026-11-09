//! True multi-process stress: separate OS processes append to the same store.

use std::process::Command;

use mmkv_rs::{InitOptions, Mode, StoreConfig};

#[test]
fn separate_processes_append_without_loss() {
    let dir = tempfile::tempdir().unwrap();
    let worker = env!("CARGO_BIN_EXE_mmkv-mp-worker");
    let id = "multi_process_stress";
    let iterations = 250usize;
    let value_len = 200usize;

    let spawn = |prefix: &str| {
        Command::new(worker)
            .arg(dir.path())
            .arg(id)
            .arg(prefix)
            .arg(iterations.to_string())
            .arg(value_len.to_string())
            .spawn()
            .expect("spawn worker")
    };
    let mut first = spawn("alpha");
    let mut second = spawn("beta");

    let first_status = first.wait().expect("wait first");
    let second_status = second.wait().expect("wait second");
    assert!(first_status.success(), "first worker failed");
    assert!(second_status.success(), "second worker failed");

    mmkv_rs::init_with(
        mmkv_rs::default_platform(),
        InitOptions {
            root_dir: Some(dir.path()),
            log_level: mmkv_rs::Level::Error,
            ..Default::default()
        },
    )
    .expect("init");
    let store = mmkv_rs::open(id, &StoreConfig::new().mode(Mode::MULTI_PROCESS)).expect("open");
    assert_eq!(store.count(), iterations * 2);
    let expected = "V".repeat(value_len);
    for index in [0usize, 1, 17, 123, iterations - 1] {
        assert_eq!(
            store.get_str(&format!("alpha-{index:05}")).as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(
            store.get_str(&format!("beta-{index:05}")).as_deref(),
            Some(expected.as_str())
        );
    }
    assert!(mmkv_rs::is_file_valid(id, None));
}
