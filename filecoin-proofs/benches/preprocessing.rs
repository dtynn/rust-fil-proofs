use std::io::{self, Read};
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion, ParameterizedBenchmark, Throughput};
use filecoin_proofs::fr32::write_padded;
use rand::{thread_rng, Rng};

fn random_data(size: usize) -> Vec<u8> {
    let mut rng = thread_rng();
    (0..size).map(|_| rng.gen()).collect()
}

fn preprocessing_benchmark(c: &mut Criterion) {
    c.bench(
        "preprocessing",
        ParameterizedBenchmark::new(
            "write_padded",
            |b, size| {
                let data = random_data(*size);

                b.iter(|| {
                    let mut buf = io::Cursor::new(Vec::new());
                    let read = write_padded(io::Cursor::new(&data), &mut buf).unwrap();
                    assert!(read >= data.len(), "{} > {}", read, data.len());
                })
            },
            vec![128, 256, 512, 256_000, 512_000, 1024_000, 2048_000],
        )
        .with_function("write_padded_new", |b, size| {
            let data = random_data(*size);

            b.iter(|| {
                let mut reader =
                    filecoin_proofs::pad_reader::PadReader::new(io::Cursor::new(&data));
                let mut buf = Vec::new();
                reader.read_to_end(&mut buf).unwrap();
                assert!(buf.len() >= data.len());
            })
        })
        .sample_size(10)
        .throughput(|s| Throughput::Bytes(*s as u64))
        .warm_up_time(Duration::from_secs(1)),
    );
}

criterion_group!(benches, preprocessing_benchmark);
criterion_main!(benches);
