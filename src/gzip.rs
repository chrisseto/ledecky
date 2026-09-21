//! Gzips sized, compressible responses for clients that accept it.
//!
//! Streamed bodies — the event stream, the terminal socket — pass through
//! untouched: gzip would buffer them.

use std::io::{Cursor, Write};

use flate2::write::GzEncoder;
use flate2::Compression;
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::{ContentType, Header};
use rocket::{Request, Response};

/// Below this, the gzip header and trailer eat most of the saving.
const MIN_LEN: usize = 1024;

pub struct Gzip;

#[rocket::async_trait]
impl Fairing for Gzip {
    fn info(&self) -> Info {
        Info {
            name: "gzip",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, req: &'r Request<'_>, res: &mut Response<'r>) {
        if !res.content_type().is_some_and(|ct| compressible(&ct)) {
            return;
        }
        res.adjoin_header(Header::new("Vary", "Accept-Encoding"));

        if !accepts_gzip(req) || res.headers().contains("Content-Encoding") {
            return;
        }
        if res.body().preset_size().is_none_or(|len| len < MIN_LEN) {
            return;
        }

        let body = match res.body_mut().to_bytes().await {
            Ok(body) => body,
            Err(err) => {
                error!("gzip: reading body: {err}");
                return;
            }
        };

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        // NB: writes to a `Vec` cannot fail.
        encoder.write_all(&body).unwrap();
        let gzipped = encoder.finish().unwrap();

        res.set_raw_header("Content-Encoding", "gzip");
        res.set_sized_body(gzipped.len(), Cursor::new(gzipped));
    }
}

fn compressible(ct: &ContentType) -> bool {
    ct.top() == "text" || ct.is_javascript() || ct.is_json() || ct.is_svg()
}

fn accepts_gzip(req: &Request<'_>) -> bool {
    req.headers()
        .get("Accept-Encoding")
        .flat_map(|value| value.split(','))
        .any(|coding| {
            let mut params = coding.split(';').map(str::trim);
            params.next() == Some("gzip") && !params.any(|p| p == "q=0" || p == "q=0.0")
        })
}
