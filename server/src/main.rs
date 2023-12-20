use anyhow::{bail, Context, Error, Result};
use futures_lite::AsyncRead;
use gix_packetline::encode::{flush_to_write, text_to_write};
use gix_packetline::{PacketLineRef, StreamingPeekableIter};
use gix_ref::file::ReferenceExt;
use log::info;
use simple_logger::SimpleLogger;
use trillium::{conn_try, Conn};
use trillium_smol;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .init()
        .unwrap();

    // port 8080
    trillium_smol::run(|conn: trillium::Conn| async move {
        if conn
            .headers()
            .get_str("Git-Protocol")
            .unwrap_or("version=2")
            != "version=2"
        {
            println!("Only Git Protocol 2 is supported");
            return conn
                .with_status(trillium::Status::NotImplemented)
                .with_body("Only Git Protocol 2 is supported")
                .halt();
        }

        let result = conn.path().rsplit_once(".git/");
        match result {
            Some((git_repo_path, service_path)) => {
                let repo_path_owned: Box<str> = git_repo_path.into();
                let protocol_path_owned: Box<str> = service_path.into();
                serve_git_protocol_2(conn, repo_path_owned, protocol_path_owned).await
            }
            None => conn
                .with_status(trillium::Status::BadRequest)
                .with_body("Path doesn't look like a git URL")
                .halt(),
        }
    });
}

async fn serve_git_protocol_2(
    mut conn: trillium::Conn,
    repo_path: Box<str>,
    protocol_path: Box<str>,
) -> Conn {
    // The git protocol recommends making sure to prevent any caching
    conn = conn.with_header(trillium::KnownHeaderName::CacheControl, "no-cache");

    info!(
        "REQUEST CONTENT TYPE {:?}",
        conn.headers().get_str("Content-Type")
    );
    info!(
        "HTTP {} {} {}",
        conn.method(),
        conn.path(),
        conn.querystring()
    );
    info!("GIT {} {}", repo_path, protocol_path);

    if protocol_path.as_ref() == "info/refs" {
        // We also expect a query parameter of ?service=git-upload-pack but I don't see a reason to check for it.

        // Part of the V2 handshake
        conn = conn.with_header(
            trillium::KnownHeaderName::ContentType,
            "application/x-git-upload-pack-advertisement",
        );
        let (reader, mut writer) = piper::pipe(4096);
        trillium_smol::spawn((|| async move {
            // Copied from github
            text_to_write(b"# service=git-upload-pack", &mut writer)
                .await
                .expect("to write to output");
            flush_to_write(&mut writer)
                .await
                .expect("to write to output");

            // Understood in the spec
            text_to_write(b"version 2", &mut writer)
                .await
                .expect("to write to output");
            text_to_write(b"agent=mizzle/dev", &mut writer)
                .await
                .expect("to write to output");

            text_to_write(b"ls-refs=unborn", &mut writer)
                .await
                .expect("to write to output");
            text_to_write(b"fetch", &mut writer).await.expect("to write to output");
            // Copied from github - yet to be implemented/confirmed
            // text_to_write(b"fetch=shallow wait-for-done filter", &mut writer).await.expect("to write to output");
            // text_to_write(b"server-option", &mut writer).await.expect("to write to output");
            // text_to_write(b"object-format=sha1", &mut writer).await.expect("to write to output");

            // Understood in the spec
            flush_to_write(&mut writer)
                .await
                .expect("to write to output");
        })());
        conn.with_status(trillium::Status::Ok)
            .with_body(trillium::Body::new_streaming(reader, None))
            .halt()
    } else if protocol_path.as_ref() == "git-upload-pack" {
        if conn
            .headers()
            .get_str(trillium::KnownHeaderName::ContentType)
            != Some("application/x-git-upload-pack-request")
        {
            return conn
                .with_status(trillium::Status::BadRequest)
                .with_body("Expected content type application/x-git-upload-pack-request")
                .halt();
        } else {
            // println!("{}", conn.request_body_string().await.unwrap());
            let mut parser = StreamingPeekableIter::new(conn.request_body().await, &[]);
            let command = conn_try!(read_command(&mut parser).await, conn);
            match command {
                Command::ListRefs => {
                    // println!("{}", conn.request_body_string().await.unwrap());
                    let args = conn_try!(read_lsrefs_args(&mut parser).await, conn);
                    // info!("LIST REFS ARGS: {:?}", args);
                    let repo = conn_try!(gix::open("."), conn).into_sync();
                    let (reader, writer) = piper::pipe(4096);
                    trillium_smol::spawn((|| async move {
                        perform_listrefs(&repo, &args, writer).await.unwrap();
                    })());
                    return conn
                        .with_status(trillium::Status::Ok)
                        .with_body(trillium::Body::new_streaming(reader, None))
                        .halt();
                }
                Command::Empty => (),
            }
        }
        conn
    } else {
        conn
    }
}

#[derive(Debug)]
enum Command {
    ListRefs,
    Empty,
}

async fn read_command<T>(parser: &mut StreamingPeekableIter<T>) -> Result<Command>
where
    T: AsyncRead + Unpin,
{
    let line = parser
        .read_line()
        .await
        .context("no line when expecting command")???;
    if matches!(line, PacketLineRef::Flush) {
        return Ok(Command::Empty);
    }
    let bstr = line.as_bstr().context("no data when expecting command")?;
    let command = bstr
        .strip_suffix(b"\n")
        .unwrap_or(bstr)
        .strip_prefix(b"command=")
        .context("expected command")?;
    match command {
        b"ls-refs" => Ok(Command::ListRefs),
        command_name => Err(Error::msg(format!(
            "unrecognised command: {:?}",
            command_name
        ))),
    }
}

