use native_runner::{CommandRunner, TokioRunner};
use serde::Deserialize;
use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Instant;
use tokio::process::Command;
use tokio::{
    io::{self, AsyncBufReadExt},
    time,
};
use wasi_process2::WasiProcess;
use wasmer_cache::{Cache, FileSystemCache};
use wasmer_wasi::WasiVersion;

use logic::{GameMode, MainOutput, RobotRunner};

use anyhow::{anyhow, bail, Context};
use itertools::Itertools;
use once_cell::sync::{Lazy, OnceCell};
use structopt::StructOpt;

mod api;
mod display;
mod server;

#[tokio::main]
async fn main() {
    env_logger::init();
    let _sentry = option_env!("SENTRY_DSN").map(sentry::init);

    if let Err(err) = try_main().await {
        eprintln!("ERROR: {}", err);
        err.chain()
            .skip(1)
            .for_each(|cause| eprintln!("because: {}", cause));
        std::process::exit(1);
    }
}

#[derive(StructOpt)]
#[structopt(name = "Robot Runner CLI", author, setting = clap::AppSettings::DeriveDisplayOrder)]
enum Rumblebot {
    /// Commands for running battles locally
    Run(Run),
    /// Commands for interacting with robotrumble.org
    Account(Account),
}

#[derive(StructOpt)]
#[structopt(setting = clap::AppSettings::DeriveDisplayOrder)]
enum Run {
    /// Run a battle and print the results in the terminal
    ///
    /// A robot is specified in one of the following ways:
    /// 1. `$USER/$ROBOT`. A robot published on robotrumble.org
    /// 2. `$PATH`. A path to a local file with robot code. It must have a file extension for one of the supported languages.
    /// 3. `inline:$LANG;$CODE`. Pass the language and code directly as an argument.
    /// 4. `command:$PATH` or `localrunner:$PATH`. The path to a native binary or wasm file, respectively. Criteria:
    ///     After initialization, it must print a `Result<(), ProgramError>` in serde_json format and a newline.
    ///     It will then start receiving newline-delimited `ProgramInput` json object. It must respond to
    ///     each one with a `ProgramOutput` json object followed by a newline. The match is over when stdin is closed, and
    ///     the process may be forcefully terminated after that.
    #[structopt(verbatim_doc_comment)]
    Term {
        #[structopt(parse(from_os_str))]
        bluebot: OsString,
        #[structopt(parse(from_os_str))]
        redbot: OsString,
        /// The number of turns to run in the match
        #[structopt(short, long, default_value = "100")]
        turn_num: usize,
        /// Avoid printing human-friendly info and just output JSON
        #[structopt(long)]
        raw: bool,
        /// Show only the blue robot's logs
        #[structopt(long)]
        blue_logs_only: bool,
        /// Show only the red robot's logs
        #[structopt(long)]
        red_logs_only: bool,
        /// Only show the results of the battle
        #[structopt(long)]
        results_only: bool,
        /// Choose the gamemode. Current supported: "Normal"
        #[structopt(long, parse(from_os_str))]
        game_mode: Option<OsString>,
        /// Specify a random seed for robot spawning. It can be of any length.
        #[structopt(long, parse(from_os_str))]
        seed: Option<OsString>,
    },
    /// Run a continuous series of games 
    ///
    /// Like `term`, but allows for running an indefinite number of games. This saves time because
    /// this means that there is no need to initialize rumblebot from scratch for every game.
    /// Expects inputs of the form `{"red": "...", "blue": "...", "seed": "(optional)", "turn_num": (optional) }`.
    /// For each input, `batch` simulates the game, prints the winner, and then waits for the next
    /// input. 
    ///
    /// For instructions on how to specify robots, see the help page for `run`.
    Batch {
        #[structopt(long, parse(from_os_str))]
        game_mode: Option<OsString>,
    },
    /// Run a battle and show the results in the normal web display
    ///
    /// For instructions on how to specify robots, see the help page for `run`.
    Web {
        /// The robots to make available to the web display. The first one will be treated as the main robot,
        /// and the rest will be available for choosing from the UI.
        #[structopt(parse(from_os_str), required = true, min_values = 2)]
        robots: Vec<OsString>,
        /// The network address to listen to.
        #[structopt(short, long, default_value = "127.0.0.1")]
        address: String,
        /// The network port to listen to.
        #[structopt(short, long, env = "PORT")]
        port: Option<u16>,
    },
}

