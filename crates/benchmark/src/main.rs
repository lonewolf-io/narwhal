// SPDX-License-Identifier: BSD-3-Clause

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;

use futures::future::join_all;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt, select};

use async_lock::Mutex;
use narwhal_client::monoio::c2s::C2sClient;
use narwhal_client::{AuthMethod, C2sConfig};
use narwhal_protocol::Nid;
use narwhal_protocol::{AclAction, AclType, QoS};
use narwhal_util::pool::Pool;
use narwhal_util::string_atom::StringAtom;
use rand::prelude::*;
use std::net::SocketAddr;
use std::str::FromStr;
use tracing::{error, info, warn};

/// Command line arguments
#[derive(Parser, Debug)]
#[command(name = "narwhal-bench")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "Narwhal performance benchmarking tool", long_about = None)]
struct Cli {
  /// Server address to connect to
  #[arg(short, long, default_value = "127.0.0.1:22622")]
  server: SocketAddr,

  /// Number of producer clients
  #[arg(short = 'p', long, default_value = "1")]
  producers: usize,

  /// Number of consumer clients
  #[arg(short = 'c', long, default_value = "10")]
  consumers: usize,

  /// Number of channels to create
  #[arg(short = 'n', long, default_value = "1")]
  channels: usize,

  /// Duration to run the benchmark
  #[arg(short, long, default_value = "30s", value_parser = parse_duration)]
  duration: Duration,

  /// Maximum size of message payload in bytes
  #[arg(long, default_value = "16384")]
  max_payload_size: usize,
}

/// Parse duration from string (supports: 30s, 5m, 1h)
fn parse_duration(s: &str) -> Result<Duration> {
  let s = s.trim();

  if let Some(secs) = s.strip_suffix('s') {
    Ok(Duration::from_secs(secs.parse()?))
  } else if let Some(mins) = s.strip_suffix('m') {
    Ok(Duration::from_secs(mins.parse::<u64>()? * 60))
  } else if let Some(hours) = s.strip_suffix('h') {
    Ok(Duration::from_secs(hours.parse::<u64>()? * 3600))
  } else {
    // Default to seconds if no suffix
    Ok(Duration::from_secs(s.parse()?))
  }
}

fn main() {
  // Initialize tracing
  tracing_subscriber::fmt().with_target(false).with_thread_ids(false).with_level(true).init();

  let cli = Cli::parse();

  info!("starting narwhal-bench...");
  info!("server: {}", cli.server);
  info!("producer(s): {}", cli.producers);
  info!("consumer(s): {}", cli.consumers);
  info!("channel(s): {}", cli.channels);
  info!("duration: {:?}", cli.duration);

  // Initialize monoio runtime
  let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new().enable_all().build().unwrap();

  // Run the benchmark
  rt.block_on(async {
    match run_benchmark(cli).await {
      Ok(metrics) => {
        info!("benchmark completed successfully");
        print_results(&metrics);
      },
      Err(e) => {
        error!("benchmark failed: {}", e);
      },
    }
  })
}

/// Benchmark metrics
#[derive(Debug)]
struct BenchmarkMetrics {
  total_messages_sent: u64,
  total_messages_received: u64,
  total_duration: Duration,
  successful_connections: usize,
  failed_connections: usize,
  errors: u64,
  latency_histogram: Option<hdrhistogram::Histogram<u64>>,
  // Producer/consumer specific metrics
  producer_count: usize,
  consumer_count: usize,
  successful_producers: usize,
  successful_consumers: usize,
}

impl BenchmarkMetrics {
  fn new() -> Self {
    Self {
      total_messages_sent: 0,
      total_messages_received: 0,
      total_duration: Duration::ZERO,
      successful_connections: 0,
      failed_connections: 0,
      errors: 0,
      latency_histogram: None,
      producer_count: 0,
      consumer_count: 0,
      successful_producers: 0,
      successful_consumers: 0,
    }
  }

