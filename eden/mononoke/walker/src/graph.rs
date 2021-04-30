/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use ahash::RandomState;
use anyhow::{format_err, Error};
use bitflags::bitflags;
use blame::BlameRoot;
use blobrepo::BlobRepo;
use blobstore_factory::SqlTierInfo;
use bookmarks::BookmarkName;
use changeset_info::ChangesetInfo;
use context::CoreContext;
use deleted_files_manifest::RootDeletedManifestId;
use derived_data::BonsaiDerivable;
use derived_data_filenodes::FilenodesOnlyPublic;
use fastlog::{unode_entry_to_fastlog_batch_key, RootFastlog};
use filenodes::FilenodeInfo;
use filestore::Alias;
use fsnodes::RootFsnodeId;
use futures::{
    compat::Future01CompatExt,
    future::BoxFuture,
    stream::{self, BoxStream},
    FutureExt, StreamExt, TryStreamExt,
};
use hash_memo::EagerHashMemoizer;
use internment::ArcIntern;
use manifest::Entry;
use mercurial_derived_data::MappedHgChangesetId;
use mercurial_types::{
    blobs::{HgBlobChangeset, HgBlobManifest},
    calculate_hg_node_id_stream, FileBytes, HgChangesetId, HgFileEnvelope, HgFileEnvelopeMut,
    HgFileNodeId, HgManifestId, HgParents,
};
use mononoke_types::{
    blame::Blame,
    deleted_files_manifest::DeletedManifest,
    fastlog_batch::FastlogBatch,
    fsnode::Fsnode,
    skeleton_manifest::SkeletonManifest,
    unode::{FileUnode, ManifestUnode},
    BlameId, BonsaiChangeset, ChangesetId, ContentId, ContentMetadata, DeletedManifestId,
    FastlogBatchId, FileUnodeId, FsnodeId, MPath, MPathHash, ManifestUnodeId, MononokeId, RepoPath,
    SkeletonManifestId,
};
use newfilenodes::PathHash;
use once_cell::sync::OnceCell;
use phases::Phase;
use skeleton_manifest::RootSkeletonManifestId;
use std::{
    fmt,
    hash::{Hash, Hasher},
    str::FromStr,
};
use unodes::RootUnodeManifestId;

use crate::walk::OutgoingEdge;

// Helper to save repetition for the type enums
macro_rules! define_type_enum {
     (enum $enum_name:ident {
         $($variant:ident),* $(,)?
     }) => {
         #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, strum_macros::AsRefStr,
         strum_macros::EnumCount, strum_macros::EnumIter, strum_macros::EnumString,
         strum_macros::EnumVariantNames, strum_macros::IntoStaticStr)]
         pub enum $enum_name {
             $($variant),*
         }
    }
}

macro_rules! incoming_type {
    ($nodetypeenum:ident, $edgetypeenum:tt, Root) => {
        None
    };
    ($nodetypeenum:ident, $edgetypeenum:tt, $sourcetype:tt) => {
        Some($nodetypeenum::$sourcetype)
    };
}

macro_rules! outgoing_type {
    // Edge isn't named exactly same as the target node type
    ($nodetypeenum:ident, $edgetypeenum:tt, $targetlabel:ident($targettype:ident)) => {
        $nodetypeenum::$targettype
    };
    // In most cases its the same
    ($nodetypeenum:ident, $edgetypeenum:tt, $targetlabel:ident) => {
        $nodetypeenum::$targetlabel
    };
}

#[doc(hidden)]
macro_rules! create_graph_impl {
    ($nodetypeenum:ident, $nodekeyenum:ident, $edgetypeenum:ident,
        {$($nodetype:tt)*} {$($nodekeys:tt)*} {$(($edgetype:tt, $edgesourcetype:tt, $($edgetargetdef:tt)+)),+ $(,)?} ) => {
        define_type_enum!{
            enum $nodetypeenum {$($nodetype)*}
        }

        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub enum $nodekeyenum {$($nodekeys)*}

        define_type_enum!{
            enum $edgetypeenum {$($edgetype),*}
        }

        impl $edgetypeenum {
            pub fn incoming_type(&self) -> Option<$nodetypeenum> {
                match self {
                    $($edgetypeenum::$edgetype => incoming_type!($nodetypeenum, $edgetypeenum, $edgesourcetype)),*
                }
            }
        }

        impl $edgetypeenum {
            pub fn outgoing_type(&self) -> $nodetypeenum {
                match self {
                    $($edgetypeenum::$edgetype => outgoing_type!($nodetypeenum, $edgetypeenum, $($edgetargetdef)+)),*
                }
            }
        }
    };
    ($nodetypeenum:ident, $nodekeyenum:ident, $edgetypeenum:ident,
        {$($nodetype:tt)*} {$($nodekeys:tt)*} {$(($edgetype:tt, $edgesourcetype:tt, $($edgetargetdef:tt)+)),* $(,)?}
            ($source:ident, $sourcekey:ty, [$($target:ident$(($targettype:ident))?),*]) $($rest:tt)*) => {
        paste::item!{
            create_graph_impl! {
                $nodetypeenum, $nodekeyenum, $edgetypeenum,
                {$($nodetype)* $source,}
                {$($nodekeys)* $source($sourcekey),}
                {
                    $(($edgetype, $edgesourcetype, $($edgetargetdef)+),)*
                    $(([<$source To $target>], $source, $target$(($targettype))? ),)*
                }
                $($rest)*
            }
        }
    };
}

