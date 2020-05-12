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
pub struct RobotInfo {
    pub id: usize,
    #[serde(with = "serde_with::rust::display_fromstr")]
    pub lang: Lang,
    // userId: usize,
}

pub async fn robot_info(user: &str, robot: &str) -> anyhow::Result<Option<RobotInfo>> {
    let res = CLIENT
        .get(build_url!("api" / "getRobotSlug" / user / robot)?)
        .send()
        .await?;
    if res.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let info = res.error_for_status()?.json().await?;
    Ok(info)
}
pub async fn robot_code(id: usize) -> anyhow::Result<Option<String>> {
    let code: String = CLIENT
        .get(build_url!("api" / "getRobotCode" / (&id.to_string()))?)
        .send()
        .await?
        .json()
        .await?;
    let code = if code.is_empty() { None } else { Some(code) };
    Ok(code)
}

pub async fn authenticate(username: &str, password: &str) -> anyhow::Result<String> {
    let res = CLIENT
        .post(build_url!("api" / "login")?)
        .json(&serde_json::json!({
            "username": username,
            "password": password,
        }))
        .send()
        .await
        .context("Can't send authentication request")?;
    match res.status() {
        StatusCode::OK => res
            .cookies()
            .find(|c| c.name() == "PLAY_SESSION")
            .map(|c| c.value().to_owned())
            .ok_or_else(|| {
                anyhow!(
                    "Authentication response returned with OK but did not set PLAY_SESSION cookie"
                )
            }),
        StatusCode::FORBIDDEN => Err(anyhow!("Incorrect username or password")),
        _ => {
            let err = res
                .error_for_status()
                .map(|_| anyhow!("Request failed"))
                .unwrap_or_else(|e| e.into());
            Err(err)
        }
    }
}
