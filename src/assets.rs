//! Serves the built web assets, and names revisions of them for the caches.
//!
//! The bundle is embedded rather than read from `static/` at request time. It
//! is a build artifact either way — `build.rs` writes it — and embedding is
//! what lets a validator be computed once and trusted afterwards, since the
//! bytes behind it cannot then change under a running server.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};

use include_dir::{include_dir, Dir, File};
use rocket::http::{ContentType, Status};
use rocket::request::Request;
use rocket::response::{self, Responder, Response};

static ASSETS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/static");

pub fn routes() -> Vec<rocket::Route> {
    routes![asset]
}

#[get("/<path..>")]
fn asset(path: PathBuf) -> Option<Asset> {
    // NB: `..` never reaches here — Rocket rejects it while parsing the
    // segments — and a lookup misses rather than escaping regardless, because
    // this reads the embedded tree rather than the filesystem.
    Some(Asset(ASSETS.get_file(&path)?))
}

pub(crate) fn etag_of(body: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    body.hash(&mut hasher);
    format!("\"{:016x}\"", hasher.finish())
}

pub(crate) fn if_none_match(req: &Request<'_>, etag: &str) -> bool {
    req.headers()
        .get("If-None-Match")
        .flat_map(|value| value.split(','))
        .any(|candidate| candidate.trim() == etag)
}

pub struct Asset(&'static File<'static>);

impl Asset {
    /// This asset's validator, hashed on first ask and then kept.
    fn etag(&self) -> Arc<str> {
        static ETAGS: LazyLock<RwLock<HashMap<&'static Path, Arc<str>>>> =
            LazyLock::new(Default::default);

        if let Some(etag) = ETAGS.read().unwrap().get(self.0.path()) {
            return etag.clone();
        }

        let etag: Arc<str> = etag_of(self.0.contents()).into();
        ETAGS.write().unwrap().insert(self.0.path(), etag.clone());
        etag
    }
}

impl<'r> Responder<'r, 'static> for Asset {
    fn respond_to(self, req: &'r Request<'_>) -> response::Result<'static> {
        let etag = self.etag();

        // `no-cache` asks for a conditional request rather than a refetch: the
        // browser keeps the response, and its compiled form, across the 304.
        // Not a `max-age`, however long the bytes outlive a request — these
        // URLs carry no content hash, so a lifetime is also how long a deploy
        // takes to reach anyone still holding the old one.
        let mut res = Response::build();
        res.raw_header("ETag", etag.to_string())
            .raw_header("Cache-Control", "no-cache");

        if if_none_match(req, &etag) {
            return res.status(Status::NotModified).ok();
        }

        if let Some(kind) = self
            .0
            .path()
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(ContentType::from_extension)
        {
            res.header(kind);
        }

        let body = self.0.contents();
        res.sized_body(body.len(), Cursor::new(body)).ok()
    }
}
