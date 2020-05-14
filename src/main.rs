use native_runner::{CommandRunner, TokioRunner};
use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tokio::{io, task};
use wasi_runner::WasiProcess;
use wasmer_runtime::{
    cache::{Cache, FileSystemCache, WasmHash},
    Module as WasmModule,
};
use wasmer_wasi::WasiVersion;

use logic::RobotRunner;

use anyhow::{anyhow, bail, Context};
use itertools::Itertools;
use once_cell::sync::{Lazy, OnceCell};
use structopt::StructOpt;

mod api;
mod server;

#[tokio::main]
async fn main() {
    if let Err(err) = try_main().await {
        eprintln!("ERROR: {}", err);
        err.chain()
            .skip(1)
            .for_each(|cause| eprintln!("because: {}", cause));
        std::process::exit(1);
    }
}

#[derive(StructOpt)]
#[structopt(name = "Robot Runner CLI", author)]
enum Rumblebot {
    /// Run 2 robots.
    ///
    /// If robot identifier matches the regex /^[_\\w]+\\/[_\\w]+$/, e.g. 'user_1/robotv3_Final`,
    /// it will be interpreted as a robot published to https://robot-rumble.org; otherwise it will be interpreted as a path
    /// to a local file that must be named with an extension of a supported language.
    ///
    /// command: or localrunner: :
    ///
    /// Each recieve a path to their source file as the first argument (after the ones provided
    /// in the command string), and after they initalize, they should print a `Result<(), ProgramError>` in
    /// serde_json format and a newline. They will then start recieving newline-delimited `ProgramInput` json objects, and
    /// for each one should output a `ProgramOutput` json object followed by a newline. The match is over when stdin is closed, and
    /// the process may be forcefully terminated after that.
    Run {
        #[structopt(parse(from_os_str))]
        robot1: OsString,
        #[structopt(parse(from_os_str))]
        robot2: OsString,
        /// The number of turns to run in the match
        #[structopt(short, long, default_value = "10")]
        turns: usize,
    },
    /// Battle robots in a web display
    WebRun {
        /// The robots to put in the web display.
        ///
        /// The first one will be the main one, run against whichever other one you choose.
        #[structopt(parse(from_os_str), min_values = 2)]
        robots: Vec<OsString>,
        /// The network address to listen to.
        #[structopt(short, long, default_value = "127.0.0.1")]
        address: String,
        /// The network port to listen to.
        #[structopt(short, long, env = "PORT")]
        port: Option<u16>,
    },
    Login {
        username: String,
        #[structopt(short)]
        password: Option<String>,
    },
    Logout {},
    Create {
        #[structopt(parse(from_os_str))]
        robot: PathBuf,
        #[structopt(long, short)]
        name: Option<String>,
        #[structopt(long, short)]
        lang: Option<Lang>,
    },
    Update {
        #[structopt(parse(from_os_str))]
        robot: PathBuf,
        #[structopt(long, short)]
        name: Option<String>,
    },
    Download {
        robot: String,
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

pub enum Runner {
    Command(CommandRunner),
    Wasi(
        TokioRunner<
            io::BufWriter<wasi_runner::WasiStdinWriter>,
            io::BufReader<wasi_runner::WasiStdoutReader>,
        >,
        /// the directory that we store the source file in; we need to keep it open
        tempfile::TempDir,
    ),
}

#[async_trait::async_trait]
impl RobotRunner for Runner {
    async fn run(&mut self, input: logic::ProgramInput) -> logic::RunnerResult {
        match self {
            Self::Command(r) => r.run(input).await,
            Self::Wasi(r, _) => r.run(input).await,
        }
    }
}

impl Runner {
    async fn new_wasm(
        module: &WasmModule,
        version: WasiVersion,
        args: &[String],
        dir: tempfile::TempDir,
    ) -> anyhow::Result<logic::ProgramResult<Self>> {
        let mut state = wasmer_wasi::state::WasiState::new("robot");
        wasi_runner::add_stdio(&mut state);
        state
            .preopen(|p| p.directory(&dir).alias("source").read(true))
            .unwrap()
            .args(args)
            .arg("/source/sourcecode");
        let imports =
            wasmer_wasi::generate_import_object_from_state(state.build().unwrap(), version);
        let instance = module
            .instantiate(&imports)
            .map_err(|_| anyhow!("error instantiating wasm module"))?;
        let mut proc = WasiProcess::spawn(instance);
        let stdin = io::BufWriter::new(proc.take_stdin().unwrap());
        let stdout = io::BufReader::new(proc.take_stdout().unwrap());
        task::spawn(proc);
        Ok(TokioRunner::new(stdin, stdout)
            .await
            .map(|r| Self::Wasi(r, dir)))
    }
    async fn from_id(id: &RobotId) -> anyhow::Result<logic::ProgramResult<Self>> {
        match id {
            RobotId::Published { user, robot } => {
                let info = api::robot_info(user, robot)
                    .await?
                    .ok_or_else(|| anyhow!("robot {}/{} not found", user, robot))?;
                let code = api::robot_code(info.id).await?.ok_or_else(|| {
                    anyhow!("robot {}/{} has not published its code yet", user, robot)
                })?;
                let sourcedir =
                    tempfile::tempdir().context("couldn't create temporary directory")?;
                fs::write(sourcedir.path().join("sourcecode"), code)
                    .context("Couldn't write published code to disk")?;
                let (module, version) = info.lang.get_wasm();
                Runner::new_wasm(module, version, &[], sourcedir).await
            }
            RobotId::Local { source, lang } => {
                let sourcedir = make_sourcedir(source)?;
                let (module, version) = lang.get_wasm();
                Runner::new_wasm(module, version, &[], sourcedir).await
            }
            RobotId::Command { command, args } => {
                let mut cmd = Command::new(command);
                cmd.args(args);
                Ok(TokioRunner::new_cmd(cmd).await.map(Self::Command))
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
                let (module, version) = wasm_from_cache_or_compile(&wasm)
                    .with_context(|| format!("couldn't compile wasm module at {}", runner))?;
                Runner::new_wasm(&module, version, &runner_args, sourcedir).await
            }
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Default, Debug, Clone)]
struct Config {
    #[serde(default)]
    auth_key: Option<String>,
    #[serde(default = "default_base_url")]
    base_url: Cow<'static, str>,
}
fn default_base_url() -> Cow<'static, str> {
    "https://robotrumble.org".into()
}
static CONFIG: OnceCell<Config> = OnceCell::new();
fn config() -> &'static Config {
    CONFIG.get().unwrap()
}

const XDG_NAME: &str = "rumblebot";

async fn try_main() -> anyhow::Result<()> {
    let opt: Rumblebot = Rumblebot::from_args();
    CONFIG
        .get_or_try_init(|| confy::load(XDG_NAME))
        .context("Unable to load config")?;

    match opt {
        Rumblebot::Run {
            robot1,
            robot2,
            turns,
        } => {
            let get_runner = |id| async move {
                let id = RobotId::parse(id).context("Couldn't parse robot identifier")?;
                let runner = Runner::from_id(&id).await?;
                Ok::<_, anyhow::Error>(runner)
            };
            let (r1, r2) = tokio::try_join!(get_runner(&robot1), get_runner(&robot2))?;
            let output = logic::run(r1, r2, turn_cb, turns).await;
            println!("Output: {:?}", output);
        }
        Rumblebot::WebRun {
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
        Rumblebot::Login { username, password } => {
            let password = match password {
                Some(pass) => pass,
                None => rpassword::read_password_from_tty(Some("Password: "))
                    .context("Error reading password (try passing the -p option)")?,
            };
            let auth_key = api::authenticate(&username, &password).await?;
            confy::store(
                XDG_NAME,
                Config {
                    auth_key: Some(auth_key),
                    ..config().clone()
                },
            )
            .context("Error storing configuration with auth_key")?;
        }
        Rumblebot::Logout {} => {
            confy::store(
                XDG_NAME,
                Config {
                    auth_key: None,
                    ..config().clone()
                },
            )
            .context("Error storing configuration with auth_key")?;
        }
        Rumblebot::Create { robot, name, lang } => {
            let code = fs::read_to_string(&robot)
                .with_context(|| format!("Couldn't read {}", robot.display()))?;
            let name = match name {
                Some(ref n) => n,
                None => robot_name_from_path(&robot)?,
            };
            let lang = match lang {
                Some(l) => l,
                None => robot.extension().and_then(Lang::from_ext).ok_or_else(|| {
                    anyhow!("Invalid language from extension, try passing the -l option")
                })?,
            };
            let info = api::create(lang, name).await?;
            api::update_code(info.id, &code).await?;
        }
        Rumblebot::Update { robot, name } => {
            let code = fs::read_to_string(&robot)
                .with_context(|| format!("Couldn't read {}", robot.display()))?;
            let name = match name {
                Some(ref n) => n,
                None => robot_name_from_path(&robot)?,
            };
            let (user, _) = api::whoami().await?;
            let info = api::robot_info(&user, name).await?.ok_or_else(|| {
                anyhow!(
                    "No existing robot of yours with name '{}'. try the `create` subcommand instead",
                    name
                )
            })?;
            api::update_code(info.id, &code).await?;
        }
        Rumblebot::Download { robot, dest } => {
            let (user, robot) = parse_published_ident(&robot)
                .ok_or_else(|| anyhow!("invalid robot ident '{}'", robot))?;
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
                .ok_or_else(|| anyhow!("robot {} has no code", robot))?;
            let dest = dest.unwrap_or_else(|| format!("{}.{}", robot, info.lang.ext()).into());
            fs::write(dest, code)?;
        }
    }

    Ok(())
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

#[derive(Clone, Copy, strum::EnumString, strum::AsRefStr)]
#[strum(serialize_all = "UPPERCASE")]
pub enum Lang {
    Python,
    Javascript,
}

fn get_wasm_cache() -> Option<FileSystemCache> {
    let dir = dirs::cache_dir()?.join(XDG_NAME).join("wasm");
    // unsafe because wasmer loads arbitrary code from this directory, but the wasmer
    // cli does the same thing, and there's no cve for it ¯\_(ツ)_/¯
    unsafe { FileSystemCache::new(dir).ok() }
}

fn wasm_from_cache_or_compile(
    wasm: &[u8],
) -> wasmer_runtime::error::CompileResult<(WasmModule, WasiVersion)> {
    let hash = WasmHash::generate(wasm);
    let cache = get_wasm_cache();
    let module = cache
        .as_ref()
        .and_then(|cache| cache.load(hash).ok())
        .map_or_else(
            || -> wasmer_runtime::error::CompileResult<_> {
                let module = wasmer_runtime::compile(wasm)?;
                if let Some(mut cache) = cache {
                    cache.store(hash, module.clone()).ok();
                }
                Ok(module)
            },
            Ok,
        )?;
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
    fn get_wasm(self) -> (&'static WasmModule, WasiVersion) {
        macro_rules! compiled_runner {
            ($name:literal) => {{
                static MODULE: Lazy<(WasmModule, WasiVersion)> = Lazy::new(|| {
                    let wasm = include_bytes!(concat!("../../logic/webapp-dist/runners/", $name));
                    wasm_from_cache_or_compile(wasm)
                        .expect(concat!("couldn't compile wasm module ", $name))
                });
                let (module, version) = &*MODULE;
                (module, *version)
            }};
        }
        match self {
            Self::Python => compiled_runner!("pyrunner.wasm"),
            Self::Javascript => compiled_runner!("jsrunner.wasm"),
        }
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
                    Ok(Self::LocalRunner { runner, runner_args, source })
                }
                _ => bail!("unknown runner type {:?}", typ)
            }
        } else if let Some(published) = Self::from_published(s) {
            Ok(published)
        } else {
            Self::from_path(PathBuf::from(s))
        }
    }
    fn valid_ident(s: &str) -> bool {
        s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    }
    fn from_published(s: &str) -> Option<Self> {
        parse_published_ident(s).and_then(|(user, robot)| {
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

fn parse_published_ident(s: &str) -> Option<(Option<&str>, &str)> {
    let mut spl = s.split('/');
    let a = spl.next()?;
    if !RobotId::valid_ident(a) {
        return None;
    }
    let b = spl.next();
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

fn turn_cb(turn_state: &logic::CallbackInput) {
    println!(
        "After turn {}:\n{}",
        turn_state.state.turn, turn_state.state.state
    );
    for (team, logs) in &turn_state.logs {
        if !logs.is_empty() {
            println!(
                "Logs for {:?}:\n{}",
                team,
                logs.iter().map(|s| s.trim()).format("\n"),
            )
        }
    }
}
