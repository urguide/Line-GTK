use super::*;
use crate::expression_jobs::DownloadJob;

const JOBS: &str = "expression_download_jobs";

impl<T: Transport> Engine<T> {
    pub(super) fn schedule_expressions(&mut self, generation: u64) {
        if !self.expression_scheduled && self.expression_jobs.iter().any(DownloadJob::pending) {
            self.expression_scheduled = true;
            self.background.push_back(Work::Expression { generation });
        }
    }

    pub(super) fn restore_expression_jobs(&mut self, generation: u64) -> Result<()> {
        self.expression_jobs =
            serde_json::from_value(self.store.catalog(JOBS, i64::MAX)?.unwrap_or(json!([])))?;
        // Keep the active pack ahead of queued packs. Its saved item cursor is
        // resumed; a crash between file rename and checkpoint simply reuses disk.
        self.schedule_expressions(generation);
        Ok(())
    }

    fn job_state(&self) -> Value {
        json!({"account":self.account,"jobs":self.expression_jobs.iter().map(|job| {
            json!({"kind":job.kind,"package":job.package,"name":job.product["name"],
                "status":job.status,"done":job.next,"total":job.pack["stickers"].as_array().map(Vec::len).unwrap_or(0),"error":job.error})
        }).collect::<Vec<_>>()})
    }

    fn save_jobs(&self) -> Result<()> {
        self.store
            .save_catalog(JOBS, &serde_json::to_value(&self.expression_jobs)?)
    }

    fn emit_jobs(&self, catalog: Option<(&str, Value)>) {
        let mut event = self.job_state();
        event["event"] = json!("expression_state");
        if let Some((kind, catalog)) = catalog {
            event["kind"] = json!(kind);
            event["catalog"] = catalog;
        }
        self.transport.emit(event);
    }

