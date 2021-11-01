# Copyright (c) Facebook, Inc. and its affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from edenscm.mercurial import error, scmutil
from edenscm.mercurial.cmdutil import changeset_printer
from edenscm.mercurial.context import memctx, memfilectx
from edenscm.mercurial.edenapi_upload import (
    getreponame,
)
from edenscm.mercurial.i18n import _
from edenscm.mercurial.node import nullid
from edenscm.mercurial.util import pickle


def _snapshot2ctx(repo, snapshot):
    """Build a memctx for this snapshot.

    This is not precisely correct as it doesn't differentiate untracked/added
    but it's good enough for diffing.
    """

    parent = snapshot["hg_parents"]
    # Once merges/conflicted states are supported, we'll need to support more
    # than one parent
    assert isinstance(parent, bytes)

    parents = (parent, nullid)
    path2filechange = {f[0]: f[1] for f in snapshot["file_changes"]}

    def token2cacheable(token):
        data = token["data"]
        return pickle.dumps((data["id"], data["bubble_id"]))

    cache = {}

    def getfile(repo, memctx, path):
        change = path2filechange.get(path)
        if change is None:
            return repo[parent][path]
        if change == "Deletion" or change == "UntrackedDeletion":
            return None
        elif "Change" in change or "UntrackedChange" in change:
            change = change.get("Change") or change["UntrackedChange"]
            token = change["upload_token"]
            key = token2cacheable(token)
            if key not in cache:
                # Possible future optimisation: Download files in parallel
                cache[key] = repo.edenapi.downloadfiletomemory(getreponame(repo), token)
            islink = change["file_type"] == "Symlink"
            isexec = change["file_type"] == "Executable"
            return memfilectx(
                repo, None, path, data=cache[key], islink=islink, isexec=isexec
            )
        else:
            raise error.Abort(_("Unknown file change {}").format(change))

    ctx = memctx(
        repo,
        parents,
        text="",
        files=list(path2filechange.keys()),
        filectxfn=getfile,
        user=None,
        date=None,
    )
    return ctx


def show(ui, repo, csid, **opts):
    try:
        snapshot = repo.edenapi.fetchsnapshot(
            getreponame(repo),
            {
                "cs_id": bytes.fromhex(csid),
            },
        )
    except Exception:
        raise error.Abort(_("snapshot doesn't exist"))
    else:
        ui.status(_("snapshot: {}\n").format(csid))
        ctx = _snapshot2ctx(repo, snapshot)
        displayer = changeset_printer(
            ui, repo, scmutil.matchall(repo), {"patch": True}, False
        )
        displayer.show(ctx)
        displayer.close()