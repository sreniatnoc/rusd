use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::time::Duration;

// Mock client for benchmarking (in real scenario, would use actual gRPC client)
struct MockClient {
    endpoint: String,
}

impl MockClient {
    fn new(endpoint: &str) -> Self {
        MockClient {
            endpoint: endpoint.to_string(),
        }
    }

    // Simulate put operation
    fn put(&self, key: &str, value: &str) {
        let _ = (key, value); // Prevent unused variable warnings
    }

    // Simulate get operation
    fn get(&self, key: &str) -> String {
        key.to_string()
    }

    // Simulate range operation
    fn range(&self, prefix: &str, limit: u32) -> Vec<(String, String)> {
        (0..limit)
            .map(|i| (format!("{}{}", prefix, i), format!("value{}", i)))
            .collect()
    }

    // Simulate transaction operation
    fn txn(&self, key: &str, expected_value: &str, new_value: &str) -> bool {
        let _ = (key, expected_value, new_value);
        true
    }

    // Simulate delete operation
    fn delete(&self, key: &str) -> bool {
        let _ = key;
        true
    }
}

// ============================================================================
// Benchmark: Put Operations
// ============================================================================

fn bench_put(c: &mut Criterion) {
    let mut group = c.benchmark_group("put_operations");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(10));

    let client = black_box(MockClient::new("http://localhost:2379"));

    // Single put operation
    group.bench_function("single_put", |b| {
        let mut counter = 0;
        b.iter(|| {
            counter += 1;
            client.put(
                &format!("test/key{}", counter),
                &format!("value{}", counter),
            )
        })
    });

    // Put with varying value sizes
    for size in [10, 100, 1000, 10000].iter() {
        group.throughput(Throughput::Bytes(*size as u64));
        let value = black_box("x".repeat(*size));
        group.bench_with_input(BenchmarkId::new("put_value_size", size), size, |b, _| {
            let mut counter = 0;
            b.iter(|| {
                counter += 1;
                client.put(&format!("test/key{}", counter), &value)
            })
        });
    }

    // Sequential puts
    group.bench_function("sequential_puts_100", |b| {
        b.iter(|| {
            for i in 0..100 {
                client.put(&format!("test/key{}", i), &format!("value{}", i))
            }
        })
    });

    // Rapid puts with small payloads
    group.throughput(Throughput::Elements(1000));
    group.bench_function("rapid_puts_small_payload", |b| {
        let mut counter = 0;
        b.iter(|| {
            for _ in 0..1000 {
                counter += 1;
                client.put("test/key", "v");
            }
        })
    });

    group.finish();
}

// ============================================================================
// Benchmark: Get Operations
// ============================================================================

fn bench_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_operations");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(10));

    let client = black_box(MockClient::new("http://localhost:2379"));

    // Single get operation
    group.bench_function("single_get", |b| b.iter(|| client.get("test/key1")));

    // Sequential gets
    group.bench_function("sequential_gets_100", |b| {
        b.iter(|| {
            for i in 0..100 {
                let _ = client.get(&format!("test/key{}", i));
            }
        })
    });

    // Rapid gets
    group.throughput(Throughput::Elements(1000));
    group.bench_function("rapid_gets_1000", |b| {
        b.iter(|| {
            for i in 0..1000 {
                let _ = client.get(&format!("test/key{}", i % 100));
            }
        })
    });

    // Get with cache misses
    group.bench_function("get_with_misses", |b| {
        b.iter(|| {
            for i in 0..100 {
                let _ = client.get(&format!("nonexistent/key{}", i));
            }
        })
    });

    group.finish();
}

// ============================================================================
// Benchmark: Range Scan Operations
// ============================================================================

