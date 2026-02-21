pub trait GitServerCallbacks: Clone {
    fn auth(&self, repo_path: &str) -> Box<str>;
}
