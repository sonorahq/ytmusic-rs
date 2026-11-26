use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::{Context as _, Result, bail};
use regex_lite::Regex;
use serde_json::Value;

const PRELUDE: &str = include_str!("../assets/js/prelude.js");
const MERIYAH: &str = include_str!("../assets/js/meriyah.js");
const ASTRING: &str = include_str!("../assets/js/astring.js");
const SOLVER: &str = include_str!("../assets/js/solver.js");

const IFRAME_URL: &str = "https://www.youtube.com/iframe_api";
const JS_STACK: usize = 16 * 1024 * 1024;
const OS_STACK: usize = 64 * 1024 * 1024;
const MEMORY: usize = 512 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Script {
    pub id: String,
    pub sts: u32,
    pub code: String,
    pub prepared: bool,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct Kept {
    id: String,
    sts: u32,
    bundle: String,
    code: String,
}

fn bundle() -> &'static str {
    static BUNDLE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        use sha1::{Digest as _, Sha1};
        let mut hasher = Sha1::new();
        for source in [PRELUDE, MERIYAH, ASTRING, SOLVER] {
            hasher.update(source);
        }
        hasher.finalize()[..4]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    });
    &BUNDLE
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Solved {
    pub sig: Option<String>,
    pub n: Option<String>,
}

struct Job {
    sig: Option<String>,
    n: Option<String>,
    reply: tokio::sync::oneshot::Sender<Result<Solved>>,
}

pub struct Solver {
    id: String,
    sts: u32,
    jobs: tokio::sync::mpsc::UnboundedSender<Job>,
}

impl Solver {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn sts(&self) -> u32 {
        self.sts
    }

    pub fn start(script: Script, cache: Option<PathBuf>) -> Result<Self> {
        let (jobs, queue) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let (ready, started) = mpsc::channel::<Result<()>>();
        let id = script.id.clone();
        let sts = script.sts;
        std::thread::Builder::new()
            .name(format!("ytmusic-deobf-{id}"))
            .stack_size(OS_STACK)
            .spawn(move || serve(script, cache, queue, ready))
            .context("cannot start the deobfuscator thread")?;
        started
            .recv()
            .context("the deobfuscator thread died while starting")??;
        Ok(Self { id, sts, jobs })
    }

    pub async fn solve(&self, sig: Option<&str>, n: Option<&str>) -> Result<Solved> {
        if sig.is_none() && n.is_none() {
            return Ok(Solved::default());
        }
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.jobs
            .send(Job {
                sig: sig.map(str::to_string),
                n: n.map(str::to_string),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("the deobfuscator is gone"))?;
        answer
            .await
            .context("the deobfuscator dropped the request")?
    }
}

fn serve(
    script: Script,
    cache: Option<PathBuf>,
    mut queue: tokio::sync::mpsc::UnboundedReceiver<Job>,
    ready: mpsc::Sender<Result<()>>,
) {
    let prepared = prepare(&script, cache.as_deref());
    drop(script);
    release();
    let context = match prepared {
        Ok(context) => {
            if ready.send(Ok(())).is_err() {
                return;
            }
            context
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    while let Some(job) = queue.blocking_recv() {
        let answered = answer(&context, job.sig.as_deref(), job.n.as_deref());
        let _ = job.reply.send(answered);
    }
}

fn prepare(script: &Script, cache: Option<&Path>) -> Result<rquickjs::Context> {
    match script.prepared {
        true => log::debug!("deobf: starting a quickjs runtime for the cached solver"),
        false => log::debug!(
            "deobf: starting a quickjs runtime to preprocess player {}, this takes seconds",
            script.id
        ),
    }
    let runtime = rquickjs::Runtime::new().context("cannot start the js runtime")?;
    runtime.set_max_stack_size(JS_STACK);
    runtime.set_memory_limit(MEMORY);
    let context = rquickjs::Context::full(&runtime).context("cannot build the js context")?;
    context.with(|ctx| {
        let sources = match script.prepared {
            true => [Some(("prelude", PRELUDE)), None, None, None],
            false => [
                Some(("prelude", PRELUDE)),
                Some(("meriyah", MERIYAH)),
                Some(("astring", ASTRING)),
                Some(("solver", SOLVER)),
            ],
        };
        for (name, source) in sources.into_iter().flatten() {
            ctx.eval::<(), _>(source)
                .map_err(|error| report(&ctx, error))
                .with_context(|| format!("cannot load {name}"))?;
        }

        let solver = match script.prepared {
            true => script.code.clone(),
            false => {
                ctx.globals()
                    .set("__player", script.code.as_str())
                    .map_err(|error| report(&ctx, error))
                    .context("cannot hand the player script to the solver")?;
                let solver: String = ctx
                    .eval(PREPROCESS)
                    .map_err(|error| report(&ctx, error))
                    .context("cannot read the player script")?;
                keep(cache, script, &solver);
                solver
            }
        };

        ctx.globals()
            .set("__solver", solver.as_str())
            .map_err(|error| report(&ctx, error))
            .context("cannot hand the solver to the runtime")?;
        ctx.eval::<(), _>(INSTALL)
            .map_err(|error| report(&ctx, error))
            .context("cannot install the solver")?;
        Ok::<_, anyhow::Error>(())
    })?;
    runtime.run_gc();
    Ok(context)
}

fn keep(cache: Option<&Path>, script: &Script, solver: &str) {
    let Some(path) = cache else {
        return;
    };
    let kept = Kept {
        id: script.id.clone(),
        sts: script.sts,
        bundle: bundle().to_string(),
        code: solver.to_owned(),
    };
    if let Err(error) = store(path, &kept) {
        log::warn!("deobf: cannot cache player {}: {error:#}", script.id);
    }
}

fn store(path: &Path, kept: &Kept) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("cannot create the cache dir")?;
    }
    let body = serde_json::to_vec(kept).context("cannot encode the solver")?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temp, body).with_context(|| format!("cannot write {}", temp.display()))?;
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error).with_context(|| format!("cannot replace {}", path.display()));
    }
    Ok(())
}

