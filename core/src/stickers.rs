//! Best-effort sticker catalogs. Failure of one package never hides other packs.
use crate::message::string;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};

fn numeric(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

pub fn package_version(owned: &Value, package: &str) -> String {
    let products = owned
        .as_array()
        .or_else(|| owned["productList"].as_array())
        .or_else(|| owned["1"].as_array());
    products
        .into_iter()
        .flatten()
        .find_map(|product| {
            let id = string(product.get("id").unwrap_or(&product["1"]));
            if id != package {
                return None;
            }
            let version = string(
                product
                    .get("latestVersion")
                    .or_else(|| product.get("version"))
                    .unwrap_or(&product["21"]),
            );
            (!version.trim().is_empty()).then(|| version.trim().to_owned())
        })
        .unwrap_or_else(|| "1".into())
}

pub fn not_owned(error: &str) -> bool {
    error.contains("USER_NOT_STICKER_OWNER") || error.to_ascii_lowercase().contains("not owned")
}

pub fn metadata(bytes: &[u8]) -> Result<Value> {
    let value: Value = serde_json::from_slice(bytes)?;
    if !value["stickers"].is_array() {
        bail!("invalid sticker package metadata");
    }
    Ok(value)
}

fn existing(path: &Value) -> Option<String> {
    path.as_str()
        .filter(|p| std::path::Path::new(p).is_file())
        .map(str::to_owned)
}

/// Recent is a synthetic tab. Real packages prefer their dedicated CDN icon;
/// failure falls back to the first sticker without discarding the package.
pub fn apply_pack_icons(catalog: &mut Value, mut icon: impl FnMut(&str) -> Result<String>) {
    for pack in catalog["packs"].as_array_mut().into_iter().flatten() {
        let package = string(&pack["id"]);
        if package == "__recent__" {
            continue;
        }
        let image = icon(&package).ok().or_else(|| {
            pack["stickers"]
                .as_array()
                .and_then(|rows| rows.first())
                .and_then(|s| s["imagePath"].as_str())
                .map(str::to_owned)
        });
        pack["iconPath"] = json!(image);
    }
}

pub fn load(
    owned: Result<Value>,
    previous: &Value,
    recent: &Value,
    mut fetch_metadata: impl FnMut(&str) -> Result<Vec<u8>>,
    mut image: impl FnMut(&str) -> Result<String>,
) -> Result<Value> {
    let old_packs: Vec<_> = previous["packs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["id"] != "__recent__")
        .cloned()
        .collect();
    let mut partial = false;
    let products = match owned.and_then(|v| {
        v["productList"]
            .as_array()
            .or_else(|| v["1"].as_array())
            .cloned()
            .context("invalid owned sticker response")
    }) {
        Ok(products) => products,
        Err(error) => {
            partial = true;
            let cached = previous["products"].as_array().cloned().or_else(|| {
                (!old_packs.is_empty()).then(|| {
                    old_packs
                        .iter()
                        .map(
                            |p| json!({"id":p["id"],"name":p["name"],"latestVersion":p["version"]}),
                        )
                        .collect()
                })
            });
            match cached {
                Some(products) => products,
                None => return Err(error),
            }
        }
    };
    let mut owned_ids = HashSet::new();
    let mut valid_products = Vec::new();
    for product in products {
        let id = string(product.get("id").unwrap_or(&product["1"]));
        if !numeric(&id) {
            partial = true;
            continue;
        }
        if owned_ids.insert(id.clone()) {
            valid_products.push(json!({"id":id,"name":product.get("name").unwrap_or(&product["11"]),"latestVersion":product.get("latestVersion").unwrap_or(&product["21"])}));
        }
    }
    let mut cached_images = BTreeMap::new();
    for pack in previous["packs"].as_array().into_iter().flatten() {
        for s in pack["stickers"].as_array().into_iter().flatten() {
            if let Some(path) = existing(&s["imagePath"]) {
                cached_images.insert((string(&s["packageId"]), string(&s["stickerId"])), path);
            }
        }
    }
    let mut resolve_image = |package: &str, id: &str| match image(id) {
        Ok(path) => Some(path),
        Err(_) => {
            partial = true;
            cached_images
                .get(&(package.to_owned(), id.to_owned()))
                .cloned()
        }
    };
    let mut packs = Vec::new();
    let mut recents = Vec::new();
    let mut seen = HashSet::new();
    for s in recent.as_array().into_iter().flatten() {
        let package = string(&s["packageId"]);
        let id = string(&s["stickerId"]);
        if !owned_ids.contains(&package)
            || !numeric(&id)
            || !seen.insert((package.clone(), id.clone()))
        {
            continue;
        }
        recents.push(json!({"packageId":package,"stickerId":id,"version":s["version"],"imagePath":resolve_image(&package,&id)}));
        if recents.len() == 24 {
            break;
        }
    }
    if !recents.is_empty() {
        packs.push(json!({"id":"__recent__","name":"Recent","recent":true,"iconPath":recents[0]["imagePath"],"stickers":recents}));
    }
    // Separate flags avoid borrowing partial while resolve_image is alive.
    let mut failed_package = false;
    for product in valid_products.iter().take(12) {
        let package = string(&product["id"]);
        let old = old_packs.iter().find(|p| p["id"] == package);
        let detail = fetch_metadata(&package).and_then(|bytes| metadata(&bytes));
        let (name, version, entries) = match detail {
            Ok(meta) => {
                let version = if meta["version"].is_null() {
                    string(&product["latestVersion"])
                } else {
                    string(&meta["version"])
                };
                let name = meta["title"]
                    .as_str()
                    .or_else(|| meta["title"]["en"].as_str())
                    .or_else(|| meta["title"]["th"].as_str())
                    .or_else(|| meta["title"]["ja"].as_str())
                    .or_else(|| product["name"].as_str())
                    .unwrap_or(&package)
                    .to_owned();
                (
                    name,
                    version,
                    meta["stickers"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|s| string(&s["id"]))
                        .collect::<Vec<_>>(),
                )
            }
            Err(_) => {
                failed_package = true;
                let Some(old) = old else {
                    continue;
                };
                (
                    old["name"].as_str().unwrap_or(&package).to_owned(),
                    string(&old["version"]),
                    old["stickers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|s| s["packageId"] == package)
                        .map(|s| string(&s["stickerId"]))
                        .collect(),
                )
            }
        };
        let mut stickers = Vec::new();
        let mut seen = HashSet::new();
        for id in entries.into_iter().take(48) {
            if !numeric(&id) {
                failed_package = true;
                continue;
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            stickers.push(json!({"stickerId":id,"packageId":package,"version":version,"imagePath":resolve_image(&package,&id)}));
        }
        if stickers.is_empty() {
            continue;
        }
        let icon = old
            .and_then(|p| existing(&p["iconPath"]))
            .or_else(|| stickers[0]["imagePath"].as_str().map(str::to_owned));
        packs.push(json!({"id":package,"name":name,"version":version,"recent":false,"iconPath":icon,"stickers":stickers}));
    }
    Ok(
        json!({"packs":packs,"products":valid_products,"ownedPackages":owned_ids.len(),"partial":partial||failed_package}),
    )
}
