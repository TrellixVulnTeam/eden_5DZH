# wireproto.py - generic wire protocol support functions
#
# Copyright 2005-2010 Matt Mackall <mpm@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

import urllib, tempfile, os
from i18n import _
from node import bin, hex
import changegroup as changegroupmod
import streamclone, repo, error, encoding, util
import pushkey as pushkey_

# client side

class wirerepository(repo.repository):
    def lookup(self, key):
        self.requirecap('lookup', _('look up remote revision'))
        d = self._call("lookup", key=key)
        success, data = d[:-1].split(" ", 1)
        if int(success):
            return bin(data)
        self._abort(error.RepoError(data))

    def heads(self):
        d = self._call("heads")
        try:
            return map(bin, d[:-1].split(" "))
        except:
            self.abort(error.ResponseError(_("unexpected response:"), d))

    def branchmap(self):
        d = self._call("branchmap")
        try:
            branchmap = {}
            for branchpart in d.splitlines():
                branchheads = branchpart.split(' ')
                branchname = urllib.unquote(branchheads[0])
                # Earlier servers (1.3.x) send branch names in (their) local
                # charset. The best we can do is assume it's identical to our
                # own local charset, in case it's not utf-8.
                try:
                    branchname.decode('utf-8')
                except UnicodeDecodeError:
                    branchname = encoding.fromlocal(branchname)
                branchheads = [bin(x) for x in branchheads[1:]]
                branchmap[branchname] = branchheads
            return branchmap
        except TypeError:
            self._abort(error.ResponseError(_("unexpected response:"), d))

    def branches(self, nodes):
        n = " ".join(map(hex, nodes))
        d = self._call("branches", nodes=n)
        try:
            br = [tuple(map(bin, b.split(" "))) for b in d.splitlines()]
            return br
        except:
            self._abort(error.ResponseError(_("unexpected response:"), d))

    def between(self, pairs):
        batch = 8 # avoid giant requests
        r = []
        for i in xrange(0, len(pairs), batch):
            n = " ".join(["-".join(map(hex, p)) for p in pairs[i:i + batch]])
            d = self._call("between", pairs=n)
            try:
                r += [l and map(bin, l.split(" ")) or []
                      for l in d.splitlines()]
            except:
                self._abort(error.ResponseError(_("unexpected response:"), d))
        return r

    def pushkey(self, namespace, key, old, new):
        if not self.capable('pushkey'):
            return False
        d = self._call("pushkey",
                      namespace=namespace, key=key, old=old, new=new)
        return bool(int(d))

    def listkeys(self, namespace):
        if not self.capable('pushkey'):
            return {}
        d = self._call("listkeys", namespace=namespace)
        r = {}
        for l in d.splitlines():
            k, v = l.split('\t')
            r[k.decode('string-escape')] = v.decode('string-escape')
        return r

    def stream_out(self):
        return self._callstream('stream_out')

    def changegroup(self, nodes, kind):
        n = " ".join(map(hex, nodes))
        f = self._callstream("changegroup", roots=n)
        return self._decompress(f)

    def changegroupsubset(self, bases, heads, kind):
        self.requirecap('changegroupsubset', _('look up remote changes'))
        bases = " ".join(map(hex, bases))
        heads = " ".join(map(hex, heads))
        return self._decompress(self._callstream("changegroupsubset",
                                                 bases=bases, heads=heads))

    def unbundle(self, cg, heads, source):
        '''Send cg (a readable file-like object representing the
        changegroup to push, typically a chunkbuffer object) to the
        remote server as a bundle. Return an integer indicating the
        result of the push (see localrepository.addchangegroup()).'''

        ret, output = self._callpush("unbundle", cg, heads=' '.join(map(hex, heads)))
        if ret == "":
            raise error.ResponseError(
                _('push failed:'), output)
        try:
            ret = int(ret)
        except ValueError, err:
            raise error.ResponseError(
                _('push failed (unexpected response):'), ret)

        for l in output.splitlines(True):
            self.ui.status(_('remote: '), l)
        return ret

# server side

def dispatch(repo, proto, command):
    if command not in commands:
        return False
    func, spec = commands[command]
    args = proto.getargs(spec)
    r = func(repo, proto, *args)
    if r != None:
        proto.respond(r)
    return True

