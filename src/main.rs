use native_runner::{CommandRunner, TokioRunner};
use std::borrow::Cow;
use std::ffi::OsStr;
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
use clap::{App, AppSettings, Arg, SubCommand};
use itertools::Itertools;
use once_cell::sync::Lazy;

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

fn app() -> App<'static, 'static> {
    App::new("Robot Runner CLI")
        .version(clap::crate_version!())
        .author(clap::crate_authors!())
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .subcommand(
            SubCommand::with_name("run")
                .about("Run 2 robots against each other")
                // TODO: polish the about string
                .long_about(
                    "Run 2 robots. If robot identifier matches the regex /^[_\\w]+\\/[_\\w]+$/, e.g. 'user_1/robotv3_Final', \
                    it will be interpreted as a robot published to https://robot-rumble.org; otherwise it will be interpreted as a path \
                    to a local file that must be named with an extension of a supported language. \
                    command: or localrunner: : \n\
                    Each recieve a path to their source file as the first argument (after the ones provided \
                    in the command string), and after they initalize, they should print a `Result<(), ProgramError>` in \
                    serde_json format and a newline. They will then start recieving newline-delimited `ProgramInput` json objects, and \
                    for each one should output a `ProgramOutput` json object followed by a newline. The match is over when stdin is closed, and \
                    the process may be forcefully terminated after that."
                )
                .arg(Arg::with_name("ROBOT1").required(true))
                .arg(Arg::with_name("ROBOT2").required(true))
                .arg(
                    Arg::with_name("turns")
                        .short("t")
                        .long("turns")
                        .takes_value(true)
                        .value_name("TURN_NUM")
                        .default_value("10")
                        .global(true)
                        .help("Sets the number of turns to run")
                )
        )
        .subcommand(
            SubCommand::with_name("webdisplay")
                .about("Battle robots in a web display")
                .arg(Arg::with_name("ROBOTS").required(true).multiple(true).min_values(2))
        )
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
                let _ = (user, robot);
                todo!("fetch published robots")
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

async fn try_main() -> anyhow::Result<()> {
    let matches = app().get_matches();

    if let Some(matches) = matches.subcommand_matches("run") {
        let nturns = clap::value_t!(matches.value_of("turns"), usize)?;
        let get_runner = |val_name| async move {
            let id = RobotId::parse(matches.value_of_os(val_name).unwrap())
                .with_context(|| format!("couldn't parse {}", val_name))?;
            let runner = Runner::from_id(&id).await?;
            Ok::<_, anyhow::Error>(runner)
        };
        let (r1, r2) = tokio::try_join!(get_runner("ROBOT1"), get_runner("ROBOT2"))?;
        let output = logic::run(r1, r2, turn_cb, nturns).await;
        println!("Output: {:?}", output);
    } else if let Some(matches) = matches.subcommand_matches("webdisplay") {
        let ids = matches
            .values_of_os("ROBOTS")
            .unwrap()
            .map(RobotId::parse)
            .collect::<Result<Vec<_>, _>>()?;
        server::serve(ids).await?;
    }

    Ok(())
}

#[derive(Clone, Copy)]
pub enum Lang {
    Python,
    Javascript,
}

fn get_wasm_cache() -> Option<FileSystemCache> {
    let dir = dirs::cache_dir()?.join("robot-rumble/wasm");
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
    fn from_published(s: &str) -> Option<Self> {
        s.split('/').collect_tuple().and_then(|(user, robot)| {
            let valid_ident = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if valid_ident(user) && valid_ident(robot) {
                Some(Self::Published {
                    user: user.to_owned(),
                    robot: robot.to_owned(),
                })
            } else {
                None
            }
        })
    }
    fn from_path(source: PathBuf) -> anyhow::Result<Self> {
        let ext = source.extension().ok_or_else(|| {
            anyhow!("your robot file must have an extension so that we know what language it's in")
        })?;
        let lang = match ext.to_str() {
            Some("py") => Lang::Python,
            Some("js") | Some("ejs") | Some("mjs") => Lang::Javascript,
            _ => bail!("unknown extension {:?}", ext),
        };
        Ok(RobotId::Local { source, lang })
    }
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
