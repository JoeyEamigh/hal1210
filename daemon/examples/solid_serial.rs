#![allow(clippy::field_reassign_with_default)]

use std::{
  env,
  error::Error,
  io::{Read, Write},
  thread,
  time::Duration,
};

use ledcomm::{BYTES_PER_LED, Frame, MAGIC, NUM_LEDS, WRITE_FEEDBACK_LEN, parse_write_feedback};

fn main() -> Result<(), Box<dyn Error>> {
  let port_path = env::args().nth(1).unwrap_or_else(|| "/dev/ttyACM0".to_string());
  println!("Connecting to serial port {port_path}");

  let mut port = serialport::new(&port_path, 2_000_000)
    .timeout(Duration::from_millis(100))
    .open()?;
  println!("Connected to {port_path}");

  let mut frame = Frame::default();
  frame.magic = MAGIC;
  frame.len = (NUM_LEDS * BYTES_PER_LED) as u16;

  let mut hue: u8 = 0;

  loop {
    let base_hue = hue;
    hue = hue.wrapping_add(10);

    for (idx, pixel) in frame.data.iter_mut().enumerate() {
      if idx < 7 {
        *pixel = [0, 0, 255];
        continue;
      }

      *pixel = [255, 255, 0];
    }

    println!("writing packet with hue {base_hue}");
    let now = std::time::Instant::now();

    let packet = frame.packet();
    port.write_all(packet)?;
    port.flush()?;

    let mut feedback = None;
    let mut rx_buf: Vec<u8> = Vec::with_capacity(WRITE_FEEDBACK_LEN * 2);
    let mut tmp = [0u8; WRITE_FEEDBACK_LEN];
    while feedback.is_none() {
      match port.read(&mut tmp) {
        Ok(0) => continue,
        Ok(read) => {
          rx_buf.extend_from_slice(&tmp[..read]);
          if let Some((fb, consumed)) = parse_write_feedback(&rx_buf) {
            feedback = Some(fb);
            rx_buf.drain(..consumed);
          } else if rx_buf.len() > WRITE_FEEDBACK_LEN * 4 {
            let drop = rx_buf.len().saturating_sub(WRITE_FEEDBACK_LEN * 2);
            rx_buf.drain(..drop);
          }
        }
        Err(ref err) if err.kind() == std::io::ErrorKind::TimedOut => continue,
        Err(err) => return Err(err.into()),
      }
    }

    if let Some(fb) = feedback {
      println!(
        "device: ingest={}ms copy={}ms spi={}ms total={}ms frame={} bytes",
        fb.ingest_us as f32 / 1000.0,
        fb.copy_us as f32 / 1000.0,
        fb.spi_us as f32 / 1000.0,
        fb.total_us as f32 / 1000.0,
        fb.frame_len()
      );
    }

    let elapsed = now.elapsed();
    println!("wrote {} bytes in {:?}", packet.len(), elapsed);

    thread::sleep(Duration::from_millis(1_000));
  }
}