macro_rules! root_edge_type {
    ($edgetypeenum:ident, Root) => {
        None
    };
    ($edgetypeenum:ident, $target:ident) => {
        Some(paste::item! {$edgetypeenum::[<RootTo $target>]})
    };
}

macro_rules! create_graph {
    ($nodetypeenum:ident, $nodekeyenum:ident, $edgetypeenum:ident,
        $(($source:ident, $sourcekey:ty, [$($target:ident$(($targettype:ident))?),*])),* $(,)?) => {
        create_graph_impl! {
            $nodetypeenum, $nodekeyenum, $edgetypeenum,
            {}
            {}
            {}
            $(($source, $sourcekey, [$($target$(($targettype))?),*]))*
        }

        impl $nodetypeenum {
            pub fn root_edge_type(&self) -> Option<$edgetypeenum> {
                match self {
                    $($nodetypeenum::$source => root_edge_type!($edgetypeenum, $source)),*
                }
            }
            pub fn parse_node(&self, s: &str) -> Result<$nodekeyenum, Error> {
                match self {
                    $($nodetypeenum::$source => Ok($nodekeyenum::$source(<$sourcekey>::from_str(s)?))),*
                }
            }
        }
        impl $nodekeyenum {
            pub fn get_type(&self) -> $nodetypeenum {
                match self {
                    $($nodekeyenum::$source(_) => $nodetypeenum::$source),*
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnitKey();

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PathKey<T: fmt::Debug + Clone + PartialEq + Eq + Hash> {
    pub id: T,
    pub path: WrappedPath,
}
impl<T: fmt::Debug + Clone + PartialEq + Eq + Hash> PathKey<T> {
    pub fn new(id: T, path: WrappedPath) -> Self {
        Self { id, path }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AliasKey(pub Alias);

/// Used for both Bonsai and HgChangesets to track if filenode data is present
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChangesetKey<T> {
    pub inner: T,
    pub filenode_known_derived: bool,
}

impl ChangesetKey<ChangesetId> {
    fn blobstore_key(&self) -> String {
        self.inner.blobstore_key()
    }

    fn sampling_fingerprint(&self) -> u64 {
        self.inner.sampling_fingerprint()
    }
}

impl ChangesetKey<HgChangesetId> {
    fn blobstore_key(&self) -> String {
        self.inner.blobstore_key()
    }

    fn sampling_fingerprint(&self) -> u64 {
        self.inner.sampling_fingerprint()
    }
}

bitflags! {
    /// Some derived data needs unodes as precondition, flags represent what is available in a compact way
    #[derive(Default)]
    pub struct UnodeFlags: u8 {
        const NONE = 0b00000000;
        const BLAME = 0b00000001;
        const FASTLOG = 0b00000010;
    }
}

/// Not all unodes should attempt to traverse blame or fastlog
/// e.g. a unode for non-public commit is not expected to have it
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnodeKey<T> {
    pub inner: T,
    pub flags: UnodeFlags,
}

impl<T: MononokeId> UnodeKey<T> {
    fn blobstore_key(&self) -> String {
        self.inner.blobstore_key()
    }

    fn sampling_fingerprint(&self) -> u64 {
        self.inner.sampling_fingerprint()
    }
}

pub type UnodeManifestEntry = Entry<ManifestUnodeId, FileUnodeId>;

/// newtype so we can implement blobstore_key()
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FastlogKey<T> {
    pub inner: T,
}

impl<T: MononokeId> FastlogKey<T> {
    fn sampling_fingerprint(&self) -> u64 {
        self.inner.sampling_fingerprint()
    }

    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl FastlogKey<FileUnodeId> {
    fn blobstore_key(&self) -> String {
        unode_entry_to_fastlog_batch_key(&UnodeManifestEntry::Leaf(self.inner))
    }
}

impl FastlogKey<ManifestUnodeId> {
    fn blobstore_key(&self) -> String {
        unode_entry_to_fastlog_batch_key(&UnodeManifestEntry::Tree(self.inner))
    }
}

create_graph!(
    NodeType,
    Node,
    EdgeType,
    (
        Root,
        UnitKey,
        [
            // Bonsai
            Bookmark,
            Changeset,
            BonsaiHgMapping,
            PhaseMapping,
            PublishedBookmarks,
            // Hg
            HgBonsaiMapping,
            HgChangeset,
            HgChangesetViaBonsai,
            HgManifest,
            HgFileEnvelope,
            HgFileNode,
            HgManifestFileNode,
            // Content
            FileContent,
            FileContentMetadata,
            AliasContentMapping,
            // Derived
            Blame,
            ChangesetInfo,
            ChangesetInfoMapping,
            DeletedManifest,
            DeletedManifestMapping,
            FastlogBatch,
            FastlogDir,
            FastlogFile,
            Fsnode,
            FsnodeMapping,
            SkeletonManifest,
            SkeletonManifestMapping,
            UnodeFile,
            UnodeManifest,
            UnodeMapping
        ]
    ),
    // Bonsai
    (Bookmark, BookmarkName, [Changeset, BonsaiHgMapping]),
    (
        Changeset,
        ChangesetKey<ChangesetId>,
        [
            FileContent,
            BonsaiParent(Changeset),
            BonsaiHgMapping,
            PhaseMapping,
            ChangesetInfo,
            ChangesetInfoMapping,
            DeletedManifestMapping,
            FsnodeMapping,
            SkeletonManifestMapping,
            UnodeMapping
        ]
    ),
    (BonsaiHgMapping, ChangesetKey<ChangesetId>, [HgBonsaiMapping, HgChangesetViaBonsai]),
    (PhaseMapping, ChangesetId, []),
    (
        PublishedBookmarks,
        UnitKey,
        [Changeset, BonsaiHgMapping]
    ),
    // Hg
    (HgBonsaiMapping, ChangesetKey<HgChangesetId>, [Changeset]),
    (
        HgChangeset,
        ChangesetKey<HgChangesetId>,
        [HgParent(HgChangesetViaBonsai), HgManifest, HgManifestFileNode]
    ),
    (HgChangesetViaBonsai, ChangesetKey<HgChangesetId>, [HgChangeset]),
    (
        HgManifest,
        PathKey<HgManifestId>,
        [HgFileEnvelope, HgFileNode, HgManifestFileNode, ChildHgManifest(HgManifest)]
    ),
    (HgFileEnvelope, HgFileNodeId, [FileContent]),
    (
        HgFileNode,
        PathKey<HgFileNodeId>,
        [
            LinkedHgBonsaiMapping(HgBonsaiMapping),
            LinkedHgChangeset(HgChangesetViaBonsai),
            HgParentFileNode(HgFileNode),
            HgCopyfromFileNode(HgFileNode)
        ]
    ),
    (
        HgManifestFileNode,
        PathKey<HgFileNodeId>,
        [
            LinkedHgBonsaiMapping(HgBonsaiMapping),
            LinkedHgChangeset(HgChangesetViaBonsai),
            HgParentFileNode(HgManifestFileNode),
            HgCopyfromFileNode(HgManifestFileNode)
        ]
    ),
    // Content
    (FileContent, ContentId, [FileContentMetadata]),
    (
        FileContentMetadata,
        ContentId,
        [
            Sha1Alias(AliasContentMapping),
            Sha256Alias(AliasContentMapping),
            GitSha1Alias(AliasContentMapping)
        ]
    ),
    (AliasContentMapping, AliasKey, [FileContent]),
    // Derived data
    (
        Blame,
        BlameId,
        [Changeset]
    ),
    (
        ChangesetInfo,
        ChangesetId,
        [ChangesetInfoParent(ChangesetInfo)]
    ),
    (
        ChangesetInfoMapping,
        ChangesetId,
        [ChangesetInfo]
    ),
    (
        DeletedManifest,
        DeletedManifestId,
        [DeletedManifestChild(DeletedManifest), LinkedChangeset(Changeset)]
    ),
    (DeletedManifestMapping, ChangesetId, [RootDeletedManifest(DeletedManifest)]),
    (
        Fsnode,
        FsnodeId,
        [ChildFsnode(Fsnode), FileContent]
    ),
    (
        FastlogBatch,
        FastlogBatchId,
        [Changeset, PreviousBatch(FastlogBatch)]
    ),
    (
        FastlogDir,
        FastlogKey<ManifestUnodeId>,
        [Changeset, PreviousBatch(FastlogBatch)]
    ),
    (
        FastlogFile,
        FastlogKey<FileUnodeId>,
        [Changeset, PreviousBatch(FastlogBatch)]
    ),
    (FsnodeMapping, ChangesetId, [RootFsnode(Fsnode)]),
    (
        SkeletonManifest,
        SkeletonManifestId,
        [SkeletonManifestChild(SkeletonManifest)]
    ),
    (SkeletonManifestMapping, ChangesetId, [RootSkeletonManifest(SkeletonManifest)]),
    (
        UnodeFile,
        UnodeKey<FileUnodeId>,
        [Blame, FastlogFile, FileContent, LinkedChangeset(Changeset), UnodeFileParent(UnodeFile)]
    ),
    (
        UnodeManifest,
        UnodeKey<ManifestUnodeId>,
        [FastlogDir, UnodeFileChild(UnodeFile), UnodeManifestChild(UnodeManifest), UnodeManifestParent(UnodeManifest), LinkedChangeset(Changeset)]
    ),
    (UnodeMapping, ChangesetId, [RootUnodeManifest(UnodeManifest)]),
);

impl fmt::Display for NodeType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl NodeType {
    /// Derived data types are keyed by their statically defined NAME
    pub fn derived_data_name(&self) -> Option<&'static str> {
        match self {
            NodeType::Root => None,
            // Bonsai
            NodeType::Bookmark => None,
            NodeType::Changeset => None,
            // from filenodes/lib.rs: If hg changeset is not generated, then root filenode can't possible be generated
            // therefore this is the same as MappedHgChangesetId + FilenodesOnlyPublic
            NodeType::BonsaiHgMapping => Some(FilenodesOnlyPublic::NAME),
            NodeType::PhaseMapping => None,
            NodeType::PublishedBookmarks => None,
            // Hg
            NodeType::HgBonsaiMapping => Some(MappedHgChangesetId::NAME),
            NodeType::HgChangeset => Some(MappedHgChangesetId::NAME),
            NodeType::HgChangesetViaBonsai => Some(MappedHgChangesetId::NAME),
            NodeType::HgManifest => Some(MappedHgChangesetId::NAME),
            NodeType::HgFileEnvelope => Some(MappedHgChangesetId::NAME),
            NodeType::HgFileNode => Some(FilenodesOnlyPublic::NAME),
            NodeType::HgManifestFileNode => Some(FilenodesOnlyPublic::NAME),
            // Content
            NodeType::FileContent => None,
            NodeType::FileContentMetadata => None,
            NodeType::AliasContentMapping => None,
            // Derived data
            NodeType::Blame => Some(BlameRoot::NAME),
            NodeType::ChangesetInfo => Some(ChangesetInfo::NAME),
            NodeType::ChangesetInfoMapping => Some(ChangesetInfo::NAME),
            NodeType::DeletedManifest => Some(RootDeletedManifestId::NAME),
            NodeType::DeletedManifestMapping => Some(RootDeletedManifestId::NAME),
            NodeType::FastlogBatch => Some(RootFastlog::NAME),
            NodeType::FastlogDir => Some(RootFastlog::NAME),
            NodeType::FastlogFile => Some(RootFastlog::NAME),
            NodeType::Fsnode => Some(RootFsnodeId::NAME),
            NodeType::FsnodeMapping => Some(RootFsnodeId::NAME),
            NodeType::SkeletonManifest => Some(RootSkeletonManifestId::NAME),
            NodeType::SkeletonManifestMapping => Some(RootSkeletonManifestId::NAME),
            NodeType::UnodeFile => Some(RootUnodeManifestId::NAME),
            NodeType::UnodeManifest => Some(RootUnodeManifestId::NAME),
            NodeType::UnodeMapping => Some(RootUnodeManifestId::NAME),
        }
    }

    // Only certain node types can have repo paths associated
    pub fn allow_repo_path(&self) -> bool {
        match self {
            NodeType::Root => false,
            // Bonsai
            NodeType::Bookmark => false,
            NodeType::Changeset => false,
            NodeType::BonsaiHgMapping => false,
            NodeType::PhaseMapping => false,
            NodeType::PublishedBookmarks => false,
            // Hg
            NodeType::HgBonsaiMapping => false,
            NodeType::HgChangeset => false,
            NodeType::HgChangesetViaBonsai => false,
            NodeType::HgManifest => true,
            NodeType::HgFileEnvelope => true,
            NodeType::HgFileNode => true,
            NodeType::HgManifestFileNode => true,
            // Content
            NodeType::FileContent => true,
            NodeType::FileContentMetadata => true,
            NodeType::AliasContentMapping => true,
            // Derived Data
            NodeType::Blame => false,
            NodeType::ChangesetInfo => false,
            NodeType::ChangesetInfoMapping => false,
            NodeType::DeletedManifest => true,
            NodeType::DeletedManifestMapping => false,
            NodeType::FastlogBatch => true,
            NodeType::FastlogDir => true,
            NodeType::FastlogFile => true,
            NodeType::Fsnode => true,
            NodeType::FsnodeMapping => false,
            NodeType::SkeletonManifest => true,
            NodeType::SkeletonManifestMapping => false,
            NodeType::UnodeFile => true,
            NodeType::UnodeManifest => true,
            NodeType::UnodeMapping => false,
        }
    }
}

const ROOT_FINGERPRINT: u64 = 0;

// Can represent Path and PathHash
pub trait WrappedPathLike {
    fn sampling_fingerprint(&self) -> u64;
    fn evolve_path<'a>(
        from_route: Option<&'a Self>,
        walk_item: &'a OutgoingEdge,
    ) -> Option<&'a Self>;
}

/// Represent root or non root path hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WrappedPathHash {
    Root,
    NonRoot(MPathHash),
}

impl WrappedPathHash {
    pub fn as_ref(&self) -> Option<&MPathHash> {
        match self {
            Self::Root => None,
            Self::NonRoot(mpath_hash) => Some(&mpath_hash),
        }
    }
}

impl WrappedPathLike for WrappedPathHash {
    fn sampling_fingerprint(&self) -> u64 {
        match self {
            WrappedPathHash::Root => ROOT_FINGERPRINT,
            WrappedPathHash::NonRoot(path_hash) => path_hash.sampling_fingerprint(),
        }
    }
    fn evolve_path<'a>(
        from_route: Option<&'a Self>,
        walk_item: &'a OutgoingEdge,
    ) -> Option<&'a Self> {
        match walk_item.path.as_ref() {
            // Step has set explicit path, e.g. bonsai file
            Some(from_step) => Some(from_step.get_path_hash()),
            None => match walk_item.target.stats_path() {
                // Path is part of node identity
                Some(from_node) => Some(from_node.get_path_hash()),
                // No per-node path, so use the route, filtering out nodes that can't have repo paths
                None => {
                    if walk_item.target.get_type().allow_repo_path() {
                        from_route
                    } else {
                        None
                    }
                }
            },
        }
    }
}

impl fmt::Display for WrappedPathHash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Root => write!(f, ""),
            Self::NonRoot(mpath_hash) => write!(f, "{}", mpath_hash.to_hex()),
        }
    }
}

