use crate::{
  gpu::Compute,
  wayland::{self, Wayland},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_notifications() {
  crate::monitoring::init_logger();
  tracing::warn!("Please do not touch the mouse or keyboard for 1 second...");

  let (_wayland_cmd_tx, wayland_cmd_rx) = calloop::channel::channel();
  let (wayland_event_tx, mut wayland_event_rx) = tokio::sync::mpsc::unbounded_channel();

  let mut event_loop = calloop::EventLoop::try_new().expect("could not create event loop");
  let handle = event_loop.handle();
  let signal = event_loop.get_signal();

  let mut wayland =
    Wayland::init(wayland_event_tx, wayland_cmd_rx, handle.clone()).expect("could not connect to wayland");
  let _compute = Compute::init().expect("could not initialize compute module");

  tokio::spawn(async move {
    while let Some(event) = wayland_event_rx.recv().await {
      if let wayland::Event::Idle(wayland::idle::IdleEvent::Idle) = event {
        tracing::info!("received idle event!");
        signal.stop();
        return;
      }
    }
  });

  event_loop
    .run(std::time::Duration::from_secs(10), &mut wayland, |_| {})
    .expect("could not run event loop");
}
