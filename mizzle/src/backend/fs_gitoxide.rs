//! Filesystem backend using gitoxide.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use anyhow::{Context, Result};
use gix::objs::Exists;
use gix::parallel::InOrderIter;
use gix::ObjectId;
use gix_ref::file::ReferenceExt;

use crate::auth_types::{CommitInfo, RefDiff, RefDiffChange, RefDiffEntry};
use crate::backend::{
    HeadInfo, PackMetadata, PackOptions, PackOutput, ReachableError, RefInfo, RefUpdate,
    RefsSnapshot, StorageBackend,
};
use crate::pack;
use crate::pack_reuse;
use crate::traits::PushKind;

/// Filesystem backend using gitoxide.
///
/// This is the default backend.  The [`open`](StorageBackend::open) method
/// returns an [`FsGitoxideRepo`] handle that is reused across all method calls
/// within a single request, avoiding repeated `gix::open()` overhead.
#[derive(Clone, Copy)]
pub struct FsGitoxide;

/// Opened repository handle for [`FsGitoxide`].
pub struct FsGitoxideRepo {
    repo: gix::ThreadSafeRepository,
}

/// Pack and index files written by [`FsGitoxide::ingest_pack`].
///
/// If auth is denied after ingestion, pass this to
/// [`FsGitoxide::rollback_ingest`] to remove the files.
pub struct FsWrittenPack {
    pack: PathBuf,
    index: PathBuf,
}

/// Maximum number of commits to walk when determining fast-forward status.
/// If exceeded, the push is conservatively classified as a force-push.
const MAX_FF_WALK: usize = 10_000;

impl StorageBackend for FsGitoxide {
    type RepoId = PathBuf;
    type Repo = FsGitoxideRepo;
    type IngestedPack = FsWrittenPack;

    fn open(&self, id: &PathBuf) -> Result<FsGitoxideRepo> {
        let repo = gix::open(id)?.into_sync();
        Ok(FsGitoxideRepo { repo })
    }

    fn list_refs(&self, repo: &FsGitoxideRepo) -> Result<RefsSnapshot> {
        let repo = &repo.repo;

        // HEAD
        let head = {
            let local = repo.to_thread_local();
            match local.head_ref()? {
                Some(mut head_ref) => {
                    let symref_name = head_ref.name().as_bstr().to_string();
                    head_ref.peel_to_id()?;
                    head_ref.inner.peeled.map(|oid| HeadInfo {
                        oid,
                        symref_target: Some(symref_name),
                    })
                }
                None => None,
            }
        };

        // All refs (excluding HEAD which is handled above)
        let mut refs = Vec::new();
        for reference in repo.refs.iter()?.all()? {
            let r = reference?;
            let name = r.name.as_bstr().to_string();
            if name == "HEAD" {
                continue;
            }

            let mut to_peel = r.clone();
            match r.target {
                gix_ref::Target::Object(oid) => {
                    let peeled_id = to_peel.peel_to_id(&repo.refs, &repo.objects.to_handle())?;
                    let peeled = if peeled_id != oid {
                        Some(peeled_id.to_owned())
                    } else {
                        None
                    };
                    refs.push(RefInfo {
                        name,
                        oid: oid.to_owned(),
                        peeled,
                        symref_target: None,
                    });
                }
                gix_ref::Target::Symbolic(target) => {
                    let peeled_id = to_peel.peel_to_id(&repo.refs, &repo.objects.to_handle())?;
                    refs.push(RefInfo {
                        name,
                        oid: peeled_id.to_owned(),
                        peeled: None,
                        symref_target: Some(target.as_bstr().to_string()),
                    });
                }
            }
        }

        Ok(RefsSnapshot { head, refs })
    }

    fn resolve_ref(&self, repo: &FsGitoxideRepo, refname: &str) -> Result<Option<ObjectId>> {
        let local = repo.repo.to_thread_local();
        match local.find_reference(refname) {
            Ok(mut r) => match r.peel_to_id() {
                Ok(id) => Ok(Some(id.detach())),
                Err(_) => Ok(None),
            },
            Err(_) => Ok(None),
        }
    }

    fn update_refs(&self, repo: &FsGitoxideRepo, updates: &[RefUpdate]) -> Result<()> {
        use gix_ref::{
            transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog},
            Target,
        };

        let local = repo.repo.to_thread_local();

