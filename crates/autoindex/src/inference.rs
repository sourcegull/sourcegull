//! Index job inference: Lua recognizers, run in a sandbox, look at a repository's paths (and the
//! contents of the files they ask for) and return index jobs.
//!
//! An inference script returns a table of named recognizers
//! (`require("sg.autoindex.config").new({...})`). The built-in recognizers are named `sg.go`,
//! `sg.java`, `sg.python`, `sg.ruby`, `sg.rust`, `sg.typescript`, `sg.dotnet` and `sg.clang`; a
//! script adds recognizers under other names, replaces nothing built in, and can disable a built-in
//! one by setting its name to `false`. A recognizer is either a path recognizer (`patterns`,
//! `patterns_for_content`, `generate`) or a fallback recognizer (a list tried in order until one
//! returns jobs). `generate(api, paths, contents_by_path)` gets the paths that match its patterns
//! and the contents of the files matching its content patterns; it returns jobs and may register
//! more recognizers with `api:register(...)`, which run in the next round with their own patterns.
//!
//! The sandbox: Lua 5.4 with only the `string`, `table`, `math` and `utf8` libraries and the base
//! functions minus `load`, `loadfile`, `dofile`, `collectgarbage` and `string.dump`; chunks are
//! text only (no bytecode); `require` only resolves the modules below; `print` writes to the
//! inference output. Each inference gets a fresh state with an instruction budget, a wall-clock
//! deadline and a memory limit ([`Limits`]).
//!
//! Modules: `path` (`ancestors`, `basename`, `dirname`, `join`, `split`), `json` (`decode`,
//! `encode`), `sg.autoindex.patterns` (`new_path_literal`, `new_path_segment`,
//! `new_path_basename`, `new_path_extension`, `new_path_glob`, `new_path_combine`,
//! `new_path_exclude`), `sg.autoindex.recognizer` (`new_path_recognizer`,
//! `new_fallback_recognizer`), `sg.autoindex.indexes` (`get(language)`: the indexer image),
//! `sg.autoindex.config`, `sg.autoindex.util`, `sg.autoindex.shared` and the built-in
//! recognizers as `sg.autoindex.<language>`.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{
    AnyUserData, Function, HookTriggers, Lua, LuaOptions, StdLib, Table, UserData, UserDataMethods,
    Value, Variadic, VmState,
};

use mlua::chunk::ChunkMode;

use crate::config::{DockerStep, IndexJobSpec};
use crate::glob::PathGlob;
use crate::indexers::IndexerMap;

/// The built-in recognizers, by name (`sg.<name>`, module `sg.autoindex.<name>`).
pub const BUILTIN_RECOGNIZERS: [&str; 8] = [
    "clang",
    "dotnet",
    "go",
    "java",
    "python",
    "ruby",
    "rust",
    "typescript",
];

/// The Lua sources of the built-in modules.
const LUA_MODULES: &[(&str, &str)] = &[
    ("sg.autoindex.config", include_str!("../lua/config.lua")),
    ("sg.autoindex.util", include_str!("../lua/util.lua")),
    ("sg.autoindex.shared", include_str!("../lua/shared.lua")),
    ("sg.autoindex.clang", include_str!("../lua/clang.lua")),
    ("sg.autoindex.dotnet", include_str!("../lua/dotnet.lua")),
    ("sg.autoindex.go", include_str!("../lua/go.lua")),
    ("sg.autoindex.java", include_str!("../lua/java.lua")),
    ("sg.autoindex.python", include_str!("../lua/python.lua")),
    ("sg.autoindex.ruby", include_str!("../lua/ruby.lua")),
    ("sg.autoindex.rust", include_str!("../lua/rust.lua")),
    (
        "sg.autoindex.typescript",
        include_str!("../lua/typescript.lua"),
    ),
];

/// Resource limits of one inference.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Lua VM instructions over the whole inference.
    pub instructions: u64,
    /// Wall-clock time of the Lua code.
    pub timeout: Duration,
    /// Memory of the Lua state.
    pub memory_bytes: usize,
    /// Files whose contents recognizers may request (default: 100).
    pub max_files_with_content: usize,
    /// Size of each requested file (default: 1 MiB).
    pub max_file_size: u64,
    /// Rounds of recognizers registering recognizers.
    pub max_rounds: usize,
    /// Inference output kept (`print`).
    pub max_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            instructions: 50_000_000,
            timeout: Duration::from_secs(10),
            memory_bytes: 128 << 20,
            max_files_with_content: 100,
            max_file_size: 1 << 20,
            max_rounds: 16,
            max_output_bytes: 1 << 20,
        }
    }
}

