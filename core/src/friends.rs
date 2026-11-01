//! Friend lookup policy; LINE RPC remains a thin protocol operation.
use anyhow::{Result, bail};
use serde_json::{Value, json};

pub fn add(userid: &str, mut call: impl FnMut(&str, Value) -> Result<Value>) -> Result<Value> {
    let userid = userid.trim();
    let userid = userid.strip_prefix('@').unwrap_or(userid);
    if userid.is_empty() {
        bail!("missing_userid");
    }
    let contact = call("bridge_find_contact", json!({"userid":userid}))?;
    let mid = contact["mid"]
        .as_str()
        .filter(|mid| !mid.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("user_not_found"))?;
    // Existing friends and differing request APIs may reject the request after
    // a successful lookup. Keep the profile without claiming a new friendship.
    let request = call("bridge_friend_request", json!({"mid":mid,"userid":userid}));
    Ok(
        json!({"mid":mid,"displayName":crate::catalog::name(&contact).unwrap_or(userid),
        "displayNameOverridden":crate::catalog::alias(&contact),
        "picturePath":crate::catalog::picture(&contact),
        "requestSent":request.is_ok(),"requestError":request.err().map(|e|e.to_string())}),
    )
}
