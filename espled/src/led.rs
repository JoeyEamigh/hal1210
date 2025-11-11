use apa102_spi::{Apa102Pixel, Apa102WriterAsync, PixelOrder, SmartLedsWriteAsync};
use esp_idf_svc::hal::{spi, sys::esp_random};
use ledcomm::NUM_LEDS;
use log::*;
use smart_leds::{
  hsv::{hsv2rgb, Hsv},
  RGB8,
};

use crate::usb::StateFrameReceiver;

type SpiBus = spi::SpiBusDriver<'static, spi::SpiDriver<'static>>;

pub const SK9822_FRAME_BYTES: usize = NUM_LEDS * 4 + START_END_BYTES + END_FRAME_BYTES;
const START_END_BYTES: usize = 8;
const END_FRAME_BYTES: usize = NUM_LEDS.div_ceil(16);
pub const SPI_DMA_TRANSFER_BYTES: usize = align4(SK9822_FRAME_BYTES);

const fn align4(n: usize) -> usize {
  (n + 3) & !3
}

pub type LedError = spi::SpiError;

// Receiver task: consumes full frames from the channel and processes them.
#[embassy_executor::task]
pub async fn frame_rx(mut driver: LedDriver, rx: StateFrameReceiver) {
  debug!("frame_rx: starting task");
  // driver.rainbow().await;

  loop {
    let frame = rx.receive().await;
    trace!("frame_rx: received frame with len {}", frame.len());

    let pixels = frame.into_iter().map(|[r, g, b]| RGB8 { r, g, b });
    if let Err(err) = driver.write(pixels).await {
      error!("frame_rx: failed to write to LEDs: {:?}", err);
    }
  }
}

pub struct LedDriver(Apa102WriterAsync<SpiBus>);

impl LedDriver {
  pub fn new(spi_bus: SpiBus) -> Self {
    let writer = Apa102WriterAsync::new(spi_bus, NUM_LEDS, PixelOrder::BGR);
    LedDriver(writer)
  }

  pub async fn write<T>(&mut self, pixels: T) -> Result<(), LedError>
  where
    T: IntoIterator<Item = RGB8>,
  {
    trace!("Writing {NUM_LEDS} pixels to LED strip");
    let mapped = pixels.into_iter().map(Apa102Pixel::from);
    self.0.write(mapped).await
  }

  #[allow(dead_code)]
  pub async fn rainbow(&mut self) {
    let mut hue = unsafe { esp_random() as u8 };
    const PIXEL_COUNT: usize = NUM_LEDS;

    loop {
      let pixels = (0..PIXEL_COUNT).map(|i: usize| {
        let pixel_hue: u8 = ((hue as usize + (i * 10)) % 256) as u8;

        let rgb = hsv2rgb(Hsv {
          hue: pixel_hue,
          sat: 255,
          val: 255,
        });

        RGB8 {
          r: rgb.r,
          g: rgb.g,
          b: rgb.b,
        }
      });
      debug!("Writing rainbow frame with hue {}; len {}", hue, pixels.len());

      self.write(pixels).await.expect("failed to write to LEDs");

      embassy_time::Timer::after_millis(100).await;

      hue = hue.wrapping_add(10);
    }
  }
}
