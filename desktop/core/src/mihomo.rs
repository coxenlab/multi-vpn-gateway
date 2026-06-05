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
}