fn kept(cache: Option<&Path>, id: &str) -> Option<Kept> {
    let body = std::fs::read(cache?).ok()?;
    let kept: Kept = serde_json::from_slice(&body).ok()?;
    if kept.id != id {
        log::debug!("deobf: the cache holds player {}, wanted {id}", kept.id);
        return None;
    }
    if kept.bundle != bundle() {
        log::debug!(
            "deobf: the cache was written by solver bundle {}, this is {}",
            kept.bundle,
            bundle()
        );
        return None;
    }
    Some(kept)
}

fn release() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        malloc_trim(0);
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
unsafe extern "C" {
    fn malloc_trim(pad: usize) -> i32;
}

const PREPROCESS: &str = r#"
var __prepared = jsc.default({
  type: "player",
  player: __player,
  requests: [],
  output_preprocessed: true,
});
__player = null;
var __code = __prepared.preprocessed_player;
__prepared = null;
__code
"#;

const INSTALL: &str = r#"
globalThis.__solvers = { sig: null, n: null };
Function("_result", __solver)(__solvers);
__solver = null;
if (!__solvers.sig && !__solvers.n) {
  throw "the player script yields no solver";
}
"#;

fn answer(context: &rquickjs::Context, sig: Option<&str>, n: Option<&str>) -> Result<Solved> {
    let output: String = context.with(|ctx| {
        ctx.globals()
            .set("__sig", sig)
            .map_err(|error| report(&ctx, error))
            .context("cannot hand the signature to the solver")?;
        ctx.globals()
            .set("__n", n)
            .map_err(|error| report(&ctx, error))
            .context("cannot hand the n parameter to the solver")?;
        ctx.eval::<String, _>(RUN)
            .map_err(|error| report(&ctx, error))
            .context("cannot run the solver")
    })?;
    let parsed: Value =
        serde_json::from_str(&output).context("the solver returned a non-json result")?;
    if let Some(error) = parsed.get("error").and_then(Value::as_str) {
        bail!("the solver failed: {}", first_line(error));
    }
    let answered = |key: &str, wanted: bool| match wanted {
        false => Ok(None),
        true => parsed
            .get(key)
            .and_then(Value::as_str)
            .map(|found| Some(found.to_string()))
            .with_context(|| format!("the solver returned no {key}")),
    };
    Ok(Solved {
        sig: answered("sig", sig.is_some())?,
        n: answered("n", n.is_some())?,
    })
}

