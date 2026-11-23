//! The task manager's rows and their ordering (WorkspaceTaskManager's
//! sortValue / sortRows / onSort), kept free of UI types so it can be tested.

use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Procs,
    Ports,
    Cpu,
    Rss,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

/// Clicking a column: the same one flips the direction; a new one starts
/// descending (biggest first), except the name, which starts ascending.
pub fn next_sort(current: (SortKey, SortDir), clicked: SortKey) -> (SortKey, SortDir) {
    let (key, dir) = current;
    if key == clicked {
        (key, if dir == SortDir::Asc { SortDir::Desc } else { SortDir::Asc })
    } else {
        (clicked, if clicked == SortKey::Name { SortDir::Asc } else { SortDir::Desc })
    }
}

/// One terminal in the task manager.
#[derive(Clone, Debug, PartialEq)]
pub struct TermRow {
    pub container_id: String,
    pub workspace_id: String,
    pub workspace_name: String,
    pub label: String,
    /// The second line: what runs, where, or user@host.
    pub sub: String,
    /// The shell's pid; None for SSH.
    pub pid: Option<i32>,
    pub ssh: bool,
    pub cpu_percent: f32,
    pub rss_bytes: u64,
    pub process_count: usize,
    pub port_count: usize,
}

fn compare(a: &TermRow, b: &TermRow, key: SortKey) -> Ordering {
    match key {
        SortKey::Name => a.label.to_lowercase().cmp(&b.label.to_lowercase()),
        SortKey::Procs => a.process_count.cmp(&b.process_count),
        SortKey::Ports => a.port_count.cmp(&b.port_count),
        SortKey::Cpu => a.cpu_percent.total_cmp(&b.cpu_percent),
        SortKey::Rss => a.rss_bytes.cmp(&b.rss_bytes),
    }
}

/// Stable: equal rows keep their layout order.
pub fn sort_rows(rows: &mut [TermRow], key: SortKey, dir: SortDir) {
    rows.sort_by(|a, b| {
        let o = compare(a, b, key);
        if dir == SortDir::Asc { o } else { o.reverse() }
    });
}

/// A workspace's line in the "By workspace" list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorkspaceTotals {
    pub terminals: usize,
    pub cpu_percent: f32,
    pub rss_bytes: u64,
    pub processes: usize,
    pub ports: usize,
}

pub fn totals<'a>(rows: impl IntoIterator<Item = &'a TermRow>) -> WorkspaceTotals {
    rows.into_iter().fold(WorkspaceTotals::default(), |mut t, r| {
        t.terminals += 1;
        t.cpu_percent += r.cpu_percent;
        t.rss_bytes += r.rss_bytes;
        t.processes += r.process_count;
        t.ports += r.port_count;
        t
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str, cpu: f32, rss: u64, procs: usize) -> TermRow {
        TermRow {
            container_id: label.into(),
            workspace_id: "w".into(),
            workspace_name: "W".into(),
            label: label.into(),
            sub: String::new(),
            pid: Some(1),
            ssh: false,
            cpu_percent: cpu,
            rss_bytes: rss,
            process_count: procs,
            port_count: 0,
        }
    }

    fn labels(rows: &[TermRow]) -> Vec<&str> {
        rows.iter().map(|r| r.label.as_str()).collect()
    }

    #[test]
    fn sorts_by_each_column() {
        let mut rows = vec![row("zsh", 1., 300, 1), row("Claude", 50., 100, 4), row("node", 12., 900, 2)];
        sort_rows(&mut rows, SortKey::Cpu, SortDir::Desc);
        assert_eq!(labels(&rows), ["Claude", "node", "zsh"]);
        sort_rows(&mut rows, SortKey::Rss, SortDir::Asc);
        assert_eq!(labels(&rows), ["Claude", "zsh", "node"]);
        sort_rows(&mut rows, SortKey::Name, SortDir::Asc);
        assert_eq!(labels(&rows), ["Claude", "node", "zsh"]);
        sort_rows(&mut rows, SortKey::Procs, SortDir::Desc);
        assert_eq!(labels(&rows), ["Claude", "node", "zsh"]);
    }

    #[test]
    fn ties_keep_their_order() {
        let mut rows = vec![row("b", 0., 1, 1), row("a", 0., 1, 1), row("c", 5., 1, 1)];
        sort_rows(&mut rows, SortKey::Cpu, SortDir::Desc);
        assert_eq!(labels(&rows), ["c", "b", "a"]);
    }

    #[test]
    fn header_clicks() {
        let start = (SortKey::Cpu, SortDir::Desc);
        assert_eq!(next_sort(start, SortKey::Cpu), (SortKey::Cpu, SortDir::Asc));
        assert_eq!(next_sort((SortKey::Cpu, SortDir::Asc), SortKey::Cpu), (SortKey::Cpu, SortDir::Desc));
        assert_eq!(next_sort(start, SortKey::Name), (SortKey::Name, SortDir::Asc));
        assert_eq!(next_sort((SortKey::Name, SortDir::Asc), SortKey::Rss), (SortKey::Rss, SortDir::Desc));
    }

    #[test]
    fn sums_a_workspace() {
        let rows = [row("a", 1.5, 100, 2), row("b", 2.5, 50, 3)];
        let t = totals(&rows);
        assert_eq!((t.terminals, t.cpu_percent, t.rss_bytes, t.processes), (2, 4., 150, 5));
    }
}
