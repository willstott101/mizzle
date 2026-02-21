mod common;
use anyhow::Result;
use std::thread;
use std::path::PathBuf;
use mizzle::{serve, traits::GitServerCallbacks};


#[derive(Clone)]
struct Config {
	bare_repo_path: PathBuf,
}

impl GitServerCallbacks for Config {
    fn auth(&self, _repo_path: &str) -> Box<str> {
        self.bare_repo_path.to_str().unwrap().into()
    }
}

#[test]
fn test_ls_remote() -> Result<()> {
	let temprepo = common::temprepo()?;

    let config = Config {
    	bare_repo_path: temprepo.path(),
    };

    let stopper = trillium_smol::Stopper::new();
    let server = trillium_smol::config()
	    	.with_stopper(stopper.clone());

    thread::spawn(|| {
	    // port 8080
	    server.run(move |conn: trillium::Conn| {
	        let config = config.clone();
	        async move {
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
	                    let full_repo_path = config.auth(repo_path_owned.as_ref());
	                    serve::serve_git_protocol_2(conn, full_repo_path, protocol_path_owned).await
	                }
	                None => conn
	                    .with_status(trillium::Status::BadRequest)
	                    .with_body("Path doesn't look like a git URL")
	                    .halt(),
	            }
	        }
	    });
    });

	let git_output_from_path = common::run_git(&temprepo.path(), ["ls-remote", temprepo.path().to_str().unwrap()])?;
	let git_output_from_server = common::run_git(&temprepo.path(), ["ls-remote", "http://localhost:8080/test.git"])?;
	println!("{}", git_output_from_path);
	println!(".....");
	println!("{}", git_output_from_server);

	assert_eq!(git_output_from_path, git_output_from_server);

	stopper.stop();

	Ok(())
}
