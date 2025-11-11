#![allow(clippy::field_reassign_with_default)]

use std::{
  env,
  error::Error,
  fmt::Write as FmtWrite,
  io::{self, Write},
};

use ledcomm::{Frame, BYTES_PER_LED, MAGIC, NUM_LEDS};

fn main() {
  if let Err(err) = run() {
    eprintln!("rainbow_packet: {err}");
    std::process::exit(1);
  }
}

enum OutputFormat {
  Hex,
  Raw,
}

fn run() -> Result<(), Box<dyn Error>> {
  let format = if env::args().any(|arg| arg == "--raw") {
    OutputFormat::Raw
  } else {
    OutputFormat::Hex
  };

  let mut frame = Frame::default();
  frame.magic = MAGIC;

  let pixel_count = NUM_LEDS;
  frame.len = (pixel_count * BYTES_PER_LED) as u16;

  for (idx, pixel) in frame.data.iter_mut().take(pixel_count).enumerate() {
    let hue = ((idx * 10) % 256) as u8;
    *pixel = hsv_to_rgb(hue, 255, 255);
  }

  let packet = frame.packet();

  match format {
    OutputFormat::Raw => {
      let mut stdout = io::stdout().lock();
      stdout.write_all(packet)?;
    }
    OutputFormat::Hex => {
      let mut buffer = String::with_capacity(packet.len() * 2);
      for byte in packet {
        buffer.write_fmt(format_args!("{byte:02X}"))?;
      }
      println!("{buffer}");
    }
  }

  Ok(())
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
