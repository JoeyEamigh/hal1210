mod freenect;
mod monitoring;
mod opencv;

#[tokio::main]
async fn main() {
  monitoring::init_logger();
  tracing::info!("starting kinect module");
}