/// Why inference failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InferenceError {
    /// A limit was hit (shown as `limitError`).
    #[error("inference limit: {0}")]
    Limit(String),
    /// The script failed or returned something that is not a recognizer or a job.
    #[error("inference script: {0}")]
    Script(String),
    /// Reading the repository failed.
    #[error("{0}")]
    Repository(String),
}

/// The repository at one commit, as inference reads it.
pub trait RepoFiles {
    /// Every file path (no directories), relative to the root.
    fn paths(&mut self) -> Result<Vec<String>, InferenceError>;
    /// The contents of `paths` (files that are missing are left out). A file larger than
    /// `max_size` is an [`InferenceError::Limit`].
    fn contents(
        &mut self,
        paths: &[String],
        max_size: u64,
    ) -> Result<Vec<(String, String)>, InferenceError>;
}

/// In-memory files, for tests and previews.
impl RepoFiles for BTreeMap<String, String> {
    fn paths(&mut self) -> Result<Vec<String>, InferenceError> {
        Ok(self.keys().cloned().collect())
    }

    fn contents(
        &mut self,
        paths: &[String],
        max_size: u64,
    ) -> Result<Vec<(String, String)>, InferenceError> {
        let mut out = Vec::new();
        for p in paths {
            if let Some(c) = self.get(p) {
                if c.len() as u64 > max_size {
                    return Err(InferenceError::Limit(format!(
                        "requested content for a file larger than {max_size} bytes ({p})"
                    )));
                }
                out.push((p.clone(), c.clone()));
            }
        }
        Ok(out)
    }
}

/// What inference produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InferenceResult {
    pub jobs: Vec<IndexJobSpec>,
    /// What the scripts printed.
    pub output: String,
}

/// Infers index jobs for the files of `files` with the built-in recognizers and, if given,
/// the site's inference script. Runs Lua on the calling thread: call it from a blocking task.
pub fn infer(
    files: &mut dyn RepoFiles,
    script: Option<&str>,
    indexers: &IndexerMap,
    limits: &Limits,
) -> Result<InferenceResult, InferenceError> {
    let sandbox = Sandbox::new(indexers, limits)?;
    let result = sandbox.run(files, script.filter(|s| !s.trim().is_empty()), limits);
    let output = sandbox.shared.output.take();
    result.map(|jobs| InferenceResult { jobs, output })
}

// --- patterns and recognizers ---

/// A tree of path patterns (`sg.autoindex.patterns`).
#[derive(Debug, Clone, Default)]
struct PathPattern {
    glob: Option<String>,
    children: Vec<PathPattern>,
    /// Paths matching this pattern's globs are removed.
    exclude: bool,
}

impl UserData for PathPattern {}

impl PathPattern {
    /// Adds the globs to `include` or (below an exclusion) `exclude`.
    fn flatten(&self, excluded: bool, include: &mut Vec<String>, exclude: &mut Vec<String>) {
        let excluded = excluded || self.exclude;
        if let Some(g) = &self.glob {
            if excluded {
                exclude.push(g.clone());
            } else {
                include.push(g.clone());
            }
        }
        for c in &self.children {
            c.flatten(excluded, include, exclude);
        }
    }
}

/// Compiled path patterns: a path matches if an include glob does and no exclude glob does.
#[derive(Debug, Default)]
struct Matcher {
    include: Vec<PathGlob>,
    exclude: Vec<PathGlob>,
}

impl Matcher {
    fn new(patterns: &[PathPattern]) -> Self {
        let (mut include, mut exclude) = (Vec::new(), Vec::new());
        for p in patterns {
            p.flatten(false, &mut include, &mut exclude);
        }
        let compile = |v: Vec<String>| {
            let set: BTreeSet<String> = v.into_iter().collect();
            set.iter().filter_map(|g| PathGlob::new(g)).collect()
        };
        Self {
            include: compile(include),
            exclude: compile(exclude),
        }
    }

    fn is_match(&self, path: &str) -> bool {
        self.include.iter().any(|g| g.is_match(path))
            && !self.exclude.iter().any(|g| g.is_match(path))
    }
}

/// A recognizer (`sg.autoindex.recognizer`).
#[derive(Clone)]
enum Recognizer {
    Path {
        patterns: Vec<PathPattern>,
        content_patterns: Vec<PathPattern>,
        /// `None` for recognizers that only give hints, which inference skips.
        generate: Option<Function>,
    },
    Fallback(Vec<Recognizer>),
}