    fn local_expressions(&self, kind: &str, generation: u64) -> Result<Value> {
        let installed = self
            .store
            .catalog(&format!("expression_installed_{kind}"), i64::MAX)?
            .or(if kind == "sticker" {
                self.store.stale_catalog("list_stickers", i64::MAX)?
            } else {
                None
            })
            .unwrap_or(json!({"packs":[]}));
        let owned = self
            .store
            .catalog(&format!("expression_owned_{kind}"), i64::MAX)?
            .unwrap_or_default();
        let products = owned["products"].as_array().cloned().unwrap_or_else(|| {
            installed["packs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["id"] != "__recent__")
                .map(|p| json!({"id":p["id"],"name":p["name"]}))
                .collect()
        });
        let recent = if kind == "sticker" {
            self.store
                .catalog("recent_stickers", i64::MAX)?
                .unwrap_or(json!([]))
        } else {
            json!([])
        };
        let mut result = self.transport.application(
            "expression_library",
            json!({"kind":kind,"installed":installed,"products":products,"recent":recent}),
            generation,
        )?;
        self.valid(generation)?;
        result["account"] = json!(self.account);
        Ok(result)
    }

    pub(super) fn expression_operation(
        &mut self,
        method: &str,
        params: &Value,
        generation: u64,
    ) -> Result<Value> {
        let kind = if params["kind"] == "emoji" {
            "emoji"
        } else {
            "sticker"
        };
        match method {
            "expression_local" => return self.local_expressions(kind, generation),
            "enqueue_expressions" => {
                let owned = self
                    .store
                    .catalog(&format!("expression_owned_{kind}"), i64::MAX)?
                    .context("purchase list unavailable; refresh purchases first")?;
                let ids = params["packages"].as_array().context("missing packages")?;
                // Validate the whole request before mutating the durable queue.
                let products: Vec<Value> = ids
                    .iter()
                    .map(|id| {
                        owned["products"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|p| p["id"] == *id)
                            .cloned()
                            .context("product not owned")
                    })
                    .collect::<Result<_>>()?;
                let mut jobs = self.expression_jobs.clone();
                let local = self.local_expressions(kind, generation)?;
                for product in products {
                    let package = crate::message::string(&product["id"]);
                    if local["products"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|p| p["id"] == package && p["downloaded"] == true)
                    {
                        continue;
                    }
                    if let Some(job) = jobs
                        .iter_mut()
                        .find(|j| j.kind == kind && j.package == package)
                    {
                        if job.pending() {
                            continue;
                        }
                        job.status = "queued".into();
                        job.error = None;
                        job.product = product;
                        // Recheck every file on manual retry (cache may have been cleared).
                        job.next = 0;
                        job.pack = Value::Null;
                    } else {
                        jobs.push(DownloadJob {
                            kind: kind.into(),
                            package,
                            product,
                            status: "queued".into(),
                            pack: Value::Null,
                            next: 0,
                            error: None,
                        });
                    }
                }
                self.store
                    .save_catalog(JOBS, &serde_json::to_value(&jobs)?)?;
                self.expression_jobs = jobs;
                self.schedule_expressions(generation);
            }
            "stop_expressions" => {
                let mut jobs = self.expression_jobs.clone();
                for job in &mut jobs {
                    if job.kind == kind && job.status == "queued" {
                        job.status = "cancelled".into();
                    }
                }
                self.store
                    .save_catalog(JOBS, &serde_json::to_value(&jobs)?)?;
                self.expression_jobs = jobs;
            }
            _ => {}
        }
        Ok(self.job_state())
    }

    pub(super) fn expressions_cache_cleared(&mut self, generation: u64) -> Result<()> {
        for job in &mut self.expression_jobs {
            if job.pending() {
                job.pack = Value::Null;
                job.next = 0;
            }
        }
        self.save_jobs()?;
        for kind in ["sticker", "emoji"] {
            if let Ok(catalog) = self.local_expressions(kind, generation) {
                self.emit_jobs(Some((kind, catalog)));
            }
        }
        self.schedule_expressions(generation);
        Ok(())
    }

    pub(super) fn expression_step(&mut self, generation: u64) -> Result<()> {
        self.valid(generation)?;
        let Some(index) = self
            .expression_jobs
            .iter()
            .position(|j| j.status == "downloading")
            .or_else(|| self.expression_jobs.iter().position(DownloadJob::pending))
        else {
            return Ok(());
        };
        let mut job = self.expression_jobs[index].clone();
        job.status = "downloading".into();
        let result = (|| -> Result<bool> {
            if job.pack.is_null() {
                job.pack = self.transport.application(
                    "expression_prepare",
                    json!({"kind":job.kind,"product":job.product}),
                    generation,
                )?;
                self.valid(generation)?;
                let items = job.pack["stickers"].as_array().context("invalid pack")?;
                if items.is_empty() {
                    bail!("empty pack");
                }
                job.next = 0;
                return Ok(false);
            }
            let items = job.pack["stickers"]
                .as_array_mut()
                .context("invalid pack")?;
            if let Some(item) = items.get_mut(job.next) {
                let value = self.transport.application(
                    "expression_item",
                    json!({"kind":job.kind,"packageId":job.package,"stickerId":item["stickerId"]}),
                    generation,
                )?;
                self.valid(generation)?;
                let path = value["imagePath"]
                    .as_str()
                    .filter(|p| !p.is_empty())
                    .context("download returned no image")?;
                item["imagePath"] = json!(path);
                job.next += 1;
            }
            if job.next < items.len() {
                return Ok(false);
            }
            job.pack["iconPath"] = job.pack["stickers"][0]["imagePath"].clone();
            job.pack["downloaded"] = json!(true);
            let key = format!("expression_installed_{}", job.kind);
            let mut installed = self
                .store
                .catalog(&key, i64::MAX)?
                .or(if job.kind == "sticker" {
                    self.store.stale_catalog("list_stickers", i64::MAX)?
                } else {
                    None
                })
                .unwrap_or(json!({"packs":[]}));
            let packs = installed["packs"]
                .as_array_mut()
                .context("invalid installed library")?;
            packs.retain(|p| p["id"] != job.pack["id"] && p["id"] != "__recent__");
            packs.push(job.pack.clone());
            self.store.save_catalog(&key, &installed)?;
            Ok(true)
        })();
        // Interrupted generations retain the previous durable checkpoint for restart.
        self.valid(generation)?;
        let completed = matches!(result, Ok(true));
        match result {
            Ok(true) => {
                job.status = "done".into();
                job.pack = Value::Null;
            }
            Ok(false) => {}
            Err(error) => {
                job.status = "failed".into();
                job.error = Some(error.to_string());
            }
        }
        let kind = job.kind.clone();
        self.expression_jobs[index] = job;
        self.save_jobs()?;
        let catalog = if completed {
            self.local_expressions(&kind, generation).ok()
        } else {
            None
        };
        self.emit_jobs(catalog.map(|c| (kind.as_str(), c)));
        self.schedule_expressions(generation);
        Ok(())
    }
}
