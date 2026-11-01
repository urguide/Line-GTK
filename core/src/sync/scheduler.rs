use super::*;

pub(super) struct PageJob {
    pub id: u64,
    pub chat: String,
    pub before: Option<Cursor>,
    pub generation: u64,
    pub gap: Option<Gap>,
}

enum Completion {
    Operation {
        id: u64,
        generation: u64,
        result: Result<Value>,
    },
    Read {
        id: u64,
        chat: String,
        generation: u64,
        result: Result<Value>,
    },
    Page(PageJob, Result<Value>),
    Media {
        id: u64,
        chat: String,
        message: String,
        generation: u64,
        memory: bool,
        result: Result<Value>,
    },
    Receipts {
        chat: String,
        generation: u64,
        ranges: bool,
        result: Result<Value>,
    },
}

pub(super) struct Scheduler {
    pub enabled: bool,
    pub focus: String,
    pub pages: VecDeque<PageJob>,
    pub downloads: VecDeque<(u64, String, Value, u64)>,
    pub operations: VecDeque<(u64, String, Value, u64)>,
    pub reads: VecDeque<(u64, Value, u64)>,
    read_running: bool,
    operations_running: usize,
    pages_running: HashMap<(String, u64), Arc<std::sync::atomic::AtomicBool>>,
    media_running: HashSet<(String, String, u64)>,
    receipts_running: bool,
    tx: mpsc::Sender<Completion>,
    rx: mpsc::Receiver<Completion>,
}

impl Default for Scheduler {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            enabled: false,
            focus: String::new(),
            pages: VecDeque::new(),
            downloads: VecDeque::new(),
            operations: VecDeque::new(),
            reads: VecDeque::new(),
            read_running: false,
            operations_running: 0,
            pages_running: HashMap::new(),
            media_running: HashSet::new(),
            receipts_running: false,
            tx,
            rx,
        }
    }
}

impl Scheduler {
    pub fn focus(&mut self, chat: String) {
        for ((mid, _), cancel) in &self.pages_running {
            if mid != &chat {
                cancel.store(true, Ordering::SeqCst);
            }
        }
        self.focus = chat;
    }
    pub fn reset(&mut self) {
        // Detached jobs keep their old sender; their results cannot reach this session.
        for cancel in self.pages_running.values() {
            cancel.store(true, Ordering::SeqCst);
        }
        *self = Self::default();
    }

    pub fn clear_work(&mut self) {
        let enabled = self.enabled;
        let focus = self.focus.clone();
        self.reset();
        self.enabled = enabled;
        self.focus = focus;
    }
}

impl<T: Transport> Engine<T> {
    pub(super) fn start_mark_read(&mut self, id: u64, params: Value, generation: u64) {
        self.scheduler.read_running = true;
        let transport = self.transport.clone();
        let tx = self.scheduler.tx.clone();
        thread::spawn(move || {
            let chat = params["chatMid"].as_str().unwrap_or_default().to_owned();
            let message = params["lastMessageId"].as_str().unwrap_or_default();
            let result = if chat.is_empty() || message.is_empty() || message.parse::<u64>().is_err()
            {
                Ok(json!({"marked":false,"skipped":"missing_ids"}))
            } else {
                transport.call("bridge_mark_read", params, generation)
            };
            let _ = tx.send(Completion::Read {
                id,
                chat,
                generation,
                result,
            });
        });
    }
    fn page_ready(&mut self, job: &PageJob, mut page: Page) -> Result<Value> {
        self.valid(job.generation)?;
        self.decorate_receipts(&job.chat, &mut page.messages)?;
        self.queue_media(&job.chat, &page.messages, job.generation);
        if job.before.is_none() {
            self.queue_gap(&job.chat, job.generation);
        }
        for row in &mut page.messages {
            if self.transport.memory_media(&self.account, &job.chat)
                && matches!(row["contentType"].as_str(), Some("IMAGE" | "VIDEO"))
            {
                row["imagePath"] = Value::Null;
                row["filePath"] = Value::Null;
                row["needsMedia"] = json!(true);
            }
            if let Some(object) = row.as_object_mut() {
                object.remove("rawMessage");
                for key in ["imagePath", "audioPath", "filePath"] {
                    if object
                        .get(key)
                        .and_then(Value::as_str)
                        .is_some_and(|p| !std::path::Path::new(p).is_file())
                    {
                        object.insert(key.into(), Value::Null);
                    }
                }
            }
        }
        Ok(serde_json::to_value(page)?)
    }

