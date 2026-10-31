use anyhow::Result;
use serde_json::{Value, json};

/// Keep compatibility fallback in Rust, with exactly one encrypted text send.
pub fn send(params: &Value, mut call: impl FnMut(&str, Value) -> Result<Value>) -> Result<Value> {
    match call("bridge_postback", params.clone()) {
        Ok(result) => Ok(result),
        Err(_) => {
            let text = ["data", "uri"]
                .iter()
                .find_map(|key| params[key].as_str().filter(|s| !s.is_empty()))
                .unwrap_or("OK");
            let mut result = call(
                "bridge_send_text",
                json!({"chatMid":params["chatMid"],"text":text,"e2ee":true}),
            )?;
            result["sent"] = json!(true);
            result["fallback"] = json!("message");
            Ok(result)
        }
    }
}
