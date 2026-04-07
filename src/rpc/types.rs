use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcRequest<'a> {
    pub jsonrpc: &'static str,
    pub id: String,
    pub method: &'a str,
    pub params: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcResponse<T> {
    #[serde(rename = "id")]
    pub _id: Option<String>,
    #[serde(rename = "jsonrpc")]
    pub _jsonrpc: Option<String>,
    pub result: Option<T>,
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Aria2GlobalStat {
    #[serde(rename = "downloadSpeed")]
    pub download_speed: String,
    #[serde(rename = "uploadSpeed")]
    pub upload_speed: String,
    #[serde(rename = "numActive")]
    pub num_active: String,
    #[serde(rename = "numWaiting")]
    pub num_waiting: String,
    #[serde(rename = "numStopped")]
    pub num_stopped: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Aria2Status {
    pub gid: String,
    pub status: String,
    #[serde(rename = "totalLength")]
    pub total_length: String,
    #[serde(rename = "completedLength")]
    pub completed_length: String,
    #[serde(rename = "downloadSpeed")]
    pub download_speed: String,
    #[serde(rename = "uploadSpeed")]
    pub upload_speed: String,
    #[serde(rename = "connections")]
    pub connections: Option<String>,
    #[serde(rename = "errorCode")]
    pub error_code: Option<String>,
    #[serde(rename = "errorMessage")]
    pub error_message: Option<String>,
    #[serde(rename = "infoHash")]
    pub info_hash: Option<String>,
    #[serde(rename = "numSeeders")]
    pub num_seeders: Option<String>,
    #[serde(rename = "followedBy")]
    pub followed_by: Option<Vec<String>>,
    #[serde(rename = "belongsTo")]
    pub belongs_to: Option<String>,
    pub files: Option<Vec<Aria2File>>,
    #[serde(rename = "bittorrent")]
    pub _bittorrent: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Aria2File {
    pub path: Option<String>,
    #[serde(rename = "selected")]
    pub _selected: Option<String>,
    pub uris: Option<Vec<Aria2Uri>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Aria2Uri {
    pub uri: String,
    pub status: String,
}
