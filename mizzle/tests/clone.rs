mod common;

use anyhow::Result;
use tempfile::tempdir;

use crate::common::{axum_server, trillium_server, Config};

#[test]
fn test_clone_trillium() -> Result<()> {
    let temprepo = common::temprepo()?;

    let config = Config {
        bare_repo_path: temprepo.path(),
    };

    let (port, stopper) = trillium_server(config);

    let git_output_from_server = common::run_git(
        tempdir()?.path(),
        [
            "clone",
            format!("http://localhost:{}/test.git", port).as_ref(),
        ],
    )?;
    println!("{}", git_output_from_server);

    stopper.stop();

    Ok(())
}

#[test]
fn test_clone_axum() -> Result<()> {
    let temprepo = common::temprepo()?;

    let config = Config {
        bare_repo_path: temprepo.path(),
    };

    let (port, tx) = axum_server(config);

    let cloned = tempdir()?;

    let git_output_from_server = common::run_git(
        cloned.path(),
        [
            "clone",
            format!("http://localhost:{}/test.git", port).as_ref(),
        ],
    )?;
    println!("{:?}", git_output_from_server);

    let _ = tx.send(());

    Ok(())
}
