/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use blobrepo::BlobRepo;
use bookmarks::{BookmarkUpdateReason, BundleReplay};
use bookmarks_types::BookmarkName;
use bytes::Bytes;
use context::CoreContext;
use hooks::HookManager;
use metaconfig_types::{
    BookmarkAttrs, InfinitepushParams, PushrebaseParams, SourceControlServiceParams,
};
use mononoke_types::{BonsaiChangeset, ChangesetId};
use reachabilityindex::LeastCommonAncestorsHint;

use crate::affected_changesets::{AdditionalChangesets, AffectedChangesets};
use crate::restrictions::{BookmarkKind, BookmarkKindRestrictions, BookmarkMoveAuthorization};
use crate::BookmarkMovementError;

/// The old and new changeset during a bookmark update.
///
/// This is a struct to make sure it is clear which is the old target and which is the new.
pub struct BookmarkUpdateTargets {
    pub old: ChangesetId,
    pub new: ChangesetId,
}

/// Which kinds of bookmark updates are allowed for a request.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BookmarkUpdatePolicy {
    /// Only allow fast-forward moves (updates where the new target is a descendant
    /// of the old target).
    FastForwardOnly,

    /// Allow any update that is permitted for the bookmark by repo config.
    AnyPermittedByConfig,
}

impl BookmarkUpdatePolicy {
    async fn check_update_permitted(
        &self,
        ctx: &CoreContext,
        repo: &BlobRepo,
        lca_hint: &dyn LeastCommonAncestorsHint,
        bookmark_attrs: &BookmarkAttrs,
        bookmark: &BookmarkName,
        targets: &BookmarkUpdateTargets,
    ) -> Result<(), BookmarkMovementError> {
        let fast_forward_only = match self {
            Self::FastForwardOnly => true,
            Self::AnyPermittedByConfig => bookmark_attrs.is_fast_forward_only(&bookmark),
        };
        if fast_forward_only && targets.old != targets.new {
            // Check that this move is a fast-forward move.
            let is_ancestor = lca_hint
                .is_ancestor(ctx, &repo.get_changeset_fetcher(), targets.old, targets.new)
                .await?;
            if !is_ancestor {
                return Err(BookmarkMovementError::NonFastForwardMove {
                    from: targets.old,
                    to: targets.new,
                });
            }
        }
        Ok(())
    }
}

pub struct UpdateBookmarkOp<'op> {
    bookmark: &'op BookmarkName,
    targets: BookmarkUpdateTargets,
    update_policy: BookmarkUpdatePolicy,
    reason: BookmarkUpdateReason,
    auth: BookmarkMoveAuthorization<'op>,
    kind_restrictions: BookmarkKindRestrictions,
    affected_changesets: AffectedChangesets,
    pushvars: Option<&'op HashMap<String, Bytes>>,
    bundle_replay: Option<&'op dyn BundleReplay>,
}

#[must_use = "UpdateBookmarkOp must be run to have an effect"]
impl<'op> UpdateBookmarkOp<'op> {
    pub fn new(
        bookmark: &'op BookmarkName,
        targets: BookmarkUpdateTargets,
        update_policy: BookmarkUpdatePolicy,
        reason: BookmarkUpdateReason,
    ) -> UpdateBookmarkOp<'op> {
        UpdateBookmarkOp {
            bookmark,
            targets,
            update_policy,
            reason,
            auth: BookmarkMoveAuthorization::User,
            kind_restrictions: BookmarkKindRestrictions::AnyKind,
            affected_changesets: AffectedChangesets::new(),
            pushvars: None,
            bundle_replay: None,
        }
    }

    /// This bookmark change is for an authenticated named service.  The change
    /// will be checked against the service's write restrictions.
    pub fn for_service(
        mut self,
        service_name: impl Into<String>,
        params: &'op SourceControlServiceParams,
    ) -> Self {
        self.auth = BookmarkMoveAuthorization::Service(service_name.into(), params);
        self
    }

    pub fn only_if_scratch(mut self) -> Self {
        self.kind_restrictions = BookmarkKindRestrictions::OnlyScratch;
        self
    }

    pub fn only_if_public(mut self) -> Self {
        self.kind_restrictions = BookmarkKindRestrictions::OnlyPublic;
        self
    }

    pub fn with_pushvars(mut self, pushvars: Option<&'op HashMap<String, Bytes>>) -> Self {
        self.pushvars = pushvars;
        self
    }

    pub fn with_bundle_replay_data(mut self, bundle_replay: Option<&'op dyn BundleReplay>) -> Self {
        self.bundle_replay = bundle_replay;
        self
    }

    /// Include bonsai changesets for changesets that have just been added to
    /// the repository.
    pub fn with_new_changesets(
        mut self,
        changesets: HashMap<ChangesetId, BonsaiChangeset>,
    ) -> Self {
        self.affected_changesets.add_new_changesets(changesets);
        self
    }

    pub async fn run(
        mut self,
        ctx: &'op CoreContext,
        repo: &'op BlobRepo,
        lca_hint: &'op Arc<dyn LeastCommonAncestorsHint>,
        infinitepush_params: &'op InfinitepushParams,
        pushrebase_params: &'op PushrebaseParams,
        bookmark_attrs: &'op BookmarkAttrs,
        hook_manager: &'op HookManager,
    ) -> Result<(), BookmarkMovementError> {
        let kind = self
            .kind_restrictions
            .check_kind(infinitepush_params, self.bookmark)?;

        self.auth
            .check_authorized(ctx, bookmark_attrs, self.bookmark, kind)?;

        self.update_policy
            .check_update_permitted(
                ctx,
                repo,
                lca_hint.as_ref(),
                bookmark_attrs,
                &self.bookmark,
                &self.targets,
            )
            .await?;

        self.affected_changesets
            .check_restrictions(
                ctx,
                repo,
                lca_hint,
                bookmark_attrs,
                hook_manager,
                self.bookmark,
                self.pushvars,
                self.reason,
                kind,
                &self.auth,
                AdditionalChangesets::Range {
                    head: self.targets.new,
                    base: self.targets.old,
                },
            )
            .await?;
        let mut txn = repo.update_bookmark_transaction(ctx.clone());
        let mut txn_hook = None;

        match kind {
            BookmarkKind::Scratch => {
                txn.update_scratch(self.bookmark, self.targets.new, self.targets.old)?;
            }
            BookmarkKind::Public => {
                crate::globalrev_mapping::require_globalrevs_disabled(pushrebase_params)?;
                txn_hook = crate::git_mapping::populate_git_mapping_txn_hook(
                    ctx,
                    repo,
                    pushrebase_params,
                    self.targets.new,
                    &self.affected_changesets.new_changesets(),
                )
                .await?;
                txn.update(
                    self.bookmark,
                    self.targets.new,
                    self.targets.old,
                    self.reason,
                    self.bundle_replay,
                )?;
            }
        }

        let ok = match txn_hook {
            Some(txn_hook) => txn.commit_with_hook(txn_hook).await?,
            None => txn.commit().await?,
        };
        if !ok {
            return Err(BookmarkMovementError::TransactionFailed);
        }

        Ok(())
    }
}