// Memoize the hash of the path as it is used frequently
#[derive(Debug)]
pub struct MPathWithHashMemo {
    mpath: MPath,
    memoized_hash: OnceCell<WrappedPathHash>,
}

impl MPathWithHashMemo {
    fn new(mpath: MPath) -> Self {
        Self {
            mpath,
            memoized_hash: OnceCell::new(),
        }
    }

    pub fn get_path_hash_memo(&self) -> &WrappedPathHash {
        self.memoized_hash
            .get_or_init(|| WrappedPathHash::NonRoot(self.mpath.get_path_hash()))
    }

    pub fn mpath(&self) -> &MPath {
        &self.mpath
    }
}

impl PartialEq for MPathWithHashMemo {
    fn eq(&self, other: &Self) -> bool {
        self.mpath == other.mpath
    }
}

impl Eq for MPathWithHashMemo {}

impl Hash for MPathWithHashMemo {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.mpath.hash(state);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum WrappedPath {
    Root,
    NonRoot(ArcIntern<EagerHashMemoizer<MPathWithHashMemo>>),
}

impl WrappedPath {
    pub fn as_ref(&self) -> Option<&MPath> {
        match self {
            WrappedPath::Root => None,
            WrappedPath::NonRoot(path) => Some(path.mpath()),
        }
    }

