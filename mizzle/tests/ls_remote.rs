mod common;
use anyhow::Result;

use crate::common::{axum_server, trillium_server, Config};

#[test]
fn test_ls_remote_trillium() -> Result<()> {
    let temprepo = common::temprepo()?;

    let config = Config {
        bare_repo_path: temprepo.path(),
    };

    let stopper = trillium_server(config);

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

#[test]
fn test_ls_remote_axum() -> Result<()> {
    let temprepo = common::temprepo()?;

    let config = Config {
        bare_repo_path: temprepo.path(),
    };

    let tx = axum_server(config);

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

    let _ = tx.send(());

    Ok(())
}
