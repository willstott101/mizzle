use mizzle::{servers::trillium::trillium_handler, traits::GitServerCallbacks};

use simple_logger::SimpleLogger;
use trillium::State;
use trillium_smol;

#[derive(Clone)]
struct Config;

impl GitServerCallbacks for Config {
    fn auth(&self, repo_path: &str) -> Box<str> {
        let repo_root = ".";

        format!("{}/{}", repo_root, repo_path).into()
    }
}

fn main() {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .init()
        .unwrap();

    let config = Config {};

    // port 8080
    trillium_smol::run((State::new(config), trillium_handler::<Config>));
}
