use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{impl_packet_for, impl_request_id, Packet, RequestId};

/// Implementation for `SSH_FXP_REMOVE`
#[derive(Debug)]
pub struct Remove {
    pub id: u32,
    pub filename: String,
    /// Raw bytes of the filename, preserving original encoding.
    /// When present, used for serialization instead of filename.
    pub filename_bytes: Option<Vec<u8>>,
}

impl Serialize for Remove {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("Remove", 2)?;
        state.serialize_field("id", &self.id)?;
        // Use raw bytes if available to preserve original encoding
        match &self.filename_bytes {
            Some(bytes) => state.serialize_field("filename", bytes)?,
            None => state.serialize_field("filename", &self.filename)?,
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for Remove {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{self, SeqAccess, Visitor};
        use std::fmt;

        struct RemoveVisitor;

        impl<'de> Visitor<'de> for RemoveVisitor {
            type Value = Remove;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("struct Remove")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Remove, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let id: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let filename_bytes: Vec<u8> = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let filename = String::from_utf8_lossy(&filename_bytes).into_owned();

                Ok(Remove {
                    id,
                    filename,
                    filename_bytes: Some(filename_bytes),
                })
            }
        }

        const FIELDS: &[&str] = &["id", "filename"];
        deserializer.deserialize_struct("Remove", FIELDS, RemoveVisitor)
    }
}

impl_request_id!(Remove);
impl_packet_for!(Remove);
