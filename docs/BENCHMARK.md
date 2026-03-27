# Benchmark Results

This document contains performance benchmarks for Narwhal's pub/sub messaging system. All benchmarks were conducted using the `narwhal-bench` tool included in the project.

## Test Environment

### Hardware

- **CPU**: Intel(R) Core(TM) Ultra 9 275HX (24 cores)
- **Memory**: 32 GB

### Configuration

- **Build Configuration**: Release mode (`--release`)
- **Server**: Local Narwhal instance (127.0.0.1:22622)
- **Test Duration**: 60 seconds per run
- **Producers**: 1
- **Consumers**: 1
- **Channels**: 1

## Benchmark Tool

The benchmark tool is located in the `crates/benchmark` directory and can be built and run as follows:

```bash
# Build the benchmark tool
cargo build --release -p narwhal-benchmark

# Run a benchmark
./target/release/narwhal-bench \
  --server 127.0.0.1:22622 \
  --producers 1 \
  --consumers 1 \
  --duration 1m \
  --max-payload-size 256
```

### Available Options

- `-s, --server <ADDR>`: Server address to connect to (default: 127.0.0.1:22622)
- `-p, --producers <N>`: Number of producer clients (default: 1)
- `-c, --consumers <N>`: Number of consumer clients (default: 10)
- `-n, --channels <N>`: Number of channels to create (default: 1)
- `-d, --duration <TIME>`: Duration to run the benchmark (supports: 30s, 5m, 1h)
- `--max-payload-size <BYTES>`: Maximum size of message payload in bytes (default: 16384)
- `-w, --worker-threads <N>`: Number of worker threads to use (default: 0, auto-detect)

## Results

### Performance Overview Table

All benchmarks were run with 1 producer and 1 consumer for 60 seconds, averaged over 10 runs per payload size.

| Payload Size | Throughput (msg/s) | Data Throughput | Mean Latency | P50 | P95 | P99 | Avg Total Messages | Runs |
|--------------|-------------------:|----------------:|-------------:|----:|----:|----:|-------------------:|-----:|
| 256 B        |            162,843 |       41.7 MB/s |        0.60ms | 0.60ms | 0.79ms | 0.93ms |          9,800,844 | 10/10 |
| 512 B        |            157,430 |       20.2 MB/s |        0.62ms | 0.61ms | 0.90ms | 1.03ms |          9,475,422 | 10/10 |
| 1 KB         |            153,214 |       78.4 MB/s |        0.65ms | 0.63ms | 0.83ms | 1.00ms |          9,223,401 | 10/10 |
| 4 KB         |            123,012 |      251.9 MB/s |        0.81ms | 0.80ms | 0.89ms | 1.06ms |          7,403,481 | 10/10 |
| 8 KB         |             92,529 |      379.0 MB/s |        1.08ms | 1.08ms | 1.17ms | 1.29ms |          5,569,542 | 10/10 |
| 16 KB        |             68,017 |      557.2 MB/s |        1.48ms | 1.48ms | 1.61ms | 1.84ms |          4,110,314 | 10/10 |
| 32 KB        |             51,197 |      838.8 MB/s |        1.93ms | 1.76ms | 2.03ms | 2.64ms |          3,111,557 | 10/10 |
| 64 KB        |             31,085 |       1.02 GB/s |        3.16ms | 2.84ms | 4.07ms | 5.08ms |          1,897,140 | 10/10 |
| 128 KB       |             16,841 |       1.10 GB/s |        5.83ms | 5.88ms | 7.32ms | 8.05ms |          1,029,126 | 10/10 |

### Visual Performance Analysis

#### Message Throughput

![Message Throughput vs Payload Size](benchmark_msg_throughput.png)

This graph shows how message throughput (messages per second) varies with payload size. Each data point is the average of 10 runs. Peak performance is achieved with small payloads, delivering over 162,000 messages per second on average.

#### Latency

![Message Latency vs Payload Size](benchmark_latency.png)

This graph compares Mean, P50, P95, and P99 latency across different payload sizes. Latency scales gradually with payload size, with P99 latency remaining in the single-digit millisecond range even at 128KB payloads.

#### Data Throughput (Bandwidth)

![Data Throughput vs Payload Size](benchmark_data_throughput.png)

This graph illustrates the actual data throughput. While message rate decreases with larger payloads, the overall bandwidth increases significantly, peaking at 1.10 GB/s with 128 KB payloads.

### Key Observations

1. **Peak Throughput**: Achieved with 256 B payloads at ~162,843 messages/second
2. **Consistent Low Latency**: Sub-1ms mean latency maintained across small payload sizes (256B - 1KB), sub-2ms up to 32KB
3. **Payload Size Impact**: As payload size increases, message throughput decreases while data throughput increases, which is expected due to larger data transfers
4. **Latency Stability**: P99 latency remains very low across all payload sizes, indicating consistent performance
5. **Zero Errors**: All benchmarks completed without any message loss or errors
6. **Peak Bandwidth**: 1.10 GB/s achieved at 128 KB payload

## Notes

- All benchmarks were run on a local machine with the server and client on the same host
- Network latency in production environments will add to the observed latency values
- The benchmark tool generates random payloads up to the specified maximum size
- Results may vary based on hardware, OS, and system load
