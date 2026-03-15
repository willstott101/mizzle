mod common;

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use tempfile::tempdir;

use common::{axum_server, test_with_servers, Config};

/// A TCP proxy that sits between the git client and the mizzle server, recording
/// all server→client bytes for every connection.  Run git commands through
/// `proxy.port`; afterwards call `has_thin_pack_in_response` to inspect the
/// captured traffic.
struct SniffingProxy {
    /// Point the git client at this port.
    pub port: u16,
    responses: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl SniffingProxy {
    fn new(upstream_port: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let responses: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let responses_clone = Arc::clone(&responses);

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(client) = stream else { break };
                let responses = Arc::clone(&responses_clone);
                thread::spawn(move || {
                    if let Ok(bytes) = proxy_connection(client, upstream_port) {
                        responses.lock().unwrap().push(bytes);
                    }
                });
            }
        });

        SniffingProxy { port, responses }
    }

    /// Returns true if any captured response contains an OBJ_REF_DELTA (type-7)
    /// pack entry.
    fn has_thin_pack_in_response(&self) -> anyhow::Result<bool> {
        // Wait for any in-flight proxy threads to finish recording.
        thread::sleep(std::time::Duration::from_millis(100));

        let responses = self.responses.lock().unwrap();
        for bytes in responses.iter() {
            if let Ok(pack) = extract_pack_from_response(bytes) {
                if is_thin_pack(&pack)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

/// Proxy one TCP connection: forward client↔server, capturing server→client bytes.
fn proxy_connection(client: TcpStream, upstream_port: u16) -> anyhow::Result<Vec<u8>> {
    let server = TcpStream::connect(format!("127.0.0.1:{upstream_port}"))?;
    let mut client_r = client.try_clone()?;
    let mut client_w = client;
    let mut server_r = server.try_clone()?;
    let mut server_w = server;

    // Forward client → server.  When the client closes its write side (git is
    // done sending), half-close the proxy→server direction so the server sees
    // EOF and can close its side promptly, unblocking our capture loop below.
    let fwd = thread::spawn(move || {
        let _ = io::copy(&mut client_r, &mut server_w);
        let _ = server_w.shutdown(std::net::Shutdown::Write);
    });

    // Forward server → client, capturing everything sent.
    let mut captured = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        match server_r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                captured.extend_from_slice(&buf[..n]);
                client_w.write_all(&buf[..n])?;
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => break,
            Err(e) => return Err(e.into()),
        }
    }
    let _ = fwd.join();
    Ok(captured)
}

/// Parse raw server→client TCP bytes and extract sideband channel-1 pack bytes
/// from a git-upload-pack response.  A single captured TCP stream may contain
/// multiple HTTP responses (HTTP/1.1 keep-alive), so this iterates over all of
/// them and returns the first one that contains packfile data.
fn extract_pack_from_response(raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut search_from = 0;

    while search_from < raw.len() {
        // Find the end of the next HTTP response headers.
        let Some(rel) = raw[search_from..].windows(4).position(|w| w == b"\r\n\r\n") else {
            break;
        };
        let header_end = search_from + rel + 4;

        let headers = std::str::from_utf8(&raw[search_from..header_end]).unwrap_or("");
        let is_chunked = headers
            .to_lowercase()
            .contains("transfer-encoding: chunked");

        // Dechunk the body, tracking how many raw bytes were consumed so we can
        // find the start of the next HTTP response on the same connection.
        let (body, body_raw_len) = dechunk_body(&raw[header_end..], is_chunked)?;

        if let Ok(pack) = pack_from_pkt_lines(&body) {
            return Ok(pack);
        }

        search_from = header_end + body_raw_len;
    }

    anyhow::bail!("no pack data found in any HTTP response")
}

/// Dechunk an HTTP response body starting at `raw`.  Returns the decoded body
/// bytes and the number of raw bytes consumed (so the caller can advance past
/// this response to the next one on a keep-alive connection).
fn dechunk_body(raw: &[u8], is_chunked: bool) -> anyhow::Result<(Vec<u8>, usize)> {
    if !is_chunked {
        // Without chunking we can't know where this response ends without a
        // Content-Length header, so conservatively consume everything.
        return Ok((raw.to_vec(), raw.len()));
    }

    let mut reader = BufReader::new(raw);
    let mut body = Vec::new();
    let mut consumed = 0;

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        consumed += n;
        let size_str = line.trim().split(';').next().unwrap_or("0");
        let size = usize::from_str_radix(size_str, 16)?;
        if size == 0 {
            // Consume the trailing \r\n after the terminating zero chunk.
            let mut crlf = [0u8; 2];
            if reader.read_exact(&mut crlf).is_ok() {
                consumed += 2;
            }
            break;
        }
        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk)?;
        body.extend_from_slice(&chunk);
        consumed += size;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
        consumed += 2;
    }

    Ok((body, consumed))
}