  fn throughput_sent(&self) -> f64 {
    if self.total_duration.as_secs_f64() > 0.0 {
      self.total_messages_sent as f64 / self.total_duration.as_secs_f64()
    } else {
      0.0
    }
  }

  fn throughput_received(&self) -> f64 {
    if self.total_duration.as_secs_f64() > 0.0 {
      self.total_messages_received as f64 / self.total_duration.as_secs_f64()
    } else {
      0.0
    }
  }
}

/// Spawns inbound message drainer tasks for all clients.
///
/// Returns a vector of spawned task handles that resolve to message counts.
async fn spawn_inbound_drainers(
  clients: &[C2sClient],
  shutdown_rx: &async_channel::Receiver<()>,
) -> Vec<monoio::task::JoinHandle<u64>> {
  let mut drainer_tasks = Vec::with_capacity(clients.len());

  for client in clients.iter() {
    let inbound_stream = client.inbound_stream().await;
    let shutdown = shutdown_rx.clone();

    let drainer_task = monoio::spawn(async move {
      let mut count = 0u64;
      loop {
        select! {
          msg = inbound_stream.recv().fuse() => {
            match msg {
              Ok((message, _payload)) => {
                match &message {
                    narwhal_protocol::Message::Error(err) => warn!("received error message: {:?}", err),
                    narwhal_protocol::Message::Message{ .. } => count += 1,
                    _ => {}
                }
              },
              Err(_) => break, // Channel closed
            }
          },
          _ = shutdown.recv().fuse() => {
            // Shutdown requested
            break;
          }
        }
      }
      count
    });

    drainer_tasks.push(drainer_task);
  }

  drainer_tasks
}

/// Gracefully leaves a channel for all clients.
async fn leave_channel_gracefully(clients: &[C2sClient], channel: StringAtom) {
  info!("clients leaving channel(s)...");

  let mut leave_tasks = Vec::new();

  for client in clients.iter() {
    let ch = channel.clone();

    let leave_future = async move {
      match client.leave_channel(ch).await {
        Ok(_) => Ok(()),
        Err(e) => {
          warn!("client failed to leave channel: {}", e);
          Err(e)
        },
      }
    };

    leave_tasks.push(leave_future);
  }

  // Wait for all clients to leave
  let _ = join_all(leave_tasks).await;

  info!("all clients left the channel: {}", channel);
}

/// Joins clients to a channel.
///
/// Returns the channel name on success.
async fn create_and_join_channel(
  clients: &[C2sClient],
  num_producers: usize,
  num_consumers: usize,
  channel_index: usize,
) -> Result<StringAtom> {
  // Generate a unique channel name using the provided index
  let channel_id = format!("!bench{}@localhost", channel_index);
  let channel: StringAtom = channel_id.into();

  // First client joins and creates the channel
  match clients[0].join_channel(channel.clone()).await {
    Ok(()) => {
      info!("channel created: {}", channel);
    },
    Err(e) => {
      error!("failed to create channel: {}", e);
      anyhow::bail!("failed to create channel: {}", e);
    },
  };

  // Set channel ACL for publish
  let mut allow_publish: Vec<Nid> = Vec::with_capacity(num_producers);
  for i in 0..num_producers {
    let producer_nid = format!("bench_producer_{}@localhost", i);
    allow_publish.push(Nid::from_str(&producer_nid).expect("valid NID"));
  }

  match clients[0].set_channel_acl(channel.clone(), AclType::Publish, AclAction::Add, allow_publish).await {
    Ok(()) => {},
    Err(e) => {
      error!("failed to set channel publish ACL: {}", e);
      anyhow::bail!("failed to set channel publish ACL: {}", e);
    },
  };

  // Set channel ACL for read
  let mut allow_read: Vec<Nid> = Vec::with_capacity(num_consumers);
  for i in 0..num_consumers {
    let consumer_nid = format!("bench_consumer_{}@localhost", i);
    allow_read.push(Nid::from_str(&consumer_nid).expect("valid NID"));
  }

  match clients[0].set_channel_acl(channel.clone(), AclType::Read, AclAction::Add, allow_read).await {
    Ok(()) => {},
    Err(e) => {
      error!("failed to set channel read ACL: {}", e);
      anyhow::bail!("failed to set channel read ACL: {}", e);
    },
  };

  // Remaining clients join the channel
  if clients.len() > 1 {
    let mut join_tasks = Vec::new();
    for client in clients.iter().skip(1) {
      let ch = channel.clone();

      let join_future = async move {
        match client.join_channel(ch).await {
          Ok(()) => Ok(()),
          Err(e) => {
            warn!("client failed to join channel: {}", e);
            Err(e)
          },
        }
      };

      join_tasks.push(join_future);
    }

    // Wait for all clients to join
    let _ = join_all(join_tasks).await;

    info!("all clients joined the channel: {}", channel);
  }

  Ok(channel)
}