fn bench_range_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("range_scan_operations");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));

    let client = black_box(MockClient::new("http://localhost:2379"));

    // Small range scans
    group.throughput(Throughput::Elements(10));
    group.bench_with_input(
        BenchmarkId::new("range_small", "10_items"),
        &10u32,
        |b, &limit| b.iter(|| client.range("test/", limit)),
    );

    // Medium range scans
    group.throughput(Throughput::Elements(100));
    group.bench_with_input(
        BenchmarkId::new("range_medium", "100_items"),
        &100u32,
        |b, &limit| b.iter(|| client.range("test/", limit)),
    );

    // Large range scans
    group.throughput(Throughput::Elements(1000));
    group.bench_with_input(
        BenchmarkId::new("range_large", "1000_items"),
        &1000u32,
        |b, &limit| b.iter(|| client.range("test/", limit)),
    );

    // Range scan with different prefixes
    for prefix_depth in [1, 2, 3, 4].iter() {
        let prefix = (0..*prefix_depth)
            .map(|i| format!("level{}/", i))
            .collect::<String>();
        group.bench_with_input(
            BenchmarkId::new("range_prefix_depth", prefix_depth),
            prefix_depth,
            |b, _| b.iter(|| client.range(&prefix, 100)),
        );
    }

    // Range scan with pagination
    group.bench_function("range_paginated_10_pages", |b| {
        b.iter(|| {
            for _ in 0..10 {
                let _ = client.range("test/", 100);
            }
        })
    });

    group.finish();
}

// ============================================================================
// Benchmark: Transaction Operations
// ============================================================================

fn bench_txn(c: &mut Criterion) {
    let mut group = c.benchmark_group("transaction_operations");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));

    let client = black_box(MockClient::new("http://localhost:2379"));

    // Simple compare-and-swap
    group.bench_function("simple_txn_cas", |b| {
        let mut counter = 0;
        b.iter(|| {
            counter += 1;
            client.txn(
                &format!("txn/key{}", counter),
                "expected_value",
                "new_value",
            )
        })
    });

    // Transaction with multiple conditions
    group.bench_function("txn_multi_condition", |b| {
        let mut counter = 0;
        b.iter(|| {
            counter += 1;
            // Simulate 5 compare conditions
            for _ in 0..5 {
                client.txn(
                    &format!("txn/key{}", counter),
                    "expected_value",
                    "new_value",
                );
            }
        })
    });

    // Conflicting transactions (simulating retry)
    group.bench_function("txn_with_retries_3", |b| {
        let mut counter = 0;
        b.iter(|| {
            counter += 1;
            for _ in 0..3 {
                // Retry 3 times
                let _ = client.txn(
                    &format!("txn/key{}", counter),
                    "expected_value",
                    "new_value",
                );
            }
        })
    });

    // Sequential transactions
    group.bench_function("sequential_txn_100", |b| {
        b.iter(|| {
            for i in 0..100 {
                client.txn(&format!("txn/key{}", i), "expected_value", "new_value");
            }
        })
    });

    group.finish();
}

// ============================================================================
// Benchmark: Delete Operations
// ============================================================================

fn bench_delete(c: &mut Criterion) {
    let mut group = c.benchmark_group("delete_operations");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(10));

    let client = black_box(MockClient::new("http://localhost:2379"));

    // Single delete
    group.bench_function("single_delete", |b| {
        let mut counter = 0;
        b.iter(|| {
            counter += 1;
            client.delete(&format!("delete/key{}", counter))
        })
    });

    // Sequential deletes
    group.throughput(Throughput::Elements(100));
    group.bench_function("sequential_deletes_100", |b| {
        b.iter(|| {
            for i in 0..100 {
                let _ = client.delete(&format!("delete/key{}", i));
            }
        })
    });

    // Range delete
    group.throughput(Throughput::Elements(1000));
    group.bench_function("range_delete_1000", |b| {
        b.iter(|| {
            for i in 0..1000 {
                let _ = client.delete(&format!("delete/key{}", i));
            }
        })
    });

    group.finish();
}

// ============================================================================
// Benchmark: Mixed Workload
// ============================================================================

