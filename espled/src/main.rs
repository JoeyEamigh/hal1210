use esp_idf_hal::prelude::*;
use log::*;

mod led;
mod usb;

static CHANNEL: usb::StateFrameChannel = embassy_sync::channel::Channel::new();

#[embassy_executor::task]
async fn run() {
  loop {
    info!("tick");
    embassy_time::Timer::after_secs(1).await;
  }
}

fn log_setup() {
  esp_idf_svc::log::EspLogger::initialize_default();

  #[cfg(debug_assertions)]
  esp_idf_svc::log::set_target_level("*", log::LevelFilter::Trace).expect("could not set log level");

  #[cfg(not(debug_assertions))]
  esp_idf_svc::log::set_target_level("*", log::LevelFilter::Warn).expect("could not set log level");

  // stfu
  esp_idf_svc::log::set_target_level("rmt(legacy)", log::LevelFilter::Warn).expect("could not set log level");
}

#[embassy_executor::main]
async fn main(spawner: embassy_executor::Spawner) {
  esp_idf_svc::sys::link_patches();
  log_setup();

  let peripherals = Peripherals::take().expect("failed to take peripherals");

  let led_pin = peripherals.pins.gpio18;
  let rmt_channel = peripherals.rmt.channel0;
  let led_driver = led::LedDriver::new(led_pin, rmt_channel);

  usb::spawn_stdin_forwarder(CHANNEL.sender());

  // spawner.spawn(run()).expect("could not spawn run task");
  spawner
    .spawn(led::frame_rx(led_driver, CHANNEL.receiver()))
    .expect("could not spawn cdc_rx task");
  #[cfg(debug_assertions)]
  spawner
    .spawn(usb::cdc_tx_emulator(CHANNEL.sender()))
    .expect("could not spawn cdc_tx_emulator task");
}