    pub fn get_path_hash(&self) -> &WrappedPathHash {
        match self {
            WrappedPath::Root => &WrappedPathHash::Root,
            WrappedPath::NonRoot(path) => path.get_path_hash_memo(),
        }
    }
}

impl WrappedPathLike for WrappedPath {
    fn sampling_fingerprint(&self) -> u64 {
        self.get_path_hash().sampling_fingerprint()
    }
    fn evolve_path<'a>(
        from_route: Option<&'a Self>,
        walk_item: &'a OutgoingEdge,
    ) -> Option<&'a Self> {
        match walk_item.path.as_ref() {
            // Step has set explicit path, e.g. bonsai file
            Some(from_step) => Some(from_step),
            None => match walk_item.target.stats_path() {
                // Path is part of node identity
                Some(from_node) => Some(from_node),
                // No per-node path, so use the route, filtering out nodes that can't have repo paths
                None => {
                    if walk_item.target.get_type().allow_repo_path() {
                        from_route
                    } else {
                        None
                    }
                }
            },
        }
    }
}

impl fmt::Display for WrappedPath {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            WrappedPath::Root => write!(f, ""),
            WrappedPath::NonRoot(path) => write!(f, "{}", path.mpath()),
        }
    }
}

static PATH_HASHER_FACTORY: OnceCell<RandomState> = OnceCell::new();