const RUN: &str = r#"
(function () {
  try {
    return JSON.stringify({
      sig: __sig === null || __sig === undefined ? null : __solvers.sig(__sig),
      n: __n === null || __n === undefined ? null : __solvers.n(__n),
    });
  } catch (error) {
    return JSON.stringify({ error: error instanceof Error ? error.message : String(error) });
  }
})()
"#;

pub async fn fetch(http: &reqwest::Client, cache: Option<&Path>) -> Result<Script> {
    let id = player_id(http).await?;
    if let Some(kept) = kept(cache, &id) {
        log::debug!(
            "deobf: player {id} read from the cache, {} bytes",
            kept.code.len()
        );
        return Ok(Script {
            id: kept.id,
            sts: kept.sts,
            code: kept.code,
            prepared: true,
        });
    }
    download(http, id).await
}

/// Downloads the script of the player named `id` and reads its signature timestamp.
async fn download(http: &reqwest::Client, id: String) -> Result<Script> {
    let url = format!("https://www.youtube.com/s/player/{id}/player_ias.vflset/en_US/base.js");
    let response = http
        .get(&url)
        .send()
        .await
        .context("cannot reach the player script")?;
    if !response.status().is_success() {
        bail!("player script {id} returned status {}", response.status());
    }
    let code = response
        .text()
        .await
        .context("cannot read the player script")?;
    let sts = signature_timestamp(&code)
        .with_context(|| format!("player script {id} has no signature timestamp"))?;
    log::debug!("deobf: player {id}, sts {sts}, {} bytes", code.len());
    Ok(Script {
        id,
        sts,
        code,
        prepared: false,
    })
}

async fn player_id(http: &reqwest::Client) -> Result<String> {
    let body = http
        .get(IFRAME_URL)
        .send()
        .await
        .context("cannot reach the iframe api")?
        .text()
        .await
        .context("cannot read the iframe api")?;
    let pattern = Regex::new(r"player\\?/([0-9a-fA-F]{8})\\?/").expect("valid pattern");
    pattern
        .captures(&body)
        .map(|found| found[1].to_string())
        .context("the iframe api names no player")
}

fn signature_timestamp(code: &str) -> Option<u32> {
    let pattern = Regex::new(r"signatureTimestamp:(\d+)").expect("valid pattern");
    pattern.captures(code)?[1].parse().ok()
}