impl UserData for Recognizer {}

impl Recognizer {
    /// The path recognizers to try, in fallback order.
    fn linearize<'a>(&'a self, out: &mut Vec<&'a Recognizer>) {
        match self {
            Self::Path { .. } => out.push(self),
            Self::Fallback(children) => {
                for c in children {
                    c.linearize(out);
                }
            }
        }
    }

    fn content_patterns(&self, out: &mut Vec<PathPattern>) {
        match self {
            Self::Path {
                content_patterns, ..
            } => out.extend(content_patterns.iter().cloned()),
            Self::Fallback(children) => {
                for c in children {
                    c.content_patterns(out);
                }
            }
        }
    }
}

/// The `api` argument of `generate`.
struct RegistrationApi(Rc<Shared>);

impl UserData for RegistrationApi {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        for name in ["register", "Register"] {
            methods.add_method(name, |_, this, ud: AnyUserData| {
                let r = ud
                    .borrow::<Recognizer>()
                    .map_err(|_| mlua::Error::runtime("api:register expects a recognizer"))?
                    .clone();
                this.0.registered.borrow_mut().push(r);
                Ok(())
            });
        }
    }
}

// --- the sandbox ---

/// State shared between the driver and the Lua callbacks.
struct Shared {
    output: RefCell<String>,
    max_output: usize,
    registered: RefCell<Vec<Recognizer>>,
    instructions: Cell<u64>,
    /// The limit that stopped the script, if one did.
    tripped: RefCell<Option<String>>,
}

impl Shared {
    fn print(&self, line: &str) {
        let mut out = self.output.borrow_mut();
        if out.len() + line.len() + 1 > self.max_output {
            return;
        }
        out.push_str(line);
        out.push('\n');
    }
}

struct Sandbox {
    lua: Lua,
    shared: Rc<Shared>,
    limits: Limits,
    loaded: Table,
}

const HOOK_EVERY: u32 = 1000;
/// Registry key of the loaded modules.
const LOADED: &str = "sourcegull.autoindex.loaded";

impl Sandbox {
    fn new(indexers: &IndexerMap, limits: &Limits) -> Result<Self, InferenceError> {
        let lua = Lua::new_with(
            StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8,
            LuaOptions::new(),
        )
        .map_err(internal)?;
        let shared = Rc::new(Shared {
            output: RefCell::default(),
            max_output: limits.max_output_bytes,
            registered: RefCell::default(),
            instructions: Cell::new(0),
            tripped: RefCell::new(None),
        });
        let loaded = lua.create_table().map_err(internal)?;
        let sandbox = Self {
            lua,
            shared,
            limits: limits.clone(),
            loaded,
        };
        sandbox.install(indexers).map_err(internal)?;
        Ok(sandbox)
    }

