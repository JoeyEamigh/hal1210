mod bridge;
mod gpu;
mod led;
mod monitoring;
mod wayland;

#[tokio::main]
async fn main() {
  monitoring::init_logger();
  tracing::info!("starting wayled");

  let (cmd_tx, cmd_rx) = calloop::channel::channel();
  let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();

  let mut event_loop = calloop::EventLoop::try_new().expect("could not create event loop");
  let handle = event_loop.handle();
  let signal = event_loop.get_signal();

  let mut wayland = wayland::Wayland::init(event_tx, cmd_rx, handle.clone()).expect("could not connect to wayland");

  let compute = gpu::Compute::init().expect("could not initialize compute module");

  let handler = bridge::Handler::new(compute, cmd_tx, event_rx, signal);
  let handler_handle = handler.spawn();

  tracing::info!("starting main loop");
  event_loop
    .run(std::time::Duration::from_secs(10), &mut wayland, |_| {})
    .expect("could not run event loop");

  handler_handle.await.expect("bridge task panicked");
  tracing::info!("exiting");
}
