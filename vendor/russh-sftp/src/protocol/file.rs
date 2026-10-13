use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::time::{Duration, UNIX_EPOCH};

use super::FileAttributes;

/// Wrapper for raw bytes that uses deserialize_byte_buf
#[derive(Debug, Clone)]
struct RawBytes(Vec<u8>);

impl Serialize for RawBytes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for RawBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RawBytesVisitor;

        impl<'de> serde::de::Visitor<'de> for RawBytesVisitor {
            type Value = RawBytes;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("raw bytes")
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RawBytes(v.to_vec()))
            }

            fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RawBytes(v))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut bytes = Vec::new();
                while let Some(byte) = seq.next_element()? {
                    bytes.push(byte);
                }
                Ok(RawBytes(bytes))
            }
        }

        deserializer.deserialize_byte_buf(RawBytesVisitor)
    }
}

#[derive(Debug, Clone)]
pub struct File {
    pub filename: String,
    /// Raw bytes of the filename, preserving original encoding.
    /// Used for serialization to send back to server.
    pub filename_bytes: Vec<u8>,
    pub longname: String,
    pub attrs: FileAttributes,
}

impl Serialize for File {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("File", 3)?;
        // Use raw bytes for serialization to preserve original encoding
        state.serialize_field("filename", &RawBytes(self.filename_bytes.clone()))?;
        state.serialize_field("longname", &self.longname)?;
        state.serialize_field("attrs", &self.attrs)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for File {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{self, SeqAccess, Visitor};
        use std::fmt;

        struct FileVisitor;

        impl<'de> Visitor<'de> for FileVisitor {
            type Value = File;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("struct File")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<File, A::Error>
            where
                A: SeqAccess<'de>,
            {
                // Field 0: filename as raw bytes using RawBytes wrapper
                let raw: RawBytes = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let filename_bytes = raw.0;
                // Field 1: longname
                let longname: String = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                // Field 2: attrs
                let attrs: FileAttributes = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(2, &self))?;

                // Decode filename from bytes using lossy UTF-8 for display
                let filename = String::from_utf8_lossy(&filename_bytes).into_owned();

                Ok(File {
                    filename,
                    filename_bytes,
                    longname,
                    attrs,
                })
            }
        }

        const FIELDS: &[&str] = &["filename", "longname", "attrs"];
        deserializer.deserialize_struct("File", FIELDS, FileVisitor)
    }
}

impl File {
    /// Omits `longname` and set dummy `attributes`. This is mainly used for [`crate::server::Handler::realpath`] as per the standard
    pub fn dummy<S: Into<String>>(filename: S) -> Self {
        let filename = filename.into();
        Self {
            filename: filename.clone(),
            filename_bytes: filename.into_bytes(),
            longname: "".to_string(),
            attrs: FileAttributes::default(),
        }
    }

    /// Implies the use of longname
    pub fn new<S: Into<String>>(filename: S, attrs: FileAttributes) -> Self {
        let filename = filename.into();
        let mut file = Self {
            filename: filename.clone(),
            filename_bytes: filename.clone().into_bytes(),
            longname: "".to_string(),
            attrs,
        };
        file.longname = file.longname();
        file
    }

    /// Get formed longname
    pub fn longname(&self) -> String {
        let directory = if self.attrs.is_dir() { "d" } else { "-" };
        let permissions = self.attrs.permissions().to_string();

        let size = self.attrs.size.unwrap_or(0);
        let mtime = self.attrs.mtime.unwrap_or(0);

        let datetime = DateTime::<Utc>::from(UNIX_EPOCH + Duration::from_secs(mtime as u64));
        let delayed = datetime.format("%b %d %Y %H:%M");

        format!(
            "{directory}{permissions} 0 {} {} {size} {delayed} {}",
            if let Some(user) = &self.attrs.user {
                user.to_string()
            } else {
                self.attrs.uid.unwrap_or(0).to_string()
            },
            if let Some(group) = &self.attrs.group {
                group.to_string()
            } else {
                self.attrs.gid.unwrap_or(0).to_string()
            },
            self.filename
        )
    }
}
