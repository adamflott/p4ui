//! Indices only: avoid duplicating file text or formatting off-screen rows.
use crate::model::Item;

#[derive(Default)]
pub(crate) struct Rows {
    identity: Option<(usize, usize, String)>,
    pub indices: Vec<usize>,
    /// Positions in `indices`, in the same order as the shared selection list.
    pub panes: [Vec<usize>; 2],
}
impl Rows {
    pub fn invalidate(&mut self) {
        self.identity = None;
    }
    pub fn update(&mut self, items: &[Item], filter: &str) {
        if self.identity.as_ref().is_some_and(|(ptr, len, key)| {
            *ptr == items.as_ptr() as usize && *len == items.len() && key == filter
        }) {
            return;
        }
        self.indices.clear();
        self.panes.iter_mut().for_each(Vec::clear);
        let needle = filter.to_lowercase();
        for (index, item) in items.iter().enumerate() {
            if !needle.is_empty() && !item.search_text().to_lowercase().contains(&needle) {
                continue;
            }
            if let Item::File(file) = item {
                self.panes[usize::from(!file.opened)].push(self.indices.len());
            }
            self.indices.push(index);
        }
        self.identity = Some((items.as_ptr() as usize, items.len(), filter.to_owned()));
    }
}

const LINE_BLOCK: usize = 128;
#[derive(Default)]
pub(crate) struct Lines {
    identity: Option<(usize, usize)>,
    /// One byte offset per 128 lines; at most 1/16 the text size even for empty lines.
    checkpoints: Vec<usize>,
}
impl Lines {
    pub fn invalidate(&mut self) {
        self.identity = None;
    }
    pub fn window<'a>(&mut self, text: &'a str, first: usize, count: usize) -> Vec<&'a str> {
        let identity = (text.as_ptr() as usize, text.len());
        if self.identity != Some(identity) {
            self.checkpoints.clear();
            let mut offset = 0;
            for (line, value) in text.split_inclusive('\n').enumerate() {
                if line % LINE_BLOCK == 0 {
                    self.checkpoints.push(offset);
                }
                offset += value.len();
            }
            self.identity = Some(identity);
        }
        self.checkpoints
            .get(first / LINE_BLOCK)
            .map(|offset| {
                text[*offset..]
                    .lines()
                    .skip(first % LINE_BLOCK)
                    .take(count)
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparse_line_windows_preserve_crlf_unicode_and_trailing_line_semantics() {
        let text = (0..100_000)
            .map(|n| format!("λ line {n}\r\n"))
            .collect::<String>();
        let mut lines = Lines::default();
        assert_eq!(
            lines.window(&text, 60_000, 3),
            ["λ line 60000", "λ line 60001", "λ line 60002"]
        );
        assert!(lines.checkpoints.len() < 800);
        assert!(lines.window(&text, 100_000, 3).is_empty());
        assert_eq!(lines.window("one\ntwo", 1, 4), ["two"]);
        assert_eq!(lines.window("one\ntwo\n", 1, 4), ["two"]);
        assert!(lines.window("", 0, 4).is_empty());
    }
}
