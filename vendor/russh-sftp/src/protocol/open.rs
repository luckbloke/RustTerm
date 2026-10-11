use std::fs;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{impl_packet_for, impl_request_id, FileAttributes, Packet, RequestId};

/// Opening flags according to the specification
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct OpenFlags(u32);

bitflags! {
    impl OpenFlags: u32 {
        const READ = 0x00000001;
        const WRITE = 0x00000002;
        const APPEND = 0x00000004;
        const CREATE = 0x00000008;
        const TRUNCATE = 0x00000010;
        const EXCLUDE = 0x00000020;
    }
}

impl From<OpenFlags> for fs::OpenOptions {
    fn from(value: OpenFlags) -> Self {
        let mut open_options = fs::OpenOptions::new();
        if value.contains(OpenFlags::READ) {
            open_options.read(true);
        }
        if value.contains(OpenFlags::WRITE) {
            open_options.write(true);
        }
        if value.contains(OpenFlags::APPEND) {
            open_options.append(true);
        }
        if value.contains(OpenFlags::CREATE) {
            // SFTPv3 spec requires the `CREATE` flag to be set if the `EXCLUDE` flag
            // is set. Rusts `OpenOptions` has different semantics: it ignores
            // whether `create` or `truncate` was set.
            // SFTPv3 spec does not say anything about read/write flags, but
            // they will be required to do anything else with the file.
            // https://datatracker.ietf.org/doc/html/draft-ietf-secsh-filexfer-02#section-6.3
            if value.contains(OpenFlags::EXCLUDE) {
                open_options.create_new(true);
            } else {
                open_options.create(true);
            }
        }
        if value.contains(OpenFlags::TRUNCATE) {
            open_options.truncate(true);
        }

        open_options
    }
}

/// Implementation for `SSH_FXP_OPEN`
#[derive(Debug)]
pub struct Open {
    pub id: u32,
    pub filename: String,
    /// Raw bytes of the filename, preserving original encoding.
    /// When present, used for serialization instead of filename.
    pub filename_bytes: Option<Vec<u8>>,
    pub pflags: OpenFlags,
    pub attrs: FileAttributes,
}

impl Serialize for Open {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("Open", 4)?;
        state.serialize_field("id", &self.id)?;
        // Use raw bytes if available to preserve original encoding
        match &self.filename_bytes {
            Some(bytes) => state.serialize_field("filename", bytes)?,
            None => state.serialize_field("filename", &self.filename)?,
        }
        state.serialize_field("pflags", &self.pflags)?;
        state.serialize_field("attrs", &self.attrs)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for Open {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{self, SeqAccess, Visitor};
        use std::fmt;

        struct OpenVisitor;

        impl<'de> Visitor<'de> for OpenVisitor {
            type Value = Open;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("struct Open")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Open, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let id: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let filename_bytes: Vec<u8> = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let pflags: OpenFlags = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(2, &self))?;
                let attrs: FileAttributes = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(3, &self))?;

                let filename = String::from_utf8_lossy(&filename_bytes).into_owned();

                Ok(Open {
                    id,
                    filename,
                    filename_bytes: Some(filename_bytes),
                    pflags,
                    attrs,
                })
            }
        }

        const FIELDS: &[&str] = &["id", "filename", "pflags", "attrs"];
        deserializer.deserialize_struct("Open", FIELDS, OpenVisitor)
    }
}

impl_request_id!(Open);
impl_packet_for!(Open);