    pub(super) fn finish_async(&mut self) {
        while let Ok(done) = self.scheduler.rx.try_recv() {
            match done {
                Completion::Operation {
                    id,
                    generation,
                    result,
                } => {
                    self.scheduler.operations_running =
                        self.scheduler.operations_running.saturating_sub(1);
                    if self.valid(generation).is_ok() {
                        self.response(id, result, generation);
                    }
                }
                Completion::Read {
                    id,
                    chat,
                    generation,
                    result,
                } => {
                    self.scheduler.read_running = false;
                    if self.valid(generation).is_err() {
                        continue;
                    }
                    let result =
                        result.unwrap_or_else(|e| json!({"marked":false,"error":e.to_string()}));
                    if result["marked"] == true {
                        let _ = self.store.mark_chat_read(&chat);
                    }
                    self.response(id, Ok(result), generation);
                }
                Completion::Page(job, result) => {
                    self.scheduler
                        .pages_running
                        .remove(&(job.chat.clone(), job.generation));
                    if result
                        .as_ref()
                        .is_err_and(|e| e.to_string() == "chat_deferred")
                        && self.valid(job.generation).is_ok()
                    {
                        self.scheduler.pages.push_back(job);
                        continue;
                    }
                    let result = (|| {
                        self.valid(job.generation)?;
                        let mut page: Page = serde_json::from_value(result?)?;
                        if page.messages.len() > 20 {
                            bail!("protocol returned oversized history page");
                        }
                        if let Some(cursor) = &page.next_cursor {
                            cursor.validate()?;
                        }
                        for row in &mut page.messages {
                            *row = crate::message::from_envelope(row, &self.account);
                        }
                        let page = self
                            .store
                            .save_page(&job.chat, job.before.as_ref(), &page)?;
                        if let Some(gap) = &job.gap {
                            let done = gap.before.message_id == gap.target_id
                                || page.messages.iter().any(|m| id(m) == gap.target_id)
                                || !page.has_more
                                || page.next_cursor.is_none()
                                || page.next_cursor.as_ref() == Some(&gap.before);
                            self.store.advance(
                                &job.chat,
                                gap,
                                if done {
                                    None
                                } else {
                                    page.next_cursor.as_ref()
                                },
                            )?;
                            self.queue_gap(&job.chat, job.generation);
                        }
                        self.page_ready(&job, page)
                    })();
                    self.response(job.id, result, job.generation);
                }
                Completion::Media {
                    id: request_id,
                    chat,
                    message,
                    generation,
                    memory,
                    result,
                } => {
                    self.scheduler.media_running.remove(&(
                        chat.clone(),
                        message.clone(),
                        generation,
                    ));
                    self.queued_media.remove(&(chat.clone(), message.clone()));
                    let result = (|| {
                        self.valid(generation)?;
                        let mut updated = result?;
                        // Unsend while downloading must not resurrect a message.
                        let current = self
                            .store
                            .message(&chat, &message)?
                            .context("message unavailable")?;
                        let memory_now = self.transport.memory_media(&self.account, &chat)
                            && matches!(current["contentType"].as_str(), Some("IMAGE" | "VIDEO"));
                        if memory && !self.transport.media_visible(&chat, &message) {
                            bail!("media_not_visible");
                        }
                        if !memory && !memory_now {
                            // Preserve newer metadata/receipts; only merge downloaded paths.
                            let mut merged = current;
                            for key in ["imagePath", "audioPath", "filePath", "needsMedia"] {
                                if let Some(value) = updated.get(key) {
                                    merged[key] = value.clone();
                                }
                            }
                            updated = merged;
                            self.store
                                .save_messages(&chat, std::slice::from_ref(&updated))?;
                        }
                        self.transport.emit(json!({"event":"media_ready","chatMid":chat,"messageId":message,"imagePath":updated["imagePath"],"audioPath":updated["audioPath"],"filePath":updated["filePath"]}));
                        Ok(
                            json!({"path":updated["filePath"],"fileName":updated["fileName"],"contentType":updated["contentType"]}),
                        )
                    })();
                    if result.is_err() && request_id == 0 && self.valid(generation).is_ok() {
                        self.transport.emit(
                            json!({"event":"media_failed","chatMid":chat,"messageId":message}),
                        );
                    }
                    self.response(request_id, result, generation);
                }
                Completion::Receipts {
                    chat,
                    generation,
                    ranges,
                    result,
                } => {
                    self.scheduler.receipts_running = false;
                    let result = (|| {
                        self.valid(generation)?;
                        let value = result?;
                        if ranges {
                            self.load_receipts(&chat)?;
                            let key = (self.account.clone(), chat.clone());
                            for row in value
                                .as_array()
                                .into_iter()
                                .flatten()
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
                        } else {
                            let mut page: Page = serde_json::from_value(value)?;
                            for row in &mut page.messages {
                                *row = crate::message::from_envelope(row, &self.account);
                            }
                            self.decorate_receipts(&chat, &mut page.messages)?;
                        }
                        self.emit_receipts(&chat)
                    })();
                    if let Err(error) = result {
                        eprintln!("[core receipts] {error}");
                    }
                }
            }
        }
    }

