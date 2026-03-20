mod common;

use common::Config;

#[test]
fn test_ls_remote() -> anyhow::Result<()> {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path(),
    };
    let server = common::axum_server(config);

    let git_output_from_path = common::run_git(
        &temprepo.path(),
        ["ls-remote", temprepo.path().to_str().unwrap()],
    )?;
    let git_output_from_server = common::run_git(
        &temprepo.path(),
        [
            "ls-remote",
            format!("http://localhost:{}/test.git", server.port).as_ref(),
        ],
    )?;

    assert_eq!(git_output_from_path, git_output_from_server);

    server.stop();
    Ok(())
}
