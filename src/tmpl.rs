use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};

use anyhow::{Context, Result};
use minijinja::{Environment, Value};
use rocket::http::{ContentType, Method, Status};
use rocket::request::Request;
use rocket::response::{self, Responder, Response};

pub struct Templates {
    icons: HashMap<String, String>,
}

impl Templates {
    pub fn load() -> Result<Self> {
        let dir = std::path::Path::new("static/icons");
        let entries = fs::read_dir(dir)
            .with_context(|| format!("reading {} — run `pnpm build` first", dir.display()))?;

        let mut icons = HashMap::new();
        for entry in entries {
            let path = entry?.path();
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let svg = fs::read_to_string(&path)?;
            icons.insert(name.to_owned(), normalize_svg(&svg, name));
        }

        Ok(Self { icons })
    }

    fn env(&self) -> Environment<'static> {
        let mut env = Environment::new();
        env.set_loader(minijinja::path_loader("templates"));

        let icons = self.icons.clone();
        env.add_function("icon", move |name: &str| match icons.get(name) {
            Some(svg) => Value::from_safe_string(svg.clone()),
            None => Value::from_safe_string(format!("<!-- missing icon: {name} -->")),
        });

        env
    }

    pub fn render(&self, name: &str, ctx: Value) -> Result<String> {
        // NB: the env is rebuilt per render so template edits land without a restart.
        // path_loader only reads the templates a render actually touches.
        Ok(self.env().get_template(name)?.render(ctx)?)
    }
}

/// Strips lucide's license comment and its own `class`, replacing it with ours,
/// and collapses the pretty-printed source onto one line.
fn normalize_svg(svg: &str, name: &str) -> String {
    let body = svg.split_once("<svg").map_or(svg, |(_, rest)| rest);

    // Drop `class="lucide lucide-foo"` so we do not emit a duplicate attribute.
    let body = match body.find("class=\"") {
        Some(start) => match body[start + 7..].find('"') {
            Some(len) => format!("{}{}", &body[..start], &body[start + 8 + len..]),
            None => body.to_owned(),
        },
        None => body.to_owned(),
    };

    let body = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    format!("<svg class=\"icon icon-{name}\" {body}")
}

/// A polled template writes this where the response's own ETag belongs.
///
/// The digest is taken over the body with the placeholder still in it and
/// substituted afterwards, so the ETag is never an input to itself.
///
/// NB: it is filled with the digest of whatever response carries it, so a
/// fragment that is sometimes rendered inside a larger page must only emit it
/// when it is the response — see `review::routes::initial`.
const ETAG_SLOT: &str = "__up_etag__";

/// Not a security boundary — a collision costs one redundant fragment swap.
/// `DefaultHasher` is stable within a build, which is all an ETag needs.
fn etag_of(body: &str) -> String {
    let mut hasher = DefaultHasher::new();
    body.hash(&mut hasher);
    format!("\"{:016x}\"", hasher.finish())
}

fn if_none_match(req: &Request<'_>, etag: &str) -> bool {
    req.headers()
        .get("If-None-Match")
        .flat_map(|value| value.split(','))
        .any(|candidate| candidate.trim() == etag)
}

/// Renders a minijinja template, pulling `Templates` out of Rocket's state.
pub struct Tmpl(pub &'static str, pub Value);

impl Tmpl {
    /// What a template puts in `up-etag` to have it filled in on the way out.
    pub const ETAG_SLOT: &'static str = ETAG_SLOT;
}

impl<'r> Responder<'r, 'static> for Tmpl {
    fn respond_to(self, req: &'r Request<'_>) -> response::Result<'static> {
        let templates = req
            .rocket()
            .state::<Templates>()
            .expect("Templates is not managed");

        let body = match templates.render(self.0, self.1) {
            Ok(body) => body,
            Err(err) => {
                rocket::error!("template {}: {err:#}", self.0);
                return Err(Status::InternalServerError);
            }
        };

        let etag = etag_of(&body);

        // Conditional requests only mean anything for GET. `Tmpl` is also the
        // error arm of a couple of POSTs, where a 304 would be a lie.
        let conditional = req.method() == Method::Get;

        // NB: this is what stops the board flashing. `[up-poll]` reloads send
        // `If-None-Match` from the fragment's `up-etag`, and a bodyless 304
        // makes unpoly skip the render outright rather than swap in identical
        // markup and throw away hover, scroll and selection with it.
        if conditional && if_none_match(req, &etag) {
            return Response::build()
                .status(Status::NotModified)
                .raw_header("ETag", etag)
                .raw_header("Cache-Control", "no-cache")
                .ok();
        }

        let body = body.replace(ETAG_SLOT, &etag);

        let mut res = Response::build();
        res.header(ContentType::HTML);
        if conditional {
            // `no-cache` rather than `no-store`: the browser should keep the
            // page for bfcache and revalidate, not refetch it.
            res.raw_header("ETag", etag)
                .raw_header("Cache-Control", "no-cache");
        }
        res.sized_body(body.len(), std::io::Cursor::new(body)).ok()
    }
}