    pub(super) fn start_async(&mut self) -> bool {
        // Reserve a page lane for the selected chat. A slow previous chat never
        // occupies both slots; queued work is selected using the latest focus.
        let focus = &self.scheduler.focus;
        let candidate = self
            .scheduler
            .pages
            .iter()
            .position(|j| &j.chat == focus)
            .or_else(|| (!self.scheduler.pages.is_empty()).then_some(0));
        if let Some(index) = candidate {
            let job = &self.scheduler.pages[index];
            let capacity = if job.chat == *focus { 2 } else { 1 };
            if self.scheduler.pages_running.len() < capacity
                && !self
                    .scheduler
                    .pages_running
                    .contains_key(&(job.chat.clone(), job.generation))
            {
                let job = self.scheduler.pages.remove(index).unwrap();
                let cached = self.valid(job.generation).and_then(|_| {
                    if let Some(cursor) = &job.before {
                        cursor.validate()?;
                    }
                    if job.gap.is_some() {
                        Ok(None)
                    } else {
                        self.store
                            .page(&job.chat, job.before.as_ref(), self.policy.messages)
                    }
                });
                match cached {
                    Ok(Some(page)) => {
                        let result = self.page_ready(&job, page);
                        self.response(job.id, result, job.generation);
                    }
                    Err(error) => self.response(job.id, Err(error), job.generation),
                    Ok(None) => {
                        let tx = self.scheduler.tx.clone();
                        let transport = self.transport.clone();
                        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                        self.scheduler
                            .pages_running
                            .insert((job.chat.clone(), job.generation), cancel.clone());
                        thread::spawn(move || {
                            let result = transport.call_cancellable(
                                "bridge_fetch_page",
                                json!({"chatMid":job.chat,"before":job.before}),
                                job.generation,
                                &cancel,
                            );
                            let _ = tx.send(Completion::Page(job, result));
                        });
                    }
                }
                return true;
            }
        }
        if self.scheduler.operations_running < 2
            && let Some((id, method, mut params, generation)) =
                self.scheduler.operations.pop_front()
        {
            params["account"] = json!(self.account);
            self.scheduler.operations_running += 1;
            let transport = self.transport.clone();
            let tx = self.scheduler.tx.clone();
            thread::spawn(move || {
                let result = transport.application(&method, params, generation);
                let _ = tx.send(Completion::Operation {
                    id,
                    generation,
                    result,
                });
            });
            return true;
        }
        if !self.scheduler.read_running
            && let Some((id, params, generation)) = self.scheduler.reads.pop_front()
        {
            self.start_mark_read(id, params, generation);
            return true;
        }
        let focus = &self.scheduler.focus;
        let downloads = self
            .scheduler
            .downloads
            .iter()
            .position(|(_, chat, _, _)| chat == focus)
            .or_else(|| (!self.scheduler.downloads.is_empty()).then_some(0));
        if let Some(index) = downloads {
            let (_, chat, row, generation) = &self.scheduler.downloads[index];
            let capacity = if chat == focus { 2 } else { 1 };
            if self.scheduler.media_running.len() < capacity
                && !self.scheduler.media_running.contains(&(
                    chat.clone(),
                    id(row).into(),
                    *generation,
                ))
            {
                let (id, chat, row, generation) = self.scheduler.downloads.remove(index).unwrap();
                self.start_media(id, chat, row, generation);
                return true;
            }
        }
        // No media or history hydration ahead of the current conversation's page.
        if self.scheduler.pages.iter().any(|j| j.chat == *focus)
            || self
                .scheduler
                .pages_running
                .keys()
                .any(|(chat, _)| chat == focus)
        {
            return false;
        }
        let media = self
            .background
            .iter()
            .position(|w| matches!(w, Work::Media { chat, .. } if chat == focus))
            .or_else(|| {
                if self.scheduler.media_running.is_empty() {
                    self.background
                        .iter()
                        .position(|w| matches!(w, Work::Media { .. }))
                } else {
                    None
                }
            });
        if let Some(index) = media
            && self.scheduler.media_running.len() < 2
            && !self
                .scheduler
                .media_running
                .iter()
                .any(|(chat, _, _)| chat == focus)
        {
            let Work::Media {
                chat,
                row,
                generation,
            } = self.background.remove(index).unwrap()
            else {
                unreachable!()
            };
            self.start_media(0, chat, row, generation);
            return true;
        }
        if !self.scheduler.receipts_running
            && let Some(index) = self
                .background
                .iter()
                .position(|w| matches!(w, Work::Receipts { chat, .. } if chat == focus))
        {
            let Work::Receipts { chat, generation } = self.background.remove(index).unwrap() else {
                unreachable!()
            };
            self.queued_receipts.remove(&chat);
            self.receipts_synced.insert(chat.clone(), Instant::now());
            self.scheduler.receipts_running = true;
            let tx = self.scheduler.tx.clone();
            let transport = self.transport.clone();
            thread::spawn(move || {
                let result =
                    transport.call("bridge_read_ranges", json!({"chatMid":chat}), generation);
                let ranges = result.as_ref().is_ok_and(|value| {
                    value
                        .as_array()
                        .is_some_and(|rows| rows.iter().any(|r| r["chatId"] == chat))
                });
                let result = if ranges {
                    result
                } else {
                    transport.call(
                        "bridge_fetch_page",
                        json!({"chatMid":chat,"before":null}),
                        generation,
                    )
                };
                let _ = tx.send(Completion::Receipts {
                    chat,
                    generation,
                    ranges,
                    result,
                });
            });
            return true;
        }
        if self.scheduler.pages.is_empty()
            && self.scheduler.pages_running.is_empty()
            && self.scheduler.media_running.is_empty()
            && let Some(index) = self
                .background
                .iter()
                .position(|w| matches!(w, Work::Gap { .. }))
        {
            let Work::Gap { chat, generation } = self.background.remove(index).unwrap() else {
                unreachable!()
            };
            self.queued_gaps.remove(&chat);
            if self.valid(generation).is_ok()
                && !self.transport.memory_media(&self.account, &chat)
                && let Ok(Some(gap)) = self.store.gap(&chat)
            {
                self.scheduler.pages.push_back(PageJob {
                    id: 0,
                    chat,
                    before: Some(gap.before.clone()),
                    generation,
                    gap: Some(gap),
                });
            }
            return true;
        }
        false
    }

