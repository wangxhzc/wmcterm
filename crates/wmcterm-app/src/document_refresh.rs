use super::*;

type Backend = Box<dyn SurfaceBackend>;
type RefreshResult = (SurfaceId, Result<(Backend, bool), String>);

pub(super) struct DocumentRefresh {
    requests: flume::Sender<(SurfaceId, Backend)>,
    results: flume::Receiver<RefreshResult>,
    pending: HashSet<SurfaceId>,
    last_poll: std::time::Instant,
    scheduled: HashMap<SurfaceId, u64>,
    sequence: u64,
}

impl DocumentRefresh {
    pub(super) fn new() -> Result<Self> {
        let (requests, incoming) = flume::bounded::<(SurfaceId, Backend)>(4);
        let (completed, results) = flume::bounded(4);
        thread::Builder::new()
            .name("wmcterm-documents".into())
            .spawn(move || {
                while let Ok((id, mut backend)) = incoming.recv() {
                    let result = backend
                        .refresh()
                        .map(|changed| (backend, changed))
                        .map_err(|error| error.to_string());
                    if completed.send((id, result)).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests,
            results,
            pending: HashSet::new(),
            last_poll: std::time::Instant::now(),
            scheduled: HashMap::new(),
            sequence: 0,
        })
    }

    pub(super) fn poll(&mut self, backends: &mut HashMap<SurfaceId, Backend>) -> bool {
        let mut changed = false;
        while let Ok((id, result)) = self.results.try_recv() {
            self.pending.remove(&id);
            let Some(current) = backends.get_mut(&id) else {
                continue;
            };
            match result {
                Ok((mut updated, content_changed)) => {
                    // Filters can be edited while the worker reads the file.
                    if let Some(current) = current.as_any().downcast_ref::<LogSurface>()
                        && let Some(updated) = updated.as_any_mut().downcast_mut::<LogSurface>()
                    {
                        updated.set_filter(current.filter().clone());
                    }
                    *current = updated;
                    changed |= content_changed;
                }
                Err(error) => tracing::warn!(%error, "refreshing document surface failed"),
            }
        }
        if self.last_poll.elapsed() < std::time::Duration::from_secs(1) {
            return changed;
        }
        self.last_poll = std::time::Instant::now();
        self.scheduled.retain(|id, _| backends.contains_key(id));
        let mut candidates: Vec<_> = backends.iter().collect();
        candidates.sort_by_key(|(id, _)| self.scheduled.get(id).copied().unwrap_or(0));
        for (&id, backend) in candidates {
            if self.pending.contains(&id) || self.requests.is_full() {
                continue;
            }
            let copy: Backend =
                if let Some(markdown) = backend.as_any().downcast_ref::<MarkdownSurface>() {
                    Box::new(markdown.clone())
                } else if let Some(log) = backend.as_any().downcast_ref::<LogSurface>() {
                    Box::new(log.clone())
                } else {
                    continue;
                };
            if self.requests.try_send((id, copy)).is_ok() {
                self.pending.insert(id);
                self.sequence = self.sequence.saturating_add(1);
                self.scheduled.insert(id, self.sequence);
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_updates_content_without_overwriting_filter_edits() {
        let path = env::temp_dir().join(format!("wmcterm-background-{}.log", uuid::Uuid::new_v4()));
        std::fs::write(&path, "INFO before\n").unwrap();
        let id = SurfaceId::new_v4();
        let mut backends: HashMap<SurfaceId, Backend> = HashMap::from([(
            id,
            Box::new(LogSurface::open(&path, 10).unwrap()) as Backend,
        )]);
        let mut refresh = DocumentRefresh::new().unwrap();
        std::fs::write(&path, "WARN changed\n").unwrap();
        refresh.last_poll -= std::time::Duration::from_secs(2);
        refresh.poll(&mut backends);
        backends
            .get_mut(&id)
            .unwrap()
            .as_any_mut()
            .downcast_mut::<LogSurface>()
            .unwrap()
            .set_filter(LogFilter {
                query: Some("changed".into()),
                levels: None,
            });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !refresh.poll(&mut backends) {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(std::time::Duration::from_millis(5));
        }
        let backend = backends[&id].as_any().downcast_ref::<LogSurface>().unwrap();
        assert_eq!(backend.filter().query.as_deref(), Some("changed"));
        assert_eq!(backend.entries().back().unwrap().text, "WARN changed");
        std::fs::remove_file(path).unwrap();
    }
}
