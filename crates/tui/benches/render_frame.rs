use std::hint::black_box;
use std::time::{Duration, Instant};

use jucode_tui::bench_support::RenderFrameBench;

const WIDTH: usize = 120;
const HEIGHT: u16 = 40;
const HISTORY_ITEMS: &[usize] = &[100, 1_000, 5_000, 10_000, 25_000];

fn main() {
    println!("render_frame benchmark");
    println!("viewport: {WIDTH}x{HEIGHT}");
    println!(
        "{:>10} {:>12} {:>12} {:>12}",
        "history", "cold avg", "cached avg", "iters"
    );

    for &history_items in HISTORY_ITEMS {
        let cold_iters = iterations_for_history(history_items, true);
        let cached_iters = iterations_for_history(history_items, false);

        let cold = measure(cold_iters, || {
            let mut bench = RenderFrameBench::new(history_items, WIDTH);
            black_box(bench.render_cold_frame(WIDTH, HEIGHT))
        });

        let mut cached_bench = RenderFrameBench::new(history_items, WIDTH);
        black_box(cached_bench.render_cached_frame(WIDTH, HEIGHT));
        let cached = measure(cached_iters, || {
            black_box(cached_bench.render_cached_frame(WIDTH, HEIGHT))
        });

        println!(
            "{history_items:>10} {:>12} {:>12} {:>12}",
            format_duration(cold / cold_iters as u32),
            format_duration(cached / cached_iters as u32),
            format!("{cold_iters}/{cached_iters}")
        );
    }
}

fn iterations_for_history(history_items: usize, cold: bool) -> usize {
    match (history_items, cold) {
        (0..=1_000, true) => 200,
        (0..=1_000, false) => 2_000,
        (0..=5_000, true) => 80,
        (0..=5_000, false) => 2_000,
        (0..=10_000, true) => 40,
        (0..=10_000, false) => 2_000,
        (_, true) => 15,
        (_, false) => 2_000,
    }
}

fn measure(iterations: usize, mut f: impl FnMut() -> usize) -> Duration {
    let start = Instant::now();
    let mut checksum = 0usize;
    for _ in 0..iterations {
        checksum ^= f();
    }
    black_box(checksum);
    start.elapsed()
}

fn format_duration(duration: Duration) -> String {
    let nanos = duration.as_nanos();
    if nanos >= 1_000_000 {
        format!("{:.3}ms", nanos as f64 / 1_000_000.0)
    } else if nanos >= 1_000 {
        format!("{:.3}us", nanos as f64 / 1_000.0)
    } else {
        format!("{nanos}ns")
    }
}