#[derive(StructOpt)]
#[structopt(setting = clap::AppSettings::DeriveDisplayOrder)]
enum Account {
    /// Login to robotrumble.org. This allows you to use the rumblebot account commands
    Login {
        username: String,
        #[structopt(short)]
        password: Option<String>,
    },
    Logout {},
    /// Create a new robot. By default, `name` and `lang` are inferred from the file path
    Create {
        #[structopt(parse(from_os_str))]
        file: PathBuf,
        #[structopt(long, short)]
        name: Option<String>,
        #[structopt(long, short)]
        lang: Option<Lang>,
    },
    /// Update a robot's code. By default, `name` is inferred from the file path
    Update {
        #[structopt(parse(from_os_str))]
        file: PathBuf,
        #[structopt(long, short)]
        name: Option<String>,
    },
    /// Download any published robot from robotrumble.org
    Download {
        /// Should take the form `$USER/$ROBOT`.
        slug: String,
        dest: Option<PathBuf>,
    },
}

fn make_sourcedir(f: impl AsRef<Path>) -> anyhow::Result<tempfile::TempDir> {
    let f = f.as_ref();
    let sourcedir = tempfile::tempdir().context("couldn't create temporary directory")?;
    let sourcecode_path = sourcedir.path().join("sourcecode");
    fs::hard_link(f, &sourcecode_path)
        .or_else(|_| fs::copy(f, sourcecode_path).map(drop))
        .context("couldn't copy file to tempdir")?;
    Ok(sourcedir)
}
fn make_sourcedir_inline(source: &str) -> anyhow::Result<tempfile::TempDir> {
    let sourcedir = tempfile::tempdir().context("couldn't create temporary directory")?;
    fs::write(sourcedir.path().join("sourcecode"), source)
        .context("Couldn't write code to disk")?;
    Ok(sourcedir)
}

type WasiRunner =
    TokioRunner<io::BufWriter<wasi_process2::WasiStdin>, io::BufReader<wasi_process2::WasiStdout>>;

enum RunnerKind {
    Command(CommandRunner),
    Wasi {
        runner: WasiRunner,
        /// the directory that we store the source file in; we need to keep it open
        _dir: tempfile::TempDir,
        memory: wasmer::Memory,
    },
}

pub struct Runner {
    kind: RunnerKind,
    timeout: Option<(Pin<Box<time::Sleep>>, time::Duration)>,
}

#[async_trait::async_trait]
impl RobotRunner for Runner {
    async fn run(&mut self, input: logic::ProgramInput<'_>) -> logic::ProgramResult {
        let kind = &mut self.kind;
        let inner = async move {
            match kind {
                RunnerKind::Command(r) => r.run(input).await,
                RunnerKind::Wasi { runner, memory, .. } => {
                    log::debug!(
                        "start of turn {} w/ {} units: {:?} allocated",
                        input.state.turn,
                        input.state.objs.len(),
                        memory.size()
                    );
                    runner.run(input).await
                }
            }
        };
        match &mut self.timeout {
            Some((timeout, dur)) => {
                tokio::select! {
                    res = inner => res,
                    _ = timeout => Err(logic::ProgramError::Timeout(*dur)),
                }
            }
            None => inner.await,
        }
    }
}

impl Runner {
    fn set_timeout(&mut self, dur: time::Duration) {
        let instant = time::Instant::now() + dur;
        self.timeout = Some((Box::pin(time::sleep_until(instant)), dur));
    }

