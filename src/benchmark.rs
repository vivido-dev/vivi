//! Opt-in, device-free inspection and playback-demux allocation benchmark.
use crate::ffmpeg::{EncodedMediaPacket, VideoDemuxer};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

struct CountingAllocator;
thread_local! { static COUNT: Cell<bool> = const { Cell::new(false) }; }
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
// SAFETY: every allocation and release is delegated unchanged to the system allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNT.try_with(Cell::get).unwrap_or(false) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        // SAFETY: caller supplies the allocator contract; System receives the same layout.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: pointer/layout are passed directly back to their allocator.
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNT.try_with(Cell::get).unwrap_or(false) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(size as u64, Ordering::Relaxed);
        }
        // SAFETY: caller supplies the allocator contract; System receives identical arguments.
        unsafe { System.realloc(pointer, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
#[ignore = "opt-in benchmark: VIVI_BENCH_MEDIA selects the local fixture"]
fn inspect_and_demux() {
    let path = std::env::var_os("VIVI_BENCH_MEDIA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "../medias/under_attack.webm".into());
    for phase in ["inspection", "video-demux"] {
        for iteration in 0..6 {
            ALLOCATIONS.store(0, Ordering::Relaxed);
            BYTES.store(0, Ordering::Relaxed);
            COUNT.set(true);
            let started = Instant::now();
            let (packets, encoded_bytes) = if phase == "inspection" {
                let info = VideoDemuxer::inspect(&path).unwrap();
                assert_eq!((info.width, info.height), (640, 480));
                (0, 0)
            } else {
                let mut input = VideoDemuxer::open(&path).unwrap();
                input.skip_audio();
                let mut packets = 0_u64;
                let mut bytes = 0_u64;
                while let Some(packet) = input.next_media_packet().unwrap() {
                    if let EncodedMediaPacket::Video(packet) = packet {
                        packets += 1;
                        bytes += packet.data.len() as u64;
                    }
                }
                (packets, bytes)
            };
            let elapsed_us = started.elapsed().as_micros();
            COUNT.set(false);
            if iteration != 0 {
                println!(
                    "phase={phase} elapsed_us={elapsed_us} allocations={} allocated_bytes={} video_packets={packets} encoded_bytes={encoded_bytes}",
                    ALLOCATIONS.load(Ordering::Relaxed),
                    BYTES.load(Ordering::Relaxed)
                );
            }
        }
    }
}