/// Scan a dechunked pkt-line body for a "packfile" section and return the
/// concatenated sideband channel-1 bytes.
fn pack_from_pkt_lines(body: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut pos = 0;
    let mut pack_bytes = Vec::new();
    let mut in_packfile = false;

    loop {
        if pos + 4 > body.len() {
            break;
        }
        let len_hex = std::str::from_utf8(&body[pos..pos + 4])?;
        let pkt_len = usize::from_str_radix(len_hex, 16)?;
        if pkt_len == 0 {
            break; // flush
        }
        if pkt_len == 1 {
            pos += 4; // delimiter
            continue;
        }
        if pos + pkt_len > body.len() {
            break;
        }
        let data = &body[pos + 4..pos + pkt_len];
        let data_stripped = data.strip_suffix(b"\n").unwrap_or(data);

        if !in_packfile && data_stripped == b"packfile" {
            in_packfile = true;
        } else if in_packfile && !data.is_empty() && data[0] == 1 {
            pack_bytes.extend_from_slice(&data[1..]);
        }

        pos += pkt_len;
    }

    if pack_bytes.is_empty() {
        anyhow::bail!("no packfile section found");
    }
    Ok(pack_bytes)
}

/// Verifies that a pack is "truly thin":
///   1. It contains at least one OBJ_REF_DELTA (type 7) entry — the delta.
///   2. It contains no full OBJ_BLOB (type 3) entries — the base blob is absent.
///
/// In this test the only differing content between commits is a large blob.  A
/// correct thin pack sends the new blob as a RefDelta against the old blob (which
/// the client already holds) and does NOT include the old blob as a full object.
/// If the base were mistakenly included as a full Blob the pack would not be thin.
fn is_thin_pack(pack: &[u8]) -> anyhow::Result<bool> {
    use gix_pack::data::{entry::Header, input};

    let mut ref_delta_count = 0usize;
    let mut full_blob_count = 0usize;

    let iter = input::BytesToEntriesIter::new_from_header(
        BufReader::new(pack),
        input::Mode::AsIs,
        input::EntryDataMode::Ignore,
        gix_hash::Kind::Sha1,
    )?;
    for entry in iter {
        match entry?.header {
            Header::RefDelta { .. } => ref_delta_count += 1,
            Header::Blob => full_blob_count += 1,
            _ => {}
        }
    }

    Ok(ref_delta_count > 0 && full_blob_count == 0)
}

test_with_servers!(test_fetch, |start_server| {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path().clone(),
    };
    let server = start_server(config);

    let cloned = tempdir()?;

    common::run_git(
        cloned.path(),
        [
            "clone",
            "--branch",
            "main",
            format!("http://localhost:{}/test.git", server.port).as_ref(),
        ],
    )?;

    let clone_dir = cloned.path().join("test");
    let main_before = common::run_git(clone_dir.as_path(), ["rev-parse", "origin/main"])?;

    // Add a new commit to the bare repo so the client can fetch it.
    let server_work = tempdir()?;
    common::run_git(
        server_work.path(),
        ["clone", temprepo.path().to_str().unwrap()],
    )?;
    let server_repo = server_work.path().join("temprepo");
    fs::write(server_repo.join("newfile.txt"), "new content\n")?;
    common::run_git(&server_repo, ["add", "newfile.txt"])?;
    common::run_git(&server_repo, ["commit", "-m", "New commit on server"])?;
    common::run_git(&server_repo, ["push", "origin", "main"])?;
    let new_commit = common::run_git(&server_repo, ["rev-parse", "HEAD"])?;

    // Fetch the new commit from the server (via HTTP)
    common::run_git(clone_dir.as_path(), ["fetch", "origin", "main"])?;

    let main_after = common::run_git(clone_dir.as_path(), ["rev-parse", "origin/main"])?;
    assert_eq!(
        main_after, new_commit,
        "origin/main should be the new commit"
    );
    assert_ne!(main_before, main_after, "origin/main should have advanced");

    server.stop();
    Ok(())
});

