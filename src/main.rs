use native_runner::TokioRunner;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use tokio::process::Command;
use tokio::{io, task};
use wasi_runner::WasiProcess;
use wasmer_runtime::{
    cache::{Cache, FileSystemCache, WasmHash},
    Module as WasmModule,
};
use wasmer_wasi::WasiVersion;

use anyhow::{anyhow, bail, Context};
use clap::{App, AppSettings, Arg, SubCommand};
use itertools::Itertools;
use once_cell::sync::Lazy;

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
        .subcommand(
            SubCommand::with_name("run")
                .about("Run 2 robots against each other")
                .long_about(
                    "Run 2 robots. If the robot identifier matches the regex /^[_\\w]+\\/[_\\w]+$/, e.g. 'user_1/robotv3_Final', \
                    it will be interpreted as a robot published to https://robot-rumble.org; otherwise it will be interpreted as a path \
                    to a local file that must be named with an extension of a supported language"
                )
                .arg(Arg::with_name("ROBOT1").required(true))
                .arg(Arg::with_name("ROBOT2").required(true))
        )
        .subcommand(
            SubCommand::with_name("run-command")
                .about("Run 2 commands as robots")
                .long_about(
                    "Run 2 commands as robots. Each recieve a path to their source file as the first argument (after the ones provided \
                    in the command string), and after they initalize, they should print a `Result<(), ProgramError>` in \
                    serde_json format and a newline. They will then start recieving newline-delimited `ProgramInput` json objects, and \
                    for each one should output a `ProgramOutput` json object followed by a newline. The match is over when stdin is closed, and \
                    the process may be forcefully terminated after that."
                )
                .arg(Arg::with_name("ROBOT1_EXE").required(true))
                .arg(Arg::with_name("ROBOT1_SOURCE").required(true))
                .arg(Arg::with_name("ROBOT2_EXE").required(true))
                .arg(Arg::with_name("ROBOT2_SOURCE").required(true))
        )
        .subcommand(
            SubCommand::with_name("wasm")
                .about("Run 2 wasi modules as robots") 
                .long_about(
                    "Run a wasi module as a robot. This will be fully sandboxed, so is (probably) safe \
                    to use with untrusted modules. The process will communicate the same way as described in the run-command about text."
                )
                .arg(Arg::with_name("ROBOT1_EXE").required(true))
                .arg(Arg::with_name("ROBOT1_SOURCE").required(true))
                .arg(Arg::with_name("ROBOT2_EXE").required(true))
                .arg(Arg::with_name("ROBOT2_SOURCE").required(true))
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

struct RunOpts {
    turns: usize,
}

async fn try_main() -> anyhow::Result<()> {
    let matches = app().get_matches();
    let runopts = RunOpts {
        turns: clap::value_t!(matches.value_of("turns"), usize)?,
    };
    if let Some(matches) = matches.subcommand_matches("run") {
        let make_input = |robot_val| -> anyhow::Result<_> {
            let id = RobotId::from_osstr(matches.value_of_os(robot_val).unwrap())?;
            match id {
                RobotId::Published { user, robot } => {
                    let _ = (user, robot);
                    todo!("fetch published robots")
                }
                RobotId::Local { source, lang } => {
                    let sourcedir = make_sourcedir(source)?;
                    let (module, version) = lang.get_wasm();
                    Ok((module, version, sourcedir))
                }
            }
        };
        let (m1, v1, p1) = make_input("ROBOT1")?;
        let (m2, v2, p2) = make_input("ROBOT2")?;
        run_wasm(runopts, (&m1, v1, p1.as_ref()), (&m2, v2, p2.as_ref())).await?
    } else if let Some(matches) = matches.subcommand_matches("wasm") {
        let make_input = |exe_val, src_val| -> anyhow::Result<_> {
            let sourcedir = make_sourcedir(matches.value_of_os(src_val).unwrap())?;

            let wasm = fs::read(matches.value_of_os(exe_val).unwrap())
                .context("couldn't read wasm source")?;
            eprintln!("compiling wasm");
            let module = wasmer_runtime::compile(&wasm).context("couldn't compile wasm module")?;
            let version = wasmer_wasi::get_wasi_version(&module, false)
                .unwrap_or(wasmer_wasi::WasiVersion::Latest);
            eprintln!("done!");

            Ok((module, version, sourcedir))
        };
        let (m1, v1, p1) = make_input("ROBOT1_EXE", "ROBOT1_SOURCE")?;
        let (m2, v2, p2) = make_input("ROBOT2_EXE", "ROBOT2_SOURCE")?;
        run_wasm(runopts, (&m1, v1, p1.path()), (&m2, v2, p2.path())).await?
    } else if let Some(matches) = matches.subcommand_matches("run-command") {
        let make_runner = |exe_val, src_val| -> anyhow::Result<_> {
            let mut args = shell_words::split(matches.value_of(exe_val).unwrap())
                .with_context(|| format!("Couldn't parse {} as shell arguments", exe_val))?
                .into_iter();
            let mut cmd = Command::new(args.next().ok_or_else(|| {
                anyhow!("you must have at least one shell 'word' in {}", exe_val)
            })?);
            cmd.args(args);
            cmd.arg(matches.value_of_os(src_val).unwrap());
            Ok(TokioRunner::new_cmd(cmd))
        };

        let (r1, r2) = tokio::join!(
            make_runner("ROBOT1_EXE", "ROBOT1_SOURCE")?,
            make_runner("ROBOT2_EXE", "ROBOT2_SOURCE")?,
        );
        run(runopts, r1, r2).await
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Lang {
    Python,
    Javascript,
}

fn get_wasm_cache() -> Option<FileSystemCache> {
    let dir = dirs::cache_dir()?.join("robot-rumble/wasm");
    // unsafe because wasmer loads arbitrary code from this directory, but the wasmer
    // cli does the same thing, and there's no cve for it ¯\_(ツ)_/¯
    unsafe { FileSystemCache::new(dir).ok() }
}

impl Lang {
    fn get_wasm(self) -> (&'static WasmModule, WasiVersion) {
        macro_rules! compiled_runner {
            ($name:literal) => {{
                static MODULE: Lazy<(WasmModule, WasiVersion)> = Lazy::new(|| {
                    let wasm = include_bytes!(concat!("../../logic/webapp-dist/runners/", $name));
                    let hash = WasmHash::generate(wasm);
                    let cache = get_wasm_cache();
                    let module = cache
                        .as_ref()
                        .and_then(|cache| cache.load(hash).ok())
                        .unwrap_or_else(|| {
                            let module = wasmer_runtime::compile(wasm)
                                .expect(concat!("couldn't compile wasm module ", $name));
                            if let Some(mut cache) = cache {
                                cache.store(hash, module.clone()).ok();
                            }
                            module
                        });
                    let version =
                        wasmer_wasi::get_wasi_version(&module, false).expect("module isn't wasi");
                    (module, version)
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

enum RobotId<'a> {
    Published { user: &'a str, robot: &'a str },
    Local { source: &'a Path, lang: Lang },
}

impl<'a> RobotId<'a> {
    fn from_osstr(s: &'a OsStr) -> anyhow::Result<Self> {
        if let Some(s) = s.to_str() {
            if let Some((user, robot)) = s.split('/').collect_tuple() {
                let valid_ident =
                    |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if valid_ident(user) && valid_ident(robot) {
                    return Ok(RobotId::Published { user, robot });
                }
            }
        }
        let source = Path::new(s);
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

type WasmInput<'a> = (&'a WasmModule, WasiVersion, &'a Path);

async fn run_wasm(
    runopts: RunOpts,
    inp1: WasmInput<'_>,
    inp2: WasmInput<'_>,
) -> anyhow::Result<()> {
    let make_runner = |(module, version, sourcedir): WasmInput| -> anyhow::Result<_> {
        let mut state = wasmer_wasi::state::WasiState::new("robot");
        wasi_runner::add_stdio(&mut state);
        state
            .preopen(|p| p.directory(sourcedir).alias("source").read(true))
            .unwrap()
            .arg("/source/sourcecode");
        let imports =
            wasmer_wasi::generate_import_object_from_state(state.build().unwrap(), version);
        let instance = module
            .instantiate(&imports)
            .map_err(|e| anyhow!("error instantiating module: {}", e))?;
        let mut proc = WasiProcess::spawn(instance);
        let stdin = io::BufWriter::new(proc.take_stdin().unwrap());
        let stdout = io::BufReader::new(proc.take_stdout().unwrap());
        task::spawn(proc);
        Ok(TokioRunner::new(stdin, stdout))
    };
    eprintln!("initializing runners");
    let (r1, r2) = tokio::join!(make_runner(inp1)?, make_runner(inp2)?);
    eprintln!("done!");
    run(runopts, r1, r2).await;
    Ok(())
}

async fn run<R: logic::RobotRunner>(
    opts: RunOpts,
    r1: logic::ProgramResult<R>,
    r2: logic::ProgramResult<R>,
) {
    let output = logic::run(r1, r2, turn_cb, opts.turns).await;
    println!("Output: {:?}", output);
}

fn turn_cb(turn_state: &logic::CallbackInput) {
    println!(
        "State after turn {turn}:\n{logs}\nOutputs: {outputs:?}\nMap:\n{map}",
        turn = turn_state.state.turn,
        logs = turn_state
            .logs
            .iter()
            .format_with("\n", |(team, logs), f| f(&format_args!(
                "Logs for {:?}:\n{}",
                team,
                logs.iter().map(|s| s.trim()).format("\n"),
            ))),
        outputs = turn_state.robot_outputs,
        map = turn_state.state.state,
    );
}
