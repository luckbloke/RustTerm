use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{impl_packet_for, impl_request_id, Packet, RequestId};

/// Implementation for `SSH_FXP_RENAME`
#[derive(Debug)]
pub struct Rename {
    pub id: u32,
    pub oldpath: String,
    pub newpath: String,
    pub oldpath_bytes: Option<Vec<u8>>,
    pub newpath_bytes: Option<Vec<u8>>,
}

impl Serialize for Rename {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("Rename", 3)?;
        state.serialize_field("id", &self.id)?;
        match &self.oldpath_bytes {
            Some(bytes) => state.serialize_field("oldpath", bytes)?,
            None => state.serialize_field("oldpath", &self.oldpath)?,
        }
        match &self.newpath_bytes {
            Some(bytes) => state.serialize_field("newpath", bytes)?,
            None => state.serialize_field("newpath", &self.newpath)?,
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for Rename {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{self, SeqAccess, Visitor};
        use std::fmt;

        struct RenameVisitor;

        impl<'de> Visitor<'de> for RenameVisitor {
            type Value = Rename;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("struct Rename")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Rename, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let id: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let oldpath_bytes: Vec<u8> = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let newpath_bytes: Vec<u8> = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(2, &self))?;
                Ok(Rename {
                    id,
                    oldpath: String::from_utf8_lossy(&oldpath_bytes).into_owned(),
                    newpath: String::from_utf8_lossy(&newpath_bytes).into_owned(),
                    oldpath_bytes: Some(oldpath_bytes),
                    newpath_bytes: Some(newpath_bytes),
                })
            }
        }

        const FIELDS: &[&str] = &["id", "oldpath", "newpath"];
        deserializer.deserialize_struct("Rename", FIELDS, RenameVisitor)
    }
}

impl_request_id!(Rename);
impl_packet_for!(Rename);