        let edits: Vec<RefEdit> = updates
            .iter()
            .map(|u| {
                let name = u
                    .refname
                    .as_str()
                    .try_into()
                    .with_context(|| format!("invalid ref name {}", u.refname))?;

                let change = if u.new_oid.is_null() {
                    // Delete: require the ref to exist and match old_oid (or just exist if
                    // old_oid is null, which the protocol shouldn't send but is defensive).
                    let expected = if u.old_oid.is_null() {
                        PreviousValue::MustExist
                    } else {
                        PreviousValue::MustExistAndMatch(Target::Object(u.old_oid))
                    };
                    Change::Delete {
                        expected,
                        log: RefLog::AndReference,
                    }
                } else if u.old_oid.is_null() {
                    // Create: ref must not already exist.
                    Change::Update {
                        log: LogChange {
                            message: "push".into(),
                            ..Default::default()
                        },
                        expected: PreviousValue::MustNotExist,
                        new: Target::Object(u.new_oid),
                    }
                } else {
                    // Update: ref must exist and match old_oid (CAS).
                    Change::Update {
                        log: LogChange {
                            message: "push".into(),
                            ..Default::default()
                        },
                        expected: PreviousValue::MustExistAndMatch(Target::Object(u.old_oid)),
                        new: Target::Object(u.new_oid),
                    }
                };

                Ok(RefEdit {
                    change,
                    name,
                    deref: false,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        // Use Fail::Immediately so a concurrent push holding the same ref's lock
        // gets an instant error rather than stalling the request for 100ms.
        local
            .refs
            .transaction()
            .prepare(
                edits,
                gix_lock::acquire::Fail::Immediately,
                gix_lock::acquire::Fail::Immediately,
            )
            .context("preparing ref transaction")?
            .commit(local.committer().transpose().context("reading committer")?)
            .context("committing ref transaction")?;

        Ok(())
    }

    fn init_repo(&self, repo_path: &PathBuf) -> Result<()> {
        if !repo_path.exists() {
            gix::init_bare(repo_path)
                .with_context(|| format!("initialising bare repo at {}", repo_path.display()))?;
        }
        Ok(())
    }

    fn has_object(&self, repo: &FsGitoxideRepo, oid: &ObjectId) -> Result<bool> {
        Ok(repo.repo.objects.to_handle().exists(oid))
    }

    fn has_objects(&self, repo: &FsGitoxideRepo, oids: &[ObjectId]) -> Result<Vec<bool>> {
        let store = repo.repo.objects.to_handle();
        Ok(oids.iter().map(|oid| store.exists(oid)).collect())
    }

    fn compute_push_kind(&self, repo: &FsGitoxideRepo, update: &RefUpdate) -> PushKind {
        if update.old_oid.is_null() {
            return PushKind::Create;
        }
        if update.new_oid.is_null() {
            return PushKind::Delete;
        }

        let local = repo.repo.to_thread_local();
        let odb = local.objects.into_inner();

        let is_ff = gix::traverse::commit::Simple::new(std::iter::once(update.new_oid), odb)
            .take(MAX_FF_WALK)
            .any(|r: Result<gix::traverse::commit::Info, _>| {
                r.map(|info| info.id == update.old_oid).unwrap_or(false)
            });

        if is_ff {
            PushKind::FastForward
        } else {
            PushKind::ForcePush
        }
    }

    fn build_pack(
        &self,
        repo: &FsGitoxideRepo,
        want: &[ObjectId],
        have: &[ObjectId],
        opts: &PackOptions,
    ) -> Result<PackOutput> {
        // Fast path: when no shallow / filter / thin-pack rewriting is
        // requested and a local pack's bitmap proves it contains exactly the
        // request closure, stream the pack file verbatim instead of running
        // the count / compress / chunk pipeline.  See [`crate::pack_reuse`].
        if opts.deepen.is_none() && opts.filter.is_none() && !opts.thin_pack {
            let pack_dir = repo.repo.objects_dir().join("pack");
            if let Some(pack_path) = pack_reuse::find_reusable_pack(&pack_dir, want, have)? {
                return ship_pack_as_is(&pack_path);
            }
        }

        let handle = repo.repo.to_thread_local().objects;

        // Try to answer the have-set via a pack reachability bitmap first.
        // If unavailable (no `.bitmap` in any pack, or a have commit isn't
        // covered) fall back to the full walker in `pack::build_have_set`.
        let bitmap_have_set = try_bitmap_have_set(repo, have);

        let pack_objects = if let Some(have_set) = bitmap_have_set {
            pack::objects_for_fetch_with_have_set(
                handle.clone().into_inner(),
                want,
                have,
                opts.deepen,
                opts.filter.as_ref(),
                have_set,
            )?
        } else {
            pack::objects_for_fetch_filtered(
                handle.clone().into_inner(),
                want,
                have,
                opts.deepen,
                opts.filter.as_ref(),
            )?
        };

        let thin_pack = opts.thin_pack;
        let objects = pack_objects.objects;

        // Bounded channel: 4 chunks in flight provides pipelining without
        // unbounded memory growth.
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(4);

        let (progress_tx, progress_rx) = mpsc::channel::<String>();

        let thread = std::thread::spawn(move || -> Result<()> {
            stream_pack_to_channel(handle, objects, thin_pack, tx, progress_tx)
        });

        let reader = PackReader {
            rx,
            current: io::Cursor::new(Vec::new()),
            thread: Some(thread),
        };

        Ok(PackOutput {
            reader: Box::new(reader),
            shallow: pack_objects.shallow,
            progress: Some(progress_rx),
        })
    }

    fn ingest_pack(
        &self,
        repo: &FsGitoxideRepo,
        staged_pack: &Path,
    ) -> Result<Option<FsWrittenPack>> {
        use std::io::{Read, Seek};

        let mut file = std::fs::File::open(staged_pack).context("opening staged pack")?;

        // Read the header to check the object count.
        let mut header = [0u8; 12];
        let n = file.read(&mut header).context("reading pack header")?;
        if mizzle_proto::receive::pack_object_count(&header[..n]).unwrap_or(0) == 0 {
            return Ok(None);
        }
        file.seek(io::SeekFrom::Start(0))
            .context("seeking staged pack to start")?;

        let local = repo.repo.to_thread_local();
        let pack_dir = local.path().join("objects").join("pack");
        std::fs::create_dir_all(&pack_dir)?;

        let temp_dir = tempfile::Builder::new()
            .prefix("mizzle_")
            .tempdir()
            .context("creating temp dir for pack")?;

        let mut progress = gix_features::progress::Discard;
        let interrupt = AtomicBool::new(false);

        gix_pack::Bundle::write_to_directory(
            &mut io::BufReader::new(file),
            Some(temp_dir.path()),
            &mut progress,
            &interrupt,
            None::<gix::objs::find::Never>,
            Default::default(),
        )
        .context("indexing received pack")?;

        // Locate the .pack and .idx files written into the temp dir.
        let mut pack_src = None;
        let mut idx_src = None;
        for entry in std::fs::read_dir(temp_dir.path()).context("reading temp dir")? {
            let path = entry?.path();
            match path.extension().and_then(|e| e.to_str()) {
                Some("pack") => pack_src = Some(path),
                Some("idx") => idx_src = Some(path),
                _ => {}
            }
        }
        let pack_src = pack_src.context("no .pack file written")?;
        let idx_src = idx_src.context("no .idx file written")?;

        let pack_dst = pack_dir.join(pack_src.file_name().unwrap());
        let idx_dst = pack_dir.join(idx_src.file_name().unwrap());
        move_file(&pack_src, &pack_dst).context("moving pack file")?;
        move_file(&idx_src, &idx_dst).context("moving index file")?;

        Ok(Some(FsWrittenPack {
            pack: pack_dst,
            index: idx_dst,
        }))
    }

    fn inspect_ingested(&self, pack: &FsWrittenPack) -> Result<PackMetadata> {
        crate::inspect::inspect_pack(&pack.pack)
    }

    fn rollback_ingest(&self, pack: FsWrittenPack) {
        let _ = std::fs::remove_file(&pack.index);
        let _ = std::fs::remove_file(&pack.pack);
    }

    fn reachable_excluding(
        &self,
        repo: &FsGitoxideRepo,
        from: &[ObjectId],
        excluding: &[ObjectId],
        cap: usize,
    ) -> std::result::Result<Vec<ObjectId>, ReachableError> {
        let local = repo.repo.to_thread_local();
        let odb = local.objects.into_inner();

        let walk = gix_traverse::commit::topo::Builder::new(odb)
            .sorting(gix_traverse::commit::topo::Sorting::TopoOrder)
            .with_tips(from.iter().copied())
            .with_ends(excluding.iter().copied())
            .build()
            .map_err(|e| ReachableError::Other(anyhow::anyhow!("topo walk init: {e}")))?;

        let mut out = Vec::new();
        for r in walk {
            let info = r.map_err(|e| ReachableError::Other(anyhow::anyhow!("topo walk: {e}")))?;
            if out.len() >= cap {
                return Err(ReachableError::CapExceeded { limit: cap });
            }
            out.push(info.id);
        }
        Ok(out)
    }

    fn tree_diff(
        &self,
        repo: &FsGitoxideRepo,
        parent_tree: Option<ObjectId>,
        child_tree: ObjectId,
    ) -> Result<RefDiff> {
        use gix_diff::tree::{
            recorder::{Change as RecChange, Location},
            Recorder,
        };
        use gix_object::FindExt;

        let store = repo.repo.objects.to_handle();

        // Empty tree object id (well-known) for the "no parent" case.
        let empty_tree = ObjectId::empty_tree(gix_hash::Kind::Sha1);
        let lhs = parent_tree.unwrap_or(empty_tree);
        let rhs = child_tree;

        let mut buf_l = Vec::new();
        let mut buf_r = Vec::new();
        let lhs_iter = store
            .find_tree_iter(&lhs, &mut buf_l)
            .with_context(|| format!("reading parent tree {lhs}"))?;
        let rhs_iter = store
            .find_tree_iter(&rhs, &mut buf_r)
            .with_context(|| format!("reading child tree {rhs}"))?;

        let mut state = gix_diff::tree::State::default();
        let mut recorder = Recorder::default().track_location(Some(Location::Path));

        gix_diff::tree(lhs_iter, rhs_iter, &mut state, &store, &mut recorder)
            .context("running tree diff")?;

        let mut entries = Vec::with_capacity(recorder.records.len());
        for rec in recorder.records {
            entries.push(match rec {
                RecChange::Addition {
                    entry_mode,
                    oid,
                    path,
                    ..
                } => RefDiffEntry {
                    path,
                    change: RefDiffChange::Added,
                    mode: u32::from(entry_mode.value()),
                    oid,
                },
                RecChange::Deletion {
                    entry_mode,
                    oid,
                    path,
                    ..
                } => RefDiffEntry {
                    path,
                    change: RefDiffChange::Removed,
                    mode: u32::from(entry_mode.value()),
                    oid,
                },
                RecChange::Modification {
                    entry_mode,
                    oid,
                    path,
                    ..
                } => RefDiffEntry {
                    path,
                    change: RefDiffChange::Modified,
                    mode: u32::from(entry_mode.value()),
                    oid,
                },
            });
        }

        Ok(RefDiff { entries })
    }

    fn read_commit_info(&self, repo: &FsGitoxideRepo, oid: ObjectId) -> Result<CommitInfo> {
        use gix_object::Find;
        let mut buf: Vec<u8> = Vec::new();
        let store = repo.repo.objects.to_handle();
        let object = store
            .try_find(&oid, &mut buf)
            .map_err(|e| anyhow::anyhow!("looking up commit {oid}: {e}"))?
            .with_context(|| format!("commit {oid} not in object store"))?;
        if object.kind != gix_object::Kind::Commit {
            anyhow::bail!("object {oid} is not a commit");
        }
        crate::inspect::parse_commit_info(object.data, oid)
    }

    fn read_blob(&self, repo: &FsGitoxideRepo, oid: ObjectId, cap: u64) -> Result<Option<Vec<u8>>> {
        use gix_object::Find;
        let mut buf: Vec<u8> = Vec::new();
        let store = repo.repo.objects.to_handle();
        let object = match store.try_find(&oid, &mut buf) {
            Ok(Some(o)) => o,
            Ok(None) => return Ok(None),
            Err(e) => return Err(anyhow::anyhow!("looking up blob {oid}: {e}")),
        };
        if object.kind != gix_object::Kind::Blob {
            return Ok(None);
        }
        if (object.data.len() as u64) > cap {
            return Ok(None);
        }
        Ok(Some(object.data.to_vec()))
    }

    fn read_object_raw(
        &self,
        repo: &FsGitoxideRepo,
        oid: ObjectId,
        cap: u64,
    ) -> Result<Option<Vec<u8>>> {
        use gix_object::Find;
        let mut buf: Vec<u8> = Vec::new();
        let store = repo.repo.objects.to_handle();
        let object = match store.try_find(&oid, &mut buf) {
            Ok(Some(o)) => o,
            Ok(None) => return Ok(None),
            Err(e) => return Err(anyhow::anyhow!("looking up object {oid}: {e}")),
        };
        if (object.data.len() as u64) > cap {
            return Ok(None);
        }
        Ok(Some(object.data.to_vec()))
    }
}

// ---------------------------------------------------------------------------
// Pack reuse (whole-pack bypass)
// ---------------------------------------------------------------------------

/// Wrap an existing on-disk `.pack` file as a [`PackOutput`].  The pack is
/// already in wire format (header + entries + trailing pack-hash), so the
/// client can consume it identically to a freshly-generated pack.
#[tracing::instrument(skip_all, fields(pack = %pack_path.display()))]
fn ship_pack_as_is(pack_path: &Path) -> Result<PackOutput> {
    let file = std::fs::File::open(pack_path)
        .with_context(|| format!("opening reusable pack {}", pack_path.display()))?;
    Ok(PackOutput {
        reader: Box::new(io::BufReader::new(file)),
        shallow: Vec::new(),
        progress: None,
    })
}

// ---------------------------------------------------------------------------
// Streaming pack generation
// ---------------------------------------------------------------------------

/// Reader that pulls pack chunks from a background thread via a bounded channel.
///
/// When the channel closes (sender dropped), the reader joins the background
/// thread to propagate any errors that occurred during pack generation.
struct PackReader {
    rx: mpsc::Receiver<Vec<u8>>,
    current: io::Cursor<Vec<u8>>,
    thread: Option<std::thread::JoinHandle<Result<()>>>,
}

impl io::Read for PackReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = self.current.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            // Current chunk exhausted — pull the next one.
            match self.rx.recv() {
                Ok(chunk) => {
                    self.current = io::Cursor::new(chunk);
                }
                Err(_) => {
                    // Channel closed — join the thread to check for errors.
                    if let Some(handle) = self.thread.take() {
                        match handle.join() {
                            Ok(Ok(())) => return Ok(0),
                            Ok(Err(e)) => {
                                return Err(io::Error::new(io::ErrorKind::Other, e));
                            }
                            Err(_) => {
                                return Err(io::Error::new(
                                    io::ErrorKind::Other,
                                    "pack generation thread panicked",
                                ));
                            }
                        }
                    }
                    return Ok(0);
                }
            }
        }
    }
}

