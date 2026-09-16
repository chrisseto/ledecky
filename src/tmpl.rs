use std::collections::HashMap;
use std::fs;

use anyhow::{Context, Result};
use minijinja::{Environment, Value};
use rocket::http::{ContentType, Status};
use rocket::request::Request;
use rocket::response::{self, Responder, Response};

pub struct Templates {
    icons: HashMap<String, String>,
}

impl Templates {
    pub fn load() -> Result<Self> {
        let dir = std::path::Path::new("static/icons");
        let entries = fs::read_dir(dir).with_context(|| {
            format!("reading {} — run `pnpm build` first", dir.display())
        })?;

        let mut icons = HashMap::new();
        for entry in entries {
            let path = entry?.path();
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else { continue };
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

/// Renders a minijinja template, pulling `Templates` out of Rocket's state.
pub struct Tmpl(pub &'static str, pub Value);

impl<'r> Responder<'r, 'static> for Tmpl {
    fn respond_to(self, req: &'r Request<'_>) -> response::Result<'static> {
        let templates = req
            .rocket()
            .state::<Templates>()
            .expect("Templates is not managed");

        match templates.render(self.0, self.1) {
            Ok(body) => Response::build()
                .header(ContentType::HTML)
                .sized_body(body.len(), std::io::Cursor::new(body))
                .ok(),
            Err(err) => {
                rocket::error!("template {}: {err:#}", self.0);
                Err(Status::InternalServerError)
            }
        }
    }
}
