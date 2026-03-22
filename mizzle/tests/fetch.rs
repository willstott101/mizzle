mod common;

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use tempfile::tempdir;

use common::Config;

dual_backend_test!(test_fetch, |make_server: fn(
    Config,
) -> common::ServerHandle| {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path().clone(),
    };
    let server = make_server(config);

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

// ── Thin-pack test (FsGitoxide-only, tests wire format) ──────────────────────

struct SniffingProxy {
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

    fn has_thin_pack_in_response(&self) -> anyhow::Result<bool> {
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

fn proxy_connection(client: TcpStream, upstream_port: u16) -> anyhow::Result<Vec<u8>> {
    let server = TcpStream::connect(format!("127.0.0.1:{upstream_port}"))?;
    let mut client_r = client.try_clone()?;
    let mut client_w = client;
    let mut server_r = server.try_clone()?;
    let mut server_w = server;

    let fwd = thread::spawn(move || {
        let _ = io::copy(&mut client_r, &mut server_w);
        let _ = server_w.shutdown(std::net::Shutdown::Write);
    });

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

fn extract_pack_from_response(raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut search_from = 0;

    while search_from < raw.len() {
        let Some(rel) = raw[search_from..].windows(4).position(|w| w == b"\r\n\r\n") else {
            break;
        };
        let header_end = search_from + rel + 4;

        let headers = std::str::from_utf8(&raw[search_from..header_end]).unwrap_or("");
        let is_chunked = headers
            .to_lowercase()
            .contains("transfer-encoding: chunked");

        let (body, body_raw_len) = dechunk_body(&raw[header_end..], is_chunked)?;

        if let Ok(pack) = pack_from_pkt_lines(&body) {
            return Ok(pack);
        }

        search_from = header_end + body_raw_len;
    }

    anyhow::bail!("no pack data found in any HTTP response")
}

fn dechunk_body(raw: &[u8], is_chunked: bool) -> anyhow::Result<(Vec<u8>, usize)> {
    if !is_chunked {
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
            break;
        }
        if pkt_len == 1 {
            pos += 4;
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

#[test]
fn fetch_with_thin_pack() -> anyhow::Result<()> {
    let temprepo = common::temprepo()?;
    let server = temprepo.path();

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

    common::run_git(&server, ["repack", "-a", "-d"])?;

    let handle = common::axum_server(Config {
        bare_repo_path: server.clone(),
    });
    let server_port = handle.port;

    let proxy = SniffingProxy::new(server_port);

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

    common::run_git(&server, ["repack", "-a", "-d"])?;

    common::run_git(&clone_dir, ["fetch", "origin", "main"])?;

    let main_after = common::run_git(&clone_dir, ["rev-parse", "origin/main"])?;
    assert_eq!(
        main_after, new_commit,
        "origin/main should point to the new commit"
    );
    assert_ne!(main_before, main_after, "origin/main should have advanced");

    common::run_git(&clone_dir, ["fsck", "--no-progress"])?;

    assert!(
        proxy.has_thin_pack_in_response()?,
        "expected a thin pack with RefDelta entries and no full base blob in the pack \
         the real git client received"
    );

    handle.stop();
    Ok(())
}
