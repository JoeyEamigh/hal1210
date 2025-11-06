use esp_idf_svc::hal::{gpio::Gpio18, rmt, sys::esp_random};
use ledcomm::NUM_LEDS;
use log::*;
use smart_leds::{
  hsv::{hsv2rgb, Hsv},
  SmartLedsWrite, RGB8,
};
use ws2812_esp32_rmt_driver::Ws2812Esp32Rmt;

use crate::usb::StateFrameReceiver;

// Receiver task: consumes full frames from the channel and processes them.
#[embassy_executor::task]
pub async fn frame_rx(mut driver: LedDriver, rx: StateFrameReceiver) {
  driver.rainbow().await;

  loop {
    let frame = rx.receive().await;

    if let Err(err) = driver.write(frame) {
      error!("frame_rx: failed to write to LEDs: {:?}", err);
    }
  }
}

pub struct LedDriver(Ws2812Esp32Rmt<'static>);

impl LedDriver {
  pub fn new(led_pin: Gpio18, rmt_channel: rmt::CHANNEL0) -> Self {
    let ws2812 = Ws2812Esp32Rmt::new(rmt_channel, led_pin).expect("failed to create WS2812 driver");
    LedDriver(ws2812)
  }

  pub fn write<T, I>(&mut self, pixels: T) -> Result<(), ws2812_esp32_rmt_driver::driver::Ws2812Esp32RmtDriverError>
  where
    T: IntoIterator<Item = I>,
    I: Into<RGB8>,
  {
    trace!("Writing {NUM_LEDS} pixels to LED strip");
    self.0.write(pixels)
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
          val: 20,
        });

        [rgb.r, rgb.g, rgb.b]
      });
      debug!("Writing rainbow frame with hue {}; len {}", hue, pixels.len());

      self.write(pixels).expect("failed to write to LEDs");

      embassy_time::Timer::after_millis(100).await;

      hue = hue.wrapping_add(10);
    }
  }
}
