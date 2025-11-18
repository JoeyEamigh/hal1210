use crate::{
  gpu::*,
  wayland::{self, Wayland},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn border_averages() {
  // crate::monitoring::init_logger();
  let (wayland_cmd_tx, wayland_cmd_rx) = calloop::channel::channel();
  let (wayland_event_tx, mut wayland_event_rx) = tokio::sync::mpsc::unbounded_channel();

  let mut event_loop = calloop::EventLoop::try_new().expect("could not create event loop");
  let handle = event_loop.handle();

  let signal = event_loop.get_signal();

  let mut wayland =
    Wayland::init(wayland_event_tx, wayland_cmd_rx, handle.clone()).expect("could not connect to wayland");

  let mut compute = Compute::init().expect("could not initialize compute module");

  println!("About to spawn");
  tokio::spawn(async move {
    println!("Spawned");
    loop {
      if let Some(wayland::Event::DmabufCreated(dmabuf)) = wayland_event_rx.recv().await {
        println!("Got dmabuf thing");
        compute.set_screen_dmabuf(dmabuf).expect("failed to set screen dmabuf");

        println!("set the dmabuf thing");
        match compute.dispatch() {
          Ok(compute_rx) => {
            println!("Got compute result");
            match compute_rx.await {
              Ok(result) => {
                println!("Got shader result");
                if let Some(color) = result.average_rgb_u8() {
                  println!("is color: {color:?}");

                  assert!(true);

                  signal.stop();
                  return;
                } else {
                  println!("is not color");
                }
              }
              Err(err) => {
                println!("Error in dispatching compute buffer");
                println!("{err}");
              }
            };
          }
          other => println!("compute dispatch unexpected result: {other:?}"),
        }
      }
    }
  });

  println!("After spawn, before event loop");

  event_loop
    .run(std::time::Duration::from_secs(10), &mut wayland, |_| {})
    .expect("could not run event loop");
}
