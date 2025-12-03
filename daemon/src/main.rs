#![feature(array_chunks)]

mod bridge;
mod cec;
mod client;
mod gpu;
mod kinect;
mod led;
mod monitoring;
mod net;
mod wayland;

#[cfg(test)]
mod __test__;

#[tokio::main]
async fn main() {
  monitoring::init_logger();
  tracing::info!("starting wayled");

  let (wayland_cmd_tx, wayland_cmd_rx) = calloop::channel::channel();
  let (wayland_event_tx, wayland_event_rx) = tokio::sync::mpsc::unbounded_channel();
  let (led_cmd_tx, led_cmd_rx) = tokio::sync::mpsc::unbounded_channel();
  let (led_event_tx, led_event_rx) = tokio::sync::mpsc::unbounded_channel();
  let (client_man_client_req_tx, client_man_client_req_rx) = tokio::sync::mpsc::unbounded_channel();
  let (client_man_server_res_tx, client_man_server_res_rx) = tokio::sync::mpsc::unbounded_channel();
  let (kinect_cmd_tx, kinect_cmd_rx) = tokio::sync::mpsc::unbounded_channel();
  let (kinect_event_tx, kinect_event_rx) = tokio::sync::mpsc::unbounded_channel();

  let mut event_loop = calloop::EventLoop::try_new().expect("could not create event loop");
  let handle = event_loop.handle();

  let signal = event_loop.get_signal();
  let cancel_token = tokio_util::sync::CancellationToken::new();

  let kinect_handle = {
    let cancel = cancel_token.child_token();
    let kinect_manager =
      tokio::task::spawn_blocking(move || kinect::KinectManager::init(kinect_event_tx, kinect_cmd_rx, cancel))
        .await
        .expect("Kinect manager task panicked during initialization");
    kinect_manager.spawn()
  };

  let mut wayland =
    wayland::Wayland::init(wayland_event_tx, wayland_cmd_rx, handle.clone()).expect("could not connect to wayland");

  let compute = gpu::Compute::init().expect("could not initialize compute module");

  let handler = bridge::Handler::new(
    compute,
    wayland_cmd_tx,
    wayland_event_rx,
    led_cmd_tx,
    led_event_rx,
    client_man_server_res_tx,
    client_man_client_req_rx,
    kinect_cmd_tx,
    kinect_event_rx,
    signal,
    cancel_token.clone(),
  );
  let handler_handle = handler.spawn();

  let led_man = led::LedManager::init(led_event_tx, led_cmd_rx, cancel_token.child_token())
    .await
    .expect("could not initialize LED manager");
  let led_man_handle = led_man.spawn();

  let client_man = client::ClientMan::init(
    client_man_client_req_tx,
    client_man_server_res_rx,
    cancel_token.child_token(),
  )
  .await
  .expect("could not initialize communication manager");
  let client_man_handle = client_man.spawn();

  tracing::info!("starting main loop");
  event_loop
    .run(std::time::Duration::from_secs(10), &mut wayland, |_| {})
    .expect("could not run event loop");

  handler_handle.await.expect("bridge task panicked");
  led_man_handle.await.expect("LED manager task panicked");
  client_man_handle.await.expect("communication manager task panicked");
  kinect_handle.await.expect("Kinect manager task panicked");

  tracing::info!("exiting");
}
