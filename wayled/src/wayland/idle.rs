use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::{
  ext_idle_notification_v1::{self, ExtIdleNotificationV1},
  ext_idle_notifier_v1::ExtIdleNotifierV1,
};

use super::Wayland;

delegate_noop!(Wayland: ExtIdleNotifierV1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEvent {
  Idle,
  Active,
}

impl From<IdleEvent> for super::Event {
  fn from(val: IdleEvent) -> Self {
    match val {
      IdleEvent::Idle => super::Event::Idle(IdleEvent::Idle),
      IdleEvent::Active => super::Event::Idle(IdleEvent::Active),
    }
  }
}

impl Dispatch<ExtIdleNotificationV1, ()> for Wayland {
  #[tracing::instrument(level = "trace", target = "idle_notification", skip_all)]
  fn event(
    data: &mut Self,
    _: &ExtIdleNotificationV1,
    event: ext_idle_notification_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Wayland>,
  ) {
    match event {
      ext_idle_notification_v1::Event::Idled => {
        tracing::info!("seat is now idle");
        if let Err(err) = data.tx.send(IdleEvent::Idle.into()) {
          tracing::error!("failed to send idle event: {}", err);
        }
      }
      ext_idle_notification_v1::Event::Resumed => {
        tracing::info!("seat is now active");
        if let Err(err) = data.tx.send(IdleEvent::Active.into()) {
          tracing::error!("failed to send active event: {}", err);
        }
      }
      _ => {}
    }
  }
}
