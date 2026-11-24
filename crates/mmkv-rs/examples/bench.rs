//! Micro-benchmark for the hot paths that used to allocate per operation.
//!
//! The global allocator counts allocations so the "no extra copy" claims are
//! visible even when wall-clock time is dominated by syscalls (flock, msync).
//!
//! ```text
//! cargo run --release -p mmkv-rs --example bench
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use mmkv_core::{CryptKey, Mode, Store, StoreConfig};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn open(root: &Path, id: &str, key: Option<&CryptKey>) -> Arc<Store> {
    let platform = mmkv_rs::default_platform();
    let mut config = StoreConfig::new().mode(Mode::SINGLE_PROCESS);
    if let Some(key) = key {
        config = config.crypt_key(key, false);
    }
    Store::open_at(platform, root, id, &config).unwrap_or_else(|error| panic!("open {id}: {error}"))
}

/// Run `body` `iterations` times and report ns/op plus allocations per 1000 ops.
fn bench(name: &str, iterations: u32, mut body: impl FnMut(u32)) {
    for index in 0..iterations.min(200) {
        body(index);
    }
    // Allocation count is deterministic; take the best of three timing passes
    // to filter scheduler noise.
    let mut best = std::time::Duration::MAX;
    let mut allocations = 0;
    for pass in 0..3 {
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let start = Instant::now();
        for index in 0..iterations {
            body(index);
        }
        best = best.min(start.elapsed());
        if pass == 0 {
            allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        }
    }
    let elapsed = best;
    let per_op = elapsed.as_nanos() as f64 / iterations as f64;
    let allocs_per_k = allocations as f64 * 1000.0 / iterations as f64;
    println!(
        "{name:<30} {per_op:>8.0} ns/op {allocs_per_k:>8.0} allocs/1K ops ({allocations} total)"
    );
}

/// Allocations performed by a single one-shot operation.
fn one_shot(name: &str, body: impl FnOnce()) {
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let start = Instant::now();
    body();
    let elapsed = start.elapsed();
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    println!(
        "{name:<30} {:>8} allocs ({:.1} ms)",
        allocations,
        elapsed.as_secs_f64() * 1000.0
    );
}

fn main() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();

    // ── plaintext writes ────────────────────────────────────────────────
    let store = open(root, "bench_plain", None);
    let keys: Vec<String> = (0..20_000).map(|index| format!("key{index:06}")).collect();
    let small = "v".repeat(32);
    bench("put new key (32B)", 20_000, |index| {
        store.put_str(&keys[index as usize], &small).expect("put");
    });
    bench("put existing key (32B)", 20_000, |index| {
        store.put_str(&keys[index as usize], &small).expect("put");
    });
    bench("get_str (32B)", 50_000, |index| {
        black_box(store.get_str(&keys[index as usize % 1000]));
    });

    // ── plaintext reads of a large value ────────────────────────────────
    let large = "L".repeat(4096);
    for index in 0..500 {
        store
            .put_str(&format!("large{index:04}"), &large)
            .expect("put");
    }
    let large_keys: Vec<String> = (0..500).map(|index| format!("large{index:04}")).collect();
    bench("get_str 4KiB", 20_000, |index| {
        black_box(store.get_str(&large_keys[index as usize % 500]));
    });
    bench("with_bytes 4KiB", 20_000, |index| {
        store.with_bytes(&large_keys[index as usize % 500], |value| {
            black_box(value.map(<[u8]>::len).unwrap_or(0))
        });
    });

    // ── full write-back (compaction) ────────────────────────────────────
    let payload = "p".repeat(200);
    let compact_keys: Vec<String> = (0..1_500).map(|index| format!("c{index:05}")).collect();
    for key in &compact_keys {
        store.put_str(key, &payload).expect("put");
    }
    one_shot("plain full write-back", || store.trim());
    drop(store);

    // ── encrypted store ─────────────────────────────────────────────────
    let key = CryptKey::new(b"0123456789abcdef".to_vec());
    let crypt = open(root, "bench_crypt", Some(&key));
    for index in 0..500 {
        crypt
            .put_str(&format!("large{index:04}"), &large)
            .expect("put");
    }
    bench("crypt get_str 4KiB", 20_000, |index| {
        black_box(crypt.get_str(&large_keys[index as usize % 500]));
    });
    bench("crypt with_bytes 4KiB", 20_000, |index| {
        crypt.with_bytes(&large_keys[index as usize % 500], |value| {
            black_box(value.map(<[u8]>::len).unwrap_or(0))
        });
    });

    let medium = "M".repeat(128);
    let medium_keys: Vec<String> = (0..500).map(|index| format!("medium{index:04}")).collect();
    for key in &medium_keys {
        crypt.put_str(key, &medium).expect("put");
    }
    bench("crypt get_str 128B (cached)", 20_000, |index| {
        black_box(crypt.get_str(&medium_keys[index as usize % 500]));
    });

    // Expiration enabled: the trailer check must not decrypt the value twice.
    let expiring_plain = open(root, "bench_expire_plain", None);
    expiring_plain
        .enable_expiration(0)
        .expect("enable expiration");
    for index in 0..500 {
        expiring_plain
            .put_str_expiring(&format!("large{index:04}"), &large, 3_600)
            .expect("put");
    }
    bench("get_str 4KiB (expiring)", 20_000, |index| {
        black_box(expiring_plain.get_str(&large_keys[index as usize % 500]));
    });

    let expiring = open(root, "bench_expire", Some(&key));
    expiring.enable_expiration(0).expect("enable expiration");
    for index in 0..500 {
        expiring
            .put_str_expiring(&format!("large{index:04}"), &large, 3_600)
            .expect("put");
    }
    bench("crypt get_str 4KiB (expiring)", 20_000, |index| {
        black_box(expiring.get_str(&large_keys[index as usize % 500]));
    });

    for key in &compact_keys {
        crypt.put_str(key, &payload).expect("put");
    }
    one_shot("crypt full write-back", || crypt.trim());
}