    async fn new_wasm(
        store: &wasmer::Store,
        module: &wasmer::Module,
        version: WasiVersion,
        args: &[String],
        dir: tempfile::TempDir,
    ) -> anyhow::Result<logic::ProgramResult<Self>> {
        let mut state = wasmer_wasi::WasiState::new("robot");
        wasi_process2::add_stdio(&mut state);
        state
            .preopen(|p| p.directory(&dir).alias("source").read(true))
            .unwrap()
            .args(args)
            .arg("/source/sourcecode");
        let env = wasmer_wasi::WasiEnv::new(state.build()?);
        let instance = {
            // imports isn't Send
            let imports = wasmer_wasi::generate_import_object_from_env(store, env, version);
            wasmer::Instance::new(module, &imports)?
        };
        let memory = instance.exports.get::<wasmer::Memory>("memory").unwrap();
        let mut proc = WasiProcess::new(&instance, Default::default())?;

        let stdin = io::BufWriter::new(proc.stdin.take().unwrap());
        let stdout = io::BufReader::new(proc.stdout.take().unwrap());

        // forward wasi stderr to io::stderr
        let mut proc_stderr = io::BufReader::new(proc.stderr.take().unwrap());
        let mut stderr = tokio::io::stderr();
        tokio::spawn(async move { tokio::io::copy(&mut proc_stderr, &mut stderr).await });

        proc.spawn();

        let program_result = TokioRunner::new(stdin, stdout).await.map(|runner| Self {
            kind: RunnerKind::Wasi {
                runner,
                _dir: dir,
                memory: memory.clone(),
            },
            timeout: None,
        });
        Ok(program_result)
    }
    async fn from_id(id: &RobotId) -> anyhow::Result<logic::ProgramResult<Self>> {
        match id {
            RobotId::Published { user, robot } => {
                let info = api::robot_info(user, robot)
                    .await?
                    .ok_or_else(|| anyhow!("robot {}/{} not found", user, robot))?;
                let code = api::robot_code(info.id).await?.ok_or_else(|| {
                    anyhow!("robot {}/{} has no open source published code", user, robot)
                })?;
                let sourcedir = make_sourcedir_inline(&code)?;
                let store = &*STORE;
                let (module, version) = info.lang.get_wasm(store)?;
                Runner::new_wasm(store, module, version, &[], sourcedir).await
            }
            RobotId::Local { source, lang } => {
                let sourcedir = make_sourcedir(source)?;
                let store = &*STORE;
                let (module, version) = lang.get_wasm(store)?;
                Runner::new_wasm(store, module, version, &[], sourcedir).await
            }
            RobotId::Command { command, args } => {
                let mut cmd = Command::new(command);
                cmd.args(args);
                let program_result = TokioRunner::new_cmd(cmd).await.map(|r| Self {
                    kind: RunnerKind::Command(r),
                    timeout: None,
                });
                Ok(program_result)
            }
            RobotId::LocalRunner {
                runner,
                runner_args,
                source,
            } => {
                let sourcedir = make_sourcedir(source)?;
                let wasm = tokio::fs::read(runner)
                    .await
                    .with_context(|| format!("couldn't read {}", runner))?;
                let store = &*STORE;
                let (module, version) = wasm_from_cache_or_compile(store, &wasm)
                    .with_context(|| format!("couldn't compile wasm module at {}", runner))?;
                Runner::new_wasm(store, &module, version, &runner_args, sourcedir).await
            }
            RobotId::Inline { lang, source } => {
                let sourcedir = make_sourcedir_inline(source)?;
                let store = &*STORE;
                let (module, version) = lang.get_wasm(store)?;
                Runner::new_wasm(store, module, version, &[], sourcedir).await
            }
        }
    }
}

static STORE: Lazy<wasmer::Store> = Lazy::new(wasmer::Store::default);