/// Creates multiple clients with the given configuration.
///
/// Returns a tuple of (clients, successful_count, failed_count).
fn create_clients(config: &C2sConfig, count: usize, client_type: &str) -> (Vec<C2sClient>, usize, usize) {
  let mut clients = Vec::with_capacity(count);
  let mut successful = 0;
  let mut failed = 0;

  for i in 0..count {
    let username = format!("bench_{}_{}", client_type, i);
    let auth_method = AuthMethod::Identify { username: username.as_str().into() };

    match C2sClient::new_with_insecure_tls(config.clone(), auth_method) {
      Ok(client) => {
        clients.push(client);
        successful += 1;
      },
      Err(e) => {
        warn!("failed to create client {}: {}", username, e);
        failed += 1;
      },
    }
  }

  info!("{}/{} {} client(s) successfully created", successful, count, client_type);

  (clients, successful, failed)
}

/// Broadcasts messages from all clients for the specified duration.
///
/// Each client continuously broadcasts messages to the channel and waits for acknowledgment.
/// Returns the total number of messages successfully sent and a histogram of latencies.
async fn broadcast_messages(
  clients: &[C2sClient],
  channels: &[StringAtom],
  duration: Duration,
  max_payload_size: usize,
) -> (u64, hdrhistogram::Histogram<u64>) {
  info!("broadcasting messages across channels for {:?}...", duration);

  let end_time = Instant::now() + duration;
  let mut broadcast_tasks = Vec::new();

  // Create a histogram for tracking latencies (max 60s, 3 significant digits)
  let arc_histogram = Arc::new(Mutex::new(hdrhistogram::Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap()));

  for (client_idx, client) in clients.iter().enumerate() {
    let client = client.clone();
    let client_channels: Vec<StringAtom> = channels.to_vec();

    let max_inflight_requests =
      client.session_info().await.ok().map(|(info, _)| info.max_inflight_requests).unwrap_or(1);

    let client_pool = Pool::new(max_inflight_requests as usize, max_payload_size);
    let client_hist = arc_histogram.clone();

    let task = monoio::spawn(async move {
      let mut count = 0u64;
      let mut channel_idx = 0usize;

      let mut task_set = FuturesUnordered::new();

      // Seed with initial concurrent requests
      for _ in 0..max_inflight_requests {
        let task_channel = client_channels[channel_idx % client_channels.len()].clone();
        let task_client = client.clone();
        let task_payload_pool = client_pool.clone();

        task_set.push(broadcast_message(task_channel, task_client, task_payload_pool, max_payload_size));

        channel_idx += 1;
      }

      while let Some(result) = task_set.next().await {
        match result {
          Ok(elapsed_ms) => {
            {
              let mut h = client_hist.lock().await;
              let _ = h.record(elapsed_ms);
            }
            count += 1;
          },
          Err(e) => {
            error!("(client {}): {}", client_idx, e);
            std::process::exit(1);
          },
        }

        // If we haven't reached the deadline... broadcast a new message
        if Instant::now() < end_time {
          let task_channel = client_channels[channel_idx % client_channels.len()].clone();
          let task_client = client.clone();
          let task_payload_pool = client_pool.clone();

          channel_idx += 1;

          task_set.push(broadcast_message(task_channel, task_client, task_payload_pool, max_payload_size));
        }
      }

      count
    });

    broadcast_tasks.push(task);
  }

  // Wait for all broadcast tasks to complete
  let results = join_all(broadcast_tasks).await;
  let total_sent: u64 = results.into_iter().sum();

  info!("total messages sent: {}", total_sent);

  let final_histogram = match Arc::try_unwrap(arc_histogram) {
    Ok(mutex) => mutex.lock().await.clone(),
    Err(arc) => arc.lock().await.clone(),
  };

  (total_sent, final_histogram)
}

