mod common;

use common::{test_with_servers, Config};

test_with_servers!(test_ls_remote, |start_server| {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path(),
    };
    let server = start_server(config);

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
});