const PROD_BASE_URL: &str = "https://robotrumble.org";

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
struct Config {
    auth_key: Option<String>,
    base_url: Option<Cow<'static, str>>,
}
impl Config {
    fn base_url(&self) -> &str {
        self.base_url.as_deref().unwrap_or(PROD_BASE_URL)
    }
}

static CONFIG: OnceCell<Config> = OnceCell::new();
fn config() -> &'static Config {
    CONFIG.get().unwrap()
}
fn store_config(path: &Path, c: &Config) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let s = toml::to_string_pretty(c)?;
    std::fs::write(path, s)?;
    Ok(())
}

async fn try_main() -> anyhow::Result<()> {
    let opt: Rumblebot = Rumblebot::from_args();
    let config_dir = directories()?.config_dir();
    let config_path = config_dir.join("config.toml");
    CONFIG
        .get_or_try_init(|| match fs::read_to_string(&config_path) {
            Ok(s) => Ok(toml::from_str(&s)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let c = Config::default();
                store_config(&config_path, &c).map(|()| c)
            }
            Err(e) => Err(e.into()),
        })
        .context("Unable to load config")?;

    match opt {
        Rumblebot::Run(run_opt) => match run_opt {
            Run::Term {
                bluebot,
                redbot,
                turn_num,
                raw,
                blue_logs_only,
                red_logs_only,
                results_only,
                game_mode: game_mode_string,
                seed,
            } => {
                let game_mode = init_game_mode(game_mode_string);
                let output = run_game(
                    GameSpec {
                        red: redbot.to_string_lossy().to_string(),
                        blue: bluebot.to_string_lossy().to_string(),
                        seed: seed.map(|k| k.to_string_lossy().to_string()),
                        turn_num: Some(turn_num)
                    },
                    game_mode,
                    !raw && !results_only,
                    red_logs_only,
                    blue_logs_only,
                )
                .await?;
                if raw {
                    let stdout = std::io::stdout();
                    serde_json::to_writer(stdout.lock(), &output).unwrap();
                } else {
                    if !results_only {
                        println!("");
                    }
                    display::display_output(output)?;
                }
            }
            Run::Batch {
                game_mode: game_mode_string,
            } => {
                let game_mode = init_game_mode(game_mode_string);
                let mut stdin = io::BufReader::new(io::stdin()).lines();
                while let Some(line) = stdin.next_line().await.unwrap() {
                    match serde_json::from_str(&line) {
                        Ok(game_spec) => {
                            let out = run_game(game_spec, game_mode, false, false, false).await?;

                            let mut value = serde_json::to_value(&out).unwrap();
                            if let serde_json::Value::Object(v) = &mut value {
                                v.retain(|key, _| ["winner"].contains(&key.as_str()))
                            };
                            println!("{}", serde_json::to_string(&value).unwrap());
                        }
                        Err(_) => {
                            println!("Did not understand batch game specification. To abort, press Ctrl-c");
                        }
                    }
                }
            }
            Run::Web {
                robots,
                address,
                port,
            } => {
                let ids = robots
                    .iter()
                    .map(|id| RobotId::parse(id))
                    .collect::<Result<Vec<_>, _>>()?;
                server::serve(ids, address, port).await?;
            }
        },

        Rumblebot::Account(account_opt) => match account_opt {
            Account::Login { username, password } => {
                let password = match password {
                    Some(pass) => pass,
                    None => rpassword::read_password_from_tty(Some("Password: "))
                        .context("Error reading password (try passing the -p option)")?,
                };
                let auth_key = api::authenticate(&username, &password).await?;
                store_config(
                    &config_path,
                    &Config {
                        auth_key: Some(auth_key),
                        ..config().clone()
                    },
                )
                .context("Error storing configuration with auth_key")?;
                println!("Logged in!")
            }
            Account::Logout {} => {
                store_config(
                    &config_path,
                    &Config {
                        auth_key: None,
                        ..config().clone()
                    },
                )
                .context("Error storing configuration with auth_key")?;
                println!("Logged out!")
            }
            Account::Create { file, name, lang } => {
                let code = fs::read_to_string(&file)
                    .with_context(|| format!("Couldn't read {}", file.display()))?;
                let name = match name {
                    Some(ref n) => n,
                    None => robot_name_from_path(&file)?,
                };
                let lang = match lang {
                    Some(l) => l,
                    None => file.extension().and_then(Lang::from_ext).ok_or_else(|| {
                        anyhow!("Invalid language from extension, try passing the -l option")
                    })?,
                };
                let info = api::create(lang, name).await?;
                api::update_code(info.id, &code).await?;
                println!("Robot {} created!", name)
            }
            Account::Update { file, name } => {
                let code = fs::read_to_string(&file)
                    .with_context(|| format!("Couldn't read {}", file.display()))?;
                let name = match name {
                    Some(ref n) => n,
                    None => robot_name_from_path(&file)?,
                };
                let (user, _) = api::whoami().await?;
                let info = api::robot_info(&user, name).await?.ok_or_else(|| {
                    anyhow!(
                        "No existing robot of yours with name '{}'. try the `create` subcommand instead",
                        name
                    )
                })?;
                api::update_code(info.id, &code).await?;
                println!("Robot {} updated!", name)
            }
            Account::Download { slug, dest } => {
                let (user, robot) = parse_published_slug(&slug)
                    .ok_or_else(|| anyhow!("invalid robot slug '{}'", slug))?;
                let whoami;
                let user = match user {
                    Some(u) => u,
                    None => {
                        whoami = api::whoami().await?.0;
                        &whoami
                    }
                };
                let info = api::robot_info(user, robot)
                    .await?
                    .ok_or_else(|| anyhow!("robot {} not found", robot))?;
                let code = api::robot_code(info.id)
                    .await?
                    .ok_or_else(|| anyhow!("robot {} has no open source published code", robot))?;
                let dest = dest.unwrap_or_else(|| format!("{}.{}", robot, info.lang.ext()).into());
                fs::write(dest.clone(), code)?;
                println!(
                    "Robot {} is downloaded and placed into {}",
                    slug,
                    dest.display()
                );
            }
        },
    }

    Ok(())
}

