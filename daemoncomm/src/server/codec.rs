use rmp_serde::Serializer;
use serde::Serialize;
use std::io;
use tokio_util::bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::codec::{HEADER_LEN, MAGIC};
use crate::{MessageToClient, MessageToServer};

pub struct Hal1210ServerCodec;

impl Decoder for Hal1210ServerCodec {
  type Item = MessageToServer;
  type Error = Error;

  fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
    if src.len() < HEADER_LEN {
      return Ok(None);
    }

    if &src[0..7] != MAGIC {
      return Err(Error::InvalidMagic);
    }

    let len = u32::from_be_bytes([0, src[7], src[8], src[9]]) as usize;

    if src.len() < HEADER_LEN + len {
      src.reserve(HEADER_LEN + len - src.len());
      return Ok(None);
    }

    let data = &src[HEADER_LEN..HEADER_LEN + len];
    tracing::trace!(target: "hal1210::com::decoder", "decoding {} bytes", len);

    let item = rmp_serde::from_slice(data).map_err(Error::Deserialize)?;

    src.advance(HEADER_LEN + len);

    Ok(Some(item))
  }
}

impl Encoder<MessageToClient> for Hal1210ServerCodec {
  type Error = Error;

  fn encode(&mut self, item: MessageToClient, dst: &mut BytesMut) -> Result<(), Self::Error> {
    let mut data = Vec::new();
    let mut serializer = Serializer::new(&mut data).with_struct_map();
    item.serialize(&mut serializer).map_err(Error::Serialize)?;

    let len = data.len();
    if len > 0xFFFFFF {
      return Err(Error::MessageTooLarge);
    }

    dst.reserve(HEADER_LEN + len);
    dst.put_slice(MAGIC);
    dst.put_slice(&(len as u32).to_be_bytes()[1..]);
    dst.put_slice(&data);

    tracing::trace!(target: "hal1210::com::encoder", "encoded {} bytes", len);

    Ok(())
  }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
  #[error(transparent)]
  Io(#[from] io::Error),
  #[error("Invalid magic bytes")]
  InvalidMagic,
  #[error("Message too large")]
  MessageTooLarge,
  #[error("Serialization error: {0}")]
  Serialize(#[from] rmp_serde::encode::Error),
  #[error("Deserialization error: {0}")]
  Deserialize(#[from] rmp_serde::decode::Error),
}
