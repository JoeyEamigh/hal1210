use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use ledcomm::{BYTES_PER_LED, NUM_LEDS};

use crate::{Color, LedCommand};

impl Serialize for LedCommand {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    #[derive(Serialize)]
    #[serde(tag = "command", content = "args", rename_all = "camelCase")]
    enum Helper<'a> {
      SetStaticColor(&'a Color),
      SetStripState(&'a serde_bytes::Bytes),
      FadeIn(&'a serde_bytes::Bytes),
      FadeOut,
      Rainbow,
      Breathing(&'a Color),
    }

    let encode_frame = |frame: &ledcomm::StateFrame| {
      let bytes: &[u8] = unsafe { core::slice::from_raw_parts(frame.as_ptr() as *const u8, NUM_LEDS * BYTES_PER_LED) };
      serde_bytes::Bytes::new(bytes)
    };

    match self {
      LedCommand::SetStaticColor(color) => Helper::SetStaticColor(color).serialize(serializer),
      LedCommand::SetStripState(frame) => {
        let encoded = encode_frame(frame);
        Helper::SetStripState(encoded).serialize(serializer)
      }
      LedCommand::FadeIn(frame) => {
        let encoded = encode_frame(frame);
        Helper::FadeIn(encoded).serialize(serializer)
      }
      LedCommand::FadeOut => Helper::FadeOut.serialize(serializer),
      LedCommand::Rainbow => Helper::Rainbow.serialize(serializer),
      LedCommand::Breathing(color) => Helper::Breathing(color).serialize(serializer),
    }
  }
}

impl<'de> Deserialize<'de> for LedCommand {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    #[derive(Deserialize)]
    #[serde(tag = "command", content = "args", rename_all = "camelCase")]
    enum Helper {
      SetStaticColor(Color),
      SetStripState(serde_bytes::ByteBuf),
      FadeIn(serde_bytes::ByteBuf),
      FadeOut,
      Rainbow,
      Breathing(Color),
    }

    let helper = Helper::deserialize(deserializer)?;

    let decode_frame = |bytes: serde_bytes::ByteBuf| -> Result<ledcomm::StateFrame, D::Error> {
      let expected_len = NUM_LEDS * BYTES_PER_LED;
      if bytes.len() != expected_len {
        return Err(D::Error::invalid_length(
          bytes.len(),
          &format!("expected {} bytes for StateFrame", expected_len).as_str(),
        ));
      }

      let mut frame = [[0u8; BYTES_PER_LED]; NUM_LEDS];
      for (i, chunk) in bytes.chunks_exact(BYTES_PER_LED).enumerate() {
        frame[i].copy_from_slice(chunk);
      }
      Ok(frame)
    };

    match helper {
      Helper::SetStaticColor(color) => Ok(LedCommand::SetStaticColor(color)),
      Helper::SetStripState(encoded) => Ok(LedCommand::SetStripState(decode_frame(encoded)?)),
      Helper::FadeIn(encoded) => Ok(LedCommand::FadeIn(decode_frame(encoded)?)),
      Helper::FadeOut => Ok(LedCommand::FadeOut),
      Helper::Rainbow => Ok(LedCommand::Rainbow),
      Helper::Breathing(color) => Ok(LedCommand::Breathing(color)),
    }
  }
}
