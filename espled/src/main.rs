use esp_idf_hal::prelude::*;
use esp_idf_svc::hal::{
  gpio,
  spi::{self, config::Config as SpiConfig, Dma, SpiDriverConfig},
  units::Hertz,
};
use log::*;

mod led;
mod usb;

static CHANNEL: usb::StateFrameChannel = embassy_sync::channel::Channel::new();

fn log_setup() {
  esp_idf_svc::log::EspLogger::initialize_default();

  #[cfg(debug_assertions)]
  {
    esp_idf_svc::log::set_target_level("espled", log::LevelFilter::Trace).expect("could not set log level");
    esp_idf_svc::log::set_target_level("espled::led", log::LevelFilter::Trace).expect("could not set log level");
    esp_idf_svc::log::set_target_level("espled::usb", log::LevelFilter::Trace).expect("could not set log level");
  }

  #[cfg(not(debug_assertions))]
  {
    esp_idf_svc::log::set_target_level("espled", log::LevelFilter::Warn).expect("could not set log level");
    esp_idf_svc::log::set_target_level("espled::led", log::LevelFilter::Warn).expect("could not set log level");
    esp_idf_svc::log::set_target_level("espled::usb", log::LevelFilter::Warn).expect("could not set log level");
  }
}

#[embassy_executor::main]
async fn main(spawner: embassy_executor::Spawner) {
  esp_idf_svc::sys::link_patches();
  log_setup();
  info!("Starting espled");

  let peripherals = Peripherals::take().expect("failed to take peripherals");
  debug!("Peripherals taken");

  let spi_sclk = peripherals.pins.gpio19;
  let spi_mosi = peripherals.pins.gpio18;

  let spi_driver_config = SpiDriverConfig::new().dma(Dma::Auto(led::SPI_DMA_TRANSFER_BYTES));
  let spi_driver = spi::SpiDriver::new(
    peripherals.spi2,
    spi_sclk,
    spi_mosi,
    gpio::AnyInputPin::none(),
    &spi_driver_config,
  )
  .expect("failed to create SPI driver");
  debug!("SPI driver created");

  let spi_config = SpiConfig::new().baudrate(Hertz(8_000_000)).write_only(true);
  let spi_bus = spi::SpiBusDriver::new(spi_driver, &spi_config).expect("failed to attach SPI device");
  debug!("SPI bus created");

  let led_driver = led::LedDriver::new(spi_bus);
  debug!("LED driver created");

  usb::spawn_stdin_forwarder(CHANNEL.sender());

  spawner
    .spawn(led::frame_rx(led_driver, CHANNEL.receiver()))
    .expect("could not spawn cdc_rx task");

  #[cfg(feature = "emulator")]
  spawner
    .spawn(usb::cdc_tx_emulator(CHANNEL.sender()))
    .expect("could not spawn cdc_tx_emulator task");
}
