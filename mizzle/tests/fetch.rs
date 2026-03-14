mod common;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

use anyhow::Result;
use tempfile::tempdir;

use common::{axum_server, Config};

/// Makes a raw git-upload-pack v2 fetch request and returns the raw pack bytes
/// (all sideband channel-1 data concatenated).
fn fetch_pack_over_http(port: u16, path: &str, want: &str, have: &str) -> anyhow::Result<Vec<u8>> {
    fn pkt(data: &[u8]) -> Vec<u8> {
        let len = data.len() + 4;
        let mut v = format!("{:04x}", len).into_bytes();
        v.extend_from_slice(data);
        v
    }

    let mut body: Vec<u8> = Vec::new();
    body.extend(pkt(b"command=fetch\n"));
    body.extend(pkt(b"agent=git/test\n"));
    body.extend_from_slice(b"0001"); // delimiter
    body.extend(pkt(format!("want {want}\n").as_bytes()));
    body.extend(pkt(format!("have {have}\n").as_bytes()));
    body.extend(pkt(b"thin-pack\n"));
    body.extend(pkt(b"ofs-delta\n"));
    body.extend(pkt(b"done\n"));
    body.extend_from_slice(b"0000"); // flush

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))?;
    write!(
        stream,
        "POST /{path}/git-upload-pack HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Git-Protocol: version=2\r\n\
         Content-Type: application/x-git-upload-pack-request\r\n\
         Accept: application/x-git-upload-pack-result\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    )?;
    stream.write_all(&body)?;

    let mut reader = BufReader::new(stream);

    // Parse HTTP headers
    let mut is_chunked = false;
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        if line.trim().is_empty() {
            break;
        }
        if line.to_lowercase().contains("transfer-encoding: chunked") {
            is_chunked = true;
        }
    }

    // Read the HTTP response body
    let mut raw_body: Vec<u8> = Vec::new();
    if is_chunked {
        loop {
            let mut size_line = String::new();
            reader.read_line(&mut size_line)?;
            let size_str = size_line.trim().split(';').next().unwrap_or("0");
            let size = usize::from_str_radix(size_str, 16)?;
            if size == 0 {
                break;
            }
            let mut chunk = vec![0u8; size];
            reader.read_exact(&mut chunk)?;
            raw_body.extend_from_slice(&chunk);
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf)?;
        }
    } else {
        reader.read_to_end(&mut raw_body)?;
    }

    // Parse pkt-lines to extract sideband channel-1 (pack data).
    // Protocol v2 fetch (with `done`) response:
    //   PKT-LINE("packfile\n")
    //   PKT-LINE(\x01 <pack-bytes>)*
    //   flush (0000)
    let mut pos = 0;
    let mut pack_bytes: Vec<u8> = Vec::new();
    let mut in_packfile = false;

    loop {
        if pos + 4 > raw_body.len() {
            break;
        }
        let len_hex = std::str::from_utf8(&raw_body[pos..pos + 4])?;
        let pkt_len = usize::from_str_radix(len_hex, 16)?;
        if pkt_len == 0 {
            break; // flush
        }
        if pkt_len == 1 {
            pos += 4; // delimiter
            continue;
        }
        if pos + pkt_len > raw_body.len() {
            break;
        }
        let data = &raw_body[pos + 4..pos + pkt_len];
        let data_stripped = data.strip_suffix(b"\n").unwrap_or(data);

        if !in_packfile && data_stripped == b"packfile" {
            in_packfile = true;
        } else if in_packfile && data[0] == 1 {
            // Channel 1 = pack data; strip the band byte
            pack_bytes.extend_from_slice(&data[1..]);
        }

        pos += pkt_len;
    }

    Ok(pack_bytes)
}

