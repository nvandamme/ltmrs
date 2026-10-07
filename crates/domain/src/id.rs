//! Domain identity types: 128-bit internal IDs, external aliases, revisions,
//! store generation, and channel/session identities.

use std::fmt;

use uuid::Uuid;

#[derive(
    Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct EntityId(Uuid);

impl EntityId {
    pub fn new(id: Uuid) -> Self {
        Self(id)
    }
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Debug for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EntityId({})", self.0)
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct StoreGeneration(u64);

impl StoreGeneration {
    pub const FIRST: Self = Self(1);
    pub fn new(value: u64) -> Self {
        Self(value)
    }
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

macro_rules! revision_newtype {
    ($name:ident) => {
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            Hash,
            PartialOrd,
            Ord,
            serde::Serialize,
            serde::Deserialize,
        )]
        pub struct $name(u64);

        impl $name {
            pub fn new(value: u64) -> Self {
                Self(value)
            }
            pub fn as_u64(&self) -> u64 {
                self.0
            }
            pub fn next(&self) -> Self {
                Self(self.0 + 1)
            }
        }
    };
}

revision_newtype!(EntityRevision);
revision_newtype!(DocumentRevision);
revision_newtype!(EligibilityRevision);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct FrontendId(Uuid);

impl FrontendId {
    pub fn new(id: Uuid) -> Self {
        Self(id)
    }
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct ChannelId(Uuid);

impl ChannelId {
    pub fn new(id: Uuid) -> Self {
        Self(id)
    }
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct SessionHandle(Uuid);

impl SessionHandle {
    pub fn new(id: Uuid) -> Self {
        Self(id)
    }
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

#[derive(
    Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct OperationId(Uuid);

impl OperationId {
    pub fn new(id: Uuid) -> Self {
        Self(id)
    }
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Debug for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OperationId({})", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ExternalAlias(String);

impl ExternalAlias {
    pub fn new(alias: impl Into<String>) -> Self {
        Self(alias.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SearchIdentity {
    pub store_generation: StoreGeneration,
    pub memory_id: EntityId,
    pub document_revision: DocumentRevision,
    pub model_fingerprint: ModelFingerprint,
    pub chunk_id: ChunkId,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct ModelFingerprint(u64);

impl ModelFingerprint {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct ChunkId(u32);

impl ChunkId {
    pub fn new(value: u32) -> Self {
        Self(value)
    }
    pub fn as_u32(&self) -> u32 {
        self.0
    }
}