    fn install(&self, indexers: &IndexerMap) -> mlua::Result<()> {
        let lua = &self.lua;
        let g = lua.globals();
        for name in ["load", "loadfile", "dofile", "collectgarbage", "loadstring"] {
            g.raw_remove(name)?;
        }
        if let Ok(string) = g.get::<Table>("string") {
            string.raw_remove("dump")?;
        }
        // Scripts written for Lua 5.1 call `unpack`.
        let unpack: Value = g.get::<Table>("table")?.get("unpack")?;
        g.raw_set("unpack", unpack)?;

        // Closures stored in Lua look Lua values up instead of capturing them, so the state
        // holds no references to itself.
        let shared = self.shared.clone();
        g.raw_set(
            "print",
            lua.create_function(move |lua, args: Variadic<Value>| {
                let tostring: Function = lua.globals().get("tostring")?;
                let mut parts = Vec::with_capacity(args.len());
                for a in args {
                    parts.push(tostring.call::<String>(a)?);
                }
                shared.print(&parts.join("\t"));
                Ok(())
            })?,
        )?;

        lua.set_named_registry_value(LOADED, self.loaded.clone())?;
        g.raw_set(
            "require",
            lua.create_function(move |lua, name: String| {
                let loaded: Table = lua.named_registry_value(LOADED)?;
                let v: Value = loaded.raw_get(name.as_str())?;
                if !v.is_nil() {
                    return Ok(v);
                }
                let Some((_, src)) = LUA_MODULES.iter().find(|(n, _)| *n == name) else {
                    return Err(mlua::Error::runtime(format!("module '{name}' not found")));
                };
                let v: Value = lua
                    .load(*src)
                    .set_name(format!("@{name}"))
                    .set_mode(ChunkMode::Text)
                    .eval()?;
                let v = if v.is_nil() { Value::Boolean(true) } else { v };
                loaded.raw_set(name.as_str(), v.clone())?;
                Ok(v)
            })?,
        )?;

        self.loaded.raw_set("path", path_module(lua)?)?;
        self.loaded.raw_set("json", json_module(lua)?)?;
        let patterns = patterns_module(lua)?;
        self.loaded
            .raw_set("sg.autoindex.patterns", patterns.clone())?;
        self.loaded.raw_set("internal_patterns", patterns)?;
        self.loaded
            .raw_set("sg.autoindex.recognizer", recognizer_module(lua)?)?;
        self.loaded
            .raw_set("sg.autoindex.indexes", indexes_module(lua, indexers)?)?;

        lua.set_memory_limit(self.limits.memory_bytes)?;
        let shared = self.shared.clone();
        let budget = self.limits.instructions;
        let deadline = Instant::now() + self.limits.timeout;
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(HOOK_EVERY),
            move |lua, _| {
                if let Some(msg) = shared.tripped.borrow().clone() {
                    return Err(mlua::Error::runtime(msg));
                }
                let n = shared.instructions.get() + u64::from(HOOK_EVERY);
                shared.instructions.set(n);
                let msg = if n > budget {
                    format!("the script ran more than {budget} instructions")
                } else if Instant::now() > deadline {
                    "the script ran too long".to_owned()
                } else {
                    return Ok(VmState::Continue);
                };
                *shared.tripped.borrow_mut() = Some(msg.clone());
                // From now on every instruction fails, so a `pcall` cannot swallow the limit
                // and carry on.
                let again = shared.clone();
                lua.set_hook(HookTriggers::new().every_nth_instruction(1), move |_, _| {
                    let msg = again.tripped.borrow().clone().unwrap_or_default();
                    Err(mlua::Error::runtime(msg))
                })?;
                Err(mlua::Error::runtime(msg))
            },
        )?;
        Ok(())
    }

    /// Maps a Lua error to a limit (if one tripped) or a script error.
    fn error(&self, e: &mlua::Error) -> InferenceError {
        if let Some(msg) = self.shared.tripped.borrow().clone() {
            return InferenceError::Limit(msg);
        }
        if is_memory_error(e) {
            return InferenceError::Limit(format!(
                "the script used more than {} MiB of memory",
                self.limits.memory_bytes >> 20
            ));
        }
        InferenceError::Script(e.to_string())
    }

    fn require(&self, name: &str) -> Result<Value, InferenceError> {
        let require: Function = self.lua.globals().get("require").map_err(internal)?;
        require.call(name).map_err(|e| self.error(&e))
    }

    fn run(
        &self,
        files: &mut dyn RepoFiles,
        script: Option<&str>,
        limits: &Limits,
    ) -> Result<Vec<IndexJobSpec>, InferenceError> {
        let mut named: BTreeMap<String, Recognizer> = BTreeMap::new();
        for name in BUILTIN_RECOGNIZERS {
            let v = self.require(&format!("sg.autoindex.{name}"))?;
            let r = recognizer_from(&v)
                .map_err(|e| InferenceError::Script(format!("built-in recognizer {name}: {e}")))?;
            named.insert(format!("sg.{name}"), r);
        }
        if let Some(script) = script {
            let v: Value = self
                .lua
                .load(script)
                .set_name("=inference script")
                .set_mode(ChunkMode::Text)
                .eval()
                .map_err(|e| self.error(&e))?;
            let Value::Table(t) = v else {
                return Err(InferenceError::Script(
                    "the script must return a table of recognizers \
                     (require(\"sg.autoindex.config\").new({...}))"
                        .into(),
                ));
            };
            for pair in t.pairs::<Value, Value>() {
                let (k, v) = pair.map_err(|e| self.error(&e))?;
                let Value::String(k) = k else {
                    continue;
                };
                let k = k.to_string_lossy();
                if k == "__type" {
                    continue;
                }
                if matches!(v, Value::Boolean(false)) {
                    named.remove(&k);
                    continue;
                }
                let r = recognizer_from(&v)
                    .map_err(|e| InferenceError::Script(format!("recognizer {k:?}: {e}")))?;
                named.insert(k, r);
            }
        }

        let paths = files.paths()?;
        let mut contents: BTreeMap<String, String> = BTreeMap::new();
        let mut requested: BTreeSet<String> = BTreeSet::new();
        let mut jobs = Vec::new();
        let mut round: Vec<Recognizer> = named.into_values().collect();
        for _ in 0..limits.max_rounds {
            if round.is_empty() {
                return Ok(jobs);
            }
            // The contents this round's recognizers ask for, fetched at once.
            let mut wanted = Vec::new();
            for r in &round {
                r.content_patterns(&mut wanted);
            }
            let m = Matcher::new(&wanted);
            let need: Vec<String> = paths
                .iter()
                .filter(|p| !requested.contains(*p) && m.is_match(p))
                .cloned()
                .collect();
            if requested.len() + need.len() > limits.max_files_with_content {
                return Err(InferenceError::Limit(format!(
                    "requested content for more than {} files ({})",
                    limits.max_files_with_content,
                    requested.len() + need.len()
                )));
            }
            if !need.is_empty() {
                requested.extend(need.iter().cloned());
                contents.extend(files.contents(&need, limits.max_file_size)?);
            }
            for r in &round {
                let mut chain = Vec::new();
                r.linearize(&mut chain);
                for leaf in chain {
                    let found = self.invoke(leaf, &paths, &contents)?;
                    if !found.is_empty() {
                        jobs.extend(found);
                        break;
                    }
                }
            }
            round = self.shared.registered.take();
        }
        if round.is_empty() {
            Ok(jobs)
        } else {
            Err(InferenceError::Limit(format!(
                "recognizers registered recognizers more than {} rounds deep",
                limits.max_rounds
            )))
        }
    }

    /// Calls one path recognizer with its paths and contents.
    fn invoke(
        &self,
        r: &Recognizer,
        paths: &[String],
        contents: &BTreeMap<String, String>,
    ) -> Result<Vec<IndexJobSpec>, InferenceError> {
        let Recognizer::Path {
            patterns,
            content_patterns,
            generate: Some(generate),
        } = r
        else {
            return Ok(Vec::new());
        };
        let pm = Matcher::new(patterns);
        let cm = Matcher::new(content_patterns);
        let mine: Vec<&String> = paths.iter().filter(|p| pm.is_match(p)).collect();
        let mine_contents: Vec<(&String, &String)> =
            contents.iter().filter(|(p, _)| cm.is_match(p)).collect();
        if mine.is_empty() && mine_contents.is_empty() {
            return Ok(Vec::new());
        }
        let call = || -> mlua::Result<Value> {
            let api = self
                .lua
                .create_userdata(RegistrationApi(self.shared.clone()))?;
            let paths = self
                .lua
                .create_sequence_from(mine.iter().map(|p| p.as_str()))?;
            let contents = self.lua.create_table()?;
            for (p, c) in &mine_contents {
                contents.raw_set(p.as_str(), c.as_str())?;
            }
            generate.call((api, paths, contents))
        };
        let v = call().map_err(|e| self.error(&e))?;
        jobs_from(&v).map_err(InferenceError::Script)
    }
}

