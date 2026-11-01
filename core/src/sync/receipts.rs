use super::*;

impl<T: Transport> Engine<T> {
    pub(super) fn load_receipts(&mut self, chat: &str) -> Result<()> {
        let key = (self.account.clone(), chat.to_owned());
        if !self.receipts.contains_key(&key) {
            self.receipts.insert(key, self.store.receipts(chat)?);
        }
        Ok(())
    }

    pub(super) fn decorate_receipts(&mut self, chat: &str, rows: &mut [Value]) -> Result<()> {
        self.load_receipts(chat)?;
        let key = (self.account.clone(), chat.to_owned());
        let receipts = self.receipts.get_mut(&key).unwrap();
        if receipts.observe(rows) {
            self.dirty_receipts.insert(key);
        }
        for row in rows {
            if row["mine"] == true
                && let Some(count) = receipts.counts.get(id(row))
            {
                // Unknown and known-zero both retain the sent-only indicator.
                row["readCount"] = json!(count);
            }
        }
        Ok(())
    }

    pub(super) fn emit_receipts(&mut self, chat: &str) -> Result<()> {
        self.load_receipts(chat)?;
        let receipts = &self.receipts[&(self.account.clone(), chat.to_owned())];
        self.transport
            .emit(json!({"event":"read_counts", "account":self.account,
            "chatMid":chat,"counts":receipts.counts}));
        Ok(())
    }

    pub(super) fn queue_receipts(&mut self, chat: &str, generation: u64) {
        if !chat.starts_with(['u', 'c', 'r']) {
            return;
        }
        if self
            .receipts_synced
            .get(chat)
            .is_some_and(|at| at.elapsed() < Duration::from_secs(10))
        {
            return;
        }
        if self.queued_receipts.insert(chat.into()) {
            self.background.push_front(Work::Receipts {
                chat: chat.into(),
                generation,
            });
        }
    }

    pub(super) fn sync_receipts(&mut self, chat: &str, generation: u64) -> Result<()> {
        self.valid(generation)?;
        self.receipts_synced.insert(chat.into(), Instant::now());
        let result = self
            .transport
            .call("bridge_read_ranges", json!({"chatMid":chat}), generation);
        self.valid(generation)?;
        self.load_receipts(chat)?;
        let key = (self.account.clone(), chat.to_owned());
        match result {
            Ok(value)
                if value
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(|r| r["chatId"] == chat)) =>
            {
                for row in value
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|r| r["chatId"] == chat)
                {
                    if self
                        .receipts
                        .get_mut(&key)
                        .unwrap()
                        .merge_ranges(&row["ranges"], &self.account)
                    {
                        self.dirty_receipts.insert(key.clone());
                    }
                }
            }
            // Device/API incompatibility: refresh count snapshots for the latest
            // page independently of the history cache, without downloading media.
            _ => {
                let value = self.transport.call(
                    "bridge_fetch_page",
                    json!({"chatMid":chat,"before":null}),
                    generation,
                )?;
                self.valid(generation)?;
                let mut page: Page = serde_json::from_value(value)?;
                for row in &mut page.messages {
                    *row = crate::message::from_envelope(row, &self.account);
                }
                self.decorate_receipts(chat, &mut page.messages)?;
            }
        }
        self.emit_receipts(chat)
    }

    pub(super) fn flush_receipts(&mut self) -> Result<()> {
        let rows = self
            .dirty_receipts
            .iter()
            .map(|key| {
                Ok((
                    key.0.clone(),
                    key.1.clone(),
                    serde_json::to_string(&self.receipts[key])?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        self.store.save_receipts(&rows)?;
        self.dirty_receipts.clear();
        Ok(())
    }

    pub(super) fn clear_receipts(&mut self) {
        self.receipts
            .retain(|(account, _), _| account != &self.account);
        self.dirty_receipts
            .retain(|(account, _)| account != &self.account);
        self.receipts_synced.clear();
        self.queued_receipts.clear();
        self.background
            .retain(|work| !matches!(work, Work::Receipts { .. }));
    }
}
