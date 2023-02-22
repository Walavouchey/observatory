// TODO: document members of the module where it makes sense

use std::str::FromStr;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde::Serialize;

use eyre::Result;
use unidiff;

use crate::structs;

const GITHUB_API_ROOT: &str = "https://api.github.com";
const GITHUB_ROOT: &str = "https://github.com";

pub struct GitHub {}
impl GitHub {
    pub fn pulls(full_repo_name: &str) -> String {
        format!("{GITHUB_API_ROOT}/repos/{full_repo_name}/pulls")
    }
    pub fn app_installations() -> String {
        format!("{GITHUB_API_ROOT}/app/installations")
    }
    pub fn installation_tokens(installation_id: i64) -> String {
        format!("{GITHUB_API_ROOT}/app/installations/{installation_id}/access_tokens")
    }
    pub fn installation_repos() -> String {
        format!("{GITHUB_API_ROOT}/installation/repositories")
    }
    pub fn comments(full_repo_name: &str, issue_number: i32) -> String {
        format!("{GITHUB_API_ROOT}/repos/{full_repo_name}/issues/{issue_number}/comments")
    }
    pub fn diff_url(full_repo_name: &str, pull_number: i32) -> String {
        // Diff links are handled by github.com, not the API subdomain.
        format!("{GITHUB_ROOT}/{full_repo_name}/pull/{pull_number}.diff")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TokenType {
    JWT,
    Installation(i64),
}

#[derive(Debug, Clone)]
pub struct Token {
    pub t: String,
    pub ttype: TokenType,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

impl Token {
    pub fn expired(&self) -> bool {
        chrono::Utc::now() < self.expires_at
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    app_id: String,
    key: String,
    http_client: reqwest::Client,

    tokens: Arc<Mutex<HashMap<TokenType, Token>>>,
    pub installations: Arc<Mutex<HashMap<i64, structs::Installation>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    exp: usize,
    iat: usize,
    iss: String,

    #[serde(skip)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip)]
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

impl Claims {
    pub fn new(app_id: &str) -> Self {
        let now = chrono::Utc::now();
        let created_at = now - chrono::Duration::minutes(1);
        let expires_at = now + chrono::Duration::minutes(7);
        Self {
            iat: created_at.timestamp().try_into().unwrap(),
            exp: expires_at.timestamp().try_into().unwrap(),
            iss: app_id.to_owned(),
            created_at,
            expires_at,
        }
    }
}

fn throw_error<T>(e: reqwest::Error, headers: Option<reqwest::header::HeaderMap>) -> Result<T> {
    log::error!(
        "Error at {}: HTTP {:?}: {:?}",
        e.url().unwrap(),
        e.status(),
        e
    );
    if let Some(headers) = headers {
        log::error!("Headers: {:?}", headers);
    }
    Err(e.into())
}

// TODO: this (as well as __text()) needs to retry certain 4xx requests, as well as 5xx coming from GitHub, which are retryable errors.
async fn __json<T>(rb: reqwest::RequestBuilder) -> Result<T>
where
    T: for<'de> serde::Deserialize<'de>,
{
    match rb.headers(Client::default_headers()).send().await {
        Ok(payload) => {
            let headers = payload.headers().clone();
            match payload.error_for_status() {
                Err(e) => throw_error(e, Some(headers)),
                Ok(res) => match res.json().await {
                    Ok(t) => Ok(t),
                    Err(e) => throw_error(e, Some(headers)),
                },
            }
        }
        Err(e) => throw_error(e, None),
    }
}

// This is identical to the above block, and the only reason it exists is because
// Rust doesn't have template specialization -- for fn<T>, all return values must be of the same type, and .text() breaks this.
async fn __text(rb: reqwest::RequestBuilder) -> Result<String> {
    match rb.headers(Client::default_headers()).send().await {
        Ok(payload) => {
            let headers = payload.headers().clone();
            match payload.error_for_status() {
                Err(e) => throw_error(e, Some(headers)),
                Ok(res) => match res.text().await {
                    Ok(t) => Ok(t),
                    Err(e) => throw_error(e, Some(headers)),
                },
            }
        }
        Err(e) => throw_error(e, None),
    }
}

impl Client {
    pub fn new(app_id: String, key: String) -> Self {
        Self {
            app_id,
            key,
            http_client: reqwest::Client::new(),
            tokens: Arc::new(Mutex::new(HashMap::new())),
            installations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn cached_token(&self, ttype: &TokenType) -> Option<String> {
        let tokens = self.tokens.lock().unwrap();
        if let Some(tt) = tokens.get(ttype) {
            if !tt.expired() {
                return Some(tt.t.clone());
            }
        }
        None
    }

    // https://docs.github.com/en/developers/apps/building-github-apps/authenticating-with-github-apps#generating-a-json-web-token-jwt
    fn generate_jwt(&self) -> Token {
        let claims = Claims::new(&self.app_id);
        let t = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_rsa_pem(self.key.as_bytes()).unwrap(),
        )
        .expect("failed to generate JWT");
        Token {
            t,
            ttype: TokenType::JWT,
            created_at: claims.created_at,
            expires_at: claims.expires_at,
        }
    }

    async fn get_jwt_token(&self) -> String {
        let ttype = TokenType::JWT;
        match self.cached_token(&ttype).await {
            Some(t) => t,
            None => {
                let token = self.generate_jwt();
                self.tokens.lock().unwrap().insert(ttype, token.clone());
                token.t
            }
        }
    }

    async fn get_installation_token(&self, installation_id: i64) -> Result<String> {
        let ttype = TokenType::Installation(installation_id);
        match self.cached_token(&ttype).await {
            Some(t) => Ok(t),
            None => {
                let jwt = self.get_jwt_token().await;
                let req = self
                    .http_client
                    .post(GitHub::installation_tokens(installation_id))
                    .bearer_auth(jwt);
                let response: structs::InstallationToken = __json(req).await?;
                let token = Token {
                    t: response.token,
                    ttype: ttype.clone(),
                    created_at: chrono::Utc::now(),
                    expires_at: response.expires_at - chrono::Duration::minutes(5),
                };
                self.tokens.lock().unwrap().insert(ttype, token.clone());
                Ok(token.t)
            }
        }
    }

    fn default_headers() -> reqwest::header::HeaderMap {
        let mut m = reqwest::header::HeaderMap::new();
        m.insert("Accept", "application/vnd.github+json".try_into().unwrap());
        m.insert("User-Agent", "observatory".try_into().unwrap());
        m
    }

    pub async fn installations(&self) -> Result<Vec<structs::Installation>> {
        let pp = self
            .http_client
            .get(GitHub::app_installations())
            .bearer_auth(self.get_jwt_token().await)
            .headers(Self::default_headers())
            .send();
        let body = pp.await.unwrap();
        let items: Vec<structs::Installation> = body.json().await?;
        Ok(items)
    }

    pub async fn discover_installations(&self) -> Result<()> {
        if let Ok(installations) = self.installations().await {
            for installation in installations {
                self.add_installation(installation).await?;
            }
        }
        Ok(())
    }

    pub async fn add_installation(&self, mut installation: structs::Installation) -> Result<()> {
        let token = self.get_installation_token(installation.id).await?;
        let req = self
            .http_client
            .get(GitHub::installation_repos())
            .bearer_auth(token);
        let response: structs::InstallationRepositories = __json(req).await?;
        installation.repositories = response.repositories;
        self.installations
            .lock()
            .unwrap()
            .insert(installation.id, installation);
        Ok(())
    }

    pub fn remove_installation(&self, installation: &structs::Installation) {
        self.installations.lock().unwrap().remove(&installation.id);
        self.tokens
            .lock()
            .unwrap()
            .remove(&TokenType::Installation(installation.id));
    }

    async fn pick_token(&self, full_repo_name: &str) -> Result<String> {
        let mut installation_id = None;
        for (k, v) in self.installations.lock().unwrap().iter() {
            if v.repositories.iter().any(|r| r.full_name == full_repo_name) {
                installation_id = Some(*k);
                break;
            }
        }
        match installation_id {
            None => eyre::bail!("No GitHub token for {} found", full_repo_name),
            Some(iid) => self.get_installation_token(iid).await,
        }
    }

    pub async fn pulls(&self, full_repo_name: &str) -> Result<Vec<structs::PullRequest>> {
        let mut out = Vec::new();
        let token = self.pick_token(full_repo_name).await?;
        for page in 1..100 {
            let req = self
                .http_client
                .get(GitHub::pulls(full_repo_name))
                .query(&[
                    ("state", "open"),
                    ("direction", "asc"),
                    ("sort", "created"),
                    ("per_page", "100"),
                    ("page", &page.to_string()),
                ])
                .bearer_auth(token.clone());
            let mut response: Vec<structs::PullRequest> = __json(req).await?;
            if response.is_empty() {
                break;
            }
            out.append(&mut response);
        }
        Ok(out)
    }

    pub async fn post_comment(
        &self,
        full_repo_name: &str,
        issue_number: i32,
        comment: String,
    ) -> Result<()> {
        let comment = serde_json::to_string(&structs::IssueComment { body: comment }).unwrap();
        let token = self.pick_token(full_repo_name).await?;
        let req = self
            .http_client
            .post(GitHub::comments(full_repo_name, issue_number))
            .body(comment)
            .bearer_auth(token);
        __json(req).await?;
        Ok(())
    }

    pub async fn read_pull_diff(
        &self,
        full_repo_name: &str,
        pull_number: i32,
    ) -> Result<unidiff::PatchSet> {
        let token = self.pick_token(full_repo_name).await?;
        let req = self
            .http_client
            .get(GitHub::diff_url(full_repo_name, pull_number))
            .bearer_auth(token);
        let response = __text(req).await?;
        Ok(unidiff::PatchSet::from_str(&response)?)
    }
}

// TODO: add tests