/// Run the gitoxide pack pipeline, sending chunks through `tx`.
///
/// Returns `Ok(())` if the pipeline completes or the receiver is dropped
/// (caller stopped reading early).
fn stream_pack_to_channel(
    mut handle: gix::OdbHandle,
    objects: Vec<ObjectId>,
    thin_pack: bool,
    tx: mpsc::SyncSender<Vec<u8>>,
    progress_tx: mpsc::Sender<String>,
) -> Result<()> {
    handle.prevent_pack_unload();
    handle.ignore_replacements = true;

    let should_interrupt = AtomicBool::new(false);
    let counting = ChannelProgress::new(progress_tx.clone(), "Enumerating objects");

    let (counts, _) = gix_pack::data::output::count::objects(
        handle.clone().into_inner(),
        Box::new(objects.into_iter().map(Ok)),
        &counting,
        &should_interrupt,
        gix_pack::data::output::count::objects::Options {
            thread_limit: None,
            chunk_size: 16,
            input_object_expansion: gix_pack::data::output::count::objects::ObjectExpansion::AsIs,
        },
    )?;
    counting.send_done();
    let counts: Vec<_> = counts.into_iter().collect();
    let num_objects = counts.len();

    let mut in_order_entries = InOrderIter::from(gix_pack::data::output::entry::iter_from_counts(
        counts,
        handle.into_inner(),
        Box::new(ChannelProgress::new(progress_tx, "Compressing objects")),
        gix_pack::data::output::entry::iter_from_counts::Options {
            thread_limit: None,
            mode: gix_pack::data::output::entry::iter_from_counts::Mode::PackCopyAndBaseObjects,
            allow_thin_pack: thin_pack,
            chunk_size: 16,
            version: Default::default(),
        },
    ));

    let buf = ChunkBuffer::new();
    let mut pack_iter = gix_pack::data::output::bytes::FromEntriesIter::new(
        in_order_entries.by_ref(),
        &buf,
        num_objects as u32,
        Default::default(),
        gix_hash::Kind::default(),
    );

    for chunk_result in &mut pack_iter {
        chunk_result?;
        let chunk = buf.drain();
        if !chunk.is_empty() {
            // If the receiver is dropped (caller stopped reading), stop.
            if tx.send(chunk).is_err() {
                return Ok(());
            }
        }
    }

    Ok(())
}

