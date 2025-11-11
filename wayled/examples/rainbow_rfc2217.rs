#![allow(clippy::field_reassign_with_default)]

use std::{env, error::Error, io::Write, thread, time::Duration};

use ledcomm::{Frame, BYTES_PER_LED, MAGIC, NUM_LEDS};

fn main() -> Result<(), Box<dyn Error>> {
  let port_path = env::args().nth(1).unwrap_or_else(|| "/dev/ttyNVT0".to_string());
  eprintln!("Connecting to serial port {port_path}");

  let mut port = serialport::new(&port_path, 115_200)
    .timeout(Duration::from_millis(100))
    .open()?;

  let mut frame = Frame::default();
  frame.magic = MAGIC;
  frame.len = (NUM_LEDS * BYTES_PER_LED) as u16;

  let mut hue: u8 = 0;

  loop {
    let base_hue = hue;
    hue = hue.wrapping_add(10);

    for (idx, pixel) in frame.data.iter_mut().enumerate() {
      let offset = ((idx as u16 * 10) % 256) as u8;
      let pixel_hue = base_hue.wrapping_add(offset);
      *pixel = hsv_to_rgb(pixel_hue, 255, 255);
    }

    let packet = frame.packet();
    port.write_all(packet)?;
    port.flush()?;

    thread::sleep(Duration::from_millis(16));
  }
}

fn hsv_to_rgb(hue: u8, sat: u8, val: u8) -> [u8; 3] {
  if sat == 0 {
    return [val, val, val];
  }

  let h = (hue as f32 / 255.0) * 6.0;
  let s = sat as f32 / 255.0;
  let v = val as f32 / 255.0;

  let sector = h.floor() as i32;
  let fraction = h - sector as f32;

  let p = v * (1.0 - s);
  let q = v * (1.0 - s * fraction);
  let t = v * (1.0 - s * (1.0 - fraction));

  let (r, g, b) = match sector.rem_euclid(6) {
    0 => (v, t, p),
    1 => (q, v, p),
    2 => (p, v, t),
    3 => (p, q, v),
    4 => (t, p, v),
    _ => (v, p, q),
  };

  [scale_channel(r), scale_channel(g), scale_channel(b)]
}

fn scale_channel(channel: f32) -> u8 {
  (channel * 255.0).round().clamp(0.0, 255.0) as u8
}