// Verifies that the server correctly honours the `thin-pack` fetch capability.
//
// For a thin pack to actually contain RefDelta entries the objects being sent
// must be stored as OfsDelta in the server's pack file, with their bases outside
// the output set.  We arrange this by:
//   1. Pushing a large file (~200 KB, moderate entropy) to the server and
//      repacking before the client clones, so existing objects live in a pack
//      with delta chains.
//   2. Pushing a new commit that *removes* the last 50 lines from that file.
//      The shortened blob is a good OFS_DELTA candidate against the original
//      (git prefers the larger object as the delta base), so after repack the
//      new blob is stored as OFS_DELTA against the blob the client already has.
//   3. Repacking again so the new objects are delta-compressed.
//
// git automatically includes `thin-pack` in its fetch capabilities whenever the
// client has existing objects, so no special client flags are needed.
//
// We verify correctness two ways:
//   a. `git fsck` — git thickens thin packs on receipt and fsck catches any pack
//      that references a base the client doesn't have.
//   b. Wire sniffing — a TCP proxy sits between the real git client and our
//      server; we parse the captured server→client bytes and assert that at least
//      one OBJ_REF_DELTA (type-7) entry appears in the pack git actually received.
#[test]
fn fetch_with_thin_pack() -> anyhow::Result<()> {
    let temprepo = common::temprepo()?;
    let server = temprepo.path();

    // Push a large file (~200 KB, moderate entropy) so git will definitely build
    // a delta chain between the old and new blob during repack.  We use numbered
    // lines so the content isn't trivially compressible — git skips delta
    // compression when zlib shrinks blobs to almost nothing.
    let setup_work = tempdir()?;
    common::run_git(setup_work.path(), ["clone", server.to_str().unwrap()])?;
    let setup_repo = setup_work.path().join("temprepo");
    let large_base: String = (0u32..3000)
        .map(|i| {
            format!("line {i:05}: payload data for thin-pack delta compression testing abc xyz\n")
        })
        .collect();
    fs::write(setup_repo.join("large.txt"), &large_base)?;
    common::run_git(&setup_repo, ["add", "large.txt"])?;
    common::run_git(&setup_repo, ["commit", "-m", "Add large.txt"])?;
    common::run_git(&setup_repo, ["push", "origin", "main"])?;

    // Pack all existing server objects so delta chains are built.
    common::run_git(&server, ["repack", "-a", "-d"])?;

    let handle = axum_server(Config {
        bare_repo_path: server.clone(),
    });
    let server_port = handle.port;

    // A sniffing proxy forwards all traffic to the server and records every
    // server→client response.  All git HTTP operations below go through it.
    let proxy = SniffingProxy::new(server_port);

    // Clone so the client holds the large file.
    let cloned = tempdir()?;
    common::run_git(
        cloned.path(),
        [
            "clone",
            "--branch",
            "main",
            format!("http://localhost:{}/test.git", proxy.port).as_ref(),
        ],
    )?;
    let clone_dir = cloned.path().join("test");
    let main_before = common::run_git(&clone_dir, ["rev-parse", "origin/main"])?;

    // Push a new commit that removes the last 50 lines from large.txt.  Shorter
    // content means the new blob is stored as OFS_DELTA against the old one (git
    // prefers larger objects as full bases), which is exactly what thin-pack
    // needs: the delta base is already held by the client.
    let server_work = tempdir()?;
    common::run_git(server_work.path(), ["clone", server.to_str().unwrap()])?;
    let server_repo = server_work.path().join("temprepo");
    let large_modified: String = (0u32..2950)
        .map(|i| {
            format!("line {i:05}: payload data for thin-pack delta compression testing abc xyz\n")
        })
        .collect();
    fs::write(server_repo.join("large.txt"), &large_modified)?;
    common::run_git(&server_repo, ["add", "large.txt"])?;
    common::run_git(
        &server_repo,
        ["commit", "-m", "Modify large.txt for thin-pack test"],
    )?;
    common::run_git(&server_repo, ["push", "origin", "main"])?;
    let new_commit = common::run_git(&server_repo, ["rev-parse", "HEAD"])?;

    // Repack so the new blob lands in a pack file and git builds a delta against
    // the old blob (which the client already holds).
    common::run_git(&server, ["repack", "-a", "-d"])?;

    // The real git client fetches through the proxy.  It automatically sends
    // `thin-pack` since it has existing objects.
    common::run_git(&clone_dir, ["fetch", "origin", "main"])?;

    let main_after = common::run_git(&clone_dir, ["rev-parse", "origin/main"])?;
    assert_eq!(
        main_after, new_commit,
        "origin/main should point to the new commit"
    );
    assert_ne!(main_before, main_after, "origin/main should have advanced");

    // fsck verifies that git successfully thickened the thin pack (resolved all
    // RefDelta bases from the local objects) and that the result is consistent.
    common::run_git(&clone_dir, ["fsck", "--no-progress"])?;

    // Verify the pack the real git client received is a true thin pack:
    // it contains RefDelta entries (the delta) but no full Blob (the base is absent).
    assert!(
        proxy.has_thin_pack_in_response()?,
        "expected a thin pack with RefDelta entries and no full base blob in the pack \
         the real git client received"
    );

    handle.stop();
    Ok(())
}
