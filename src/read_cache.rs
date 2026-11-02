//! Short-lived, bounded caches owned exclusively by the serialized reader.
use crate::model::{Info, Item, Tab, View};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

pub(crate) const TTL: Duration = Duration::from_secs(10);
const VIEW_BUDGET: usize = 2 * 1024 * 1024;
const DETAIL_BUDGET: usize = 1024 * 1024;

#[derive(Default)]
pub(crate) struct ReadCache {
    info: Option<(Instant, Info)>,
    views: VecDeque<(Tab, usize, Instant, View, usize)>,
    details: VecDeque<(String, Instant, String)>,
}
impl ReadCache {
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub fn info(&self) -> Option<Info> {
        self.info
            .as_ref()
            .filter(|(time, _)| time.elapsed() < TTL)
            .map(|(_, info)| info.clone())
    }
    pub fn observe(&mut self, info: &Info) {
        if self.info.as_ref().is_some_and(|(_, old)| old != info) {
            self.clear();
        }
        if self.info().is_none() {
            self.info = Some((Instant::now(), info.clone()));
        }
    }
    pub fn view(&self, tab: Tab, limit: usize) -> Option<View> {
        let info = self.info()?;
        self.views
            .iter()
            .find(|(key, size, time, view, _)| {
                *key == tab && *size == limit && time.elapsed() < TTL && view.info == info
            })
            .map(|(_, _, _, view, _)| view.clone())
    }
    pub fn put_view(&mut self, tab: Tab, limit: usize, view: &View) {
        self.observe(&view.info);
        if matches!(tab, Tab::Status | Tab::Resolve) {
            return;
        }
        let bytes = view_bytes(view);
        if bytes > VIEW_BUDGET || view.items.len() > 1000 {
            return;
        }
        self.views
            .retain(|(key, _, time, _, _)| *key != tab && time.elapsed() < TTL);
        while self
            .views
            .iter()
            .map(|(_, _, _, _, bytes)| bytes)
            .sum::<usize>()
            + bytes
            > VIEW_BUDGET
        {
            self.views.pop_front();
        }
        self.views
            .push_back((tab, limit, Instant::now(), view.clone(), bytes));
    }
    pub fn detail(&self, key: &str) -> Option<String> {
        self.info()?;
        self.details
            .iter()
            .find(|(id, time, _)| id == key && time.elapsed() < TTL)
            .map(|(_, _, text)| text.clone())
    }
    pub fn put_detail(&mut self, key: String, text: &str) {
        if key.len() + text.len() > DETAIL_BUDGET {
            return;
        }
        self.details
            .retain(|(id, time, _)| id != &key && time.elapsed() < TTL);
        while self.details.len() >= 8
            || self
                .details
                .iter()
                .map(|(key, _, text)| key.len() + text.len())
                .sum::<usize>()
                + key.len()
                + text.len()
                > DETAIL_BUDGET
        {
            self.details.pop_front();
        }
        self.details
            .push_back((key, Instant::now(), text.to_owned()));
    }
}

fn view_bytes(view: &View) -> usize {
    let mut bytes =
        std::mem::size_of::<View>() + view.warnings.iter().map(String::len).sum::<usize>();
    for item in &view.items {
        bytes += std::mem::size_of::<Item>()
            + match item {
                Item::File(f) => [&f.depot, &f.local, &f.action, &f.change, &f.file_type]
                    .into_iter()
                    .map(String::len)
                    .sum::<usize>(),
                Item::Revision {
                    depot,
                    revision,
                    action,
                    change,
                    ..
                } => depot.len() + revision.len() + action.len() + change.len(),
                Item::Change {
                    id,
                    description,
                    user,
                    client,
                    status,
                } => [id, description, user, client, status]
                    .into_iter()
                    .map(String::len)
                    .sum(),
                Item::Stream { path, parent, kind } => path.len() + parent.len() + kind.len(),
                Item::Workspace { name, root, stream } => name.len() + root.len() + stream.len(),
                Item::Label { name, description } => name.len() + description.len(),
            };
    }
    bytes
        + [
            &view.info.user,
            &view.info.client,
            &view.info.root,
            &view.info.server,
            &view.info.stream,
        ]
        .into_iter()
        .map(String::len)
        .sum::<usize>()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn cache_memory_budgets_and_metadata_expiry_do_not_slide() {
        let mut cache = ReadCache::default();
        let view = View {
            items: vec![Item::Label {
                name: "large".into(),
                description: "x".repeat(512 * 1024),
            }],
            ..View::default()
        };
        for tab in [
            Tab::Log,
            Tab::Changes,
            Tab::Shelves,
            Tab::Files,
            Tab::Streams,
            Tab::Workspaces,
            Tab::Labels,
        ] {
            cache.put_view(tab, 200, &view);
            assert!(
                cache
                    .views
                    .iter()
                    .map(|(_, _, _, _, bytes)| bytes)
                    .sum::<usize>()
                    <= VIEW_BUDGET
            );
        }
        for index in 0..20 {
            cache.put_detail(index.to_string(), &"x".repeat(128 * 1024));
            assert!(cache.details.len() <= 8);
            assert!(
                cache
                    .details
                    .iter()
                    .map(|(key, _, text)| key.len() + text.len())
                    .sum::<usize>()
                    <= DETAIL_BUDGET
            );
        }
        cache.info.as_mut().unwrap().0 -= TTL / 2;
        let stamp = cache.info.as_ref().unwrap().0;
        cache.observe(&view.info);
        assert_eq!(cache.info.as_ref().unwrap().0, stamp);
    }
    #[test]
    fn caches_expire_and_reject_oversized_or_different_identity_results() {
        let mut cache = ReadCache::default();
        let view = View {
            info: Info {
                client: "test".into(),
                ..Info::default()
            },
            ..View::default()
        };
        cache.put_view(Tab::Files, 200, &view);
        cache.put_detail("Files:file".into(), "preview");
        assert!(cache.view(Tab::Files, 200).is_some());
        assert!(cache.view(Tab::Files, 201).is_none());
        assert!(cache.detail("Files:file").is_some());
        cache.info.as_mut().unwrap().0 -= TTL;
        assert!(cache.view(Tab::Files, 200).is_none());
        assert!(cache.detail("Files:file").is_none());
        cache.observe(&Info {
            client: "other".into(),
            ..Info::default()
        });
        assert!(cache.views.is_empty() && cache.details.is_empty());
        cache.put_detail("huge".into(), &"x".repeat(DETAIL_BUDGET + 1));
        assert!(cache.details.is_empty());
        cache.put_view(Tab::Status, 200, &view);
        assert!(cache.views.is_empty());
        cache.clear();
        assert!(cache.info().is_none());
    }
}