impl From<Option<MPath>> for WrappedPath {
    fn from(mpath: Option<MPath>) -> Self {
        let hasher_fac = PATH_HASHER_FACTORY.get_or_init(|| RandomState::default());
        match mpath {
            Some(mpath) => WrappedPath::NonRoot(ArcIntern::new(EagerHashMemoizer::new(
                MPathWithHashMemo::new(mpath),
                hasher_fac,
            ))),
            None => WrappedPath::Root,
        }
    }
}

define_type_enum! {
    enum AliasType {
        GitSha1,
        Sha1,
        Sha256,
    }
}

impl fmt::Display for EdgeType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// File content gets a special two-state content so we can chose when to read the data
pub enum FileContentData {
    ContentStream(BoxStream<'static, Result<FileBytes, Error>>),
    Consumed(usize),
}

impl fmt::Debug for FileContentData {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FileContentData::ContentStream(_s) => write!(f, "FileContentData::ContentStream(_)"),
            FileContentData::Consumed(s) => write!(f, "FileContentData::Consumed({})", s),
        }
    }
}

/// The data from the walk - this is the "full" form but not necessarily fully loaded.
/// e.g. file content streams are passed to you to read, they aren't pre-loaded to bytes.
#[derive(Debug)]
pub enum NodeData {
    ErrorAsData(Node),
    // Weren't able to find node
    MissingAsData(Node),
    // Node has an invalid hash
    HashValidationFailureAsData(Node),
    NotRequired,
    // Bonsai
    Bookmark(ChangesetId),
    Changeset(BonsaiChangeset),
    BonsaiHgMapping(Option<HgChangesetId>),
    PhaseMapping(Option<Phase>),
    PublishedBookmarks,
    // Hg
    HgBonsaiMapping(Option<ChangesetId>),
    HgChangeset(HgBlobChangeset),
    HgChangesetViaBonsai(HgChangesetId),
    HgManifest(HgBlobManifest),
    HgFileEnvelope(HgFileEnvelope),
    HgFileNode(Option<FilenodeInfo>),
    HgManifestFileNode(Option<FilenodeInfo>),
    // Content
    FileContent(FileContentData),
    FileContentMetadata(Option<ContentMetadata>),
    AliasContentMapping(ContentId),
    // Derived data
    Blame(Option<Blame>),
    ChangesetInfo(Option<ChangesetInfo>),
    ChangesetInfoMapping(Option<ChangesetId>),
    DeletedManifest(Option<DeletedManifest>),
    DeletedManifestMapping(Option<DeletedManifestId>),
    FastlogBatch(Option<FastlogBatch>),
    FastlogDir(Option<FastlogBatch>),
    FastlogFile(Option<FastlogBatch>),
    Fsnode(Fsnode),
    FsnodeMapping(Option<FsnodeId>),
    SkeletonManifest(Option<SkeletonManifest>),
    SkeletonManifestMapping(Option<SkeletonManifestId>),
    UnodeFile(FileUnode),
    UnodeManifest(ManifestUnode),
    UnodeMapping(Option<ManifestUnodeId>),
}