/// Returns true if the pack contains at least one OBJ_REF_DELTA (type 7) entry.
fn has_ref_delta_entries(pack: &[u8]) -> anyhow::Result<bool> {
    use gix_pack::data::{entry::Header, input};

    let iter = input::BytesToEntriesIter::new_from_header(
        BufReader::new(pack),
        input::Mode::AsIs,
        input::EntryDataMode::Ignore,
        gix_hash::Kind::Sha1,
    )?;
    for entry in iter {
        if matches!(entry?.header, Header::RefDelta { .. }) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[test]
fn test_fetch_axum() -> Result<()> {
    let temprepo = common::temprepo()?;

    let config = Config {
        bare_repo_path: temprepo.path().clone(),
    };

    let (port, tx) = axum_server(config);

    let cloned = tempdir()?;

    // Clone from the axum server
    common::run_git(
        cloned.path(),
        [
            "clone",
            "--branch",
            "main",
            format!("http://localhost:{}/test.git", port).as_ref(),
        ],
    )?;

    let clone_dir = cloned.path().join("test");
    let main_before = common::run_git(clone_dir.as_path(), ["rev-parse", "origin/main"])?;

    // Add a new commit to the bare repo so the client can fetch it. We push via the
    // filesystem to the same bare repo the server serves—not through HTTP (server
    // doesn't support push).
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

    // Verify we got the new commit
    let main_after = common::run_git(clone_dir.as_path(), ["rev-parse", "origin/main"])?;
    assert_eq!(main_after, new_commit, "origin/main should be the new commit");
    assert_ne!(
        main_before, main_after,
        "origin/main should have advanced"
    );

    let _ = tx.send(());

    Ok(())
}

// Verifies that the server correctly honours the `thin-pack` fetch capability.
//
// For a thin pack to actually contain RefDelta entries the objects being sent
// must be stored as OfsDelta in the server's pack file, with their bases outside
// the output set.  We arrange this by:
//   1. Pushing a large file (guaranteed to be delta-compressed by git) to the
//      server and repacking before the client clones.
//   2. Pushing a new commit that modifies that large file by one line — the
//      updated blob is an excellent delta candidate against the original.
//   3. Repacking again so the new blob is delta-compressed against the old one
//      which the client already holds.
//
// git automatically includes `thin-pack` in its fetch capabilities whenever the
// client has existing objects, so no special client flags are needed.
//
// We verify correctness two ways:
//   a. `git fsck` — git thickens thin packs on receipt and fsck catches any pack
//      that references a base the client doesn't have.
//   b. Direct wire inspection — we make a raw HTTP git-upload-pack request and
//      parse the returned pack bytes with `BytesToEntriesIter`, asserting that at
//      least one OBJ_REF_DELTA (type-7) entry is present.
#[test]
fn fetch_with_thin_pack() -> anyhow::Result<()> {
    let temprepo = common::temprepo()?;
    let server = temprepo.path();

    // Push a large file (~200 KB, moderate entropy) so git will definitely build a delta chain
    // between the old and new blob during repack.  We use numbered lines so the content isn't
    // trivially compressible — git skips delta compression when zlib shrinks blobs to almost
    // nothing, which happens with purely repeated text.
    let setup_work = tempdir()?;
    common::run_git(setup_work.path(), ["clone", server.to_str().unwrap()])?;
    let setup_repo = setup_work.path().join("temprepo");
    let large_base: String = (0u32..3000)
        .map(|i| format!("line {i:05}: payload data for thin-pack delta compression testing abc xyz\n"))
        .collect();
    fs::write(setup_repo.join("large.txt"), &large_base)?;
    common::run_git(&setup_repo, ["add", "large.txt"])?;
    common::run_git(&setup_repo, ["commit", "-m", "Add large.txt"])?;
    common::run_git(&setup_repo, ["push", "origin", "main"])?;

    // Pack all existing server objects so delta chains are built.
    common::run_git(&server, ["repack", "-a", "-d"])?;

    let config = Config {
        bare_repo_path: server.clone(),
    };
    let (port, tx) = axum_server(config);

    // Clone so the client has the large file.
    let cloned = tempdir()?;
    common::run_git(
        cloned.path(),
        [
            "clone",
            "--branch",
            "main",
            format!("http://localhost:{}/test.git", port).as_ref(),
        ],
    )?;
    let clone_dir = cloned.path().join("test");
    let main_before = common::run_git(&clone_dir, ["rev-parse", "origin/main"])?;

    // Push a new commit that *removes* the last 50 lines from large.txt.  Shorter
    // content means the new blob is stored as OFS_DELTA against the old one (git
    // prefers larger objects as full bases), which is exactly what thin-pack needs:
    // the delta base (old blob) is already held by the client.
    let server_work = tempdir()?;
    common::run_git(server_work.path(), ["clone", server.to_str().unwrap()])?;
    let server_repo = server_work.path().join("temprepo");
    let large_modified: String = (0u32..2950)
        .map(|i| format!("line {i:05}: payload data for thin-pack delta compression testing abc xyz\n"))
        .collect();
    fs::write(server_repo.join("large.txt"), &large_modified)?;
    common::run_git(&server_repo, ["add", "large.txt"])?;
    common::run_git(&server_repo, ["commit", "-m", "Modify large.txt for thin-pack test"])?;
    common::run_git(&server_repo, ["push", "origin", "main"])?;
    let new_commit = common::run_git(&server_repo, ["rev-parse", "HEAD"])?;

    // Repack so the new blob lands in a pack file and git builds a delta between
    // the old and new large.txt blobs.
    common::run_git(&server, ["repack", "-a", "-d"])?;

    // Fetch — git automatically sends `thin-pack` since the client has existing
    // objects, so the server will produce RefDelta entries for any blobs whose
    // delta base is held by the client.
    common::run_git(&clone_dir, ["fetch", "origin", "main"])?;

    let main_after = common::run_git(&clone_dir, ["rev-parse", "origin/main"])?;
    assert_eq!(main_after, new_commit, "origin/main should point to the new commit");
    assert_ne!(main_before, main_after, "origin/main should have advanced");

    // fsck verifies that git successfully thickened the thin pack (resolved all
    // RefDelta bases from the local objects) and that the result is consistent.
    common::run_git(&clone_dir, ["fsck", "--no-progress"])?;

    // Verify that the server actually sends a thin pack (OBJ_REF_DELTA entries)
    // over the wire by making a raw HTTP fetch request and inspecting the pack.
    let pack = fetch_pack_over_http(port, "test.git", &new_commit, &main_before)?;
    assert!(
        has_ref_delta_entries(&pack)?,
        "expected at least one RefDelta entry in the thin pack sent over the wire"
    );

    let _ = tx.send(());
    Ok(())
}

