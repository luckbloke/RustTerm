use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{impl_packet_for, impl_request_id, Packet, RequestId};

/// Implementation for `SSH_FXP_OPENDIR`
#[derive(Debug)]
pub struct OpenDir {
    pub id: u32,
    pub path: String,
    /// Raw bytes of the path, preserving original encoding.
    /// When present, used for serialization instead of path.
    pub path_bytes: Option<Vec<u8>>,
}

impl Serialize for OpenDir {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("OpenDir", 2)?;
        state.serialize_field("id", &self.id)?;
        // Use raw bytes if available to preserve original encoding
        match &self.path_bytes {
            Some(bytes) => state.serialize_field("path", bytes)?,
            None => state.serialize_field("path", &self.path)?,
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for OpenDir {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{self, SeqAccess, Visitor};
        use std::fmt;

        struct OpenDirVisitor;

        impl<'de> Visitor<'de> for OpenDirVisitor {
            type Value = OpenDir;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("struct OpenDir")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<OpenDir, A::Error>
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

                Ok(OpenDir {
                    id,
                    path,
                    path_bytes: Some(path_bytes),
                })
            }
        }

        const FIELDS: &[&str] = &["id", "path"];
        deserializer.deserialize_struct("OpenDir", FIELDS, OpenDirVisitor)
    }
}

impl_request_id!(OpenDir);
impl_packet_for!(OpenDir);
