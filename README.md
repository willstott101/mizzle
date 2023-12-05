git is a graph database implemented as flat files, and blob storage on top of a block device

gitoxide is a pure-rust implementation of git

mizzle is a library for:
* accessing and mutating horizontally-scalable git repositories stored using cloud-native technologies
* writing horizontally-scalable servers exposing git's http protocol

mizzle thanks gitoxide, git, and rust


Things to fix in git docs:

	S: 200 OK
	S: <Some headers>
	S: ...
	S:
	S: 000eversion 2\n
	S: <capability-advertisement>

then goes on to say that `<capability-advertisement>`` contains `000eversion 2\n`