def between(repo, proto, pairs):
    pairs = [map(bin, p.split("-")) for p in pairs.split(" ")]
    r = []
    for b in repo.between(pairs):
        r.append(" ".join(map(hex, b)) + "\n")
    return "".join(r)

def branchmap(repo, proto):
    branchmap = repo.branchmap()
    heads = []
    for branch, nodes in branchmap.iteritems():
        branchname = urllib.quote(branch)
        branchnodes = [hex(node) for node in nodes]
        heads.append('%s %s' % (branchname, ' '.join(branchnodes)))
    return '\n'.join(heads)

def branches(repo, proto, nodes):
    nodes = map(bin, nodes.split(" "))
    r = []
    for b in repo.branches(nodes):
        r.append(" ".join(map(hex, b)) + "\n")
    return "".join(r)

def changegroup(repo, proto, roots):
    nodes = map(bin, roots.split(" "))
    cg = repo.changegroup(nodes, 'serve')
    proto.sendchangegroup(cg)

def changegroupsubset(repo, proto, bases, heads):
    bases = [bin(n) for n in bases.split(' ')]
    heads = [bin(n) for n in heads.split(' ')]
    cg = repo.changegroupsubset(bases, heads, 'serve')
    proto.sendchangegroup(cg)

def heads(repo, proto):
    h = repo.heads()
    return " ".join(map(hex, h)) + "\n"

def listkeys(repo, proto, namespace):
    d = pushkey_.list(repo, namespace).items()
    t = '\n'.join(['%s\t%s' % (k.encode('string-escape'),
                               v.encode('string-escape')) for k, v in d])
    return t

def lookup(repo, proto, key):
    try:
        r = hex(repo.lookup(key))
        success = 1
    except Exception, inst:
        r = str(inst)
        success = 0
    return "%s %s\n" % (success, r)

def pushkey(repo, proto, namespace, key, old, new):
    r = pushkey_.push(repo, namespace, key, old, new)
    return '%s\n' % int(r)

def stream(repo, proto):
    try:
        proto.sendstream(streamclone.stream_out(repo))
    except streamclone.StreamException, inst:
        return str(inst)

def unbundle(repo, proto, heads):
    their_heads = heads.split()

    def check_heads():
        heads = map(hex, repo.heads())
        return their_heads == [hex('force')] or their_heads == heads

    # fail early if possible
    if not check_heads():
        repo.respond(_('unsynced changes'))
        return

    # write bundle data to temporary file because it can be big
    fd, tempname = tempfile.mkstemp(prefix='hg-unbundle-')
    fp = os.fdopen(fd, 'wb+')
    r = 0
    proto.redirect()
    try:
        proto.getfile(fp)
        lock = repo.lock()
        try:
            if not check_heads():
                # someone else committed/pushed/unbundled while we
                # were transferring data
                proto.respond(_('unsynced changes'))
                return

            # push can proceed
            fp.seek(0)
            header = fp.read(6)
            if header.startswith('HG'):
                if not header.startswith('HG10'):
                    raise ValueError('unknown bundle version')
                elif header not in changegroupmod.bundletypes:
                    raise ValueError('unknown bundle compression type')
            gen = changegroupmod.unbundle(header, fp)

            try:
                r = repo.addchangegroup(gen, 'serve', proto._client(),
                                        lock=lock)
            except util.Abort, inst:
                sys.stderr.write("abort: %s\n" % inst)
        finally:
            lock.release()
            proto.respondpush(r)

    finally:
        fp.close()
        os.unlink(tempname)

commands = {
    'between': (between, 'pairs'),
    'branchmap': (branchmap, ''),
    'branches': (branches, 'nodes'),
    'changegroup': (changegroup, 'roots'),
    'changegroupsubset': (changegroupsubset, 'bases heads'),
    'heads': (heads, ''),
    'listkeys': (listkeys, 'namespace'),
    'lookup': (lookup, 'key'),
    'pushkey': (pushkey, 'namespace key old new'),
    'stream_out': (stream, ''),
    'unbundle': (unbundle, 'heads'),
}
