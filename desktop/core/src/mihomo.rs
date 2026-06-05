use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct Controller {
    pub client: reqwest::Client,
    pub base: String,
    pub secret: String,
}

impl Controller {
    pub fn new(base: String, secret: String) -> Self {
        Self { client: reqwest::Client::new(), base, secret }
    }

    pub async fn alive(&self) -> bool {
        match self
            .client
            .get(format!("{}/version", self.base))
            .bearer_auth(&self.secret)
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            Ok(r) => r.status().as_u16() == 200,
            Err(_) => false,
        }
    }

    pub async fn proxies(&self) -> Vec<Value> {
        let fetch = async {
            let r = self
                .client
                .get(format!("{}/proxies", self.base))
                .bearer_auth(&self.secret)
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .ok()?;
            let j: Value = r.json().await.ok()?;
            Some(filter_ch_proxies(&j))
        };
        fetch.await.unwrap_or_default()
    }

    pub async fn connections(&self) -> Value {
        let fetch = async {
            let r = self
                .client
                .get(format!("{}/connections", self.base))
                .bearer_auth(&self.secret)
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .ok()?;
            r.json::<Value>().await.ok()
        };
        fetch
            .await
            .unwrap_or_else(|| json!({"connections": [], "downloadTotal": 0, "uploadTotal": 0}))
    }
}

/// 命门 #7:只保留 ch-* 通道节点,形状 {name,alive,type}。
pub fn filter_ch_proxies(j: &Value) -> Vec<Value> {
    let mut out = vec![];
    if let Some(map) = j.get("proxies").and_then(|p| p.as_object()) {
        for (name, p) in map {
            if name.starts_with("ch-") {
                out.push(json!({
                    "name": name,
                    "alive": p.get("alive").and_then(|a| a.as_bool()).unwrap_or(false),
                    "type": p.get("type").cloned().unwrap_or(Value::Null),
                }));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filter_ch_only() {
        let j = json!({
            "proxies": {
                "ch-abc": {"alive": true,  "type": "Socks5"},
                "ch-def": {"alive": false, "type": "Socks5"},
                "DIRECT": {"alive": true,  "type": "Direct"},
                "vpn-router": {"alive": true, "type": "Selector"}
            }
        });
        let out = filter_ch_proxies(&j);
        assert_eq!(out.len(), 2, "only ch-* nodes (命门 #7)");
        assert!(out.iter().all(|p| p["name"].as_str().unwrap().starts_with("ch-")));
        let abc = out.iter().find(|p| p["name"] == "ch-abc").unwrap();
        assert_eq!(abc["alive"], true);
        assert_eq!(abc["type"], "Socks5");
    }

    #[test]
    fn filter_empty_when_no_proxies_key() {
        assert!(filter_ch_proxies(&json!({})).is_empty());
    }

    #[tokio::test]
    async fn alive_false_against_dead_controller() {
        let c = Controller::new("http://127.0.0.1:1".into(), "".into());
        assert!(!c.alive().await);
        assert!(c.proxies().await.is_empty());
        let conns = c.connections().await;
        assert_eq!(conns["connections"], serde_json::json!([]));
    }
}
