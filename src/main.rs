#[macro_use]
extern crate actix_web;
extern crate serde_json;
#[macro_use]
extern crate serde_derive;

mod cache;
mod color;
mod error;

use crate::{
    cache::CacheState,
    color::{ColorKind, ToCode},
    error::Error,
};
use actix_web::{
    error::ErrorBadRequest,
    http::{
        self,
        header::{CacheControl, CacheDirective, Expires},
    },
    middleware, web, App, HttpResponse, HttpServer,
};
use badge::{Badge, BadgeOptions};
use bytes::Bytes;
use futures::{unsync::mpsc, Stream};
use git2::Repository;
use std::{
    convert::TryFrom,
    fs::create_dir_all,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, SystemTime},
};
use structopt::StructOpt;

struct State {
    repos: String,
    cache: String,
}

const INDEX: &str = include_str!("../static/index.html");
const CSS: &str = include_str!("../static/tacit-css.min.css");

#[derive(StructOpt, Debug)]
struct Opt {
    #[structopt(
        short = "o",
        long = "outdir",
        parse(from_os_str),
        default_value = "./repos"
    )]
    /// Path to store cloned repositories
    outdir: PathBuf,
    #[structopt(
        short = "c",
        long = "cachedir",
        parse(from_os_str),
        default_value = "./cache"
    )]
    /// Path to store cache
    cachedir: PathBuf,
    #[structopt(short = "p", long = "port", default_value = "8080")]
    /// Port to listen on
    port: u16,
    #[structopt(short = "h", long = "host", default_value = "0.0.0.0")]
    /// Interface to listen on
    host: String,
}

#[derive(Debug, Deserialize)]
struct BadgeQuery {
    color: Option<String>,
}

fn pull(path: impl AsRef<Path>) -> Result<(), Error> {
    let repo = Repository::open_bare(path)?;
    let mut origin = repo.find_remote("origin")?;
    origin.fetch(&["refs/heads/*:refs/heads/*"], None, None)?;
    Ok(())
}

fn hoc(repo: &str, repo_dir: &str, cache_dir: &str) -> Result<u64, Error> {
    let repo_dir = format!("{}/{}", repo_dir, repo);
    let cache_dir = format!("{}/{}.json", cache_dir, repo);
    let cache_dir = Path::new(&cache_dir);
    let head = format!(
        "{}",
        Repository::open_bare(&repo_dir)?
            .head()?
            .target()
            .ok_or(Error::Internal)?
    );
    let mut arg = vec![
        "log".to_string(),
        "--pretty=tformat:".to_string(),
        "--numstat".to_string(),
        "--ignore-space-change".to_string(),
        "--ignore-all-space".to_string(),
        "--ignore-submodules".to_string(),
        "--no-color".to_string(),
        "--find-copies-harder".to_string(),
        "-M".to_string(),
        "--diff-filter=ACDM".to_string(),
    ];
    let cache = CacheState::read_from_file(&cache_dir, &head)?;
    match &cache {
        CacheState::Current(res) => return Ok(*res),
        CacheState::Old(cache) => {
            arg.push(format!("{}..HEAD", cache.head));
        }
        CacheState::No => {}
    };
    arg.push("--".to_string());
    arg.push(".".to_string());
    let output = Command::new("git")
        .args(&arg)
        .current_dir(&repo_dir)
        .output()?
        .stdout;
    let output = String::from_utf8_lossy(&output);
    let count: u64 = output
        .lines()
        .map(|s| {
            s.split_whitespace()
                .take(2)
                .map(str::parse::<u64>)
                .filter_map(Result::ok)
                .sum::<u64>()
        })
        .sum();

    let cache = cache.calculate_new_cache(count, head);
    cache.write_to_file(cache_dir)?;

    Ok(cache.count)
}

fn calculate_hoc(
    service: &str,
    state: web::Data<Arc<State>>,
    data: web::Path<(String, String)>,
    color: web::Query<BadgeQuery>,
) -> Result<HttpResponse, Error> {
    let service_path = format!("{}/{}/{}", service, data.0, data.1);
    let path = format!("{}/{}", state.repos, service_path);
    let file = Path::new(&path);
    if !file.exists() {
        create_dir_all(file)?;
        let repo = Repository::init_bare(file)?;
        repo.remote_add_fetch("origin", "refs/heads/*:refs/heads/*")?;
        repo.remote_set_url("origin", &format!("https://{}", service_path))?;
    }
    pull(&path)?;
    let hoc = hoc(&service_path, &state.repos, &state.cache)?;
    let color = color
        .into_inner()
        .color
        .map(|s| ColorKind::try_from(s.as_str()))
        .and_then(Result::ok)
        .unwrap_or_default();
    let badge_opt = BadgeOptions {
        subject: "Hits-of-Code".to_string(),
        color: color.to_code(),
        status: hoc.to_string(),
    };
    let badge = Badge::new(badge_opt)?;

    let (tx, rx_body) = mpsc::unbounded();
    let _ = tx.unbounded_send(Bytes::from(badge.to_svg().as_bytes()));

    let expiration = SystemTime::now() + Duration::from_secs(30);
    Ok(HttpResponse::Ok()
        .content_type("image/svg+xml")
        .set(Expires(expiration.into()))
        .set(CacheControl(vec![
            CacheDirective::MaxAge(0u32),
            CacheDirective::MustRevalidate,
            CacheDirective::NoCache,
            CacheDirective::NoStore,
        ]))
        .streaming(rx_body.map_err(|_| ErrorBadRequest("bad request"))))
}

