#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    reason = "the isolated allocation fixture requires Linux process counters and successful worker joins"
)]

#[path = "../src/heap_allocator.rs"]
mod heap_allocator;

use std::{
    hint::black_box,
    sync::{Arc, Barrier},
    thread,
    time::Duration,
};

// Arbitrary nonzero fill makes each allocated page resident.
const PAGE_FILL: u8 = 0x55;

#[test]
fn freed_worker_buffers_do_not_stay_resident_behind_small_live_objects() {
    // Each worker frees 4 MiB while retaining 2 KiB interleaved with those buffers.
    let workload = WorkerBuffers {
        workers: 32,
        buffers_per_worker: 64,
        buffer_bytes: 64 * 1024,
        retained_bytes: 32,
    };
    let resident = workload.observe_reclamation();

    // Of the fixture's 128 MiB burst, at least 96 MiB must leave the resident set.
    // The allowance covers thread stacks, allocator metadata and continuing work.
    assert!(
        resident.peak_kib.saturating_sub(resident.after_kib) >= 96 * 1024,
        "temporary buffers stayed resident: {resident:?}"
    );
}

struct WorkerBuffers {
    workers: usize,
    buffers_per_worker: usize,
    buffer_bytes: usize,
    retained_bytes: usize,
}

#[derive(Debug)]
struct ResidentObservation {
    peak_kib: usize,
    after_kib: usize,
}

impl WorkerBuffers {
    fn observe_reclamation(self) -> ResidentObservation {
        let barrier = Arc::new(Barrier::new(self.workers + 1));
        let handles: Vec<_> = (0..self.workers)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let mut buffers = Vec::with_capacity(self.buffers_per_worker);
                    let mut retained = Vec::with_capacity(self.buffers_per_worker);
                    for _ in 0..self.buffers_per_worker {
                        buffers.push(black_box(vec![PAGE_FILL; self.buffer_bytes]));
                        retained.push(black_box(vec![PAGE_FILL; self.retained_bytes]));
                    }
                    barrier.wait();
                    barrier.wait();
                    drop(buffers);
                    continue_small_allocations();
                    barrier.wait();
                    barrier.wait();
                    black_box(retained);
                })
            })
            .collect();
        barrier.wait();
        let peak_kib = resident_kib();
        barrier.wait();
        barrier.wait();
        let after_kib = resident_kib();
        barrier.wait();
        for handle in handles {
            handle.join().expect("allocation worker completed");
        }
        ResidentObservation {
            peak_kib,
            after_kib,
        }
    }
}

fn continue_small_allocations() {
    // Keep worker heaps active through ordinary allocations, without forced collection.
    for _ in 0..2500 {
        black_box((0..128).map(|_| vec![PAGE_FILL; 1024]).collect::<Vec<_>>());
        thread::sleep(Duration::from_millis(1));
    }
}

fn resident_kib() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .expect("Linux exposes the test process status")
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .expect("process status includes resident memory")
        .split_whitespace()
        .next()
        .expect("resident memory has a value")
        .parse()
        .expect("resident memory is a KiB count")
}
