use anyhow::Context as _;
use futures_util::never::Never;
use futures_util::{FutureExt, StreamExt};
use itertools::Itertools;
use owning_ref::OwningRef;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::{io, net, prelude::*, task};
use warp::Filter;

use super::{RobotId, Runner};

#[derive(Clone)]
struct Context {
    r1: OwningRef<Arc<Vec<RobotId>>, RobotId>,
    ids: Arc<Vec<RobotId>>,
}

pub async fn serve(ids: Vec<RobotId>, address: String, port: Option<u16>) -> anyhow::Result<()> {
    let ids = Arc::new(ids);
    let r1 = OwningRef::new(ids.clone()).map(|v| v.first().unwrap());

    let ctx = Context { r1, ids };
    let ctx = warp::any().map(move || ctx.clone());

    let route = warp::path("getflags")
        .and(ctx.clone())
        .and(warp::get())
        .map(move |Context { r1, .. }| {
            let (user1, robot1) = r1.display_id();
            let body = serde_json::json!({
                "user": user1,
                "robot": robot1,
            });
            warp::reply::json(&body)
        })
        .or(warp::path("run")
            .and(warp::get())
            .and(ctx.clone())
            .and(warp::query::<RunParams>())
            .and_then(run))
        .or(warp::path!("getrobots" / String)
            .and(warp::get())
            .and(ctx)
            .map(|_user: String, Context { ids, .. }| {
                warp::reply::json(
                    &ids.iter()
                        .enumerate()
                        .skip(1)
                        .map(|(i, id)| {
                            let (user, robot) = id.display_id();
                            serde_json::json!({
                                "id": i,
                                "name": format!("{} / {}", user, robot),
                                "rating": 0,
                                "lang": "n/a",
                                "published": true,
                            })
                        })
                        .collect_vec(),
                )
            }))
        .or(static_dir::static_dir!("dist"));

    let server = warp::serve(route);

    let addr: std::net::IpAddr = address.parse().context("Invalid address provided")?;
    let bind = |port| net::TcpListener::bind((addr, port));

    let listener = match bind(port.unwrap_or(5252)).await {
        Ok(l) => l,
        Err(e) => match port {
            Some(port) => anyhow::bail!(
                anyhow::Error::new(e).context(format!("Couldn't bind on port {}", port))
            ),
            None => bind(0) // random port
                .await
                .context("couldn't bind to any port")?,
        },
    };

    let domain = if address == "127.0.0.1" {
        "localhost"
    } else {
        &address
    };
    let url = format!("http://{}:{}", domain, listener.local_addr()?.port());

    webbrowser::open(&url).ok();
    println!("Website running at {}", url);
    eprintln!("Press Enter to stop");

    let mut stdin = io::stdin();
    let mut buf = [0];
    tokio::select! {
        _ = server.run_incoming(listener) => {}
        _ = stdin.read(&mut buf) => {}
    }

    Ok(())
}

#[derive(serde::Deserialize)]
struct RunParams {
    id: usize,
    turns: usize,
}

async fn run(
    Context { r1, ids }: Context,
    params: RunParams,
) -> Result<impl warp::Reply, warp::Rejection> {
    let r2 = OwningRef::new(ids).try_map(|ids| ids.get(params.id).ok_or_else(|| warp::reject()))?;
    let (tx, rx) = mpsc::unbounded_channel();
    let join_handle = task::spawn(async move {
        let make_runner = |id| {
            Runner::from_id(id)
                .map(|res| res.unwrap_or_else(|err| Err(logic::ProgramError::IO(err.to_string()))))
        };
        let (r1, r2) = tokio::join!(make_runner(&r1), make_runner(&r2));
        let runners = maplit::btreemap! {
            logic::Team::Blue => r1,
            logic::Team::Red => r2,
        };
        let tx = tx;
        let output = logic::run(
            runners,
            |inp| {
                tx.send(warp::sse::json(serde_json::json!({
                    "type": "getProgress",
                    "data": inp,
                })))
                // if the reciever has been dropped, the stream has closed, so we can just panic to stop this task
                .unwrap_or_else(|_| panic!("stop"));
            },
            params.turns,
        )
        .await;
        // we don't really care if it's successful or not; we're done anyways
        let _ = tx.send(warp::sse::json(serde_json::json!({
            "type": "getOutput",
            "data": output,
        })));
        drop(tx)
    });
    // so that it doesn't print a panic message
    task::spawn(join_handle);
    Ok(warp::sse::reply(
        warp::sse::keep_alive().stream(rx.map(Ok::<_, Never>)),
    ))
}
