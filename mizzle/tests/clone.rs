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

    let stopper = trillium_server(config);

    let git_output_from_server = common::run_git(
        tempdir()?.path(),
        ["clone", "http://localhost:8080/test.git"],
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

    let tx = axum_server(config);

    let cloned = tempdir()?;

    let git_output_from_server =
        common::run_git(cloned.path(), ["clone", "http://localhost:8080/test.git"])?;
    println!("{}", git_output_from_server);

    let _ = tx.send(());

    Ok(())
}