async fn broadcast_message(
  channel: StringAtom,
  client: C2sClient,
  payload_pool: Pool,
  max_payload_size: usize,
) -> anyhow::Result<u64> {
  let mut payload_buffer = payload_pool.acquire_buffer().await;
  let random_size = rand::rng().random_range(1..=max_payload_size);
  let payload = payload_buffer.freeze(random_size);

  let start = Instant::now();

  client.broadcast(channel, Some(QoS::AckOnReceived), payload).await?;
  let elapsed_us = start.elapsed().as_micros() as u64;

  Ok(elapsed_us)
}

/// Run the benchmark with the given configuration
async fn run_benchmark(cli: Cli) -> Result<BenchmarkMetrics> {
  let mut metrics = BenchmarkMetrics::new();

  // Set producer/consumer counts
  metrics.producer_count = cli.producers;
  metrics.consumer_count = cli.consumers;

  let total_clients = cli.producers + cli.consumers;
  info!(
    "connecting {} client(s) ({} producer(s), {} consumer(s)) to {}...",
    total_clients, cli.producers, cli.consumers, cli.server
  );

  // Print additional information
  info!("max payload size: {} bytes", cli.max_payload_size);

  let start_time = Instant::now();

  info!("running benchmark...");
  perform_benchmark(&cli, &mut metrics).await?;

  metrics.total_duration = start_time.elapsed();

  Ok(metrics)
}

/// Run the benchmark with configurable producers and consumers
async fn perform_benchmark(cli: &Cli, metrics: &mut BenchmarkMetrics) -> Result<()> {
  // Create client configuration
  let client_config = C2sConfig {
    address: cli.server.to_string(),
    heartbeat_interval: Duration::from_secs(60),
    connect_timeout: Duration::from_secs(5),
    timeout: Duration::from_secs(20),
    payload_read_timeout: Duration::from_secs(5),
    backoff_initial_delay: Duration::from_millis(100),
    backoff_max_delay: Duration::from_secs(30),
    backoff_max_retries: 5,
  };

  // Create producers
  info!("creating {} producer client(s)...", cli.producers);
  let (producer_clients, producer_successful, producer_failed) =
    create_clients(&client_config, cli.producers, "producer");

  // Create consumers
  info!("creating {} consumer client(s)...", cli.consumers);
  let (consumer_clients, consumer_successful, consumer_failed) =
    create_clients(&client_config, cli.consumers, "consumer");

  // Combine metrics from producers and consumers
  let successful = producer_successful + consumer_successful;
  let failed = producer_failed + consumer_failed;

  metrics.successful_connections = successful;
  metrics.failed_connections = failed;
  metrics.successful_producers = producer_successful;
  metrics.successful_consumers = consumer_successful;

  // Check if we have any clients
  if producer_clients.is_empty() && consumer_clients.is_empty() {
    anyhow::bail!("no clients connected successfully");
  }

  if producer_clients.is_empty() {
    anyhow::bail!("no producer clients connected successfully");
  }

  // Create a combined list of all clients
  let mut all_clients = Vec::with_capacity(producer_clients.len() + consumer_clients.len());
  all_clients.extend_from_slice(&producer_clients);
  all_clients.extend_from_slice(&consumer_clients);

  // Create a shutdown channel for all drainer tasks
  let (drainer_shutdown_tx, drainer_shutdown_rx) = async_channel::bounded::<()>(1);

  // Spawn inbound message drainer tasks for all clients
  let drainer_tasks = spawn_inbound_drainers(&all_clients, &drainer_shutdown_rx).await;

  // Create channels and have all clients join all channels
  let mut channels = Vec::with_capacity(cli.channels);
  for i in 0..cli.channels {
    if cli.channels > 1 {
      info!("creating channel {} of {}...", i + 1, cli.channels);
    }
    let channel = create_and_join_channel(&all_clients, cli.producers, cli.consumers, i + 1).await?;
    channels.push(channel);
  }

  // Only producers broadcast messages (round-robin across all channels)
  let (messages_sent, latency_histogram) =
    broadcast_messages(&producer_clients, &channels, cli.duration, cli.max_payload_size).await;
  metrics.total_messages_sent = messages_sent;
  metrics.latency_histogram = Some(latency_histogram);

  // Leave all channels gracefully
  for channel in &channels {
    let _ = leave_channel_gracefully(&all_clients, channel.clone()).await;
  }

  // Cleanup: shutdown all clients
  info!("shutting down clients...");

  // Shutdown all clients
  for client in all_clients {
    let _ = client.shutdown().await;
  }

  // Signal all drainer tasks to stop
  info!("cancelling message drainers...");
  drainer_shutdown_tx.close();

  // Wait for drainer tasks to complete and collect received message counts
  info!("collecting received message counts...");
  let mut total_received = 0u64;
  for task in drainer_tasks {
    match monoio::time::timeout(std::time::Duration::from_secs(5), task).await {
      Ok(count) => {
        total_received += count;
      },
      Err(_) => {
        warn!("drainer task timed out after 5 seconds");
      },
    }
  }

  info!("total messages received: {}", total_received);
  metrics.total_messages_received = total_received;

  Ok(())
}