#[derive(Clone)]
pub struct SqlShardInfo {
    pub filenodes: SqlTierInfo,
    pub active_keys_per_shard: Option<usize>,
}

// Which type of non-blobstore Mononoke sql shard this node needs access to
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SqlShard {
    Metadata,
    HgFileNode(usize),
}

impl Node {
    /// Map node to an SqlShard if any
    pub fn sql_shard(&self, shard_info: &SqlShardInfo) -> Option<SqlShard> {
        // Only report shards if there is a limit of keys per shard
        shard_info.active_keys_per_shard?;

        match self {
            Node::Root(_) => None,
            // Bonsai
            Node::Bookmark(_) => Some(SqlShard::Metadata),
            Node::Changeset(_) => None,
            Node::BonsaiHgMapping(_) => Some(SqlShard::Metadata),
            Node::PhaseMapping(_) => Some(SqlShard::Metadata),
            Node::PublishedBookmarks(_) => Some(SqlShard::Metadata),
            // Hg
            Node::HgBonsaiMapping(_) => Some(SqlShard::Metadata),
            Node::HgChangeset(_) => None,
            Node::HgChangesetViaBonsai(_) => Some(SqlShard::Metadata),
            Node::HgManifest(PathKey { id: _, path: _ }) => None,
            Node::HgFileEnvelope(_) => None,
            Node::HgFileNode(PathKey { id: _, path }) => {
                let path = path
                    .as_ref()
                    .map_or(RepoPath::RootPath, |p| RepoPath::FilePath(p.clone()));
                let path_hash = PathHash::from_repo_path(&path);
                let shard_num = path_hash.shard_number(shard_info.filenodes.shard_num.unwrap_or(1));
                Some(SqlShard::HgFileNode(shard_num))
            }
            Node::HgManifestFileNode(PathKey { id: _, path }) => {
                let path = path
                    .as_ref()
                    .map_or(RepoPath::RootPath, |p| RepoPath::DirectoryPath(p.clone()));
                let path_hash = PathHash::from_repo_path(&path);
                let shard_num = path_hash.shard_number(shard_info.filenodes.shard_num.unwrap_or(1));
                Some(SqlShard::HgFileNode(shard_num))
            }
            // Content
            Node::FileContent(_) => None,
            Node::FileContentMetadata(_) => None,
            Node::AliasContentMapping(_) => None,
            // Derived data
            Node::Blame(_) => None,
            Node::ChangesetInfo(_) => None,
            Node::ChangesetInfoMapping(_) => None,
            Node::DeletedManifest(_) => None,
            Node::DeletedManifestMapping(_) => None,
            Node::FastlogBatch(_) => None,
            Node::FastlogDir(_) => None,
            Node::FastlogFile(_) => None,
            Node::Fsnode(_) => None,
            Node::FsnodeMapping(_) => None,
            Node::SkeletonManifest(_) => None,
            Node::SkeletonManifestMapping(_) => None,
            Node::UnodeFile(_) => None,
            Node::UnodeManifest(_) => None,
            Node::UnodeMapping(_) => None,
        }
    }

    pub fn stats_key(&self) -> String {
        match self {
            Node::Root(_) => "root".to_string(),
            // Bonsai
            Node::Bookmark(k) => k.to_string(),
            Node::Changeset(k) => k.blobstore_key(),
            Node::BonsaiHgMapping(k) => k.blobstore_key(),
            Node::PhaseMapping(k) => k.blobstore_key(),
            Node::PublishedBookmarks(_) => "published_bookmarks".to_string(),
            // Hg
            Node::HgBonsaiMapping(k) => k.blobstore_key(),
            Node::HgChangeset(k) => k.blobstore_key(),
            Node::HgChangesetViaBonsai(k) => k.blobstore_key(),
            Node::HgManifest(PathKey { id, path: _ }) => id.blobstore_key(),
            Node::HgFileEnvelope(k) => k.blobstore_key(),
            Node::HgFileNode(PathKey { id, path: _ }) => id.blobstore_key(),
            Node::HgManifestFileNode(PathKey { id, path: _ }) => id.blobstore_key(),
            // Content
            Node::FileContent(k) => k.blobstore_key(),
            Node::FileContentMetadata(k) => k.blobstore_key(),
            Node::AliasContentMapping(k) => k.0.blobstore_key(),
            // Derived data
            Node::Blame(k) => k.blobstore_key(),
            Node::ChangesetInfo(k) => k.blobstore_key(),
            Node::ChangesetInfoMapping(k) => k.blobstore_key(),
            Node::DeletedManifest(k) => k.blobstore_key(),
            Node::DeletedManifestMapping(k) => k.blobstore_key(),
            Node::FastlogBatch(k) => k.blobstore_key(),
            Node::FastlogDir(k) => k.blobstore_key(),
            Node::FastlogFile(k) => k.blobstore_key(),
            Node::Fsnode(k) => k.blobstore_key(),
            Node::FsnodeMapping(k) => k.blobstore_key(),
            Node::SkeletonManifest(k) => k.blobstore_key(),
            Node::SkeletonManifestMapping(k) => k.blobstore_key(),
            Node::UnodeFile(k) => k.blobstore_key(),
            Node::UnodeManifest(k) => k.blobstore_key(),
            Node::UnodeMapping(k) => k.blobstore_key(),
        }
    }

