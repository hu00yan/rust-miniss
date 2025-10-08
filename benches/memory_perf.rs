use criterion::{criterion_group, criterion_main, Bencher};
use rust_miniss::buffer::{get_buffer, recycle_buffer};
use slab::Token;

fn bench_slab_get(b: &mut Bencher) {
    b.iter(|| {
        let buf = get_buffer(4096);
        // Use
        criterion::black_box(buf);
    });
}

fn bench_arena_alloc(b: &mut Bencher) {
    #[cfg(feature = "arena")]
    {
        use bumpalo::Bump;
        let arena = Bump::new();
        b.iter(|| {
            let _s = arena.alloc_str("test");
            criterion::black_box(_s);
        });
        // Assert time <1us per alloc
    }
}

criterion_group!(benches, slab_get, arena_alloc);
criterion_main!(benches);