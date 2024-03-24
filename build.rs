use std::io::Write;
use std::path::PathBuf;
use std::{env, fs};
use wasmer_engine::ArtifactCreate;

#[cfg(feature = "build-cranelift")]
use wasmer_compiler_cranelift::Cranelift as Compiler;
#[cfg(feature = "build-llvm")]
use wasmer_compiler_llvm::LLVM as Compiler;

enum CompilationSource {
    Precompiled(PathBuf),
    #[cfg(any(feature = "build-cranelift", feature = "build-llvm"))]
    Compiler {
        engine: wasmer::UniversalEngine,
        runners_dir: PathBuf,
        jit_ext: &'static str,
        tunables: wasmer::BaseTunables,
    },
}

fn main() {
    let source = match env::var_os("COMPILED_RUNNERS") {
        Some(dir) => CompilationSource::Precompiled(fs::canonicalize(&dir).unwrap()),
        #[cfg(not(any(feature = "build-cranelift", feature = "build-llvm")))]
        None => {
            panic!("need build-cranelift or build-llvm or the COMPILED_RUNNERS env var")
        }
        #[cfg(any(feature = "build-cranelift", feature = "build-llvm"))]
        None => {
            let mut features = wasmer::CpuFeature::set();
            for feat in env::var("CARGO_CFG_TARGET_FEATURE").unwrap().split(',') {
                if let Ok(feat) = feat.parse() {
                    features.insert(feat);
                }
            }
            let target =
                wasmer::Target::new(env::var("TARGET").unwrap().parse().unwrap(), features);
            let tunables = wasmer::BaseTunables::for_target(&target);
            let jit_ext = wasmer::UniversalArtifact::get_default_extension(target.triple());
            let engine = wasmer::Universal::new(Compiler::new())
                .target(target)
                .engine();

            let runners_dir = fs::canonicalize("../logic/wasm-dist/lang-runners")
                .expect("need to run logic/build-wasm.sh");

            CompilationSource::Compiler {
                engine,
                runners_dir,
                jit_ext,
                tunables,
            }
        }
    };

    let lang_runners = [("Python", "pyrunner"), ("Javascript", "jsrunner")];

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    let mut match_lang = fs::File::create(out_dir.join("lang_runners.rs")).unwrap();
    writeln!(match_lang, "match lang {{").unwrap();

    for (lang, runner) in &lang_runners {
        let (path, include_bin) = match &source {
            CompilationSource::Precompiled(dir) => {
                let mut wjit = dir.join(runner);
                wjit.set_extension("wjit");
                (wjit, true)
            }
            #[cfg(any(feature = "build-cranelift", feature = "build-llvm"))]
            CompilationSource::Compiler {
                engine,
                runners_dir,
                jit_ext,
                tunables,
            } => {
                let mut src = runners_dir.join(runner);
                src.set_extension("wasm");
                let mut dst = out_dir.join(runner);
                dst.set_extension(*jit_ext);

                println!("compiling {}", runner);

                println!("cargo:rerun-if-changed={}", src.display());

                let needs_updating = src
                    .metadata()
                    .and_then(|m| Ok((m, dst.metadata()?)))
                    .and_then(|(src, dst)| Ok(src.modified()? > dst.modified()?))
                    .unwrap_or(true);

                if needs_updating {
                    let wasm_source = fs::read(&src).unwrap();
                    let artifact =
                        wasmer::UniversalArtifact::new(engine, &wasm_source, tunables).unwrap();

                    fs::write(&dst, artifact.serialize().unwrap()).unwrap();
                }

                (dst, cfg!(feature = "build-llvm"))
            }
        };

        writeln!(
            match_lang,
            "    Lang::{} => lang_runner!({}({:?}){}),",
            lang,
            if include_bin {
                "include_bytes!"
            } else {
                "&std::fs::read"
            },
            path,
            if include_bin {
                ""
            } else {
                r#".expect("should compile with --features=build-llvm when distributing")"#
            }
        )
        .unwrap();
    }

    writeln!(match_lang, "}}").unwrap();
}
