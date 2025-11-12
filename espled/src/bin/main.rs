#![no_std]
#![no_main]
#![deny(
  clippy::mem_forget,
  reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use defmt::{debug, error, info, trace, warn};
use esp_hal::{
  clock::CpuClock,
  delay::Delay,
  gpio::NoPin,
  main,
  spi::{
    self,
    master::{Config as SpiConfig, Spi},
    Mode,
  },
  time::{Instant, Rate},
  usb_serial_jtag::UsbSerialJtag,
  Blocking,
};
use ledcomm::{
  clamp_payload_len, state_frame_as_bytes_mut, WriteFeedback, BYTES_PER_LED, FRAME_LEN, HEADER_LEN, MAGIC, MAGIC_LEN,
  NUM_LEDS, STATE_BYTES,
};
use panic_rtt_target as _;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[main]
fn main() -> ! {
  rtt_target::rtt_init_defmt!();

  let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
  let esp_hal::peripherals::Peripherals {
    USB_DEVICE: usb_device,
    SPI2: spi2,
    GPIO18: gpio18,
    GPIO19: gpio19,
    ..
  } = esp_hal::init(config);

  esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 65536);

  let led_driver = match LedDriver::new(spi2, gpio18, gpio19) {
    Ok(parts) => parts,
    Err(err) => {
      error!("failed to initialise LED driver: {}", err);
      let delay = Delay::new();
      loop {
        delay.delay_millis(500u32);
      }
    }
  };

  let usb_serial = UsbSerialJtag::new(usb_device);

  info!("espled ready");

  #[cfg(feature = "emulator")]
  run_emulator(led_driver);

  #[cfg(not(feature = "emulator"))]
  frame_loop(led_driver, usb_serial);
}

#[cfg(not(feature = "emulator"))]
fn frame_loop(mut driver: LedDriver, serial: UsbSerialJtag<'static, Blocking>) -> ! {
  let (mut rx, mut tx) = serial.split();
  rx.unlisten_rx_packet_recv_interrupt();

  info!("waiting for LED frames over USB CDC (USB Serial/JTAG)");

  let mut buf = [0u8; FRAME_LEN];
  let mut have = 0usize;
  let mut pixels = ledcomm::zero_state_frame();
  let mut frame_start = Instant::now();
  let mut frame_in_progress = false;
  let mut frame_expected = 0usize;

  loop {
    let space = buf.len().saturating_sub(have);
    if space == 0 {
      warn!("frame buffer overflow; dropping {} bytes", have);
      have = 0;
      frame_in_progress = false;
      continue;
    }

    let read = rx.drain_rx_fifo(&mut buf[have..have + space]);

    if read == 0 {
      core::hint::spin_loop();
      continue;
    }

    have += read;
    trace!("ingest: read {} bytes (have={})", read, have);

    while have >= HEADER_LEN {
      if buf[..MAGIC_LEN] != MAGIC {
        trace!(
          "bad magic; shifting buffer (head={:?}, have={})",
          &buf[..MAGIC_LEN.min(have)],
          have
        );
        buf.copy_within(1..have, 0);
        have -= 1;
        continue;
      }

      let declared_len = u16::from_le_bytes([buf[4], buf[5]]) as usize;
      let frame_size = HEADER_LEN + declared_len;
      if !frame_in_progress {
        frame_start = Instant::now();
        frame_in_progress = true;
        frame_expected = frame_size;
      }

      if frame_size > FRAME_LEN {
        warn!("declared frame too large: {}", frame_size);
        have = 0;
        frame_in_progress = false;
        break;
      }

      if have < frame_size {
        break;
      }

      let payload_len = clamp_payload_len(declared_len);
      if payload_len != declared_len {
        warn!(
          "payload length {} trimmed to {} (must be multiple of {})",
          declared_len, payload_len, BYTES_PER_LED
        );
      }

      let rx_elapsed_us = frame_start.elapsed().as_micros() as u32;
      frame_in_progress = false;

      let copy_start = Instant::now();
      {
        let frame_bytes = state_frame_as_bytes_mut(&mut pixels);
        frame_bytes[..payload_len].copy_from_slice(&buf[HEADER_LEN..HEADER_LEN + payload_len]);
        frame_bytes[payload_len..STATE_BYTES].fill(0u8);
      }
      let copy_elapsed_us = copy_start.elapsed().as_micros() as u32;

      let dispatch_start = Instant::now();
      let pixel_count = payload_len / BYTES_PER_LED;
      debug!("dispatching USB frame with {} LEDs", pixel_count);

      if let Err(err) = driver.write_pixels(&pixels) {
        error!("SPI write failed: {:?}", err);
      }
      let dispatch_elapsed_us = dispatch_start.elapsed().as_micros() as u32;

      let total_elapsed_us = rx_elapsed_us
        .saturating_add(copy_elapsed_us)
        .saturating_add(dispatch_elapsed_us);
      trace!(
        "frame timings: ingest={=u32}us copy={=u32}us spi={=u32}us total={=u32}us expected_bytes={=usize}",
        rx_elapsed_us,
        copy_elapsed_us,
        dispatch_elapsed_us,
        total_elapsed_us,
        frame_expected
      );

      let feedback = WriteFeedback::new(
        rx_elapsed_us,
        copy_elapsed_us,
        dispatch_elapsed_us,
        total_elapsed_us,
        frame_size,
      );
      if let Err(_err) = tx.write(feedback.as_bytes()) {
        warn!("failed to send write feedback over USB");
      }

      let remaining = have - frame_size;
      if remaining > 0 {
        buf.copy_within(frame_size..frame_size + remaining, 0);
      }
      have = remaining;
      frame_start = Instant::now();
      frame_expected = 0;
    }
  }
}

