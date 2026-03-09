// SPDX-License-Identifier: BSD-3-Clause

use async_broadcast;
use async_channel;
use futures::{FutureExt, select};
use tracing::warn;

use narwhal_modulator::OutboundPrivatePayload;

mod config;
mod listener;

pub mod conn;
pub mod router;

pub use config::Limits;
pub use config::{Config, ListenerConfig};
pub use listener::C2sListener;
pub use router::Router;

use narwhal_protocol::{Message, ModDirectParameters};

/// Routes private payloads from the modulator to connected clients.
///
/// This function spawns an asynchronous task that listens for outbound private
/// payloads from the modulator (M2S) and routes them to the appropriate client
/// connections. Each payload is wrapped in a ModDirect message and sent to all
/// specified target users.
///
/// # Arguments
///
/// * `m2s_payload_rx` - A broadcast receiver for [`OutboundPrivatePayload`] messages
///   coming from the modulator. Each payload contains the data to send and a list
///   of target usernames.
/// * `router` - The [`Router`] instance used to route messages to connected clients.
///   The router's local domain is used as the 'from' field in ModDirect messages.
///
/// # Returns
///
/// A tuple containing:
/// * `monoio::task::JoinHandle<()>` - A handle to the spawned routing task
///   that can be awaited for completion
/// * `async_channel::Sender<()>` - A sender that can be closed to gracefully shut
///   down the routing task
pub fn route_m2s_private_payload(
  m2s_payload_rx: async_broadcast::Receiver<OutboundPrivatePayload>,
  router: Router,
) -> (monoio::task::JoinHandle<()>, async_channel::Sender<()>) {
  let (shutdown_tx, shutdown_rx) = async_channel::bounded::<()>(1);

  let handle = monoio::spawn(route_loop(m2s_payload_rx, router, shutdown_rx));

  (handle, shutdown_tx)
}

async fn route_loop(
  mut m2s_payload_rx: async_broadcast::Receiver<OutboundPrivatePayload>,
  router: Router,
  shutdown_rx: async_channel::Receiver<()>,
) {
  loop {
    select! {
      result = m2s_payload_rx.recv().fuse() => {
        match result {
          Ok(outbound) => {
            let mod_priv_msg = Message::ModDirect(ModDirectParameters {
              id: None,
              from: router.local_domain(),
              length: outbound.payload.len() as u32,
            });
            for target in outbound.targets {
              match router.route_to(mod_priv_msg.clone(), Some(outbound.payload.clone()), target, None).await {
                Ok(_) => {},
                Err(err) => {
                  warn!("failed to route private payload: {}", err);
                }
              }
            }
          }
          Err(async_broadcast::RecvError::Overflowed(_)) => {
            continue;
          }
          Err(async_broadcast::RecvError::Closed) => {
            break;
          }
        }
      }
      _ = shutdown_rx.recv().fuse() => {
        break;
      }
    }
  }
}
