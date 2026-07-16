//! Criterion perf-regression benches for the pure `motion` algebra.
//!
//! The load-bearing cost is the `UnitBezier` Newton–Raphson (+ bisection
//! fallback) curve solve; `frame_decay` is the shared snow/bell decay factor.
//! These are CPU-only + GPU-free, so they gate on any runner.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use mado::motion::{frame_decay, Curve, EasingKind};

fn bench_curve_ease(c: &mut Criterion) {
    // SonicBoom is the most-curved token — the slowest to converge, so it
    // exercises the Newton path + the occasional bisection fallback.
    let sonic = Curve::named(EasingKind::SonicBoom);
    c.bench_function("curve_ease_sonic_boom_sweep", |b| {
        b.iter(|| {
            let mut acc = 0.0f32;
            for i in 0..=64 {
                let t = i as f32 / 64.0;
                acc += sonic.ease(black_box(t));
            }
            black_box(acc)
        })
    });

    // A single mid-domain solve — isolates one Newton+bisection run.
    let standard = Curve::named(EasingKind::Standard);
    c.bench_function("curve_ease_standard_single", |b| {
        b.iter(|| standard.ease(black_box(0.42)))
    });

    // Linear short-circuits the solve — the identity fast-path baseline.
    let linear = Curve::Linear;
    c.bench_function("curve_ease_linear", |b| b.iter(|| linear.ease(black_box(0.37))));
}

fn bench_frame_decay(c: &mut Criterion) {
    // `retain^(dt*60)` — one powf; the shared snow typing-pulse + bell glow
    // decay factor.
    c.bench_function("frame_decay", |b| {
        b.iter(|| frame_decay(black_box(1.0 / 60.0), black_box(0.92)))
    });
}

criterion_group!(benches, bench_curve_ease, bench_frame_decay);
criterion_main!(benches);