fn internal(e: mlua::Error) -> InferenceError {
    InferenceError::Script(format!("sandbox: {e}"))
}

fn is_memory_error(e: &mlua::Error) -> bool {
    match e {
        mlua::Error::MemoryError(_) => true,
        mlua::Error::CallbackError { cause, .. } => is_memory_error(cause),
        mlua::Error::WithContext { cause, .. } => is_memory_error(cause),
        _ => e.to_string().contains("not enough memory"),
    }
}

// --- conversions ---

fn recognizer_from(v: &Value) -> Result<Recognizer, String> {
    match v {
        Value::UserData(ud) => ud
            .borrow::<Recognizer>()
            .map(|r| r.clone())
            .map_err(|_| "expected a recognizer".to_owned()),
        _ => Err(format!("expected a recognizer, got {}", v.type_name())),
    }
}

/// Patterns from a pattern or a (nested) list of them.
fn patterns_from(v: &Value, out: &mut Vec<PathPattern>) -> mlua::Result<()> {
    match v {
        Value::UserData(ud) => {
            let p = ud
                .borrow::<PathPattern>()
                .map_err(|_| mlua::Error::runtime("expected a path pattern"))?;
            out.push(p.clone());
            Ok(())
        }
        Value::Table(t) => {
            for item in t.sequence_values::<Value>() {
                patterns_from(&item?, out)?;
            }
            Ok(())
        }
        other => Err(mlua::Error::runtime(format!(
            "expected a path pattern, got {}",
            other.type_name()
        ))),
    }
}

