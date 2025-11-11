#![no_std]

use core::{
  fmt,
  ops::{Deref, DerefMut},
};

pub const MAGIC: [u8; 4] = *b"LEDS";
pub const MAGIC_LEN: usize = MAGIC.len();
pub const LEN_LEN: usize = 2;
pub const HEADER_LEN: usize = MAGIC_LEN + LEN_LEN;
pub const BYTES_PER_LED: usize = 3;
pub const NUM_LEDS: usize = 60 * 5; // 60 LEDs per meter, 5 meters
pub const STATE_BYTES: usize = NUM_LEDS * BYTES_PER_LED;
pub const FRAME_LEN: usize = HEADER_LEN + STATE_BYTES;

pub type Pixel = [u8; BYTES_PER_LED];
pub type StateFrame = [Pixel; NUM_LEDS];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Frame {
  pub magic: [u8; MAGIC_LEN],
  pub len: u16,
  pub data: StateFrame,
}

impl Frame {
  pub fn new(magic: [u8; MAGIC_LEN], len: u16) -> Self {
    Self {
      magic,
      len,
      ..Default::default()
    }
  }
}

impl Default for Frame {
  fn default() -> Self {
    Self {
      magic: [0u8; MAGIC_LEN],
      len: 0u16,
      data: [[0u8; BYTES_PER_LED]; NUM_LEDS],
    }
  }
}

const _: [u8; FRAME_LEN] = [0u8; core::mem::size_of::<Frame>()];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
  PayloadTooLarge,
  MisalignedPayload,
}

impl fmt::Display for FrameError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let msg = match self {
      FrameError::PayloadTooLarge => "frame payload exceeds maximum capacity",
      FrameError::MisalignedPayload => "frame payload length must be a multiple of BYTES_PER_LED",
    };
    f.write_str(msg)
  }
}

#[inline]
pub fn state_frame_as_bytes(frame: &StateFrame) -> &[u8] {
  // SAFETY: StateFrame is a tightly packed array of byte triples.
  unsafe { core::slice::from_raw_parts(frame.as_ptr() as *const u8, STATE_BYTES) }
}

#[inline]
pub fn state_frame_as_bytes_mut(frame: &mut StateFrame) -> &mut [u8] {
  // SAFETY: StateFrame is a tightly packed array of byte triples.
  unsafe { core::slice::from_raw_parts_mut(frame.as_mut_ptr() as *mut u8, STATE_BYTES) }
}

impl Frame {
  #[inline]
  pub fn as_bytes(&self) -> &[u8] {
    // SAFETY: Frame is repr(C) and consists solely of byte-sized fields.
    unsafe { core::slice::from_raw_parts((self as *const Frame).cast(), FRAME_LEN) }
  }

  #[inline]
  pub fn as_bytes_mut(&mut self) -> &mut [u8] {
    // SAFETY: Frame is repr(C) and consists solely of byte-sized fields.
    unsafe { core::slice::from_raw_parts_mut((self as *mut Frame).cast(), FRAME_LEN) }
  }

  #[inline]
  pub fn packet(&self) -> &[u8] {
    let used = HEADER_LEN + self.len as usize;
    &self.as_bytes()[..used.min(FRAME_LEN)]
  }

  #[inline]
  pub fn packet_mut(&mut self) -> &mut [u8] {
    let used = HEADER_LEN + self.len as usize;
    &mut self.as_bytes_mut()[..used.min(FRAME_LEN)]
  }

  #[inline]
  pub fn pixel_count(&self) -> usize {
    (self.len as usize) / BYTES_PER_LED
  }

  #[inline]
  pub fn pixels(&self) -> &[Pixel] {
    &self.data[..self.pixel_count().min(NUM_LEDS)]
  }

  #[inline]
  pub fn pixels_mut(&mut self) -> &mut [Pixel] {
    let count = self.pixel_count().min(NUM_LEDS);
    &mut self.data[..count]
  }

  #[inline]
  pub fn payload_bytes(&self) -> &[u8] {
    &self.packet()[HEADER_LEN..]
  }

  #[inline]
  pub fn payload_bytes_mut(&mut self) -> &mut [u8] {
    &mut self.packet_mut()[HEADER_LEN..]
  }

  pub fn write_pixels(&mut self, pixels: &[[u8; BYTES_PER_LED]]) -> Result<(), FrameError> {
    if pixels.len() > NUM_LEDS {
      return Err(FrameError::PayloadTooLarge);
    }

    self.magic = MAGIC;
    self.len = (pixels.len() * BYTES_PER_LED) as u16;
    self.data[..pixels.len()].copy_from_slice(pixels);
    for slot in &mut self.data[pixels.len()..] {
      *slot = [0u8; BYTES_PER_LED];
    }

    Ok(())
  }

  pub fn write_pixels_iter<I>(&mut self, iter: I) -> Result<(), FrameError>
  where
    I: IntoIterator<Item = [u8; BYTES_PER_LED]>,
  {
    self.magic = MAGIC;
    let mut count = 0usize;

    for pixel in iter.into_iter() {
      if count >= NUM_LEDS {
        return Err(FrameError::PayloadTooLarge);
      }
      self.data[count] = pixel;
      count += 1;
    }

    for slot in &mut self.data[count..] {
      *slot = [0u8; BYTES_PER_LED];
    }

    self.len = (count * BYTES_PER_LED) as u16;
    Ok(())
  }

  pub fn write_payload_bytes(&mut self, payload: &[u8]) -> Result<(), FrameError> {
    if payload.len() > STATE_BYTES {
      return Err(FrameError::PayloadTooLarge);
    }
    #[allow(clippy::manual_is_multiple_of)]
    if payload.len() % BYTES_PER_LED != 0 {
      return Err(FrameError::MisalignedPayload);
    }

    self.magic = MAGIC;
    self.len = payload.len() as u16;

    let frame_bytes = state_frame_as_bytes_mut(&mut self.data);
    frame_bytes[..payload.len()].copy_from_slice(payload);
    frame_bytes[payload.len()..].fill(0u8);

    Ok(())
  }

  pub fn from_pixels(pixels: &[[u8; BYTES_PER_LED]]) -> Result<Self, FrameError> {
    let mut frame = Self::default();
    frame.write_pixels(pixels)?;
    Ok(frame)
  }

  pub fn from_payload_bytes(payload: &[u8]) -> Result<Self, FrameError> {
    let mut frame = Self::default();
    frame.write_payload_bytes(payload)?;
    Ok(frame)
  }

  #[inline]
  pub fn is_magic_valid(&self) -> bool {
    self.magic == MAGIC
  }
}

impl Deref for Frame {
  type Target = [u8];

  fn deref(&self) -> &Self::Target {
    self.as_bytes()
  }
}

impl DerefMut for Frame {
  fn deref_mut(&mut self) -> &mut Self::Target {
    self.as_bytes_mut()
  }
}

impl AsRef<[u8]> for Frame {
  fn as_ref(&self) -> &[u8] {
    self.as_bytes()
  }
}

impl AsMut<[u8]> for Frame {
  fn as_mut(&mut self) -> &mut [u8] {
    self.as_bytes_mut()
  }
}

#[inline]
pub fn zero_state_frame() -> StateFrame {
  [[0u8; BYTES_PER_LED]; NUM_LEDS]
}

#[inline]
pub fn clamp_payload_len(len: usize) -> usize {
  let capped = len.min(STATE_BYTES);
  capped - (capped % BYTES_PER_LED)
}
