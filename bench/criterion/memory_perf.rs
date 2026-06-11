use criterion::{criterion_group, criterion_main, Criterion};
use rust_miniss::buffer::{Buffer, BufferPool};

fn bench_buffer_pool_roundtrip(c: &mut Criterion) {
    c.bench_function("buffer_pool_roundtrip_4096", |b| {
        b.iter(|| {
            let buf = BufferPool::get(4096);
            criterion::black_box(buf.capacity());
            BufferPool::recycle(buf);
        });
    });
}

fn bench_buffer_clone(c: &mut Criterion) {
    let data = vec![0u8; 4096];
    c.bench_function("buffer_clone_4k", |b| {
        b.iter(|| {
            let buf = Buffer::from(data.clone());
            criterion::black_box(buf.len());
        });
    });
}

criterion_group!(benches, bench_buffer_pool_roundtrip, bench_buffer_clone);
criterion_main!(benches);
