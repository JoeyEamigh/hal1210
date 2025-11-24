use serde::de::Error as DeError;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use ledcomm::{BYTES_PER_LED, NUM_LEDS};

use crate::{Color, LedCommand};

impl Serialize for LedCommand {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    let mut map = serializer.serialize_map(None)?;
    match self {
      LedCommand::SetStaticColor(color) => {
        map.serialize_entry("command", "setStaticColor")?;
        map.serialize_entry("args", color)?;
      }
      LedCommand::SetStripState(frame) => {
        map.serialize_entry("command", "setStripState")?;
        let encoded = encode_frame(frame);
        map.serialize_entry("args", &encoded)?;
      }
      LedCommand::FadeIn { state, duration_ms } => {
        map.serialize_entry("command", "fadeIn")?;
        let encoded = encode_frame(state);
        let args = FadeInArgsSer {
          state: encoded,
          duration_ms,
        };
        map.serialize_entry("args", &args)?;
      }
      LedCommand::FadeOut { duration_ms } => {
        map.serialize_entry("command", "fadeOut")?;
        if duration_ms.is_some() {
          let args = FadeOutArgsSer { duration_ms };
          map.serialize_entry("args", &args)?;
        }
      }
      LedCommand::Rainbow => {
        map.serialize_entry("command", "rainbow")?;
      }
      LedCommand::Breathing(color) => {
        map.serialize_entry("command", "breathing")?;
        map.serialize_entry("args", color)?;
      }
    }

    map.end()
  }
}

fn encode_frame(frame: &ledcomm::StateFrame) -> &serde_bytes::Bytes {
  let bytes: &[u8] = unsafe { core::slice::from_raw_parts(frame.as_ptr() as *const u8, NUM_LEDS * BYTES_PER_LED) };
  serde_bytes::Bytes::new(bytes)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FadeInArgsSer<'a> {
  state: &'a serde_bytes::Bytes,
  #[serde(skip_serializing_if = "Option::is_none")]
  duration_ms: &'a Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FadeOutArgsSer<'a> {
  #[serde(skip_serializing_if = "Option::is_none")]
  duration_ms: &'a Option<u64>,
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
      FadeIn(FadeInArgsDe),
      FadeOut(#[serde(default)] FadeOutArgsDe),
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
      Helper::FadeIn(args) => match args {
        FadeInArgsDe::Legacy(encoded) => Ok(LedCommand::FadeIn {
          state: decode_frame(encoded)?,
          duration_ms: None,
        }),
        FadeInArgsDe::Detailed(data) => Ok(LedCommand::FadeIn {
          state: decode_frame(data.state)?,
          duration_ms: data.duration_ms,
        }),
      },
      Helper::FadeOut(args) => Ok(LedCommand::FadeOut {
        duration_ms: args.duration_ms,
      }),
      Helper::Rainbow => Ok(LedCommand::Rainbow),
      Helper::Breathing(color) => Ok(LedCommand::Breathing(color)),
    }
  }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FadeInArgsDe {
  Legacy(serde_bytes::ByteBuf),
  Detailed(FadeInDetailedArgs),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FadeInDetailedArgs {
  state: serde_bytes::ByteBuf,
  #[serde(default)]
  duration_ms: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FadeOutArgsDe {
  #[serde(default)]
  duration_ms: Option<u64>,
}
