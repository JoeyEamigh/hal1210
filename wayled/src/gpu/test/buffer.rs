use crate::{
  gpu::Compute,
  wayland::{self, Wayland},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_copy_contains_pixels() {
  crate::monitoring::init_logger();
  let (_wayland_cmd_tx, wayland_cmd_rx) = calloop::channel::channel();
  let (wayland_event_tx, mut wayland_event_rx) = tokio::sync::mpsc::unbounded_channel();

  let mut event_loop = calloop::EventLoop::try_new().expect("could not create event loop");
  let handle = event_loop.handle();
  let signal = event_loop.get_signal();

  let mut wayland =
    Wayland::init(wayland_event_tx, wayland_cmd_rx, handle.clone()).expect("could not connect to wayland");
  let mut compute = Compute::init().expect("could not initialize compute module");

  tokio::spawn(async move {
    let mut pending_dmabuf: Option<wayland::Dmabuf> = None;

    while let Some(event) = wayland_event_rx.recv().await {
      match event {
        wayland::Event::DmabufCreated(dmabuf) => {
          pending_dmabuf = Some(dmabuf);
        }
        wayland::Event::FrameReady => {
          let Some(dmabuf) = pending_dmabuf.take() else {
            continue;
          };

          compute.set_screen_dmabuf(dmabuf).expect("failed to set screen dmabuf");

          let buffer = compute.copy_screen_to_host().expect("failed to copy dmabuf to host");
          assert!(
            buffer.iter().any(|&byte| byte != 0),
            "captured dmabuf buffer was all zeros"
          );

          println!("First 64 bytes of buffer: {:?}", &buffer[..64.min(buffer.len())]);

          signal.stop();
          return;
        }
      }
    }
  });

  event_loop
    .run(std::time::Duration::from_secs(10), &mut wayland, |_| {})
    .expect("could not run event loop");
}
