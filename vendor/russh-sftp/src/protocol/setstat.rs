use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{impl_packet_for, impl_request_id, FileAttributes, Packet, RequestId};

/// Implementation for `SSH_FXP_SETSTAT` and `MKDIR`
#[derive(Debug)]
pub struct SetStat {
    pub id: u32,
    pub path: String,
    pub path_bytes: Option<Vec<u8>>,
    pub attrs: FileAttributes,
}

impl Serialize for SetStat {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("SetStat", 3)?;
        state.serialize_field("id", &self.id)?;
        match &self.path_bytes {
            Some(bytes) => state.serialize_field("path", bytes)?,
            None => state.serialize_field("path", &self.path)?,
        }
        state.serialize_field("attrs", &self.attrs)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for SetStat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{self, SeqAccess, Visitor};
        use std::fmt;

        struct SetStatVisitor;

        impl<'de> Visitor<'de> for SetStatVisitor {
            type Value = SetStat;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("struct SetStat")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<SetStat, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let id: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let path_bytes: Vec<u8> = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let attrs: FileAttributes = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(2, &self))?;
                let path = String::from_utf8_lossy(&path_bytes).into_owned();
                Ok(SetStat {
                    id,
                    path,
                    path_bytes: Some(path_bytes),
                    attrs,
                })
            }
        }

        const FIELDS: &[&str] = &["id", "path", "attrs"];
        deserializer.deserialize_struct("SetStat", FIELDS, SetStatVisitor)
    }
}

impl_request_id!(SetStat);
impl_packet_for!(SetStat);
