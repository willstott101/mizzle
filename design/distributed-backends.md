# Distributed Backend Candidates

The thin storage trait has two distinct access patterns worth separating
when targeting distributed systems:

- **Objects** (`read_object`, `has_object`, `write_objects`): immutable,
  content-addressed — any CAS or blob store works.
- **Refs** (`update_refs`): mutable pointers requiring compare-and-swap —
  the hard constraint that rules out eventually-consistent stores.

## FoundationDB

Strictly serializable ACID transactions, used by Apple at iCloud scale.
Objects stored as immutable KV pairs (`oid → serialized object`). Refs use
FoundationDB's transactional CAS — `update_refs` becomes a transaction that
reads the current ref value, validates the old OID, and atomically sets the
new one. Racing pushes cause one transaction to abort; the client receives a
clean rejection.

[Tigris](https://www.tigrisdata.com/) is an S3-compatible object store
built on FoundationDB and would expose the same strong consistency with a
familiar API.

## Cloudflare Durable Objects + R2

The most direct path to the "serverless git" model. Each repo becomes a
Durable Object — single-actor serialized execution means ref updates are
implicitly serialized without any explicit locking. R2 (or any blob store)
holds the objects.

Maps onto the thin storage trait cleanly: one Durable Object per repo
handles `list_refs` and `update_refs`; R2 handles the object side.

## Google Spanner

TrueTime gives external consistency — push ordering is globally agreed upon
across datacenters without clock skew ambiguity. Expensive to operate but
architecturally the most correct option if linearizability across regions is
a hard requirement.

## CockroachDB / YugabyteDB

Distributed PostgreSQL that lands directly on the SQL backend schema. The
`commit_parents` graph traversal for fetch negotiation becomes a distributed
recursive CTE. `update_refs` is a `SELECT ... FOR UPDATE` + `UPDATE` in a
serializable transaction.

## etcd (refs) + object storage (objects)

Mirrors git's own model. etcd is purpose-built for distributed locking and
CAS — its native `txn` operation does compare-and-swap with watches,
mapping directly to git's old-OID/new-OID ref update semantics. Only refs
live in etcd (tiny data volume); objects go to any blob store. etcd's watch
API also gives free ref-change event streaming for CI trigger hooks.

## NATS JetStream KV

Lighter operational footprint. JetStream KV provides CAS via
revision-based `Update(key, value, revision)` — optimistic locking that
maps directly to git's ref update semantics.

## TiKV

Exposes two APIs over the same cluster: a raw KV API (high throughput, no
transactions) and a transactional API (Percolator-based). Objects go via
raw KV. Refs go via the transactional API with CAS semantics. One cluster,
two access patterns, no operational split.

---

## Locking model shared by the ACID options

FoundationDB, Spanner, CockroachDB, and TiKV all handle concurrent pushes
the same way: `update_refs` is a serializable transaction that reads current
ref values, validates old OIDs against the incoming push, and commits or
aborts. If two clients push to the same ref simultaneously, one transaction
wins and the other gets a conflict error, which mizzle surfaces as a
standard ref update rejection. This matches git's own semantics and requires
no additional lock infrastructure in mizzle.