    pub fn stats_path(&self) -> Option<&WrappedPath> {
        match self {
            Node::Root(_) => None,
            // Bonsai
            Node::Bookmark(_) => None,
            Node::Changeset(_) => None,
            Node::BonsaiHgMapping(_) => None,
            Node::PhaseMapping(_) => None,
            Node::PublishedBookmarks(_) => None,
            // Hg
            Node::HgBonsaiMapping(_) => None,
            Node::HgChangeset(_) => None,
            Node::HgChangesetViaBonsai(_) => None,
            Node::HgManifest(PathKey { id: _, path }) => Some(&path),
            Node::HgFileEnvelope(_) => None,
            Node::HgFileNode(PathKey { id: _, path }) => Some(&path),
            Node::HgManifestFileNode(PathKey { id: _, path }) => Some(&path),
            // Content
            Node::FileContent(_) => None,
            Node::FileContentMetadata(_) => None,
            Node::AliasContentMapping(_) => None,
            // Derived data
            Node::Blame(_) => None,
            Node::ChangesetInfo(_) => None,
            Node::ChangesetInfoMapping(_) => None,
            Node::DeletedManifest(_) => None,
            Node::DeletedManifestMapping(_) => None,
            Node::FastlogBatch(_) => None,
            Node::FastlogDir(_) => None,
            Node::FastlogFile(_) => None,
            Node::Fsnode(_) => None,
            Node::FsnodeMapping(_) => None,
            Node::SkeletonManifest(_) => None,
            Node::SkeletonManifestMapping(_) => None,
            Node::UnodeFile(_) => None,
            Node::UnodeManifest(_) => None,
            Node::UnodeMapping(_) => None,
        }
    }

    /// None means not hash based
    pub fn sampling_fingerprint(&self) -> Option<u64> {
        match self {
            Node::Root(_) => None,
            // Bonsai
            Node::Bookmark(_k) => None,
            Node::Changeset(k) => Some(k.sampling_fingerprint()),
            Node::BonsaiHgMapping(k) => Some(k.sampling_fingerprint()),
            Node::PhaseMapping(k) => Some(k.sampling_fingerprint()),
            Node::PublishedBookmarks(_) => None,
            // Hg
            Node::HgBonsaiMapping(k) => Some(k.sampling_fingerprint()),
            Node::HgChangeset(k) => Some(k.sampling_fingerprint()),
            Node::HgChangesetViaBonsai(k) => Some(k.sampling_fingerprint()),
            Node::HgManifest(PathKey { id, path: _ }) => Some(id.sampling_fingerprint()),
            Node::HgFileEnvelope(k) => Some(k.sampling_fingerprint()),
            Node::HgFileNode(PathKey { id, path: _ }) => Some(id.sampling_fingerprint()),
            Node::HgManifestFileNode(PathKey { id, path: _ }) => Some(id.sampling_fingerprint()),
            // Content
            Node::FileContent(k) => Some(k.sampling_fingerprint()),
            Node::FileContentMetadata(k) => Some(k.sampling_fingerprint()),
            Node::AliasContentMapping(k) => Some(k.0.sampling_fingerprint()),
            // Derived data
            Node::Blame(k) => Some(k.sampling_fingerprint()),
            Node::ChangesetInfo(k) => Some(k.sampling_fingerprint()),
            Node::ChangesetInfoMapping(k) => Some(k.sampling_fingerprint()),
            Node::DeletedManifest(k) => Some(k.sampling_fingerprint()),
            Node::DeletedManifestMapping(k) => Some(k.sampling_fingerprint()),
            Node::FastlogBatch(k) => Some(k.sampling_fingerprint()),
            Node::FastlogDir(k) => Some(k.sampling_fingerprint()),
            Node::FastlogFile(k) => Some(k.sampling_fingerprint()),
            Node::Fsnode(k) => Some(k.sampling_fingerprint()),
            Node::FsnodeMapping(k) => Some(k.sampling_fingerprint()),
            Node::SkeletonManifest(k) => Some(k.sampling_fingerprint()),
            Node::SkeletonManifestMapping(k) => Some(k.sampling_fingerprint()),
            Node::UnodeFile(k) => Some(k.sampling_fingerprint()),
            Node::UnodeManifest(k) => Some(k.sampling_fingerprint()),
            Node::UnodeMapping(k) => Some(k.sampling_fingerprint()),
        }
    }

