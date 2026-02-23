mod common;
use anyhow::Result;
use mizzle::{servers::trillium::trillium_handler, traits::GitServerCallbacks};
use std::path::PathBuf;
use std::thread;
use trillium::State;

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
    let server = trillium_smol::config().with_stopper(stopper.clone());

    thread::spawn(|| {
        // port 8080
        server.run((State::new(config), trillium_handler::<Config>));
    });

    let git_output_from_path = common::run_git(
        &temprepo.path(),
        ["ls-remote", temprepo.path().to_str().unwrap()],
    )?;
    let git_output_from_server = common::run_git(
        &temprepo.path(),
        ["ls-remote", "http://localhost:8080/test.git"],
    )?;
    println!("{}", git_output_from_path);
    println!(".....");
    println!("{}", git_output_from_server);

    assert_eq!(git_output_from_path, git_output_from_server);

    stopper.stop();

    Ok(())
}