#[derive(Debug)]
struct ListRefsArgs {
    /// In addition to the object pointed by it, show the underlying ref
    /// pointed by it when showing a symbolic ref.
    symrefs: bool,
    /// Show peeled tags.
    peel: bool,
    /// When specified, only references having a prefix matching one of
    /// the provided prefixes are displayed. Multiple instances may be
    /// given, in which case references matching any prefix will be
    /// shown. Note that this is purely for optimization; a server MAY
    /// show refs not matching the prefix if it chooses, and clients
    /// should filter the result themselves.
    prefixes: Vec<Box<[u8]>>,
    /// The server will send information about HEAD even if it is a symref
    /// pointing to an unborn branch in the form "unborn HEAD
    /// symref-target:<target>".
    unborn: bool,
}

async fn read_lsrefs_args<T>(parser: &mut StreamingPeekableIter<T>) -> Result<ListRefsArgs>
where
    T: AsyncRead + Unpin,
{
    // "command=ls-refs"
    // "agent=git/2.40.1"
    // None (delimiter)
    // "peel"
    // "symrefs"
    // "ref-prefix HEAD"
    // "ref-prefix refs/heads/"
    // "ref-prefix refs/tags/"
    skip_till_delimiter(parser).await?; // TODO: Is this info ever useful?
    let mut args = ListRefsArgs {
        symrefs: false,
        peel: false,
        unborn: false,
        prefixes: Vec::new(),
    };
    loop {
        let line = parser
            .read_line()
            .await
            .context("unexpected eof (missing flush packet?)")???;
        match line {
            PacketLineRef::ResponseEnd | PacketLineRef::Flush => break,
            PacketLineRef::Delimiter => bail!("unexpected delimiter"),
            PacketLineRef::Data(d) => {
                let arg = d.strip_suffix(b"\n").unwrap_or(d);
                match arg.strip_prefix(b"ref-prefix ") {
                    Some(prefix) => args.prefixes.push(prefix.into()),
                    None => {
                        match arg {
                            b"peel" => args.peel = true,
                            b"symrefs" => args.symrefs = true,
                            b"unborn" => args.unborn = true,
                            _ => bail!("unrecognised lsrefs argument"),
                        };
                    }
                };
            }
        }
    }
    Ok(args)
}

fn get_head_info(repo: &gix::ThreadSafeRepository, args: &ListRefsArgs) -> Result<Option<String>> {
    Ok(match repo.to_thread_local().head_ref()? {
        Some(mut head_ref) => {
            head_ref.peel_to_id_in_place()?;
            match head_ref.inner.peeled {
                None => None,
                Some(oid) => {
                    if args.symrefs {
                        Some(format!(
                            "{} HEAD symref-target:{}",
                            oid,
                            head_ref.name().as_bstr()
                        ))
                    } else {
                        Some(format!("{} HEAD", oid))
                    }
                }
            }
        }
        None => None,
    })
}

async fn perform_listrefs(
    repo: &gix::ThreadSafeRepository,
    args: &ListRefsArgs,
    mut writer: piper::Writer,
) -> Result<()> {
    match get_head_info(repo, args)? {
        Some(packetline) => {
            text_to_write(packetline.as_bytes(), &mut writer)
                .await
                .expect("to write to output");
        }
        None => {
            if args.unborn {
                text_to_write(b"unborn HEAD", &mut writer)
                    .await
                    .expect("to write to output");
            }
        }
    }

    for reference in repo.refs.iter()?.all()? {
        // TODO: packet-line style error handling
        let r = reference?;
        let mut to_peel = r.clone();

        match r.target {
            // This reference is to an annotated tag or a commit.
            gix_ref::Target::Peeled(oid) => {
                if args.peel {
                    // We check if this is an annotated tag by peeling it
                    let peeled =
                        to_peel.peel_to_id_in_place(&repo.refs, &repo.objects.to_handle())?;
                    // The peeled result changes so this must have been an annotated tag
                    if peeled != oid {
                        // Output the anotated tag's oid & name but also the underlying commit's oid
                        text_to_write(
                            format!("{} {} peeled:{}", oid, r.name, peeled).as_bytes(),
                            &mut writer,
                        )
                        .await
                        .expect("to write to output");
                        continue;
                    }
                }
                // Either this isn't an annotated tag, or we weren't asked to peel
                // So we just return the oid directly
                text_to_write(format!("{} {}", oid, r.name).as_bytes(), &mut writer)
                    .await
                    .expect("to write to output");
            }
            // This is a symbolic reference (such as HEAD)
            gix_ref::Target::Symbolic(symref_target) => {
                // We always need to find the underlying commit oid of the symbolic reference so we do that first.
                let peeled = to_peel.peel_to_id_in_place(&repo.refs, &repo.objects.to_handle())?;
                // We only output the name of the intermediate reference if requested
                if args.symrefs {
                    text_to_write(
                        format!("{} {} symref-target:{}", peeled, r.name, symref_target).as_bytes(),
                        &mut writer,
                    )
                    .await
                    .expect("to write to output");
                } else {
                    text_to_write(format!("{} {}", peeled, r.name).as_bytes(), &mut writer)
                        .await
                        .expect("to write to output");
                }
            }
        }
    }

    flush_to_write(&mut writer)
        .await
        .expect("to write to output");
    Ok(())
}

async fn skip_till_delimiter<T>(parser: &mut StreamingPeekableIter<T>) -> Result<()>
where
    T: AsyncRead + Unpin,
{
    loop {
        let line = parser.read_line().await.context("expected delimiter")???;
        match line {
            PacketLineRef::ResponseEnd | PacketLineRef::Flush => {
                bail!("found end of response expected delimiter")
            }
            PacketLineRef::Delimiter => return Ok(()),
            PacketLineRef::Data(_) => (),
        }
    }
}