/// Print benchmark results
fn print_results(metrics: &BenchmarkMetrics) {
  println!("\n========================================");
  println!("         Benchmark Results");
  println!("========================================");
  println!();
  println!("Client Configuration:");
  println!("  Producers:   {} (successful: {})", metrics.producer_count, metrics.successful_producers);
  println!("  Consumers:   {} (successful: {})", metrics.consumer_count, metrics.successful_consumers);
  println!(
    "  Total:       {} (successful: {}, failed: {})",
    metrics.producer_count + metrics.consumer_count,
    metrics.successful_connections,
    metrics.failed_connections
  );
  println!();
  println!("Messages:");
  println!("  Produced:    {}", metrics.total_messages_sent);
  println!("  Consumed:    {}", metrics.total_messages_received);
  println!("  Errors:      {}", metrics.errors);
  println!();
  println!("Per-client Metrics:");
  if metrics.successful_producers > 0 {
    println!("  Msgs/producer: {:.2}", metrics.total_messages_sent as f64 / metrics.successful_producers as f64);
  }
  println!();
  println!("Throughput:");
  println!("  Production:  {:.2} msg/s", metrics.throughput_sent());
  println!("  Consumption: {:.2} msg/s", metrics.throughput_received());
  println!();

  if let Some(ref hist) = metrics.latency_histogram
    && !hist.is_empty()
  {
    println!("Message Latency:");
    println!("  Min:         {:.2}ms", hist.min() as f64 / 1000.0);
    println!("  Max:         {:.2}ms", hist.max() as f64 / 1000.0);
    println!("  Mean:        {:.2}ms", hist.mean() / 1000.0);
    println!("  P50:         {:.2}ms", hist.value_at_quantile(0.50) as f64 / 1000.0);
    println!("  P95:         {:.2}ms", hist.value_at_quantile(0.95) as f64 / 1000.0);
    println!("  P99:         {:.2}ms", hist.value_at_quantile(0.99) as f64 / 1000.0);
    println!();
  }

  println!("Duration:      {:.2}s", metrics.total_duration.as_secs_f64());
  println!("========================================");
}
