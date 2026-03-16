git is a graph database implemented as flat files, and blob storage on top of a block device

gitoxide is a pure-rust implementation of git

mizzle is a library for:
* accessing and mutating horizontally-scalable git repositories stored using cloud-native technologies
* writing horizontally-scalable servers exposing git's http protocol

mizzle thanks gitoxide, git, and rust

## Design rules

**Authorisers must never open the repository.**
`RepoAccess::authorize_push` receives all the information needed to make an
authorisation decision as plain values — repo path, ref name, and a `PushKind`
enum that encodes create/delete/fast-forward/force.  If an authoriser needs to
inspect the object graph it is a bug in mizzle's callback interface, not in the
authoriser.

## TODO

- [ ] Shallow clone (`--depth N`) — essential for CI/CD workloads
- [ ] Protocol v1 support — compatibility with older git clients and tooling that doesn't send `Git-Protocol: version=2`
- [ ] Fetch negotiation — proper ACK/NAK handling so incremental fetches send minimal packs rather than always recomputing from scratch
- [x] Server-side hooks — `post_receive` callback on `RepoAccess` called after refs are updated
- [x] Repository auto-init — `auto_init()` on `RepoAccess`; mizzle calls `git init --bare` on first push if the path doesn't exist
- [ ] Partial clone filters (`--filter=blob:none`, `--filter=tree:0`)
- [x] Ref-in-want
- [x] `wait-for-done`


Things to fix in git docs:

	S: 200 OK
	S: <Some headers>
	S: ...
	S:
	S: 000eversion 2\n
	S: <capability-advertisement>

then goes on to say that `<capability-advertisement>`` contains `000eversion 2\n`

not all the fetch args specify that multiple entries of that arg can be specified