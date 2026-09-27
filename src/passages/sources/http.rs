//! Shared HTTP for ingest sources: fixed https host allowlist, no redirects, 8 s timeout,
//! response size cap, and per-source request spacing. Never fetches client-supplied URLs.

use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use super::SourceError;

pub const JSON_CAP: usize = 512 * 1024;
pub const TEXT_CAP: usize = 5 * 1024 * 1024;

pub struct RateLimiter {
    interval: Duration,
    next: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next: Mutex::new(Instant::now()),
        }
    }

    pub async fn wait(&self) {
        let mut next = self.next.lock().await;
        if *next > Instant::now() {
            tokio::time::sleep_until(*next).await;
        }
        *next = Instant::now() + self.interval;
    }
}

pub struct SourceHttp {
    client: reqwest::Client,
    hosts: &'static [&'static str],
    limiter: RateLimiter,
    max_bytes: usize,
}

impl SourceHttp {
    pub fn new(hosts: &'static [&'static str], interval: Duration, max_bytes: usize) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(8))
            .user_agent(
                "any-player-sync-server/1.5 (DJ passage ingest; https://github.com/neboman11)",
            )
            .build()
            .expect("reqwest client");
        Self {
            client,
            hosts,
            limiter: RateLimiter::new(interval),
            max_bytes,
        }
    }

    fn check(&self, url: &str) -> Result<(), SourceError> {
        let parsed = reqwest::Url::parse(url).map_err(|e| SourceError::Permanent(e.to_string()))?;
        if parsed.scheme() != "https" || !parsed.host_str().is_some_and(|h| self.hosts.contains(&h))
        {
            // Drop the query string: it may carry an API key, even though this error is never
            // stored (Permanent errors are recorded as `no_match`, see worker::outcome_for).
            return Err(SourceError::Permanent(format!(
                "host not allowed: {}://{}{}",
                parsed.scheme(),
                parsed.host_str().unwrap_or(""),
                parsed.path()
            )));
        }
        Ok(())
    }

    async fn get_bytes(&self, url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>, SourceError> {
        self.check(url)?;
        self.limiter.wait().await;
        let mut request = self.client.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let mut response = request
            .send()
            .await
            .map_err(|e| SourceError::Transient(e.without_url().to_string()))?;
        let status = response.status();
        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SourceError::Transient(format!("HTTP {status}")));
        }
        if !status.is_success() {
            return Err(SourceError::Permanent(format!("HTTP {status}")));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| SourceError::Transient(e.without_url().to_string()))?
        {
            if body.len() + chunk.len() > self.max_bytes {
                return Err(SourceError::Permanent("response too large".into()));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    pub async fn get_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<serde_json::Value, SourceError> {
        let body = self.get_bytes(url, headers).await?;
        serde_json::from_slice(&body).map_err(|e| SourceError::Permanent(format!("bad JSON: {e}")))
    }

    pub async fn get_text(&self, url: &str) -> Result<String, SourceError> {
        Ok(String::from_utf8_lossy(&self.get_bytes(url, &[]).await?).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_hosts_outside_the_allowlist_and_plain_http() {
        let http = SourceHttp::new(&["en.wikipedia.org"], Duration::ZERO, JSON_CAP);
        assert!(matches!(
            http.get_text("https://evil.example/x").await,
            Err(SourceError::Permanent(_))
        ));
        assert!(matches!(
            http.get_text("http://en.wikipedia.org/x").await,
            Err(SourceError::Permanent(_))
        ));
    }

    #[tokio::test]
    async fn network_errors_never_include_the_request_url_or_its_secrets() {
        // Port 1 refuses the connection immediately (nothing listens on a privileged port), so
        // this exercises the `send()` error path without waiting on the 8 s timeout.
        let http = SourceHttp::new(&["127.0.0.1"], Duration::ZERO, JSON_CAP);
        let err = http
            .get_text("https://127.0.0.1:1/x?api_key=SECRET")
            .await
            .unwrap_err();
        let message = match err {
            SourceError::Transient(m) | SourceError::Permanent(m) => m,
        };
        assert!(
            !message.contains("SECRET"),
            "message leaked a secret: {message}"
        );
        assert!(!message.contains("api_key"));
    }

    #[tokio::test]
    async fn limiter_spaces_calls() {
        let limiter = RateLimiter::new(Duration::from_millis(50));
        let start = tokio::time::Instant::now();
        for _ in 0..3 {
            limiter.wait().await;
        }
        assert!(start.elapsed() >= Duration::from_millis(100));
    }
}