fn init_game_mode(game_mode_string: Option<OsString>) -> logic::GameMode {
    match game_mode_string {
        Some(s) => {
            let s_json = format!("\"{}\"", s.to_str().unwrap());
            serde_json::from_str::<logic::GameMode>(&s_json).expect("Unknown gamemode")
        }
        None => logic::GameMode::Normal,
    }
}

fn robot_name_from_path(path: &Path) -> anyhow::Result<&str> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| {
            if RobotId::valid_ident(s) {
                Some(s)
            } else {
                None
            }
        })
        .ok_or_else(|| {
            anyhow!(
                "Invalid name from the file name {:?}, try passing the robot name explicitly with the -n option",
                path
            )
        })
}

fn directories() -> anyhow::Result<&'static directories::ProjectDirs> {
    static DIRS: OnceCell<directories::ProjectDirs> = OnceCell::new();
    DIRS.get_or_try_init(|| {
        directories::ProjectDirs::from("org", "Robot Rumble", "rumblebot")
            .context("couldn't find configuration directory")
    })
}

#[derive(Clone, Copy, serde::Deserialize, strum::EnumString, strum::AsRefStr)]
pub enum Lang {
    Python,
    Javascript,
}

fn get_wasm_cache() -> anyhow::Result<FileSystemCache> {
    let dir = directories()?.cache_dir().join("wasm");
    Ok(FileSystemCache::new(dir)?)
}

