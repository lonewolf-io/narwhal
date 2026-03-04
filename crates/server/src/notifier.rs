// SPDX-License-Identifier: BSD-3-Clause

use std::sync::Arc;

use anyhow::Context;

use crate::router::GlobalRouter;
use crate::transmitter::Resource;
use narwhal_modulator::Modulator;
use narwhal_modulator::modulator::Operation;
use narwhal_protocol::{Event, Nid};

/// A notification service that handles routing events to multiple targets.
///
/// The `Notifier` is responsible for distributing event messages to specified
/// targets through the global router. It can optionally integrate with a
/// modulator client for additional message processing capabilities.
#[derive(Clone, Debug)]
pub struct Notifier {
  /// The global router used for routing notifications.
  router: GlobalRouter,

  /// The modulator, if any.
  modulator: Option<Arc<dyn Modulator>>,
}

impl Notifier {
  /// Creates a new `Notifier` instance.
  ///
  /// # Arguments
  ///
  /// * `router` - The global router that will handle message distribution
  /// * `modulator` - An optional modulator client for additional message processing
  ///
  /// # Returns
  ///
  /// A new `Notifier` instance configured with the provided router and optional
  /// modulator client.
  pub fn new(router: GlobalRouter, modulator: Option<Arc<dyn Modulator>>) -> Self {
    Notifier { router, modulator }
  }

  /// Sends an event to multiple target recipients.
  ///
  /// This method first forwards the event to the modulator client if available,
  /// then routes it to all specified targets through the global router. It can
  /// optionally exclude a specific resource from receiving the notification.
  ///
  /// # Arguments
  ///
  /// * `event` - The event to be sent
  /// * `targets` - An iterator of target IDs that should receive the event
  /// * `excluding_resource` - An optional resource to exclude from receiving the notification,
  ///   useful when broadcasting changes made by a specific resource
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the event was successfully forwarded (if applicable) and routed,
  /// or an error if either the forwarding or routing operation failed.
  pub async fn notify<'a>(
    &self,
    event: Event,
    targets: impl IntoIterator<Item = &'a Nid>,
    excluding_resource: Option<Resource>,
  ) -> anyhow::Result<()> {
    // Forward the event to the modulator if available
    if let Some(modulator) = self.modulator.as_ref()
      && modulator.operations().await?.contains(Operation::ForwardEvent)
    {
      modulator
        .forward_event(narwhal_modulator::modulator::ForwardEventRequest { event: event.clone() })
        .await
        .context("failed to forward event to modulator")?;
    }

    self.router.route_to_many(event.into(), None, targets, excluding_resource).await?;

    Ok(())
  }
}
