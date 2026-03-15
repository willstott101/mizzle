mod common;

use tempfile::tempdir;

use common::{test_with_servers, Config};

test_with_servers!(test_clone, |start_server| {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path(),
    };
    let server = start_server(config);

    let git_output = common::run_git(
        tempdir()?.path(),
        [
            "clone",
            format!("http://localhost:{}/test.git", server.port).as_ref(),
        ],
    )?;
    println!("{}", git_output);

    server.stop();
    Ok(())
});
