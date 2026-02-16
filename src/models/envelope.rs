use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEnvelope {
    pub version: u8,
    pub request_id: String,
    pub session_id: String,
    pub session_name: String,
    pub tmux_pane: Option<String>,
    pub hook_event_name: String,
    pub blocking: bool,
    pub cwd: String,
    pub payload: Value,
}