fn wasm_from_cache_or_compile(
    store: &wasmer::Store,
    wasm: &[u8],
) -> anyhow::Result<(wasmer::Module, WasiVersion)> {
    let module = match get_wasm_cache() {
        Ok(mut cache) => {
            let hash = wasmer_cache::Hash::generate(wasm);
            // unsafe because wasmer loads arbitrary code from this directory, but the wasmer
            // cli does the same thing, and there's no cve for it ¯\_(ツ)_/¯
            let module = unsafe { cache.load(store, hash) };
            match module {
                Ok(m) => m,
                Err(_) => {
                    let module = wasmer::Module::new(store, wasm)?;
                    let _ = cache.store(hash, &module);
                    module
                }
            }
        }
        Err(_) => wasmer::Module::new(store, wasm)?,
    };
    let version = wasmer_wasi::get_wasi_version(&module, false).unwrap_or(WasiVersion::Latest);
    Ok((module, version))
}

impl Lang {
    fn from_ext(ext: &OsStr) -> Option<Self> {
        let lang = match ext.to_str()? {
            "py" => Lang::Python,
            "js" | "ejs" | "mjs" => Lang::Javascript,
            _ => return None,
        };
        Some(lang)
    }
    fn ext(self) -> &'static str {
        match self {
            Self::Python => "py",
            Self::Javascript => "js",
        }
    }
    fn get_wasm(
        self,
        store: &wasmer::Store,
    ) -> anyhow::Result<(&'static wasmer::Module, WasiVersion)> {
        macro_rules! lang_runner {
            ($bytes:expr) => {{
                static MODULE: OnceCell<(wasmer::Module, WasiVersion)> = OnceCell::new();
                let (module, version) = MODULE.get_or_try_init(|| {
                    let module = unsafe { wasmer::Module::deserialize(store, $bytes)? };
                    let version = wasmer_wasi::get_wasi_version(&module, false)
                        .unwrap_or(WasiVersion::Latest);
                    Ok::<_, anyhow::Error>((module, version))
                })?;
                (module, *version)
            }};
        }
        let lang = self;
        Ok(include!(concat!(env!("OUT_DIR"), "/lang_runners.rs")))
    }
}

#[derive(Clone)]
pub enum RobotId {
    Published {
        user: String,
        robot: String,
    },
    Local {
        source: PathBuf,
        lang: Lang,
    },
    Command {
        command: String,
        args: Vec<String>,
    },
    LocalRunner {
        runner: String,
        runner_args: Vec<String>,
        source: String,
    },
    Inline {
        lang: Lang,
        source: String,
    },
}

