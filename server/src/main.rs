use trillium_smol;
use simple_logger::SimpleLogger;
use log::info;
use form_urlencoded;
use std::collections::HashMap;
use std::borrow::Cow;


fn main() {
    SimpleLogger::new().init().unwrap();

    // port 8080
    trillium_smol::run(|conn: trillium::Conn| async move {
        if conn.headers().get_str("Git-Protocol").unwrap_or("2") != "2" {
            return conn.with_status(501).with_body("Only Git Protocol 2 is supported").halt();
        }

        let params: HashMap<_, _> = form_urlencoded::parse(conn.querystring().as_bytes()).collect();
        if params.get("service") != Some(&Cow::from("upload-pack")) {
            return conn.with_status(501).with_body("Only upload-pack is supported").halt();
        }

        info!("{} {} {}", conn.method(), conn.path(), conn.querystring());
        conn.ok("hello from trillium!")
    });
}
