use std::time::Duration;

use color_eyre::eyre::{Context, Result, bail};
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::rpc::types::{JsonRpcRequest, JsonRpcResponse};

#[derive(Debug, Clone)]
pub struct Aria2RpcClient {
    client: Client,
    endpoint: String,
    secret: String,
}

impl Aria2RpcClient {
    pub fn new(endpoint: String, secret: String, timeout: Duration) -> Result<Self> {
        let client = Client::builder().timeout(timeout).build()?;
        Ok(Self {
            client,
            endpoint,
            secret,
        })
    }

    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Vec<Value>) -> Result<T> {
        let mut all_params = Vec::with_capacity(params.len() + 1);
        all_params.push(json!(format!("token:{}", self.secret)));
        all_params.extend(params);

        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id: format!("ariatui-{method}"),
            method,
            params: all_params,
        };

        let response = self
            .client
            .post(&self.endpoint)
            .json(&request)
            .send()
            .await
            .wrap_err_with(|| format!("rpc request failed for {method}"))?;

        let body: JsonRpcResponse<T> = response.json().await.wrap_err("invalid rpc response")?;
        if let Some(error) = body.error {
            bail!("aria2 rpc error {}: {}", error.code, error.message);
        }
        body.result
            .ok_or_else(|| color_eyre::eyre::eyre!("missing rpc result"))
    }
}
