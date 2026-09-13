#![allow(
    clippy::expect_used,
    reason = "the isolated allocation fixture requires Linux process counters and successful worker joins"
)]

use std::{
    hint::black_box,
    process::Command,
    sync::{Arc, Barrier, mpsc},
    thread,
    time::{Duration, Instant},
};

// Arbitrary nonzero fill makes each allocated page resident.
const PAGE_FILL: u8 = 0x55;
const COMPLETION: &str = "heap retention verified";

#[test]
fn freed_buffers_leave_resident_memory_while_allocating_workers_are_idle() {
    let output = Command::new(std::env::current_exe().expect("daemon test executable"))
        .args([
            "--exact",
            "heap_retention::allocation_workload",
            "--ignored",
            "--nocapture",
        ])
        .output()
        .expect("isolated daemon allocation test starts");
    assert!(
        output.status.success(),
        "isolated allocator regression failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(COMPLETION),
        "the daemon test executable did not run its allocation workload"
    );
}

#[test]
#[ignore = "invoked by the parent regression in an isolated daemon test process"]
fn allocation_workload() {
    // One live object in each size class keeps sparse worker pages alive.
    // The 32 classes span small collection and buffer allocations.
    let workload = SparseWorkerBuffers {
        workers: 32,
        sizes: &[
            32, 48, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 640, 768, 896,
            1024, 1280, 1536, 1792, 2048, 2560, 3072, 3584, 4096, 5120, 6144, 7168, 8192, 16384,
        ],
        bytes_per_class: 128 * 1024,
    };
    // Reclaim at least 96 MiB from the roughly 128 MiB burst; stacks,
    // live objects, slab fragmentation and allocator metadata retain the rest.
    let minimum_released_kib = 96 * 1024;
    // Allow background decay to run on a loaded test host, without forcing it.
    let deadline = Duration::from_secs(30);
    let resident = workload.observe_reclamation(minimum_released_kib, deadline);
    assert!(
        resident.peak_kib.saturating_sub(resident.after_kib) >= minimum_released_kib,
        "temporary buffers stayed resident: {resident:?}"
    );
    println!("{COMPLETION}: {resident:?}");
}

struct SparseWorkerBuffers {
    workers: usize,
    sizes: &'static [usize],
    bytes_per_class: usize,
}

#[derive(Debug)]
struct ResidentObservation {
    peak_kib: usize,
    after_kib: usize,
}

impl SparseWorkerBuffers {
    fn observe_reclamation(
        self,
        minimum_released_kib: usize,
        deadline: Duration,
    ) -> ResidentObservation {
        let release_workers = Arc::new(Barrier::new(self.workers + 1));
        let (send, receive) = mpsc::channel();
        let handles: Vec<_> = (0..self.workers)
            .map(|_| {
                let release_workers = Arc::clone(&release_workers);
                let send = send.clone();
                thread::spawn(move || {
                    let mut buffers = Vec::new();
                    let mut retained = Vec::new();
                    for &size in self.sizes {
                        retained.push(black_box(vec![PAGE_FILL; size]));
                        for _ in 0..self.bytes_per_class / size {
                            buffers.push(black_box(vec![PAGE_FILL; size]));
                        }
                    }
                    send.send(buffers).expect("freeing thread receives buffers");
                    drop(send);
                    release_workers.wait();
                    black_box(retained);
                })
            })
            .collect();
        drop(send);
        let buffers = receive.into_iter().collect::<Vec<_>>();
        let peak_kib = resident_kib();
        drop(buffers);
        let start = Instant::now();
        let mut after_kib = resident_kib();
        while peak_kib.saturating_sub(after_kib) < minimum_released_kib
            && start.elapsed() < deadline
        {
            // Only the freeing thread stays active; allocating workers remain idle.
            black_box((0..128).map(|_| vec![PAGE_FILL; 1024]).collect::<Vec<_>>());
            thread::sleep(Duration::from_millis(1));
            after_kib = resident_kib();
        }
        release_workers.wait();
        for handle in handles {
            handle.join().expect("allocation worker completed");
        }
        ResidentObservation {
            peak_kib,
            after_kib,
        }
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