/// A write target that accumulates bytes between pack iterator steps.
struct ChunkBuffer {
    data: std::sync::Mutex<Vec<u8>>,
}

impl ChunkBuffer {
    fn new() -> Self {
        Self {
            data: std::sync::Mutex::new(Vec::new()),
        }
    }
    fn drain(&self) -> Vec<u8> {
        std::mem::take(&mut *self.data.lock().unwrap())
    }
}

impl io::Write for &ChunkBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.data.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Move a file, falling back to copy+delete for cross-filesystem moves.
fn move_file(src: &Path, dst: &Path) -> Result<()> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dst).context("copying file cross-filesystem")?;
    std::fs::remove_file(src).context("removing source after copy")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Progress reporting via channel
// ---------------------------------------------------------------------------

/// Progress implementation that sends formatted status lines through a channel.
///
/// Used to produce git-compatible sideband progress output during pack
/// generation (e.g. "Counting objects: 42\r", "Compressing objects: 100% (3/3), done.\n").
struct ChannelProgress {
    tx: Mutex<mpsc::Sender<String>>,
    name: Mutex<String>,
    step: Arc<AtomicUsize>,
    max: Mutex<Option<usize>>,
    last_sent_ms: AtomicU64,
}

impl ChannelProgress {
    fn new(tx: mpsc::Sender<String>, name: &str) -> Self {
        Self {
            tx: Mutex::new(tx),
            name: Mutex::new(name.to_string()),
            step: Arc::new(AtomicUsize::new(0)),
            max: Mutex::new(None),
            last_sent_ms: AtomicU64::new(0),
        }
    }