    pub fn validate_hash(
        &self,
        ctx: CoreContext,
        repo: BlobRepo,
        node_data: &NodeData,
    ) -> BoxFuture<Result<(), Error>> {
        match (&self, node_data) {
            (Node::HgFileEnvelope(hg_filenode_id), NodeData::HgFileEnvelope(envelope)) => {
                let hg_filenode_id = hg_filenode_id.clone();
                let envelope = envelope.clone();
                async move {
                    let content_id = envelope.content_id();
                    let file_bytes =
                        filestore::fetch(repo.blobstore(), ctx, &envelope.content_id().into())
                            .await?;

                    let file_bytes = file_bytes.ok_or_else(|| {
                        format_err!(
                            "content {} not found for filenode {}",
                            content_id,
                            hg_filenode_id
                        )
                    })?;
                    let HgFileEnvelopeMut {
                        p1, p2, metadata, ..
                    } = envelope.into_mut();
                    let p1 = p1.map(|p| p.into_nodehash());
                    let p2 = p2.map(|p| p.into_nodehash());
                    let actual = calculate_hg_node_id_stream(
                        stream::once(async { Ok(metadata) })
                            .chain(file_bytes)
                            .boxed()
                            .compat(),
                        &HgParents::new(p1, p2),
                    )
                    .compat()
                    .await?;
                    let actual = HgFileNodeId::new(actual);

                    if actual != hg_filenode_id {
                        return Err(format_err!(
                            "failed to validate filenode hash: expected {} actual {}",
                            hg_filenode_id,
                            actual
                        ));
                    }
                    Ok(())
                }
                .boxed()
            }
            _ => {
                let ty = self.get_type();
                async move {
                    let s: &str = ty.into();
                    Err(format_err!("hash validation for {} is not supported", s,))
                }
                .boxed()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, iter::FromIterator, mem::size_of};
    use strum::{EnumCount, IntoEnumIterator};

    #[test]
    fn test_node_size() {
        // Node size is important as we have lots of them, add a test to check for accidental changes
        assert_eq!(48, size_of::<Node>());
    }

    #[test]
    fn test_node_type_max_ordinal() {
        // Check the macros worked consistently
        for t in NodeType::iter() {
            assert!((t as usize) < NodeType::COUNT)
        }
    }

    #[test]
    fn test_small_graphs() -> Result<(), Error> {
        create_graph!(
            Test1NodeType,
            Test1Node,
            Test1EdgeType,
            (Root, UnitKey, [Foo]),
            (Foo, u32, []),
        );
        assert_eq!(Test1NodeType::Root, Test1Node::Root(UnitKey()).get_type());
        assert_eq!(Test1NodeType::Foo, Test1Node::Foo(42).get_type());
        assert_eq!(Test1EdgeType::RootToFoo.incoming_type(), None);
        assert_eq!(Test1EdgeType::RootToFoo.outgoing_type(), Test1NodeType::Foo);
        assert_eq!(Test1NodeType::Foo.parse_node("123")?, Test1Node::Foo(123));

        // Make sure type names don't clash
        create_graph!(
            Test2NodeType,
            Test2Node,
            Test2EdgeType,
            (Root, UnitKey, [Foo, Bar]),
            (Foo, u32, [Bar]),
            (Bar, u32, []),
        );
        assert_eq!(Test2NodeType::Root, Test2Node::Root(UnitKey()).get_type());
        assert_eq!(Test2NodeType::Foo, Test2Node::Foo(42).get_type());
        assert_eq!(Test2NodeType::Bar, Test2Node::Bar(42).get_type());
        assert_eq!(Test2EdgeType::RootToFoo.incoming_type(), None);
        assert_eq!(Test2EdgeType::RootToFoo.outgoing_type(), Test2NodeType::Foo);
        assert_eq!(Test2EdgeType::RootToBar.incoming_type(), None);
        assert_eq!(Test2EdgeType::RootToBar.outgoing_type(), Test2NodeType::Bar);
        assert_eq!(
            Test2EdgeType::FooToBar.incoming_type(),
            Some(Test2NodeType::Foo)
        );
        assert_eq!(Test2EdgeType::FooToBar.outgoing_type(), Test2NodeType::Bar);
        assert_eq!(Test2NodeType::Bar.parse_node("123")?, Test2Node::Bar(123));
        Ok(())
    }

    #[test]
    fn test_all_derived_data_types_supported() {
        // All types blobrepo can support
        let a = test_repo_factory::default_test_repo_config()
            .derived_data_config
            .enabled
            .types;

        // supported in graph
        let mut s = HashSet::new();
        for t in NodeType::iter() {
            if let Some(d) = t.derived_data_name() {
                assert!(
                    a.contains(d),
                    "graph derived data type {} for {} is not known by default_test_repo_config()",
                    d,
                    t
                );
                s.insert(d);
            }
        }

        // If you are adding a new derived data type, please add it to the walker graph rather than to this
        // list, otherwise it won't get scrubbed and thus you would be unaware of different representation
        // in different stores
        let grandfathered: HashSet<&'static str> =
            HashSet::from_iter(vec!["git_trees"].into_iter());
        let mut missing = HashSet::new();
        for t in &a {
            if s.contains(t.as_str()) {
                assert!(
                    !grandfathered.contains(t.as_str()),
                    "You've added support for {}, please remove it from the grandfathered missing set",
                    t
                );
            } else if !grandfathered.contains(t.as_str()) {
                missing.insert(t);
            }
        }
        assert!(
            missing.is_empty(),
            "blobrepo derived data types {:?} not supported by walker graph",
            missing,
        );
    }
}
