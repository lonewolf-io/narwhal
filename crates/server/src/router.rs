// SPDX-License-Identifier: BSD-3-Clause

use crate::{c2s, transmitter::Resource};

use narwhal_protocol::Nid;
use narwhal_util::pool::PoolBuffer;

#[derive(Clone)]
pub struct GlobalRouter {
  c2s_router: c2s::Router,
}

// ===== impl GlobalRouter =====

impl GlobalRouter {
  /// Creates a new `GlobalRouter` instance.
  ///
  /// # Arguments
  ///
  /// * `c2s_router` - The client-to-server router that handles local domain routing
  ///
  /// # Returns
  ///
  /// A new `GlobalRouter` instance initialized with the provided C2S router
  pub fn new(c2s_router: c2s::Router) -> Self {
    Self { c2s_router }
  }

  /// Routes a message to a single target destination.
  ///
  /// # Arguments
  ///
  /// * `msg` - The protocol message to be routed
  /// * `payload_opt` - Optional payload buffer associated with the message
  /// * `target` - The NID of the target destination
  /// * `excluding_resource` - Optional resource to exclude from routing (useful to avoid routing back to sender)
  ///
  /// # Returns
  ///
  /// * `Ok(())` if the message was successfully routed
  /// * `Err` if routing failed
  pub async fn route_to(
    &self,
    msg: narwhal_protocol::Message,
    payload_opt: Option<PoolBuffer>,
    target: Nid,
    excluding_resource: Option<Resource>,
  ) -> anyhow::Result<()> {
    self.route_to_many(msg, payload_opt, std::iter::once(&target), excluding_resource).await
  }

  /// Routes a message to multiple target destinations.
  ///
  /// # Arguments
  ///
  /// * `msg` - The protocol message to be routed
  /// * `payload_opt` - Optional payload buffer associated with the message
  /// * `targets` - An iterator of target NID references
  /// * `excluding_resource` - Optional resource to exclude from routing (useful to avoid routing back to sender)
  ///
  /// # Returns
  ///
  /// * `Ok(())` if the message was successfully routed to all targets
  /// * `Err` if routing failed for any target
  pub async fn route_to_many<'a>(
    &self,
    msg: narwhal_protocol::Message,
    payload_opt: Option<PoolBuffer>,
    targets: impl IntoIterator<Item = &'a Nid>,
    excluding_resource: Option<Resource>,
  ) -> anyhow::Result<()> {
    let local_domain = self.c2s_router.local_domain();

    let excluding_local_handler = {
      match excluding_resource.as_ref() {
        Some(resource) => {
          if resource.domain.is_none() {
            Some(resource.handler)
          } else {
            None
          }
        },
        None => None,
      }
    };

    for target in targets {
      if target.domain == local_domain {
        self.c2s_router.route_to(msg.clone(), payload_opt.clone(), target.username.clone(), excluding_local_handler)?;
      } else {
        // TODO(ortuman): Implement S2S routing
      }
    }

    Ok(())
  }

  /// Returns a reference to the underlying C2S router.
  ///
  /// This provides access to the C2S router for operations that need direct
  /// interaction with the client-to-server routing layer.
  ///
  /// # Returns
  ///
  /// A reference to the internal `c2s::Router`
  pub fn c2s_router(&self) -> &c2s::Router {
    &self.c2s_router
  }
}

impl std::fmt::Debug for GlobalRouter {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("GlobalRouter")
      .field("local_domain", &self.c2s_router().local_domain())
      .field("c2s_total_connections", &self.c2s_router().len())
      .finish()
  }
}
