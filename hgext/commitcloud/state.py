# Copyright 2018 Facebook, Inc.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

from __future__ import absolute_import

# Standard Library
import hashlib
import json

from mercurial.i18n import _

from . import commitcloudcommon, commitcloudutil


class SyncState(object):
    """
    Stores the local record of what state was stored in the cloud at the
    last sync.
    """

    prefix = "commitcloudstate."

    @classmethod
    def _filename(cls, workspace):
        # make a unique valid filename
        return (
            cls.prefix
            + "".join(x for x in workspace if x.isalnum())
            + ".%s" % (hashlib.sha256(workspace).hexdigest()[0:5])
        )

    @classmethod
    def erasestate(cls, repo, workspace):
        filename = cls._filename(workspace)
        # clean up the current state in force recover mode
        repo.svfs.tryunlink(filename)

    def __init__(self, repo, workspace):
        self.filename = self._filename(workspace)
        self.repo = repo
        if repo.svfs.exists(self.filename):
            with repo.svfs.open(self.filename, "r") as f:
                try:
                    data = json.load(f)
                except Exception:
                    raise commitcloudcommon.InvalidWorkspaceDataError(
                        repo.ui, _("failed to parse %s") % self.filename
                    )

                self.version = data["version"]
                self.heads = [h.encode() for h in data["heads"]]
                self.bookmarks = {
                    n.encode("utf-8"): v.encode() for n, v in data["bookmarks"].items()
                }
                self.omittedheads = [h.encode() for h in data.get("omittedheads", ())]
                self.omittedbookmarks = [
                    n.encode("utf-8") for n in data.get("omittedbookmarks", ())
                ]
        else:
            self.version = 0
            self.heads = []
            self.bookmarks = {}
            self.omittedheads = []
            self.omittedbookmarks = []

    def update(
        self, newversion, newheads, newbookmarks, newomittedheads, newomittedbookmarks
    ):
        data = {
            "version": newversion,
            "heads": newheads,
            "bookmarks": newbookmarks,
            "omittedheads": newomittedheads,
            "omittedbookmarks": newomittedbookmarks,
        }
        with self.repo.svfs.open(self.filename, "w", atomictemp=True) as f:
            json.dump(data, f)
        self.version = newversion
        self.heads = newheads
        self.bookmarks = newbookmarks
        self.omittedheads = newomittedheads
        self.omittedbookmarks = newomittedbookmarks