fn github(
    state: web::Data<Arc<State>>,
    data: web::Path<(String, String)>,
    color: web::Query<BadgeQuery>,
) -> Result<HttpResponse, Error> {
    calculate_hoc("github.com", state, data, color)
}

fn gitlab(
    state: web::Data<Arc<State>>,
    data: web::Path<(String, String)>,
    color: web::Query<BadgeQuery>,
) -> Result<HttpResponse, Error> {
    calculate_hoc("gitlab.com", state, data, color)
}

fn bitbucket(
    state: web::Data<Arc<State>>,
    data: web::Path<(String, String)>,
    color: web::Query<BadgeQuery>,
) -> Result<HttpResponse, Error> {
    calculate_hoc("bitbucket.org", state, data, color)
}

#[get("/badge")]
fn badge_example(col: web::Query<BadgeQuery>) -> Result<HttpResponse, Error> {
    let col = col.into_inner();
    let color = col
        .color
        .clone()
        .map(|s| ColorKind::try_from(s.as_str()))
        .transpose()?
        // .and_then(Result::ok)
        .unwrap_or_default();
    let badge_opt = BadgeOptions {
        subject: "Hits-of-Code".to_string(),
        color: color.to_code(),
        status: col.color.unwrap_or_else(|| "success".to_string()),
    };
    let badge = Badge::new(badge_opt)?;

    let (tx, rx_body) = mpsc::unbounded();
    let _ = tx.unbounded_send(Bytes::from(badge.to_svg().as_bytes()));

    let expiration = SystemTime::now() + Duration::from_secs(60 * 60 * 24 * 365);
    Ok(HttpResponse::Ok()
        .content_type("image/svg+xml")
        .set(Expires(expiration.into()))
        .set(CacheControl(vec![CacheDirective::Public]))
        .streaming(rx_body.map_err(|_| ErrorBadRequest("bad request"))))
}

fn overview(_: web::Path<(String, String)>) -> HttpResponse {
    HttpResponse::TemporaryRedirect()
        .header(http::header::LOCATION, "/")
        .finish()
}

#[get("/")]
fn index() -> HttpResponse {
    let (tx, rx_body) = mpsc::unbounded();
    let _ = tx.unbounded_send(Bytes::from(INDEX.as_bytes()));

    HttpResponse::Ok()
        .content_type("text/html")
        .streaming(rx_body.map_err(|_| ErrorBadRequest("bad request")))
}

#[get("/tacit-css.min.css")]
fn css() -> HttpResponse {
    let (tx, rx_body) = mpsc::unbounded();
    let _ = tx.unbounded_send(Bytes::from(CSS.as_bytes()));

    HttpResponse::Ok()
        .content_type("text/css")
        .streaming(rx_body.map_err(|_| ErrorBadRequest("bad request")))
}

fn main() -> std::io::Result<()> {
    std::env::set_var("RUST_LOG", "actix_web=info");
    pretty_env_logger::init();
    openssl_probe::init_ssl_cert_env_vars();
    let opt = Opt::from_args();
    let interface = format!("{}:{}", opt.host, opt.port);
    let state = Arc::new(State {
        repos: opt.outdir.display().to_string(),
        cache: opt.cachedir.display().to_string(),
    });
    HttpServer::new(move || {
        App::new()
            .data(state.clone())
            .wrap(middleware::Logger::default())
            .service(index)
            .service(css)
            .service(badge_example)
            .service(web::resource("/github/{user}/{repo}").to(github))
            .service(web::resource("/gitlab/{user}/{repo}").to(gitlab))
            .service(web::resource("/bitbucket/{user}/{repo}").to(bitbucket))
            .service(web::resource("/view/github/{user}/{repo}").to(overview))
            .service(web::resource("/view/gitlab/{user}/{repo}").to(overview))
            .service(web::resource("/view/github/{user}/{repo}").to(overview))
    })
    .bind(interface)?
    .run()
}