fn report(ctx: &rquickjs::Ctx<'_>, error: rquickjs::Error) -> anyhow::Error {
    if !matches!(error, rquickjs::Error::Exception) {
        return anyhow::anyhow!("{error}");
    }
    let caught = ctx.catch();
    if let Some(exception) = caught.as_exception()
        && let Some(message) = exception.message()
    {
        return anyhow::anyhow!("{message}");
    }
    match caught.as_string().and_then(|text| text.to_string().ok()) {
        Some(thrown) => anyhow::anyhow!("{thrown}"),
        None => anyhow::anyhow!("the solver threw a value it cannot describe"),
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE_PLAYER: &str = r#"
(function() {
  class Wrap {
    constructor(params) { this.params = params; }
    xform() {
      this.params.set("s", (this.params.get("s") || "").split("").reverse().join(""));
      this.params.set("n", (this.params.get("n") || "").toUpperCase());
    }
    set(key, value) { this.params.set(key, String(value)); }
    get(key) { return this.params.get(key); }
    clone() { return this; }
  }
  buildUrl = function(base, key, value) {
    var params = new Map();
    var wrapped = new Wrap(params);
    if (value !== undefined) { wrapped.set(key, value); }
    wrapped.set("alr", "yes");
    return wrapped;
  };
}).call(this);
"#;

    fn fake() -> Script {
        Script {
            id: "test".into(),
            sts: 1,
            code: FAKE_PLAYER.into(),
            prepared: false,
        }
    }

    fn write_cache(path: &Path, bundle: &str) {
        let stored = Kept {
            id: "b1558f06".into(),
            sts: 20675,
            bundle: bundle.into(),
            code: "var x=1".into(),
        };
        std::fs::write(path, serde_json::to_vec(&stored).expect("encodes")).expect("writes");
    }

    #[test]
    fn takes_a_cache_from_this_bundle() {
        let path = std::env::temp_dir().join("ytmusic-bundle-match.json");
        write_cache(&path, bundle());
        let found = kept(Some(&path), "b1558f06").expect("a matching cache is taken");
        assert_eq!(found.sts, 20675);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ignores_a_cache_from_another_bundle() {
        let path = std::env::temp_dir().join("ytmusic-bundle-stale.json");
        write_cache(&path, "deadbeef");
        assert!(kept(Some(&path), "b1558f06").is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ignores_a_cache_for_another_player() {
        let path = std::env::temp_dir().join("ytmusic-bundle-other.json");
        write_cache(&path, bundle());
        assert!(kept(Some(&path), "0d2d49a1").is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bundle_is_stable_and_short() {
        assert_eq!(bundle().len(), 8);
        assert_eq!(bundle(), bundle());
    }

    #[test]
    fn reads_signature_timestamp() {
        let code = "referer:document.location.toString(),signatureTimestamp:20675},a=g.Bd()";
        assert_eq!(signature_timestamp(code), Some(20675));
    }

    #[test]
    fn missing_signature_timestamp() {
        assert_eq!(signature_timestamp("var a=1"), None);
    }

    #[tokio::test]
    async fn solves_against_a_fake_player() {
        let solver = Solver::start(fake(), None).expect("the fake player yields a solver");
        assert_eq!(solver.id(), "test");
        let solved = solver
            .solve(Some("abc"), Some("xy"))
            .await
            .expect("the pipeline runs end to end in quickjs");
        assert_eq!(solved.sig.as_deref(), Some("cba"));
        assert_eq!(solved.n.as_deref(), Some("XY"));
    }

    #[tokio::test]
    async fn solves_one_side_only() {
        let solver = Solver::start(fake(), None).expect("the fake player yields a solver");
        let solved = solver
            .solve(None, Some("xy"))
            .await
            .expect("n alone solves");
        assert_eq!(
            solved,
            Solved {
                sig: None,
                n: Some("XY".into())
            }
        );
    }

    #[tokio::test]
    async fn reuses_one_context() {
        let solver = Solver::start(fake(), None).expect("the fake player yields a solver");
        for _ in 0..3 {
            let solved = solver.solve(Some("abc"), None).await.expect("solves again");
            assert_eq!(solved.sig.as_deref(), Some("cba"));
        }
    }

    #[tokio::test]
    async fn nothing_to_solve() {
        let solver = Solver::start(fake(), None).expect("the fake player yields a solver");
        assert_eq!(
            solver
                .solve(None, None)
                .await
                .expect("no challenges is fine"),
            Solved::default()
        );
    }

    #[test]
    fn rejects_a_player_it_cannot_read() {
        let script = Script {
            id: "broken".into(),
            sts: 1,
            code: "var x=1".into(),
            prepared: false,
        };
        let Err(error) = Solver::start(script, None) else {
            panic!("a non-player is an error");
        };
        assert!(format!("{error:#}").contains("unexpected structure"));
    }

    async fn live(id: &str) -> Solver {
        let script = download(&reqwest::Client::new(), id.to_string())
            .await
            .expect("youtube serves the player script");
        tokio::task::spawn_blocking(move || Solver::start(script, None))
            .await
            .expect("the deobfuscator thread joins")
            .expect("the player script yields a solver")
    }

    #[tokio::test]
    #[ignore = "downloads a player script from youtube"]
    async fn solves_a_player_with_prototype_helpers() {
        let solved = live("92eef845")
            .await
            .solve(None, Some("rYKKd90cGmViBQaX"))
            .await
            .expect("the n parameter solves");
        assert_eq!(solved.n.as_deref(), Some("3tU2ya-IJqhNt"));
    }

    #[tokio::test]
    #[ignore = "downloads the current player script from youtube"]
    async fn solves_the_current_player() {
        let http = reqwest::Client::new();
        let id = player_id(&http)
            .await
            .expect("the iframe api names a player");
        let signature: String = ('a'..='z').chain('A'..='Z').chain('0'..='9').collect();
        let solved = live(&id)
            .await
            .solve(Some(&signature), Some("rYKKd90cGmViBQaX"))
            .await
            .expect("the current player solves");
        let sig = solved.sig.expect("a signature comes back");
        let n = solved.n.expect("an n parameter comes back");
        assert!(
            !sig.is_empty() && sig != signature,
            "player {id} left the signature as it was"
        );
        assert!(
            !n.is_empty() && n != "rYKKd90cGmViBQaX",
            "player {id} left n as it was"
        );
    }
}