    fn maybe_send(&self) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let prev = self.last_sent_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) < 100 {
            return; // throttle to ~10 updates/sec
        }
        self.last_sent_ms.store(now_ms, Ordering::Relaxed);

        let name = self.name.lock().unwrap().clone();
        let step = self.step.load(Ordering::Relaxed);
        let max = *self.max.lock().unwrap();

        let msg = match max {
            Some(max) if max > 0 => {
                let pct = (step as f64 / max as f64 * 100.0) as u32;
                format!("{name}: {pct}% ({step}/{max})\r")
            }
            _ => format!("{name}: {step}\r"),
        };

        let _ = self.tx.lock().unwrap().send(msg);
    }

    fn send_done(&self) {
        let name = self.name.lock().unwrap().clone();
        let step = self.step.load(Ordering::Relaxed);
        let max = *self.max.lock().unwrap();

        let msg = match max {
            Some(max) if max > 0 => {
                format!("{name}: 100% ({max}/{max}), done.\n")
            }
            _ => format!("{name}: {step}, done.\n"),
        };

        let _ = self.tx.lock().unwrap().send(msg);
    }
}

impl gix_features::progress::prodash::Count for ChannelProgress {
    fn set(&self, step: usize) {
        self.step.store(step, Ordering::Relaxed);
        self.maybe_send();
    }

    fn step(&self) -> usize {
        self.step.load(Ordering::Relaxed)
    }

    fn inc_by(&self, step: usize) {
        self.step.fetch_add(step, Ordering::Relaxed);
        self.maybe_send();
    }

    fn counter(&self) -> gix_features::progress::StepShared {
        self.step.clone()
    }
}

impl gix_features::progress::prodash::Progress for ChannelProgress {
    fn init(&mut self, max: Option<usize>, _unit: Option<gix_features::progress::Unit>) {
        *self.max.lock().unwrap() = max;
        self.step.store(0, Ordering::Relaxed);
    }

    fn set_name(&mut self, name: String) {
        *self.name.lock().unwrap() = name;
    }

    fn name(&self) -> Option<String> {
        Some(self.name.lock().unwrap().clone())
    }

    fn id(&self) -> gix_features::progress::Id {
        gix_features::progress::UNKNOWN
    }

    fn message(&self, _level: gix_features::progress::MessageLevel, message: String) {
        let _ = self.tx.lock().unwrap().send(format!("{message}\n"));
    }

    fn show_throughput(&self, _start: std::time::Instant) {
        self.send_done();
    }

    fn show_throughput_with(
        &self,
        _start: std::time::Instant,
        _step: usize,
        _unit: gix_features::progress::Unit,
        _level: gix_features::progress::MessageLevel,
    ) {
        self.send_done();
    }
}

impl gix_features::progress::NestedProgress for ChannelProgress {
    type SubProgress = Self;

    fn add_child(&mut self, name: impl Into<String>) -> Self::SubProgress {
        let tx = self.tx.lock().unwrap().clone();
        ChannelProgress::new(tx, &name.into())
    }

    fn add_child_with_id(
        &mut self,
        name: impl Into<String>,
        _id: gix_features::progress::Id,
    ) -> Self::SubProgress {
        self.add_child(name)
    }
}

/// Try to compute `build_have_set` via a pack reachability bitmap.  Returns
/// `None` if no pack in the repo has a usable `.bitmap` / `.rev` sidecar,
/// or if any have OID is not a bitmap entry — the caller then falls back
/// to the walker.
#[tracing::instrument(skip_all, fields(have = have.len()))]
fn try_bitmap_have_set(
    repo: &FsGitoxideRepo,
    have: &[ObjectId],
) -> Option<std::collections::HashSet<ObjectId>> {
    if have.is_empty() {
        return Some(std::collections::HashSet::new());
    }

    let pack_dir = repo.repo.objects_dir().join("pack");
    let entries = std::fs::read_dir(&pack_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("idx") {
            continue;
        }
        if let Some(set) = try_single_pack_bitmap(&path, have) {
            tracing::debug!(have_set_len = set.len(), "have-set built (bitmap)");
            return Some(set);
        }
    }
    None
}