impl RobotId {
    pub fn display_id(&self) -> (&str, Cow<str>) {
        match self {
            Self::Published { user, robot } => (user, robot.into()),
            Self::Local { source, .. } => (".local", source.to_string_lossy()),
            Self::Command { command, args } => (
                ".command",
                std::iter::once(command).chain(args).join(" ").into(),
            ),
            Self::LocalRunner {
                runner,
                runner_args,
                source,
            } => (
                ".localrunner",
                std::iter::once(runner)
                    .chain(runner_args)
                    .chain(std::iter::once(source))
                    .join(" ")
                    .into(),
            ),
            Self::Inline { .. } => (".inline", ".".into()),
        }
    }
    pub fn parse(s: &OsStr) -> anyhow::Result<Self> {
        let s = match s.to_str() {
            Some(s) => s,
            None => return Self::from_path(PathBuf::from(s)),
        };
        let parse_command = |s| -> anyhow::Result<_> {
            let mut args = shell_words::split(s)
                .context("Couldn't parse as shell arguments")?
                .into_iter();
            let command = args.next().ok_or_else(|| {
                anyhow!("you must have at least one shell 'word' in the command string")
            })?;
            Ok((command, args.collect_vec()))
        };
        if let Some((typ, content)) = s.splitn(2, ':').collect_tuple() {
            match typ {
                "file" | "local" => Self::from_path(PathBuf::from(content)),
                "published" => Self::from_published(content).ok_or_else(|| {
                    anyhow!(
                        "invalid published robot id {:?}; it must be in the form of `user/robot` with only \
                        alphanumeric characters and underscores",
                        content
                    )
                }),
                "command" => {
                    let (command, args) = parse_command(content)?;
                    Ok(Self::Command { command, args })
                }
                "localrunner" => {
                    let (runner, mut runner_args) = parse_command(content)?;
                    let source = runner_args.pop().ok_or_else(|| {
                        anyhow!("you must have a source argument to the local runner")
                    })?;
                    Ok(Self::LocalRunner {
                        runner,
                        runner_args,
                        source,
                    })
                }
                "inline" => {
                    let (lang, source) = content
                        .splitn(2, ';')
                        .collect_tuple()
                        .ok_or_else(|| anyhow!("Missing language in inline robot"))?;
                    let lang = lang.parse().map_err(|_| anyhow!("invalid language"))?;
                    Ok(RobotId::Inline {
                        lang,
                        source: source.to_owned(),
                    })
                }
                _ => bail!("unknown runner type {:?}", typ),
            }
        } else if let Some(published) = Self::from_published(s) {
            Ok(published)
        } else {
            Self::from_path(PathBuf::from(s))
        }
    }
    fn valid_ident(s: &str) -> bool {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }
    fn from_published(s: &str) -> Option<Self> {
        parse_published_slug(s).and_then(|(user, robot)| {
            user.map(|user| Self::Published {
                user: user.to_owned(),
                robot: robot.to_owned(),
            })
        })
    }
    fn from_path(source: PathBuf) -> anyhow::Result<Self> {
        let ext = source.extension().ok_or_else(|| {
            anyhow!("your robot file must have an extension so that we know what language it's in")
        })?;
        let lang = Lang::from_ext(ext).ok_or_else(|| anyhow!("unknown extension {:?}", ext))?;
        Ok(RobotId::Local { source, lang })
    }
}

fn parse_published_slug(s: &str) -> Option<(Option<&str>, &str)> {
    let mut spl = s.split('/');
    let a = spl.next()?;
    if !RobotId::valid_ident(a) {
        return None;
    }
    let b = spl.next();
    if spl.next().is_some() {
        return None;
    }
    let ret = match b {
        Some(robot) => {
            if !RobotId::valid_ident(robot) {
                return None;
            }
            (Some(a), robot)
        }
        None => (None, a),
    };
    Some(ret)
}

async fn run_game(
    spec: GameSpec,
    game_mode: GameMode,
    display_turns: bool,
    red_logs_only: bool,
    blue_logs_only: bool,
) -> anyhow::Result<MainOutput> {
    let setup_time_start = Instant::now();

    let get_runner = |id| async move {
        let id = RobotId::parse(id).context("Couldn't parse robot identifier")?;
        let runner = Runner::from_id(&id).await?;
        Ok::<_, anyhow::Error>(runner)
    };
    let blue_os: OsString = spec.blue.into();
    let red_os: OsString = spec.red.into();
    let (blue, red) = tokio::try_join!(get_runner(&blue_os), get_runner(&red_os))?;
    let runners = maplit::btreemap! {
        logic::Team::Blue => blue,
        logic::Team::Red => red,
    };

    let setup_time_end = Instant::now();
    eprintln!("Setup took {:?}", setup_time_end - setup_time_start);

    let output = logic::run(
        runners,
        |turn_state| {
            if display_turns {
                display::display_turn(turn_state, !red_logs_only, !blue_logs_only)
                    .expect("printing failed");
            }
        },
        spec.turn_num.unwrap_or(100),
        true,
        None,
        game_mode,
        spec.seed.map(|s| s).as_deref(),
    )
    .await;

    let game_end_time = Instant::now();
    eprintln!("Game took {:?}", game_end_time - setup_time_end);

    Ok(output)
}

#[derive(Deserialize)]
struct GameSpec {
    red: String,
    blue: String,
    seed: Option<String>,
    turn_num: Option<usize>
}