fn string_list(v: &Value, field: &str) -> Result<Vec<String>, String> {
    let Value::Table(t) = v else {
        return Err(format!("{field}: expected a list of strings"));
    };
    let mut out = Vec::new();
    for item in t.sequence_values::<Value>() {
        out.push(match item.map_err(|e| e.to_string())? {
            Value::String(s) => s.to_string_lossy(),
            Value::Integer(i) => i.to_string(),
            Value::Number(n) => n.to_string(),
            other => {
                return Err(format!(
                    "{field}: expected strings, got {}",
                    other.type_name()
                ));
            }
        });
    }
    Ok(out)
}

fn string_field(v: &Value, field: &str) -> Result<String, String> {
    match v {
        Value::String(s) => Ok(s.to_string_lossy()),
        Value::Nil => Ok(String::new()),
        other => Err(format!(
            "{field}: expected a string, got {}",
            other.type_name()
        )),
    }
}

/// Index jobs from a `generate` result: nothing, one job, or a list of jobs.
fn jobs_from(v: &Value) -> Result<Vec<IndexJobSpec>, String> {
    let Value::Table(t) = v else {
        return match v {
            Value::Nil => Ok(Vec::new()),
            other => Err(format!(
                "generate must return index jobs, got {}",
                other.type_name()
            )),
        };
    };
    if t.raw_len() > 0 {
        let mut out = Vec::new();
        for item in t.sequence_values::<Value>() {
            let item = item.map_err(|e| e.to_string())?;
            let Value::Table(job) = item else {
                return Err("generate must return index jobs (tables)".into());
            };
            out.push(job_from(&job)?);
        }
        return Ok(out);
    }
    if t.pairs::<Value, Value>().next().is_none() {
        return Ok(Vec::new());
    }
    Ok(vec![job_from(t)?])
}

fn job_from(t: &Table) -> Result<IndexJobSpec, String> {
    let mut job = IndexJobSpec::default();
    for pair in t.pairs::<String, Value>() {
        let (k, v) = pair.map_err(|e| format!("index job: {e}"))?;
        match k.as_str() {
            "steps" => {
                let Value::Table(steps) = &v else {
                    return Err("steps: expected a list of steps".into());
                };
                for s in steps.sequence_values::<Table>() {
                    job.steps.push(step_from(&s.map_err(|e| e.to_string())?)?);
                }
            }
            "local_steps" => job.local_steps = string_list(&v, &k)?,
            "root" => job.root = string_field(&v, &k)?,
            "indexer" => job.indexer = string_field(&v, &k)?,
            "indexer_args" => job.indexer_args = string_list(&v, &k)?,
            "outfile" => job.outfile = string_field(&v, &k)?,
            "requested_envvars" => job.requested_env_vars = string_list(&v, &k)?,
            other => return Err(format!("index job: unknown field {other:?}")),
        }
    }
    if job.indexer.is_empty() {
        return Err("index job: no indexer supplied".into());
    }
    Ok(job)
}

fn step_from(t: &Table) -> Result<DockerStep, String> {
    let mut step = DockerStep::default();
    for pair in t.pairs::<String, Value>() {
        let (k, v) = pair.map_err(|e| format!("step: {e}"))?;
        match k.as_str() {
            "root" => step.root = string_field(&v, &k)?,
            "image" => step.image = string_field(&v, &k)?,
            "commands" => step.commands = string_list(&v, &k)?,
            other => return Err(format!("step: unknown field {other:?}")),
        }
    }
    if step.image.is_empty() {
        return Err("step: no image supplied".into());
    }
    Ok(step)
}

// --- modules ---

/// `path.dirname`: `""` for files at the root.
pub fn dirname(path: &str) -> &str {
    let path = path.trim_end_matches('/');
    match path.rfind('/') {
        Some(i) => path[..i].trim_start_matches('/'),
        None => "",
    }
}

/// `path.ancestors`: the directories above `path`, nearest first, ending with `""`.
pub fn ancestors(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut dir = dirname(path);
    while !dir.is_empty() {
        out.push(dir.to_owned());
        dir = dirname(dir);
    }
    out.push(String::new());
    out
}

fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return if path.is_empty() { "." } else { "/" };
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// `path.join`: joins and cleans (`.` and empty segments dropped, `..` resolved).
fn join(a: &str, b: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in a.split('/').chain(b.split('/')) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn path_module(lua: &Lua) -> mlua::Result<Table> {
    let m = lua.create_table()?;
    m.raw_set(
        "ancestors",
        lua.create_function(|lua, p: String| lua.create_sequence_from(ancestors(&p)))?,
    )?;
    m.raw_set(
        "basename",
        lua.create_function(|_, p: String| Ok(basename(&p).to_owned()))?,
    )?;
    m.raw_set(
        "dirname",
        lua.create_function(|_, p: String| Ok(dirname(&p).to_owned()))?,
    )?;
    m.raw_set(
        "join",
        lua.create_function(|_, (a, b): (String, String)| Ok(join(&a, &b)))?,
    )?;
    m.raw_set(
        "split",
        lua.create_function(|_, p: String| Ok((dirname(&p).to_owned(), basename(&p).to_owned())))?,
    )?;
    Ok(m)
}

fn json_to_lua(lua: &Lua, v: &serde_json::Value) -> mlua::Result<Value> {
    use serde_json::Value as J;
    Ok(match v {
        J::Null => Value::Nil,
        J::Bool(b) => Value::Boolean(*b),
        J::Number(n) => match n.as_i64() {
            Some(i) => Value::Integer(i),
            None => Value::Number(n.as_f64().unwrap_or(0.0)),
        },
        J::String(s) => Value::String(lua.create_string(s)?),
        J::Array(a) => {
            let t = lua.create_table_with_capacity(a.len(), 0)?;
            for (i, item) in a.iter().enumerate() {
                t.raw_set(i + 1, json_to_lua(lua, item)?)?;
            }
            Value::Table(t)
        }
        J::Object(o) => {
            let t = lua.create_table_with_capacity(0, o.len())?;
            for (k, item) in o {
                t.raw_set(k.as_str(), json_to_lua(lua, item)?)?;
            }
            Value::Table(t)
        }
    })
}

fn lua_to_json(v: &Value, depth: usize) -> mlua::Result<serde_json::Value> {
    use serde_json::Value as J;
    if depth > 64 {
        return Err(mlua::Error::runtime("json.encode: value nested too deeply"));
    }
    Ok(match v {
        Value::Nil => J::Null,
        Value::Boolean(b) => J::Bool(*b),
        Value::Integer(i) => J::from(*i),
        Value::Number(n) => serde_json::Number::from_f64(*n).map_or(J::Null, J::Number),
        Value::String(s) => J::String(s.to_string_lossy()),
        Value::Table(t) => {
            if t.raw_len() > 0 {
                let mut a = Vec::new();
                for item in t.sequence_values::<Value>() {
                    a.push(lua_to_json(&item?, depth + 1)?);
                }
                J::Array(a)
            } else {
                let mut o = serde_json::Map::new();
                for pair in t.pairs::<String, Value>() {
                    let (k, item) = pair?;
                    o.insert(k, lua_to_json(&item, depth + 1)?);
                }
                J::Object(o)
            }
        }
        other => {
            return Err(mlua::Error::runtime(format!(
                "json.encode: cannot encode a {}",
                other.type_name()
            )));
        }
    })
}

fn json_module(lua: &Lua) -> mlua::Result<Table> {
    let m = lua.create_table()?;
    m.raw_set(
        "decode",
        lua.create_function(|lua, s: String| {
            let v: serde_json::Value = serde_json::from_str(&s)
                .map_err(|e| mlua::Error::runtime(format!("json.decode: {e}")))?;
            json_to_lua(lua, &v)
        })?,
    )?;
    m.raw_set(
        "encode",
        lua.create_function(|_, v: Value| Ok(lua_to_json(&v, 0)?.to_string()))?,
    )?;
    Ok(m)
}

fn new_pattern(lua: &Lua, glob: String) -> mlua::Result<AnyUserData> {
    if PathGlob::new(&glob).is_none() {
        return Err(mlua::Error::runtime(format!(
            "invalid path pattern {glob:?}"
        )));
    }
    lua.create_userdata(PathPattern {
        glob: Some(glob),
        ..Default::default()
    })
}

fn patterns_module(lua: &Lua) -> mlua::Result<Table> {
    let m = lua.create_table()?;
    // `/BUILD.bazel`: the file at the root.
    m.raw_set(
        "new_path_literal",
        lua.create_function(|lua, s: String| new_pattern(lua, format!("/{s}")))?,
    )?;
    // `web/`: everything below a directory of that name.
    m.raw_set(
        "new_path_segment",
        lua.create_function(|lua, s: String| new_pattern(lua, format!("{s}/")))?,
    )?;
    // `go.mod`: files of that name at any depth.
    m.raw_set(
        "new_path_basename",
        lua.create_function(|lua, s: String| new_pattern(lua, s))?,
    )?;
    // `*.go`.
    m.raw_set(
        "new_path_extension",
        lua.create_function(|lua, s: String| new_pattern(lua, format!("*.{s}")))?,
    )?;
    m.raw_set(
        "new_path_glob",
        lua.create_function(|lua, s: String| new_pattern(lua, s))?,
    )?;
    // `backdoor(glob, pathspecs)`; pathspecs are not needed here.
    m.raw_set(
        "backdoor",
        lua.create_function(|lua, (s, _): (String, Value)| new_pattern(lua, s))?,
    )?;
    let combine = |exclude: bool| {
        move |lua: &Lua, args: Variadic<Value>| {
            let mut children = Vec::new();
            for a in args.iter() {
                patterns_from(a, &mut children)?;
            }
            lua.create_userdata(PathPattern {
                glob: None,
                children,
                exclude,
            })
        }
    };
    m.raw_set("new_path_combine", lua.create_function(combine(false))?)?;
    m.raw_set("new_path_exclude", lua.create_function(combine(true))?)?;
    m.raw_set("path_combine", lua.create_function(combine(false))?)?;
    m.raw_set("path_exclude", lua.create_function(combine(true))?)?;
    Ok(m)
}

fn recognizer_module(lua: &Lua) -> mlua::Result<Table> {
    let m = lua.create_table()?;
    m.raw_set(
        "new_path_recognizer",
        lua.create_function(|lua, t: Table| {
            let (mut patterns, mut content_patterns) = (Vec::new(), Vec::new());
            let (mut generate, mut hints) = (None, false);
            for pair in t.pairs::<String, Value>() {
                let (k, v) = pair?;
                match k.as_str() {
                    "patterns" => patterns_from(&v, &mut patterns)?,
                    "patterns_for_content" => patterns_from(&v, &mut content_patterns)?,
                    "generate" => match v {
                        Value::Function(f) => generate = Some(f),
                        _ => return Err(mlua::Error::runtime("generate must be a function")),
                    },
                    "hints" => hints = matches!(v, Value::Function(_)),
                    other => {
                        return Err(mlua::Error::runtime(format!(
                            "unknown recognizer field {other:?}"
                        )));
                    }
                }
            }
            if generate.is_none() && !hints {
                return Err(mlua::Error::runtime(
                    "no generate function supplied for the recognizer",
                ));
            }
            if patterns.is_empty() && content_patterns.is_empty() {
                return Err(mlua::Error::runtime(
                    "no patterns supplied for the recognizer",
                ));
            }
            lua.create_userdata(Recognizer::Path {
                patterns,
                content_patterns,
                generate,
            })
        })?,
    )?;
    m.raw_set(
        "new_fallback_recognizer",
        lua.create_function(|lua, t: Table| {
            let mut children = Vec::new();
            for item in t.sequence_values::<Value>() {
                children.push(recognizer_from(&item?).map_err(mlua::Error::runtime)?);
            }
            lua.create_userdata(Recognizer::Fallback(children))
        })?,
    )?;
    Ok(m)
}

fn indexes_module(lua: &Lua, indexers: &IndexerMap) -> mlua::Result<Table> {
    let m = lua.create_table()?;
    let indexers = indexers.clone();
    m.raw_set(
        "get",
        lua.create_function(move |_, language: String| {
            indexers.get(&language).map(str::to_owned).ok_or_else(|| {
                mlua::Error::runtime(format!("no indexer is registered for {language:?}"))
            })
        })?,
    )?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_helpers() {
        assert_eq!(dirname("main.go"), "");
        assert_eq!(dirname("foo/bar/go.mod"), "foo/bar");
        assert_eq!(dirname("/a"), "");
        assert_eq!(ancestors("foo/bar/tsconfig.json"), ["foo/bar", "foo", ""]);
        assert_eq!(ancestors(""), [""]);
        assert_eq!(basename("a/b.txt"), "b.txt");
        assert_eq!(join("", "package.json"), "package.json");
        assert_eq!(join("a/", "./b"), "a/b");
        assert_eq!(join("a/b", "../c"), "a/c");
    }
}