#[cfg(feature = "emulator")]
fn run_emulator(mut driver: LedDriver) -> ! {
  use smart_leds::hsv::{hsv2rgb, Hsv};

  info!("starting rainbow emulator");

  let delay = Delay::new();
  let mut hue = 0u8;
  let mut pixels = ledcomm::zero_state_frame();

  loop {
    for (idx, pixel) in pixels.iter_mut().enumerate() {
      let offset = ((idx as u16 * 10) & 0xFF) as u8;
      let rgb = hsv2rgb(Hsv {
        hue: hue.wrapping_add(offset),
        sat: 255,
        val: 255,
      });
      *pixel = [rgb.r, rgb.g, rgb.b];
    }

    if let Err(err) = driver.write_pixels(&pixels) {
      error!("SPI write failed: {:?}", err);
    }

    hue = hue.wrapping_add(3);
    delay.delay_millis(16u32);
  }
}

const START_FRAME_BYTES: usize = 4;
const SK9822_LATCH_BYTES: usize = 4;
const BYTES_PER_APA102_PIXEL: usize = 4;
const END_FRAME_BYTES: usize = NUM_LEDS.div_ceil(16);
const SK9822_FRAME_BYTES: usize =
  START_FRAME_BYTES + (NUM_LEDS * BYTES_PER_APA102_PIXEL) + SK9822_LATCH_BYTES + END_FRAME_BYTES;
const SK9822_FRAME_ALIGNED_BYTES: usize = align_to_word(SK9822_FRAME_BYTES);
const LED_BRIGHTNESS: u8 = 0x1F;

const fn align_to_word(n: usize) -> usize {
  (n + 3) & !3
}

struct LedDriver {
  spi: Spi<'static, Blocking>,
  frame: [u8; SK9822_FRAME_ALIGNED_BYTES],
}

#[derive(defmt::Format)]
enum LedInitError {
  SpiConfig(spi::master::ConfigError),
}

impl LedDriver {
  fn new(
    spi2: esp_hal::peripherals::SPI2<'static>,
    gpio18: esp_hal::peripherals::GPIO18<'static>,
    gpio19: esp_hal::peripherals::GPIO19<'static>,
  ) -> Result<Self, LedInitError> {
    let spi_config = SpiConfig::default()
      .with_frequency(Rate::from_hz(8_000_000))
      .with_mode(Mode::_0);

    let spi = Spi::new(spi2, spi_config)
      .map_err(LedInitError::SpiConfig)?
      .with_sck(gpio19)
      .with_mosi(gpio18)
      .with_miso(NoPin)
      .with_cs(NoPin);

    Ok(Self {
      spi,
      frame: [0u8; SK9822_FRAME_ALIGNED_BYTES],
    })
  }

  fn write_pixels(&mut self, pixels: &[[u8; BYTES_PER_LED]]) -> Result<(), spi::Error> {
    let frame_len = Self::encode_pixels(&mut self.frame, pixels);
    self.spi.write(&self.frame[..frame_len])
  }

  fn encode_pixels(frame: &mut [u8; SK9822_FRAME_ALIGNED_BYTES], pixels: &[[u8; BYTES_PER_LED]]) -> usize {
    let mut offset = 0usize;

    frame[offset..offset + START_FRAME_BYTES].fill(0);
    offset += START_FRAME_BYTES;

    for [r, g, b] in pixels.iter().take(NUM_LEDS) {
      frame[offset] = 0b1110_0000 | LED_BRIGHTNESS;
      frame[offset + 1] = *b;
      frame[offset + 2] = *g;
      frame[offset + 3] = *r;
      offset += BYTES_PER_APA102_PIXEL;
    }

    frame[offset..offset + SK9822_LATCH_BYTES].fill(0);
    offset += SK9822_LATCH_BYTES;

    frame[offset..offset + END_FRAME_BYTES].fill(0);
    offset += END_FRAME_BYTES;

    if offset < frame.len() {
      frame[offset..].fill(0);
    }

    offset
  }
}