fn bench_mixed_workload(c: &mut Criterion) {
    let mut group = c.benchmark_group("mixed_workload");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(15));

    let client = black_box(MockClient::new("http://localhost:2379"));

    // Read-heavy workload (70% reads, 20% writes, 10% deletes)
    group.throughput(Throughput::Elements(100));
    group.bench_function("read_heavy_70_20_10", |b| {
        let mut counter = 0;
        b.iter(|| {
            for i in 0..100 {
                counter += 1;
                let op = i % 10;
                match op {
                    0..=1 => {
                        // 20% writes
                        client.put(&format!("mixed/key{}", counter), "value");
                    }
                    2 => {
                        // 10% deletes
                        let _ = client.delete(&format!("mixed/key{}", counter));
                    }
                    _ => {
                        // 70% reads
                        let _ = client.get(&format!("mixed/key{}", counter % 50));
                    }
                }
            }
        })
    });

    // Write-heavy workload (70% writes, 20% reads, 10% deletes)
    group.throughput(Throughput::Elements(100));
    group.bench_function("write_heavy_70_20_10", |b| {
        let mut counter = 0;
        b.iter(|| {
            for i in 0..100 {
                counter += 1;
                let op = i % 10;
                match op {
                    0..=6 => {
                        // 70% writes
                        client.put(&format!("mixed/key{}", counter), "value");
                    }
                    7..=8 => {
                        // 20% reads
                        let _ = client.get(&format!("mixed/key{}", counter % 50));
                    }
                    _ => {
                        // 10% deletes
                        let _ = client.delete(&format!("mixed/key{}", counter));
                    }
                }
            }
        })
    });

    // Balanced workload (33% reads, 33% writes, 34% range scans)
    group.throughput(Throughput::Elements(99));
    group.bench_function("balanced_33_33_34", |b| {
        let mut counter = 0;
        b.iter(|| {
            for i in 0..99 {
                counter += 1;
                let op = i % 3;
                match op {
                    0 => {
                        // 33% reads
                        let _ = client.get(&format!("mixed/key{}", counter % 50));
                    }
                    1 => {
                        // 33% writes
                        client.put(&format!("mixed/key{}", counter), "value");
                    }
                    _ => {
                        // 34% range scans
                        let _ = client.range("mixed/", 10);
                    }
                }
            }
        })
    });

    group.finish();
}

// ============================================================================
// Benchmark: Concurrent Operations
// ============================================================================

fn bench_concurrent(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_operations");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(15));

    let client = std::sync::Arc::new(black_box(MockClient::new("http://localhost:2379")));

    // Concurrent puts (simulated)
    group.throughput(Throughput::Elements(100));
    group.bench_function("concurrent_puts_100", |b| {
        let client = client.clone();
        b.iter(|| {
            let mut handles = vec![];
            for i in 0..100 {
                let client = client.clone();
                let key = format!("concurrent/key{}", i);
                let value = format!("value{}", i);
                // Simulate concurrent put
                client.put(&key, &value);
                let _ = handles.push(i);
            }
        })
    });

    // Concurrent reads
    group.throughput(Throughput::Elements(100));
    group.bench_function("concurrent_reads_100", |b| {
        let client = client.clone();
        b.iter(|| {
            for i in 0..100 {
                let _ = client.get(&format!("concurrent/key{}", i % 50));
            }
        })
    });

    // Concurrent mixed operations
    group.throughput(Throughput::Elements(100));
    group.bench_function("concurrent_mixed_100", |b| {
        let client = client.clone();
        b.iter(|| {
            for i in 0..100 {
                match i % 3 {
                    0 => client.put(&format!("concurrent/key{}", i), "value"),
                    1 => {
                        let _ = client.get(&format!("concurrent/key{}", i));
                    }
                    _ => {
                        let _ = client.range("concurrent/", 10);
                    }
                }
            }
        })
    });

    group.finish();
}

// ============================================================================
// Criterion Configuration
// ============================================================================

criterion_group!(
    benches,
    bench_put,
    bench_get,
    bench_delete,
    bench_range_scan,
    bench_txn,
    bench_mixed_workload,
    bench_concurrent
);

criterion_main!(benches);