fn try_single_pack_bitmap(
    idx_path: &Path,
    have: &[ObjectId],
) -> Option<std::collections::HashSet<ObjectId>> {
    let pack_idx = gix_pack::index::File::at(idx_path, gix_hash::Kind::Sha1).ok()?;
    let obj_count = pack_idx.num_objects();
    let mut bitmap = crate::bitmap::PackBitmap::load(idx_path, obj_count).ok()??;
    bitmap.build_oid_index(|pos| pack_idx.oid_at_index(pos).try_into().ok());
    bitmap.have_reachable(have)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// Create a bare repo with known commits and refs for testing.
    ///
    /// Layout:
    ///   HEAD -> refs/heads/main (symbolic)
    ///   refs/heads/main  — 2 commits (Initial, Add hello.txt)
    ///   refs/heads/dev   — 3 commits (above + Dev commit)
    ///   refs/tags/v1.0.0 — lightweight tag on main tip
    fn test_bare_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("test.git");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();

        let git = |cwd: &Path, args: &[&str]| {
            let out = Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "t@t.com")
                .env("GIT_AUTHOR_DATE", "1700000000 +0000")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "t@t.com")
                .env("GIT_COMMITTER_DATE", "1700000000 +0000")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        git(&work, &["init", "-b", "main"]);
        git(&work, &["config", "user.email", "t@t.com"]);
        git(&work, &["config", "user.name", "T"]);
        git(&work, &["config", "commit.gpgsign", "false"]);
        std::fs::write(work.join("README.md"), "# Demo\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "Initial"]);
        std::fs::write(work.join("hello.txt"), "hello\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "Add hello.txt"]);

        git(&work, &["checkout", "-b", "dev"]);
        std::fs::write(work.join("dev.txt"), "dev\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "Dev commit"]);
        git(&work, &["checkout", "main"]);
        git(&work, &["tag", "v1.0.0"]);

        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--bare"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&work, &["push", "--mirror", "origin"]);
        git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        (dir, bare)
    }

    #[test]
    fn test_init_repo_creates_bare_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("test.git");
        assert!(!repo_path.exists());
        FsGitoxide.init_repo(&repo_path).unwrap();
        assert!(repo_path.exists());
        let repo = gix::open(&repo_path).unwrap();
        let refs: Vec<_> = repo.references().unwrap().all().unwrap().collect();
        assert!(refs.is_empty(), "freshly init'd repo should have no refs");
        // Calling again is a no-op (already exists)
        FsGitoxide.init_repo(&repo_path).unwrap();
    }

    #[test]
    fn list_refs_returns_head_and_branches() {
        let (_dir, bare) = test_bare_repo();
        let repo = FsGitoxide.open(&bare).unwrap();
        let snap = FsGitoxide.list_refs(&repo).unwrap();

        // HEAD should be present and point at main
        let head = snap.head.as_ref().expect("HEAD should exist");
        assert_eq!(
            head.symref_target.as_deref(),
            Some("refs/heads/main"),
            "HEAD should be a symref to main"
        );

        let ref_names: Vec<&str> = snap.refs.iter().map(|r| r.name.as_str()).collect();
        assert!(ref_names.contains(&"refs/heads/main"), "missing main");
        assert!(ref_names.contains(&"refs/heads/dev"), "missing dev");
        assert!(ref_names.contains(&"refs/tags/v1.0.0"), "missing tag");

        // HEAD oid should match refs/heads/main oid
        let main_ref = snap
            .refs
            .iter()
            .find(|r| r.name == "refs/heads/main")
            .unwrap();
        assert_eq!(head.oid, main_ref.oid, "HEAD oid should match main");

        // Lightweight tag should not have a peeled oid (same as tag target)
        let tag = snap
            .refs
            .iter()
            .find(|r| r.name == "refs/tags/v1.0.0")
            .unwrap();
        assert!(
            tag.peeled.is_none(),
            "lightweight tag should not have peeled oid"
        );
    }

    #[test]
    fn refs_snapshot_as_upload_pack_v1() {
        let (_dir, bare) = test_bare_repo();
        let repo = FsGitoxide.open(&bare).unwrap();
        let snap = FsGitoxide.list_refs(&repo).unwrap();
        let v1 = snap.as_upload_pack_v1();

        assert!(!v1.is_empty());
        // HEAD should be first
        assert_eq!(v1[0].1, "HEAD");
        // All other entries should start with refs/
        for (_, name) in &v1[1..] {
            assert!(
                name.starts_with("refs/"),
                "expected refs/ prefix, got {name}"
            );
        }
    }

    #[test]
    fn refs_snapshot_as_receive_pack() {
        let (_dir, bare) = test_bare_repo();
        let repo = FsGitoxide.open(&bare).unwrap();
        let snap = FsGitoxide.list_refs(&repo).unwrap();
        let rp = snap.as_receive_pack();

        // Should not contain HEAD
        for (_, name) in &rp {
            assert_ne!(name, "HEAD", "receive-pack should not include HEAD");
            assert!(name.starts_with("refs/"));
        }
    }

    #[test]
    fn resolve_ref_existing_and_nonexistent() {
        let (_dir, bare) = test_bare_repo();
        let repo = FsGitoxide.open(&bare).unwrap();

        let main_oid = FsGitoxide.resolve_ref(&repo, "refs/heads/main").unwrap();
        assert!(main_oid.is_some(), "main should resolve");

        let dev_oid = FsGitoxide.resolve_ref(&repo, "refs/heads/dev").unwrap();
        assert!(dev_oid.is_some(), "dev should resolve");
        assert_ne!(main_oid, dev_oid, "main and dev should differ");

        let none = FsGitoxide
            .resolve_ref(&repo, "refs/heads/nonexistent")
            .unwrap();
        assert!(none.is_none(), "nonexistent ref should return None");
    }

    #[test]
    fn has_object_and_has_objects() {
        let (_dir, bare) = test_bare_repo();
        let repo = FsGitoxide.open(&bare).unwrap();
        let main_oid = FsGitoxide
            .resolve_ref(&repo, "refs/heads/main")
            .unwrap()
            .unwrap();

        assert!(FsGitoxide.has_object(&repo, &main_oid).unwrap());

        let fake_oid = ObjectId::from_hex(b"0000000000000000000000000000000000000001").unwrap();
        assert!(!FsGitoxide.has_object(&repo, &fake_oid).unwrap());

        let results = FsGitoxide
            .has_objects(&repo, &[main_oid, fake_oid])
            .unwrap();
        assert_eq!(results, vec![true, false]);
    }

    #[test]
    fn update_refs_creates_new_ref() {
        let (_dir, bare) = test_bare_repo();
        let repo = FsGitoxide.open(&bare).unwrap();
        let main_oid = FsGitoxide
            .resolve_ref(&repo, "refs/heads/main")
            .unwrap()
            .unwrap();

        FsGitoxide
            .update_refs(
                &repo,
                &[RefUpdate {
                    old_oid: ObjectId::null(gix_hash::Kind::Sha1),
                    new_oid: main_oid,
                    refname: "refs/heads/new-branch".to_string(),
                }],
            )
            .unwrap();

        let resolved = FsGitoxide
            .resolve_ref(&repo, "refs/heads/new-branch")
            .unwrap();
        assert_eq!(resolved, Some(main_oid));
    }

    #[test]
    fn compute_push_kind_create() {
        let (_dir, bare) = test_bare_repo();
        let repo = FsGitoxide.open(&bare).unwrap();
        let main_oid = FsGitoxide
            .resolve_ref(&repo, "refs/heads/main")
            .unwrap()
            .unwrap();

        let kind = FsGitoxide.compute_push_kind(
            &repo,
            &RefUpdate {
                old_oid: ObjectId::null(gix_hash::Kind::Sha1),
                new_oid: main_oid,
                refname: "refs/heads/new".to_string(),
            },
        );
        assert_eq!(kind, PushKind::Create);
    }

    #[test]
    fn compute_push_kind_delete() {
        let (_dir, bare) = test_bare_repo();
        let repo = FsGitoxide.open(&bare).unwrap();
        let main_oid = FsGitoxide
            .resolve_ref(&repo, "refs/heads/main")
            .unwrap()
            .unwrap();

        let kind = FsGitoxide.compute_push_kind(
            &repo,
            &RefUpdate {
                old_oid: main_oid,
                new_oid: ObjectId::null(gix_hash::Kind::Sha1),
                refname: "refs/heads/main".to_string(),
            },
        );
        assert_eq!(kind, PushKind::Delete);
    }

    #[test]
    fn compute_push_kind_fast_forward() {
        let (_dir, bare) = test_bare_repo();
        let repo = FsGitoxide.open(&bare).unwrap();
        let main_oid = FsGitoxide
            .resolve_ref(&repo, "refs/heads/main")
            .unwrap()
            .unwrap();
        let dev_oid = FsGitoxide
            .resolve_ref(&repo, "refs/heads/dev")
            .unwrap()
            .unwrap();

        // dev is ahead of main (main is ancestor of dev), so main->dev is a fast-forward
        let kind = FsGitoxide.compute_push_kind(
            &repo,
            &RefUpdate {
                old_oid: main_oid,
                new_oid: dev_oid,
                refname: "refs/heads/main".to_string(),
            },
        );
        assert_eq!(kind, PushKind::FastForward);
    }

    #[test]
    fn compute_push_kind_force_push() {
        let (_dir, bare) = test_bare_repo();
        let repo = FsGitoxide.open(&bare).unwrap();
        let main_oid = FsGitoxide
            .resolve_ref(&repo, "refs/heads/main")
            .unwrap()
            .unwrap();
        let dev_oid = FsGitoxide
            .resolve_ref(&repo, "refs/heads/dev")
            .unwrap()
            .unwrap();

        // dev->main is not a fast-forward (main is not ahead of dev)
        let kind = FsGitoxide.compute_push_kind(
            &repo,
            &RefUpdate {
                old_oid: dev_oid,
                new_oid: main_oid,
                refname: "refs/heads/main".to_string(),
            },
        );
        assert_eq!(kind, PushKind::ForcePush);
    }

    #[test]
    fn build_pack_returns_valid_pack_data() {
        let (_dir, bare) = test_bare_repo();
        let repo = FsGitoxide.open(&bare).unwrap();
        let main_oid = FsGitoxide
            .resolve_ref(&repo, "refs/heads/main")
            .unwrap()
            .unwrap();

        let mut output = FsGitoxide
            .build_pack(
                &repo,
                &[main_oid],
                &[],
                &PackOptions {
                    deepen: None,
                    filter: None,
                    thin_pack: false,
                },
            )
            .unwrap();

        // Read the full pack
        let mut data = Vec::new();
        io::Read::read_to_end(&mut output.reader, &mut data).unwrap();

        // Pack data should start with "PACK"
        assert!(data.len() >= 12, "pack too short: {} bytes", data.len());
        assert_eq!(&data[0..4], b"PACK", "pack should start with PACK magic");
    }

    #[test]
    fn build_pack_with_have_produces_smaller_pack() {
        let (_dir, bare) = test_bare_repo();
        let repo = FsGitoxide.open(&bare).unwrap();
        let main_oid = FsGitoxide
            .resolve_ref(&repo, "refs/heads/main")
            .unwrap()
            .unwrap();
        let dev_oid = FsGitoxide
            .resolve_ref(&repo, "refs/heads/dev")
            .unwrap()
            .unwrap();

        // Full pack: want dev, have nothing
        let mut full = FsGitoxide
            .build_pack(
                &repo,
                &[dev_oid],
                &[],
                &PackOptions {
                    deepen: None,
                    filter: None,
                    thin_pack: false,
                },
            )
            .unwrap();
        let mut full_data = Vec::new();
        io::Read::read_to_end(&mut full.reader, &mut full_data).unwrap();

        // Incremental pack: want dev, have main
        let mut incr = FsGitoxide
            .build_pack(
                &repo,
                &[dev_oid],
                &[main_oid],
                &PackOptions {
                    deepen: None,
                    filter: None,
                    thin_pack: false,
                },
            )
            .unwrap();
        let mut incr_data = Vec::new();
        io::Read::read_to_end(&mut incr.reader, &mut incr_data).unwrap();

        assert!(
            incr_data.len() < full_data.len(),
            "incremental pack ({} bytes) should be smaller than full pack ({} bytes)",
            incr_data.len(),
            full_data.len()
        );
    }

    /// On a `git repack -adb`'d bare repo, a clone-shaped `build_pack` must
    /// take the reuse fast path and stream the on-disk pack verbatim — the
    /// bytes returned must match the file in `objects/pack/`.
    #[test]
    fn build_pack_ships_existing_pack_as_is() {
        let (_dir, bare) = test_bare_repo();
        // Run `git repack -adb` on the bare repo so it has exactly one pack
        // with `.bitmap` + `.rev` sidecars.
        let out = Command::new("git")
            .current_dir(&bare)
            .args(["repack", "-adb"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git repack -adb failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let repo = FsGitoxide.open(&bare).unwrap();
        let main_oid = FsGitoxide
            .resolve_ref(&repo, "refs/heads/main")
            .unwrap()
            .unwrap();
        let dev_oid = FsGitoxide
            .resolve_ref(&repo, "refs/heads/dev")
            .unwrap()
            .unwrap();
        let tag_oid = FsGitoxide
            .resolve_ref(&repo, "refs/tags/v1.0.0")
            .unwrap()
            .unwrap();

        // Find the on-disk pack file.
        let pack_dir = bare.join("objects").join("pack");
        let on_disk_pack = std::fs::read_dir(&pack_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.path().extension().and_then(|s| s.to_str()) == Some("pack"))
            .expect("repacked bare repo should have one .pack")
            .path();
        let on_disk_bytes = std::fs::read(&on_disk_pack).unwrap();

        // Want every tip — closure equals the entire repo, which equals the
        // pack contents.
        let mut output = FsGitoxide
            .build_pack(
                &repo,
                &[main_oid, dev_oid, tag_oid],
                &[],
                &PackOptions {
                    deepen: None,
                    filter: None,
                    thin_pack: false,
                },
            )
            .unwrap();
        let mut returned = Vec::new();
        io::Read::read_to_end(&mut output.reader, &mut returned).unwrap();

        assert_eq!(
            returned, on_disk_bytes,
            "reuse path must stream the pack file verbatim"
        );
        assert!(
            output.progress.is_none(),
            "reuse path emits no per-object progress"
        );
        assert!(output.shallow.is_empty());
    }

    /// Filter / depth / thin-pack must disable the reuse path: those
    /// transformations require rewriting pack contents.
    #[test]
    fn build_pack_with_filter_does_not_reuse() {
        use mizzle_proto::pack::Filter;

        let (_dir, bare) = test_bare_repo();
        let _ = Command::new("git")
            .current_dir(&bare)
            .args(["repack", "-adb"])
            .stdin(Stdio::null())
            .output()
            .unwrap();

        let repo = FsGitoxide.open(&bare).unwrap();
        let main_oid = FsGitoxide
            .resolve_ref(&repo, "refs/heads/main")
            .unwrap()
            .unwrap();

        // blob:none should produce a smaller pack than the on-disk pack —
        // proves the reuse path bailed out.
        let pack_dir = bare.join("objects").join("pack");
        let on_disk_size = std::fs::read_dir(&pack_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.path().extension().and_then(|s| s.to_str()) == Some("pack"))
            .map(|e| e.metadata().unwrap().len())
            .unwrap();

        let mut output = FsGitoxide
            .build_pack(
                &repo,
                &[main_oid],
                &[],
                &PackOptions {
                    deepen: None,
                    filter: Some(Filter::BlobNone),
                    thin_pack: false,
                },
            )
            .unwrap();
        let mut data = Vec::new();
        io::Read::read_to_end(&mut output.reader, &mut data).unwrap();

        assert!(
            (data.len() as u64) < on_disk_size,
            "filtered pack ({} bytes) should be smaller than on-disk pack ({} bytes)",
            data.len(),
            on_disk_size
        );
    }

    #[test]
    fn ingest_pack_and_rollback() {
        let (_dir, bare) = test_bare_repo();
        let repo = FsGitoxide.open(&bare).unwrap();
        let main_oid = FsGitoxide
            .resolve_ref(&repo, "refs/heads/main")
            .unwrap()
            .unwrap();

        // Build a pack from the existing repo
        let mut output = FsGitoxide
            .build_pack(
                &repo,
                &[main_oid],
                &[],
                &PackOptions {
                    deepen: None,
                    filter: None,
                    thin_pack: false,
                },
            )
            .unwrap();
        let mut pack_data = Vec::new();
        io::Read::read_to_end(&mut output.reader, &mut pack_data).unwrap();

        // Create a fresh bare repo to ingest into
        let target_dir = tempfile::tempdir().unwrap();
        let target = target_dir.path().join("target.git");
        FsGitoxide.init_repo(&target).unwrap();
        let target_repo = FsGitoxide.open(&target).unwrap();

        // Stage the pack to a temp file
        let staged = target_dir.path().join("staged.pack");
        std::fs::write(&staged, &pack_data).unwrap();

        let written = FsGitoxide.ingest_pack(&target_repo, &staged).unwrap();
        assert!(written.is_some(), "non-empty pack should return Some");
        let written = written.unwrap();

        // The pack and index files should exist
        assert!(written.pack.exists(), "pack file should exist");
        assert!(written.index.exists(), "index file should exist");

        // The objects should now be accessible
        assert!(FsGitoxide.has_object(&target_repo, &main_oid).unwrap());

        // Rollback should remove the files
        FsGitoxide.rollback_ingest(written);
        // After rollback we can't guarantee object lookup fails (gitoxide may
        // cache), but the files themselves should be gone.
    }

    #[test]
    fn ingest_empty_pack_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("test.git");
        FsGitoxide.init_repo(&bare).unwrap();
        let repo = FsGitoxide.open(&bare).unwrap();

        // Create a pack header with 0 objects.  We only need the first 12 bytes
        // to be valid for the object-count check — ingest_pack returns None
        // before attempting to index.
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes()); // version
        pack.extend_from_slice(&0u32.to_be_bytes()); // 0 objects

        let staged = dir.path().join("empty.pack");
        std::fs::write(&staged, &pack).unwrap();

        let result = FsGitoxide.ingest_pack(&repo, &staged).unwrap();
        assert!(result.is_none(), "empty pack should return None");
    }
}
