use std::thread;

use embassy_sync::{
  blocking_mutex::raw::CriticalSectionRawMutex,
  channel::{Channel, Receiver, Sender, TrySendError},
};
use esp_idf_svc::sys;
use log::*;
use ledcomm::{state_frame_as_bytes_mut, Frame, StateFrame, FRAME_LEN, HEADER_LEN, MAGIC, MAGIC_LEN};

pub type StateFrameSender = Sender<'static, CriticalSectionRawMutex, StateFrame, 1>;
pub type StateFrameReceiver = Receiver<'static, CriticalSectionRawMutex, StateFrame, 1>;
pub type StateFrameChannel = Channel<CriticalSectionRawMutex, StateFrame, 1>;

/// Spawn a dedicated thread that performs blocking reads from the USB CDC driver,
/// frames data, and forwards complete frames into the Embassy channel.
pub fn spawn_stdin_forwarder(tx: StateFrameSender) {
  thread::Builder::new()
    .name("stdin_forwarder".into())
    .spawn(move || {
      let mut buf = Frame::default();
      let mut have = 0usize;

      unsafe {
        let mut cfg = sys::usb_serial_jtag_driver_config_t {
          tx_buffer_size: FRAME_LEN as u32,
          rx_buffer_size: FRAME_LEN as u32,
        };
        let res = sys::usb_serial_jtag_driver_install(&mut cfg as *mut _);
        if res != sys::ESP_OK && res != sys::ESP_ERR_INVALID_STATE {
          warn!("stdin_forwarder: driver install error: {}", res);
        }
      }

      loop {
        let space = buf.len() - have;
        if space == 0 {
          warn!("stdin_forwarder: buffer full without frame, resetting");
          have = 0;
          continue;
        }

        let read =
          unsafe { sys::usb_serial_jtag_read_bytes(buf[have..].as_mut_ptr() as *mut _, space as u32, u32::MAX) };

        if read < 0 {
          warn!("stdin_forwarder: usb read error: {}", read);
          have = 0;
          thread::yield_now();
          continue;
        } else if read == 0 {
          thread::yield_now();
          continue;
        }

        let n = read as usize;
        debug!("stdin_forwarder: read {} bytes", n);
        trace!("stdin_forwarder: data: {:02X?}", &buf[have..have + n]);
        have += n;

        while have >= HEADER_LEN {
          if &buf[..MAGIC_LEN] != MAGIC.as_slice() {
            buf.copy_within(1..have, 0);
            have -= 1;
            continue;
          }

          let size = u16::from_le_bytes([buf[4], buf[5]]) as usize;
          let need = HEADER_LEN + size;
          if need > FRAME_LEN {
            warn!("stdin_forwarder: frame length {} too large", need);
            have = 0;
            break;
          }
          if have < need {
            break;
          }

          let mut frame = ledcomm::zero_state_frame();
          {
            let frame_bytes = state_frame_as_bytes_mut(&mut frame);
            frame_bytes[..size].copy_from_slice(&buf[HEADER_LEN..need]);
            frame_bytes[size..].fill(0u8);
          }

          let mut value = frame;
          while let Err(TrySendError::Full(v)) = tx.try_send(value) {
            value = v;
            thread::yield_now();
          }

          let rem = have - need;
          if rem > 0 {
            buf.copy_within(need..need + rem, 0);
          }
          have = rem;
        }
      }
    })
    .expect("could not spawn stdin_forwarder thread");
}

// for emulator: every 5 seconds send a prebuilt frame into the channel
#[cfg(debug_assertions)]
#[embassy_executor::task]
pub async fn cdc_tx_emulator(tx: StateFrameSender) {
  let mut frame = ledcomm::zero_state_frame();

  // build a single repeating frame
  for (i, pixel) in frame.iter_mut().enumerate() {
    let r = ((i % 32) * 8) as u8;
    let g = ((i % 16) * 16) as u8;
    let b = ((i % 8) * 32) as u8;
    *pixel = [r, g, b];
  }

  loop {
    tx.send(frame).await;
    trace!("cdc_tx_emulator: sent frame into channel");

    embassy_time::Timer::after_secs(5).await;
  }
}
