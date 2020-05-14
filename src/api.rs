use anyhow::{anyhow, Context};
use once_cell::sync::Lazy;
use reqwest::{header, Client, StatusCode, Url};

use super::Lang;

fn base_url() -> anyhow::Result<Url> {
    Url::parse(&super::config().base_url).context("Invalid base url")
}
macro_rules! build_url {
    ($($segment:tt)/+) => {{
        let ret: anyhow::Result<Url> = base_url().and_then(|mut url| {
            url.path_segments_mut()
                .map_err(|()| anyhow!("Invalid base url"))?
                .extend(&[$($segment),+]);
            Ok(url)
        });
        ret
    }};
}

static CLIENT: Lazy<Client> = Lazy::new(client);

const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
fn client() -> Client {
    let mut builder = Client::builder().user_agent(USER_AGENT);
    if let Some(ref jwt) = super::config().auth_key {
        let mut headers = header::HeaderMap::with_capacity(1);
        headers.append(
            reqwest::header::COOKIE,
            format!("PLAY_SESSION={}", jwt)
                .parse()
                .expect("Badly formatted jwt; can't parse in a header"),
        );
        builder = builder.default_headers(headers);
    }
    builder.build().expect("Couldn't build client")
}

#[derive(serde::Deserialize)]
struct Error {
    #[serde(rename = "")]
    msg: Vec<String>,
}

#[derive(serde::Deserialize)]
pub struct RobotInfo {
    pub id: usize,
    #[serde(with = "serde_with::rust::display_fromstr")]
    pub lang: Lang,
    // userId: usize,
}

async fn handle_response(res: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    match res.status() {
        StatusCode::OK => Ok(res),
        StatusCode::FORBIDDEN => Err(anyhow!(
            "Error authenticating: {}",
            res.json::<Error>()
                .await?
                .msg
                .first()
                .map_or("unknown error", |s| &s),
        )),
        _ => {
            let err = res
                .error_for_status()
                .map(|_| anyhow!("Request failed"))
                .unwrap_or_else(|e| e.into());
            Err(err)
        }
    }
}

pub async fn robot_info(user: &str, robot: &str) -> anyhow::Result<Option<RobotInfo>> {
    let res = CLIENT
        .get(build_url!("api" / "get-robot" / user / robot)?)
        .send()
        .await?;
    if res.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let info = handle_response(res).await?.json().await?;
    Ok(info)
}

pub async fn robot_code(id: usize) -> anyhow::Result<Option<String>> {
    let res = CLIENT
        .get(build_url!("api" / "get-robot-code" / (&id.to_string()))?)
        .send()
        .await?;
    let code: String = handle_response(res).await?.json().await?;
    let code = if code.is_empty() { None } else { Some(code) };
    Ok(code)
}

pub async fn authenticate(username: &str, password: &str) -> anyhow::Result<String> {
    #[derive(serde::Serialize)]
    struct Request<'a> {
        username: &'a str,
        password: &'a str,
    }
    let res = CLIENT
        .post(build_url!("api" / "login")?)
        .json(&Request { username, password })
        .send()
        .await
        .context("Couldn't send authentication request")?;
    handle_response(res)
        .await?
        .cookies()
        .find(|c| c.name() == "PLAY_SESSION")
        .map(|c| c.value().to_owned())
        .ok_or_else(|| {
            anyhow!("Authentication response returned with OK but did not set PLAY_SESSION cookie")
        })
}

pub async fn create(lang: Lang, name: &str) -> anyhow::Result<RobotInfo> {
    #[derive(serde::Serialize)]
    struct Request<'a> {
        lang: &'a str,
        name: &'a str,
    }
    let res = CLIENT
        .post(build_url!("api" / "create-robot")?)
        .json(&Request {
            lang: lang.as_ref(),
            name,
        })
        .send()
        .await?;
    let info = handle_response(res).await?.json().await?;
    Ok(info)
}

pub async fn update_code(id: usize, code: &str) -> anyhow::Result<()> {
    #[derive(serde::Serialize)]
    struct Request<'a> {
        code: &'a str,
    }
    let res = CLIENT
        .post(build_url!("api" / "update-robot-code" / (&id.to_string()))?)
        .json(&Request { code })
        .send()
        .await?;
    handle_response(res).await.map(drop)
}

pub async fn whoami() -> anyhow::Result<(String, usize)> {
    let res = CLIENT.get(build_url!("api" / "whoami")?).send().await?;
    let ret = handle_response(res).await?.json().await?;
    Ok(ret)
}
