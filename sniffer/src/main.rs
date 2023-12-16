use anyhow::{bail, Context, Error, Result};
use futures_lite::AsyncRead;
use log::info;
use simple_logger::SimpleLogger;
use trillium::{conn_try, Conn};
use trillium_smol::ClientConfig;
use log::LevelFilter;
use trillium_client::Client;
use trillium_smol;
use trillium::Method;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    SimpleLogger::new().with_level(LevelFilter::Info).init().unwrap();

    // port 8080
    trillium_smol::run(|mut conn: trillium::Conn| async move {
        let client = Client::new(trillium_rustls::RustlsConfig::<trillium_smol::ClientConfig>::default())
            .with_default_pool();

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

        let url = format!("https://github.com{}", conn.path());
        println!("REQ {}", url);

        let mut upstream_conn = match conn.method() {
            Method::Get => {
                client.get(url.as_str())
            },
            Method::Post => {
                let body = conn.request_body_string().await.unwrap();
                println!("POST BODY");
                println!("{}", body);
                client.post(url.as_str()).with_body(body)
            },
            _ => todo!(),
        };

        match conn
            .request_headers()
            .get_str(trillium::KnownHeaderName::ContentType) {
                Some(v) => {
                    upstream_conn.request_headers().append(trillium::KnownHeaderName::ContentType, v.to_owned());
                },
                None => (),
            }

        match conn
            .request_headers()
            .get_str("Git-Protocol") {
                Some(v) => {
                    upstream_conn.request_headers().append("Git-Protocol", v.to_owned());
                },
                None => (),
            }

        // for h in conn.headers().iter() {
        //     upstream_conn.request_headers().append(h.0, h.1.clone());
        // }

        upstream_conn = upstream_conn.await.unwrap();

        let response_body = upstream_conn.response_body();
        let body = response_body.read_bytes().await.unwrap();

        println!("RESPONSE BODY");
        println!("{:?}", body);

        conn.with_body(body)
    });
}
