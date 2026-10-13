use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{impl_packet_for, impl_request_id, Packet, RequestId};

/// Implementation for `SSH_FXP_LSTAT`
#[derive(Debug)]
pub struct Lstat {
    pub id: u32,
    pub path: String,
    pub path_bytes: Option<Vec<u8>>,
}

impl Serialize for Lstat {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("Lstat", 2)?;
        state.serialize_field("id", &self.id)?;
        match &self.path_bytes {
            Some(bytes) => state.serialize_field("path", bytes)?,
            None => state.serialize_field("path", &self.path)?,
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for Lstat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{self, SeqAccess, Visitor};
        use std::fmt;

        struct LstatVisitor;

        impl<'de> Visitor<'de> for LstatVisitor {
            type Value = Lstat;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("struct Lstat")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Lstat, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let id: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let path_bytes: Vec<u8> = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let path = String::from_utf8_lossy(&path_bytes).into_owned();
                Ok(Lstat {
                    id,
                    path,
                    path_bytes: Some(path_bytes),
                })
            }
        }

        const FIELDS: &[&str] = &["id", "path"];
        deserializer.deserialize_struct("Lstat", FIELDS, LstatVisitor)
    }
}

impl_request_id!(Lstat);
impl_packet_for!(Lstat);