    fn start_media(&mut self, request_id: u64, chat: String, row: Value, generation: u64) {
        let message = id(&row).to_owned();
        self.background.retain(|w| !matches!(w, Work::Media { chat: mid, row, .. } if mid == &chat && id(row) == message));
        let memory = self.transport.memory_media(&self.account, &chat)
            && matches!(row["contentType"].as_str(), Some("IMAGE" | "VIDEO"));
        let result = self
            .valid(generation)
            .and_then(|_| self.store.message(&chat, &message))
            .and_then(|r| r.context("message unavailable"));
        let current = match result {
            Ok(mut row) if !memory || self.transport.media_visible(&chat, &message) => {
                row["chatMid"] = json!(chat);
                row
            }
            _ => {
                self.queued_media.remove(&(chat, message));
                self.response(
                    request_id,
                    Err(anyhow::anyhow!("message unavailable")),
                    generation,
                );
                return;
            }
        };
        self.scheduler
            .media_running
            .insert((chat.clone(), message.clone(), generation));
        let tx = self.scheduler.tx.clone();
        let transport = self.transport.clone();
        thread::spawn(move || {
            let result = transport.materialize(&current, generation);
            let _ = tx.send(Completion::Media {
                id: request_id,
                chat,
                message,
                generation,
                memory,
                result,
            });
        });
    }

    pub(super) fn next_background(&mut self) -> Option<Work> {
        // Downloads, receipts and gap pages use asynchronous lanes above.
        // Explicit expression batches only advance after page requests yield.
        if !self.scheduler.pages.is_empty() || !self.scheduler.pages_running.is_empty() {
            return None;
        }
        let index = self
            .background
            .iter()
            .position(|w| matches!(w, Work::Expression { .. }))?;
        self.background.remove(index)
    }
}
