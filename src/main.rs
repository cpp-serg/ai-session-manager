use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;

struct Session {
    file_path: PathBuf,
    project_path: String,
    title: String,
    search_text: String,
    started_at: String,
    git_branch: String,
    message_count: u32,
    mtime: SystemTime,
    /// For a subagent transcript, the session file that spawned it.
    parent: Option<PathBuf>,
    full_text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MatchMode {
    OnlyTitle,
    FullText,
}

/// How the filter text is matched. Plain is the default; fuzzy is opt-in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Matching {
    /// The filter as one literal substring, spaces included.
    Plain,
    /// nucleo's fzf-style syntax: space-separated fuzzy words, `'`, `^`, `$`, `!`.
    Fuzzy,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Only sessions whose cwd equals the directory the app was launched from.
    CurrentDir,
    /// Every session on the host.
    All,
}

/// Which pane owns the keyboard.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tree,
    Viewer,
}

/// Vim-style modality: keys are commands until `/` starts a filter edit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Filter,
    Rename,
}

/// A `claude -r` invocation the run loop should perform once it has released
/// the terminal. This replaces the process, so it never comes back.
struct Resume {
    /// Directory to run in: the session's own project path.
    cwd: String,
    /// Session id, which is the log file's stem.
    id: String,
}

/// How much of a directory node is marked for deletion.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    None,
    Some,
    All,
    /// Not marked itself, but inside a marked session's directory, so it goes too.
    Implied,
}

/// One visible line in the table: either a foldable directory header or a session under it.
enum RowItem {
    Dir {
        path: String,
        count: usize,
        subs: usize,
        msgs: u32,
        latest: SystemTime,
        folded: bool,
        /// The project directory is gone from disk, so the row is flagged.
        missing: bool,
        /// Every session under this node, folded or not, so marking works either way.
        children: Vec<PathBuf>,
    },
    Session {
        idx: usize,
        /// Last of its sibling group, so the connector closes the branch.
        last: bool,
        /// 1 for a session under a project, 2 for a subagent under a session.
        depth: u8,
        /// Subagent transcripts spawned by this session.
        kids: usize,
        folded: bool,
    },
}

struct App {
    /// Every session on the host, subagent transcripts included. Scanned once.
    sessions: Vec<Session>,
    match_mode: MatchMode,
    matching: Matching,
    scope: Scope,
    cwd: String,
    filter: String,
    /// Filter text as it was before the current `/` edit, restored on Esc.
    filter_backup: String,
    /// Set by `c`; the run loop drops out of the TUI and runs `claude -r` for it.
    resume: Option<Resume>,
    /// Session being renamed, and the title being typed for it.
    rename_target: Option<PathBuf>,
    rename_buf: String,
    input_mode: InputMode,
    focus: Focus,
    viewer: Option<Viewer>,
    help: bool,
    /// True after a bare `g`, waiting for the second `g` of `gg`.
    pending_g: bool,
    folded: HashSet<String>,
    /// Project paths that no longer exist on disk. Checked once, at startup.
    missing_dirs: HashSet<String>,
    rows: Vec<RowItem>,
    match_count: usize,
    list_state: ListState,
    /// Rows visible in the tree, updated each draw so paging keys know the step.
    view_height: u16,
    matcher: Matcher,
    /// Sessions marked with space, deleted together by `d`.
    marked: HashSet<PathBuf>,
    confirm_delete: Option<Vec<PathBuf>>,
    status: Option<String>,
}

impl App {
    fn new(force_all: bool) -> io::Result<Self> {
        let root = projects_root()?;
        let sessions = scan_sessions(&root)?;
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let has_cwd_sessions = !force_all && sessions.iter().any(|s| s.project_path == cwd);
        // Subagents are read but start folded: every session that spawned one is a
        // closed node until the user opens it.
        let folded: HashSet<String> = sessions
            .iter()
            .filter_map(|s| s.parent.as_ref())
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        // Only real paths are checked; "(unknown)" is a placeholder, not a directory.
        let missing_dirs: HashSet<String> = sessions
            .iter()
            .map(|s| s.project_path.as_str())
            .filter(|p| p.starts_with('/'))
            .collect::<HashSet<_>>()
            .into_iter()
            .filter(|p| !Path::new(p).is_dir())
            .map(str::to_string)
            .collect();
        let mut app = Self {
            sessions,
            match_mode: MatchMode::OnlyTitle,
            matching: Matching::Plain,
            scope: if has_cwd_sessions { Scope::CurrentDir } else { Scope::All },
            cwd,
            filter: String::new(),
            filter_backup: String::new(),
            resume: None,
            rename_target: None,
            rename_buf: String::new(),
            input_mode: InputMode::Normal,
            focus: Focus::Tree,
            viewer: None,
            help: false,
            pending_g: false,
            folded,
            missing_dirs,
            rows: Vec::new(),
            match_count: 0,
            list_state: ListState::default(),
            view_height: 20,
            matcher: Matcher::new(Config::DEFAULT),
            marked: HashSet::new(),
            confirm_delete: None,
            status: None,
        };
        app.refresh_results();
        app.list_state
            .select(if app.rows.is_empty() { None } else { Some(0) });
        Ok(app)
    }

    /// Title for a path, used by the delete confirmation.
    fn title_for(&self, path: &PathBuf) -> String {
        self.sessions
            .iter()
            .find(|s| s.file_path == *path)
            .map(|s| s.title.clone())
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string()
            })
    }

    fn active_sessions(&self) -> &[Session] {
        &self.sessions
    }

    fn selected_row(&self) -> Option<&RowItem> {
        self.rows.get(self.list_state.selected()?)
    }

    fn selected_file(&self) -> Option<PathBuf> {
        match self.selected_row()? {
            RowItem::Session { idx, .. } => {
                Some(self.active_sessions().get(*idx)?.file_path.clone())
            }
            RowItem::Dir { .. } => None,
        }
    }

    /// Fold key for a row: a project path for a directory, a file path for a session.
    fn fold_key(&self, i: usize) -> Option<String> {
        match self.rows.get(i)? {
            RowItem::Dir { path, .. } => Some(path.clone()),
            RowItem::Session { idx, .. } => Some(
                self.active_sessions()
                    .get(*idx)?
                    .file_path
                    .to_string_lossy()
                    .into_owned(),
            ),
        }
    }

    /// Left: fold an open node, otherwise step out to its parent row.
    fn collapse(&mut self) {
        let Some(i) = self.list_state.selected() else {
            return;
        };
        let foldable = match &self.rows[i] {
            RowItem::Dir { folded, .. } => !*folded,
            RowItem::Session { kids, folded, .. } => *kids > 0 && !*folded,
        };
        if foldable {
            if let Some(k) = self.fold_key(i) {
                self.set_folded(k, true);
            }
            return;
        }
        // Step out: a subagent goes to its session, anything else to its directory.
        let target = match &self.rows[i] {
            RowItem::Session { depth: 2, .. } => self.rows[..i]
                .iter()
                .rposition(|r| matches!(r, RowItem::Session { depth: 1, .. })),
            _ => self.rows[..i]
                .iter()
                .rposition(|r| matches!(r, RowItem::Dir { .. })),
        };
        if let Some(t) = target {
            self.list_state.select(Some(t));
        }
    }

    /// Right: unfold a folded node.
    fn expand(&mut self) {
        let Some(i) = self.list_state.selected() else {
            return;
        };
        let folded = match &self.rows[i] {
            RowItem::Dir { folded, .. } => *folded,
            RowItem::Session { kids, folded, .. } => *kids > 0 && *folded,
        };
        if folded
            && let Some(k) = self.fold_key(i) {
                self.set_folded(k, false);
            }
    }


    /// Move the tree selection by `delta` rows, clamped to the ends.
    fn move_sel(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as isize;
        let last = self.rows.len() as isize - 1;
        self.list_state
            .select(Some((cur + delta).clamp(0, last) as usize));
    }

    fn set_folded(&mut self, key: String, folded: bool) {
        if folded {
            self.folded.insert(key.clone());
        } else {
            self.folded.remove(&key);
        }
        self.refresh_results();
        self.select_key(&key);
    }

    /// Reselect a row by its fold key, so folding does not move the cursor.
    /// Every session that spawned subagents, as fold keys. Read from the session
    /// list rather than the rows, so it covers nodes hidden inside folded directories.
    fn session_node_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .sessions
            .iter()
            .filter_map(|s| s.parent.as_ref())
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        keys.sort();
        keys.dedup();
        keys
    }

    /// Fold or unfold every session node at once, showing or hiding all subagents.
    fn toggle_fold_sessions(&mut self) {
        let keys = self.session_node_keys();
        if keys.is_empty() {
            self.status = Some("no subagent sessions".to_string());
            return;
        }
        let showing = keys.iter().any(|k| !self.folded.contains(k));
        let keep = self.list_state.selected().and_then(|i| self.fold_key(i));
        if showing {
            self.folded.extend(keys.iter().cloned());
        } else {
            for k in &keys {
                self.folded.remove(k);
            }
        }
        self.refresh_results();
        if let Some(k) = keep {
            self.select_key(&k);
        }
        self.status = Some(if showing {
            format!("subagents folded ({} nodes)", keys.len())
        } else {
            format!("subagents shown ({} nodes)", keys.len())
        });
    }

    /// Fold or unfold whatever node the cursor is on.
    fn toggle_node(&mut self) {
        let Some(i) = self.list_state.selected() else {
            return;
        };
        let folded = match &self.rows[i] {
            RowItem::Dir { folded, .. } => Some(*folded),
            RowItem::Session { kids, folded, .. } => (*kids > 0).then_some(*folded),
        };
        if let (Some(folded), Some(key)) = (folded, self.fold_key(i)) {
            self.set_folded(key, !folded);
        }
    }

    fn select_key(&mut self, key: &str) {
        let pos = (0..self.rows.len()).find(|i| self.fold_key(*i).as_deref() == Some(key));
        if let Some(pos) = pos {
            self.list_state.select(Some(pos));
        }
    }

    /// Fold every directory if any is open; otherwise unfold all of them.
    fn toggle_fold_all(&mut self) {
        let any_open = self.rows.iter().any(|r| match r {
            RowItem::Dir { folded, .. } => !*folded,
            RowItem::Session { kids, folded, .. } => *kids > 0 && !*folded,
        });
        let keep = self.list_state.selected().and_then(|i| self.fold_key(i));
        if any_open {
            let keys: Vec<String> = (0..self.rows.len())
                .filter(|i| {
                    matches!(self.rows[*i], RowItem::Dir { .. })
                        || matches!(self.rows[*i], RowItem::Session { kids, .. } if kids > 0)
                })
                .filter_map(|i| self.fold_key(i))
                .collect();
            self.folded.extend(keys);
        } else {
            self.folded.clear();
        }
        self.refresh_results();
        if let Some(k) = keep {
            self.select_key(&k);
        }
    }

    fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            Scope::CurrentDir => Scope::All,
            Scope::All => Scope::CurrentDir,
        };
        self.refresh_results();
        if self.scope == Scope::CurrentDir && self.rows.is_empty() {
            self.status = Some(format!("no sessions for {}", self.cwd));
        }
    }

    /// Start editing the selected session's title, seeded with the current one.
    fn begin_rename(&mut self) {
        let Some(i) = self.list_state.selected() else {
            return;
        };
        let RowItem::Session { idx, .. } = self.rows[i] else {
            self.status = Some("select a session to rename".to_string());
            return;
        };
        let Some(s) = self.sessions.get(idx) else {
            return;
        };
        self.rename_target = Some(s.file_path.clone());
        self.rename_buf = s.title.clone();
        self.input_mode = InputMode::Rename;
    }

    /// Append a `custom-title` record, the same shape `/rename` writes, and update
    /// the in-memory session so the tree reflects it without a rescan.
    fn commit_rename(&mut self) {
        self.input_mode = InputMode::Normal;
        let Some(path) = self.rename_target.take() else {
            return;
        };
        let title = self.rename_buf.trim().to_string();
        if title.is_empty() {
            self.status = Some("rename cancelled: empty title".to_string());
            return;
        }
        if self.title_for(&path) == title {
            self.status = Some("title unchanged".to_string());
            return;
        }

        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let line = format!(
            "{}\n",
            serde_json::json!({
                "type": "custom-title",
                "customTitle": title,
                "sessionId": id,
            })
        );

        let written = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
        if let Err(e) = written {
            self.status = Some(format!("rename failed: {}", e));
            return;
        }

        if let Some(s) = self.sessions.iter_mut().find(|s| s.file_path == path) {
            s.full_text.push_str(&line);
            s.search_text = format!("{} {}", title, s.search_text);
            s.title = title.clone();
            if let Ok(m) = fs::metadata(&path).and_then(|m| m.modified()) {
                s.mtime = m;
            }
        }
        self.refresh_results();
        let key = path.to_string_lossy().into_owned();
        self.select_key(&key);
        self.status = Some(format!("renamed to \"{}\"", title));
    }

    /// Ask the run loop to resume the selected session with the `claude` CLI.
    fn request_resume(&mut self) {
        let Some(i) = self.list_state.selected() else {
            return;
        };
        let RowItem::Session { idx, .. } = self.rows[i] else {
            self.status = Some("select a session to resume".to_string());
            return;
        };
        let Some(path) = self.sessions.get(idx).map(|s| s.file_path.clone()) else {
            return;
        };
        self.request_resume_file(&path);
    }

    /// Resume the session logged in `file`. A subagent resumes the session that
    /// spawned it, since an `agent-*` transcript is not itself a resumable
    /// session id — even when its parent was filtered out and it sits at top level.
    fn request_resume_file(&mut self, file: &PathBuf) {
        let Some(s) = self.sessions.iter().find(|s| s.file_path == *file) else {
            return;
        };
        let path = s.parent.clone().unwrap_or_else(|| s.file_path.clone());
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            return;
        };
        let cwd = self.title_owner(&path).unwrap_or_else(|| s.project_path.clone());
        if !std::path::Path::new(&cwd).is_dir() {
            self.status = Some(format!("project directory is gone: {}", cwd));
            return;
        }
        self.resume = Some(Resume { cwd, id });
    }

    /// The `project_path` recorded for a given log file.
    fn title_owner(&self, path: &PathBuf) -> Option<String> {
        self.sessions
            .iter()
            .find(|s| s.file_path == *path)
            .map(|s| s.project_path.clone())
    }

    /// Open the transcript viewer for the selected session row.
    fn open_viewer(&mut self) {
        let Some(i) = self.list_state.selected() else {
            return;
        };
        let RowItem::Session { idx, .. } = self.rows[i] else {
            return;
        };
        let Some(s) = self.active_sessions().get(idx) else {
            return;
        };
        self.viewer = Some(Viewer::new(
            s.title.clone(),
            s.file_path.clone(),
            s.full_text.clone(),
        ));
        self.focus = Focus::Viewer;
    }

    /// Space toggles one session, or every session under a directory node.
    fn toggle_mark(&mut self) {
        let Some(i) = self.list_state.selected() else {
            return;
        };
        match &self.rows[i] {
            RowItem::Session { idx, .. } => {
                let Some(p) = self.active_sessions().get(*idx).map(|s| s.file_path.clone())
                else {
                    return;
                };
                // A session's subagents live inside its directory and die with it,
                // so they follow the parent's mark rather than being marked apart.
                let kids: Vec<PathBuf> = self
                    .active_sessions()
                    .iter()
                    .filter(|c| c.parent.as_ref() == Some(&p))
                    .map(|c| c.file_path.clone())
                    .collect();
                if self.marked.contains(&p) {
                    self.marked.remove(&p);
                    for k in &kids {
                        self.marked.remove(k);
                    }
                } else {
                    self.marked.insert(p);
                    self.marked.extend(kids);
                }
            }
            RowItem::Dir { children, .. } => {
                // Invert each child independently, so a partly marked directory
                // swaps its two halves rather than collapsing to all or nothing.
                for p in children.clone() {
                    if !self.marked.remove(&p) {
                        self.marked.insert(p);
                    }
                }
            }
        }
    }

    /// How much of a directory node is marked, for its row indicator.
    fn dir_mark(&self, children: &[PathBuf]) -> Mark {
        let n = children.iter().filter(|p| self.marked.contains(*p)).count();
        if n == 0 {
            Mark::None
        } else if n == children.len() {
            Mark::All
        } else {
            Mark::Some
        }
    }

    /// The paths `d` would delete: everything marked, else the selected row —
    /// one session, or every session the filter leaves under a directory.
    fn delete_targets(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = if !self.marked.is_empty() {
            self.marked.iter().cloned().collect()
        } else {
            match self.selected_row() {
                Some(RowItem::Dir { children, .. }) => children.clone(),
                _ => self.selected_file().into_iter().collect(),
            }
        };
        v.sort();
        // Deleting a session removes its directory, so a subagent inside one
        // would already be gone. Drop it rather than fail on it later.
        let covered: Vec<PathBuf> = v.iter().map(|p| p.with_extension("")).collect();
        v.retain(|p| !covered.iter().any(|c| p.starts_with(c)));
        v
    }

    fn delete_paths(&mut self, paths: &[PathBuf]) {
        let mut errors: Vec<String> = Vec::new();
        let mut done = 0usize;
        for path in paths {
            let mut ok = true;
            if let Err(e) = fs::remove_file(path) {
                errors.push(format!("{}: {}", path.display(), e));
                ok = false;
            }
            let dir = path.with_extension("");
            let had_dir = dir.is_dir();
            if had_dir
                && let Err(e) = fs::remove_dir_all(&dir) {
                    errors.push(format!("{}: {}", dir.display(), e));
                    ok = false;
                }
            if ok {
                done += 1;
            }
            self.marked.remove(path);
            // `dir` held this session's subagent transcripts; they are gone with it.
            self.marked.retain(|m| !m.starts_with(&dir));
            self.sessions
                .retain(|s| s.file_path != *path && !s.file_path.starts_with(&dir));
        }
        self.refresh_results();
        self.status = Some(if errors.is_empty() {
            match paths {
                [one] => format!("deleted {}", one.display()),
                _ => format!("deleted {} sessions", done),
            }
        } else {
            format!("{} deleted, {} failed: {}", done, errors.len(), errors.join("; "))
        });
    }

    /// Rebuild the visible rows: apply scope + filter, then lay the survivors out as
    /// project -> session -> subagent, each level ordered by most recent update.
    fn refresh_results(&mut self) {
        let sessions = &self.sessions;

        let (case, norm) = (CaseMatching::Smart, Normalization::Smart);
        let atoms: Vec<Atom> = if self.filter.is_empty() {
            Vec::new()
        } else {
            match self.matching {
                Matching::Plain => {
                    vec![Atom::new(&self.filter, case, norm, AtomKind::Substring, false)]
                }
                Matching::Fuzzy => Pattern::parse(&self.filter, case, norm).atoms,
            }
        };
        let mut buf: Vec<char> = Vec::new();
        let mut keep: Vec<usize> = Vec::new();

        for (i, s) in sessions.iter().enumerate() {
            if self.scope == Scope::CurrentDir && s.project_path != self.cwd {
                continue;
            }
            if !atoms.is_empty() {
                let haystack = match self.match_mode {
                    MatchMode::OnlyTitle => s.search_text.as_str(),
                    MatchMode::FullText => s.full_text.as_str(),
                };
                buf.clear();
                let utf32 = nucleo_matcher::Utf32Str::new(haystack, &mut buf);
                if atoms.iter().any(|a| a.score(utf32, &mut self.matcher).is_none()) {
                    continue;
                }
            }
            keep.push(i);
        }

        // Resolve parent links inside the surviving set. A subagent whose parent did
        // not survive the filter is promoted to top level rather than hidden.
        let by_path: BTreeMap<&PathBuf, usize> =
            keep.iter().map(|i| (&sessions[*i].file_path, *i)).collect();
        let mut kids: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for &i in &keep {
            match sessions[i]
                .parent
                .as_ref()
                .and_then(|p| by_path.get(p))
                .copied()
            {
                Some(parent) => kids.entry(parent).or_default().push(i),
                None => groups
                    .entry(sessions[i].project_path.as_str())
                    .or_default()
                    .push(i),
            }
        }

        let newest_first = |a: &usize, b: &usize| sessions[*b].mtime.cmp(&sessions[*a].mtime);
        for v in kids.values_mut() {
            v.sort_by(newest_first);
        }
        let mut dirs: Vec<(&str, Vec<usize>)> = groups.into_iter().collect();
        for (_, tops) in dirs.iter_mut() {
            tops.sort_by(newest_first);
        }
        dirs.sort_by(|a, b| {
            sessions[b.1[0]]
                .mtime
                .cmp(&sessions[a.1[0]].mtime)
                .then_with(|| a.0.cmp(b.0))
        });

        let mut rows = Vec::new();
        let mut match_count = 0;
        for (path, tops) in dirs {
            let dir_folded = self.folded.contains(path);
            let subs: usize = tops.iter().map(|i| kids.get(i).map_or(0, |k| k.len())).sum();
            let mut children: Vec<PathBuf> = Vec::new();
            let mut msgs = 0u32;
            for i in &tops {
                children.push(sessions[*i].file_path.clone());
                msgs += sessions[*i].message_count;
                for k in kids.get(i).into_iter().flatten() {
                    children.push(sessions[*k].file_path.clone());
                    msgs += sessions[*k].message_count;
                }
            }
            match_count += children.len();

            rows.push(RowItem::Dir {
                path: path.to_string(),
                count: tops.len(),
                subs,
                msgs,
                latest: sessions[tops[0]].mtime,
                folded: dir_folded,
                missing: self.missing_dirs.contains(path),
                children,
            });
            if dir_folded {
                continue;
            }

            let n_top = tops.len();
            for (t, i) in tops.into_iter().enumerate() {
                let my_kids = kids.get(&i).map_or(&[][..], |v| v.as_slice());
                let key = sessions[i].file_path.to_string_lossy().into_owned();
                let folded = self.folded.contains(&key);
                rows.push(RowItem::Session {
                    idx: i,
                    last: t + 1 == n_top,
                    depth: 1,
                    kids: my_kids.len(),
                    folded,
                });
                if folded {
                    continue;
                }
                let n_kids = my_kids.len();
                for (k, ki) in my_kids.iter().enumerate() {
                    rows.push(RowItem::Session {
                        idx: *ki,
                        last: k + 1 == n_kids,
                        depth: 2,
                        kids: 0,
                        folded: false,
                    });
                }
            }
        }
        self.rows = rows;
        self.match_count = match_count;

        if self.rows.is_empty() {
            self.list_state.select(None);
        } else {
            let cur = self.list_state.selected().unwrap_or(0);
            self.list_state.select(Some(cur.min(self.rows.len() - 1)));
        }
    }
}

fn projects_root() -> io::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home dir"))?;
    Ok(home.join(".claude").join("projects"))
}

fn scan_sessions(root: &Path) -> io::Result<Vec<Session>> {
    let mut sessions = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, _depth)) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push((path, 0));
            } else if ft.is_file()
                && path.extension().and_then(|s| s.to_str()) == Some("jsonl")
                    && let Some(s) = parse_session(&path) {
                        sessions.push(s);
                    }
        }
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime));
    Ok(sessions)
}

fn parse_session(path: &PathBuf) -> Option<Session> {
    let content = fs::read_to_string(path).ok()?;
    let mtime = fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut project_path = String::new();
    let mut ai_title = String::new();
    let mut custom_title = String::new();
    let mut first_user_content = String::new();
    let mut started_at = String::new();
    let mut git_branch = String::new();
    let mut user_msg_count: u32 = 0;
    let mut reported_msg_count: u32 = 0;

    for line in content.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if project_path.is_empty()
            && let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
                project_path = cwd.to_string();
            }
        if let Some(gb) = v.get("gitBranch").and_then(|g| g.as_str())
            && !gb.is_empty() {
                git_branch = gb.to_string();
            }
        if started_at.is_empty()
            && let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
                started_at = ts.get(..10).unwrap_or(ts).to_string();
            }
        if let Some(mc) = v.get("messageCount").and_then(|m| m.as_u64()) {
            reported_msg_count = reported_msg_count.max(mc as u32);
        }

        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if ty == "custom-title" {
            // A rename appends this record; the newest one wins.
            if let Some(t) = v.get("customTitle").and_then(|t| t.as_str())
                && !t.is_empty() {
                    custom_title = t.to_string();
                }
        } else if ty == "ai-title" && ai_title.is_empty() {
            if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
                ai_title = t.to_string();
            }
        } else if ty == "user" && v.get("isMeta").and_then(|m| m.as_bool()) != Some(true) {
            user_msg_count = user_msg_count.saturating_add(1);
            if first_user_content.is_empty()
                && let Some(msg) = v.get("message") {
                    let c = extract_user_text(msg.get("content"));
                    let cleaned = clean_prompt(&c);
                    if !cleaned.is_empty() {
                        first_user_content = cleaned;
                    }
                }
        }
    }

    if project_path.is_empty()
        && custom_title.is_empty()
        && ai_title.is_empty()
        && first_user_content.is_empty()
    {
        return None;
    }
    if project_path.is_empty() {
        project_path = "(unknown)".to_string();
    }

    // A user's rename beats the generated title, which beats the opening prompt.
    let title = if !custom_title.is_empty() {
        custom_title.clone()
    } else if !ai_title.is_empty() {
        ai_title.clone()
    } else if !first_user_content.is_empty() {
        first_user_content.clone()
    } else {
        "(untitled)".to_string()
    };

    // Every name the session has gone by stays searchable.
    let search_text = [
        custom_title.as_str(),
        ai_title.as_str(),
        first_user_content.as_str(),
    ]
    .iter()
    .filter(|p| !p.is_empty())
    .copied()
    .collect::<Vec<&str>>()
    .join(" ");

    let message_count = reported_msg_count.max(user_msg_count);

    Some(Session {
        file_path: path.clone(),
        project_path,
        title,
        search_text,
        started_at,
        git_branch,
        message_count,
        mtime,
        parent: parent_session(path),
        full_text: content,
    })
}

/// Subagent transcripts live at `<project>/<parent-uuid>/subagents/agent-*.jsonl`,
/// so the session that spawned one is `<project>/<parent-uuid>.jsonl`.
fn parent_session(path: &PathBuf) -> Option<PathBuf> {
    let subagents = path.parent()?;
    if subagents.file_name()?.to_str()? != "subagents" {
        return None;
    }
    let session_dir = subagents.parent()?;
    let id = session_dir.file_name()?.to_str()?;
    let parent = session_dir.parent()?.join(format!("{}.jsonl", id));
    (parent != *path).then_some(parent)
}

fn extract_user_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn clean_prompt(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => {
                if c.is_whitespace() {
                    if !out.ends_with(' ') {
                        out.push(' ');
                    }
                } else {
                    out.push(c);
                }
            }
            _ => {}
        }
    }
    out.trim().to_string()
}

fn age_secs(t: SystemTime) -> u64 {
    SystemTime::now()
        .duration_since(t)
        .unwrap_or_default()
        .as_secs()
}

fn format_age(t: SystemTime) -> String {
    let s = age_secs(t);
    if s < 60 {
        "now".to_string()
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else if s < 30 * 86400 {
        format!("{}d", s / 86400)
    } else if s < 365 * 86400 {
        format!("{}mo", s / (30 * 86400))
    } else {
        format!("{}y", s / (365 * 86400))
    }
}

/// green ≤ 1 week, cyan ≤ 1 month, orange ≤ 3 months, bright red beyond.
fn age_color(t: SystemTime) -> Color {
    const DAY: u64 = 86400;
    let s = age_secs(t);
    if s <= 7 * DAY {
        Color::Green
    } else if s <= 30 * DAY {
        Color::Cyan
    } else if s <= 90 * DAY {
        Color::Indexed(208)
    } else {
        Color::LightRed
    }
}


// ---------------------------------------------------------------------------
// CLI and entry point
// ---------------------------------------------------------------------------

const USAGE: &str = "\
AI Session Manager — browse and prune Claude Code session logs

USAGE:
    ai-session-manager [OPTIONS]

OPTIONS:
    -a, --all     Start showing sessions from every project on the host.
                  Without this, the app starts scoped to the current
                  directory when it has sessions, and shows all otherwise.
                  Ctrl-A toggles the scope at any time.
    -h, --help    Print this help and exit.

KEYS (session tree):
    j k h l           move, fold, unfold        (arrows work too)
                      subagent sessions nest under the session that spawned
                      them, folded until you open them
    c                 resume the session: replaces this process with
                      `claude -r <id>`, run from its project directory
    r                 rename the selected session
    -                 fold / unfold the node under the cursor
    gg G              first / last row
    ctrl-f ctrl-b     page down / up
    ctrl-d ctrl-u     half page down / up
    enter             open the session, or fold the directory
    /                 filter; enter accepts, esc cancels
    esc               clear marks, then the active filter
    ctrl-e            fold or unfold every directory
    ctrl-a            current directory / all sessions
    ctrl-r            fold / unfold every session node (all subagents)
    ctrl-t            match titles / full text
    ctrl-s            plain substring / fuzzy matching
    space             mark / unmark; a session takes its subagents with it,
                      a directory inverts its children
    d, del            delete everything marked, else the selected session,
                      or on a directory every session under it;
                      y, enter or a second del confirms
    f1                key help
    q, ctrl-c         quit

KEYS (session viewer):
    j k               scroll a line             (arrows work too)
    ctrl-f ctrl-b     scroll a page
    ctrl-d ctrl-u     scroll half a page
    gg G              top / bottom
    t                 show or hide thinking blocks
    o                 expand or collapse tool output
    c                 resume the session, as in the tree
    esc, q            back to the session tree
";

struct Cli {
    all: bool,
}

/// Parse argv. Returns `None` when the app should not start (help, or a bad flag).
fn parse_args<I: Iterator<Item = String>>(args: I) -> Option<Cli> {
    let mut cli = Cli { all: false };
    for arg in args {
        match arg.as_str() {
            "-a" | "--all" => cli.all = true,
            "-h" | "--help" => {
                print!("{}", USAGE);
                return None;
            }
            other => {
                eprintln!("error: unrecognized argument '{}'\n", other);
                eprint!("{}", USAGE);
                std::process::exit(2);
            }
        }
    }
    Some(cli)
}

fn main() -> io::Result<()> {
    let cli = match parse_args(std::env::args().skip(1)) {
        Some(c) => c,
        None => return Ok(()),
    };

    let mut app = App::new(cli.all)?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    enter_terminal(&mut terminal)?;

    let result = run(&mut terminal, &mut app);

    leave_terminal(&mut terminal)?;
    result
}

fn run<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|f| draw(f, app))?;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                continue;
            }
            if handle_key(app, key) {
                return Ok(());
            }
        }
        // `claude` needs the real terminal, so this can only happen out here,
        // between draws, where the TUI can be torn down and rebuilt.
        if let Some(r) = app.resume.take() {
            resume_session(terminal, app, r)?;
        }
    }
}

/// Hand the terminal and the process over to `claude -r <id>`, run in the
/// session's project directory. On success this never returns: the manager is
/// replaced, so claude is left as the only thing on the terminal rather than
/// running underneath a suspended TUI. Control comes back only if the exec
/// itself failed, in which case the TUI is restored and the reason reported.
fn resume_session<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    r: Resume,
) -> io::Result<()> {
    leave_terminal(terminal)?;

    let err = resume_command(&r).exec();

    enter_terminal(terminal)?;
    app.status = Some(match err.kind() {
        io::ErrorKind::NotFound => "`claude` not found on PATH".to_string(),
        _ => format!("could not run claude: {}", err),
    });
    Ok(())
}

/// `claude -r <session-id>`, run from the session's own project directory.
fn resume_command(r: &Resume) -> std::process::Command {
    let mut cmd = std::process::Command::new("claude");
    cmd.arg("-r").arg(&r.id).current_dir(&r.cwd);
    cmd
}

fn enter_terminal<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>) -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    terminal.clear()?;
    Ok(())
}

fn leave_terminal<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

const HELP_TREE: &[(&str, &str)] = &[
    ("j / k, ↓ / ↑", "move down / up"),
    ("h / l, ← / →", "fold / unfold, or step out"),
    ("-", "fold / unfold the node under the cursor"),
    ("gg / G", "first / last row"),
    ("ctrl-f / ctrl-b", "page down / up"),
    ("ctrl-d / ctrl-u", "half page down / up"),
    ("pgdn / pgup, home / end", "page and jump"),
    ("enter", "open session, or fold directory"),
    ("/", "filter (enter accepts, esc cancels)"),
    ("esc", "clear marks, then the active filter"),
    ("ctrl-e", "fold or unfold every directory"),
    ("ctrl-a", "current directory / all sessions"),
    ("ctrl-r", "fold / unfold every session node"),
    ("ctrl-t", "match titles / full text"),
    ("ctrl-s", "plain substring / fuzzy matching"),
    ("c", "resume with `claude -r` and exit this app"),
    ("r", "rename the selected session"),
    ("space", "mark / unmark (session: + its subagents)"),
    ("d, del", "delete marked, or the selected session / directory"),
    ("f1", "this help"),
    ("q, ctrl-c", "quit"),
];

const HELP_VIEWER: &[(&str, &str)] = &[
    ("j / k, ↓ / ↑", "scroll one line"),
    ("ctrl-f / ctrl-b", "scroll one page"),
    ("ctrl-d / ctrl-u", "scroll half a page"),
    ("gg / G, home / end", "top / bottom"),
    ("pgdn / pgup", "page down / up"),
    ("t", "show or hide thinking blocks"),
    ("o", "expand or collapse tool output"),
    ("c", "resume with `claude -r` and exit this app"),
    ("esc, q", "back to the session tree"),
    ("f1", "this help"),
    ("ctrl-c", "quit"),
];

/// Returns true when the app should exit.
fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && key.code == KeyCode::Char('c') {
        return true;
    }

    // Modal overlays consume keys before anything else.
    if let Some(paths) = app.confirm_delete.clone() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter | KeyCode::Delete => {
                app.confirm_delete = None;
                app.delete_paths(&paths);
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                app.confirm_delete = None;
                app.status = Some("delete cancelled".to_string());
            }
            _ => {}
        }
        return false;
    }
    if app.help {
        app.help = false;
        return false;
    }

    if app.focus == Focus::Viewer {
        handle_viewer_key(app, key, ctrl);
        return false;
    }
    if app.input_mode == InputMode::Rename {
        handle_rename_key(app, key);
        return false;
    }
    if app.input_mode == InputMode::Filter {
        handle_filter_key(app, key);
        return false;
    }
    handle_tree_key(app, key, ctrl)
}

fn handle_filter_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.filter = std::mem::take(&mut app.filter_backup);
            app.input_mode = InputMode::Normal;
            app.refresh_results();
        }
        KeyCode::Enter => {
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            app.filter.pop();
            app.refresh_results();
        }
        KeyCode::Char(c) => {
            app.filter.push(c);
            app.refresh_results();
        }
        _ => {}
    }
}

fn handle_rename_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
            app.rename_target = None;
            app.status = Some("rename cancelled".to_string());
        }
        KeyCode::Enter => app.commit_rename(),
        KeyCode::Backspace => {
            app.rename_buf.pop();
        }
        KeyCode::Char(c) => app.rename_buf.push(c),
        _ => {}
    }
}

fn handle_viewer_key(app: &mut App, key: KeyEvent, ctrl: bool) {
    app.status = None;
    let Some(v) = app.viewer.as_mut() else {
        app.focus = Focus::Tree;
        return;
    };
    let page = v.height.max(1) as isize;
    let pending_g = std::mem::take(&mut app.pending_g);

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.focus = Focus::Tree;
            app.viewer = None;
        }
        KeyCode::Char('j') | KeyCode::Down => v.scroll_by(1),
        KeyCode::Char('k') | KeyCode::Up => v.scroll_by(-1),
        KeyCode::Char('f') if ctrl => v.scroll_by(page),
        KeyCode::Char('b') if ctrl => v.scroll_by(-page),
        KeyCode::Char('d') if ctrl => v.scroll_by(page / 2),
        KeyCode::Char('u') if ctrl => v.scroll_by(-page / 2),
        KeyCode::PageDown | KeyCode::Char(' ') => v.scroll_by(page),
        KeyCode::PageUp => v.scroll_by(-page),
        KeyCode::Char('g') => {
            if pending_g {
                v.scroll = 0;
            } else {
                app.pending_g = true;
            }
        }
        KeyCode::Home => v.scroll = 0,
        KeyCode::Char('G') | KeyCode::End => v.scroll = v.max_scroll(),
        KeyCode::Char('t') => {
            v.show_thinking = !v.show_thinking;
            v.rebuild();
        }
        KeyCode::Char('o') => {
            v.expand_tools = !v.expand_tools;
            v.rebuild();
        }
        KeyCode::Char('c') if !ctrl => {
            let path = v.path.clone();
            app.request_resume_file(&path);
        }
        KeyCode::F(1) => app.help = true,
        _ => {}
    }
}

fn handle_tree_key(app: &mut App, key: KeyEvent, ctrl: bool) -> bool {
    app.status = None;
    let page = app.view_height.max(1) as isize;
    let pending_g = std::mem::take(&mut app.pending_g);

    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::F(1) => app.help = true,
        KeyCode::Char('/') => {
            app.filter_backup = app.filter.clone();
            app.input_mode = InputMode::Filter;
        }
        KeyCode::Esc => {
            // Marks first: they are the more surprising state to leave behind.
            if !app.marked.is_empty() {
                let n = app.marked.len();
                app.marked.clear();
                app.status = Some(format!("cleared {} mark{}", n, if n == 1 { "" } else { "s" }));
            } else if !app.filter.is_empty() {
                app.filter.clear();
                app.refresh_results();
            }
        }
        KeyCode::Char(' ') => {
            app.toggle_mark();
            app.move_sel(1);
        }

        // movement
        KeyCode::Char('j') | KeyCode::Down => app.move_sel(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_sel(-1),
        KeyCode::Char('f') if ctrl => app.move_sel(page),
        KeyCode::Char('b') if ctrl => app.move_sel(-page),
        KeyCode::Char('u') if ctrl => app.move_sel(-page / 2),
        KeyCode::PageDown => app.move_sel(page),
        KeyCode::PageUp => app.move_sel(-page),
        KeyCode::Char('g') => {
            if pending_g {
                app.move_sel(isize::MIN / 2);
            } else {
                app.pending_g = true;
            }
        }
        KeyCode::Home => app.move_sel(isize::MIN / 2),
        KeyCode::Char('G') | KeyCode::End => app.move_sel(isize::MAX / 2),

        // folding
        KeyCode::Char('c') if !ctrl => app.request_resume(),
        KeyCode::Char('r') if !ctrl => app.begin_rename(),
        KeyCode::Char('-') => app.toggle_node(),
        KeyCode::Char('h') | KeyCode::Left => app.collapse(),
        KeyCode::Char('l') | KeyCode::Right => app.expand(),
        KeyCode::Enter => match app.selected_row() {
            Some(RowItem::Dir { path, folded, .. }) => {
                let (p, f) = (path.clone(), !*folded);
                app.set_folded(p, f);
            }
            Some(RowItem::Session { .. }) => app.open_viewer(),
            None => {}
        },
        KeyCode::Char('e') if ctrl => app.toggle_fold_all(),

        // toggles
        KeyCode::Char('a') if ctrl => app.toggle_scope(),
        KeyCode::Char('r') if ctrl => app.toggle_fold_sessions(),
        KeyCode::Char('t') if ctrl => {
            app.match_mode = match app.match_mode {
                MatchMode::OnlyTitle => MatchMode::FullText,
                MatchMode::FullText => MatchMode::OnlyTitle,
            };
            app.refresh_results();
        }
        KeyCode::Char('s') if ctrl => {
            app.matching = match app.matching {
                Matching::Plain => Matching::Fuzzy,
                Matching::Fuzzy => Matching::Plain,
            };
            app.refresh_results();
        }

        // delete. ctrl-d is half-page-down here, so deletion is `d` or Delete.
        KeyCode::Char('d') if ctrl => app.move_sel(page / 2),
        KeyCode::Char('d') | KeyCode::Delete => {
            let targets = app.delete_targets();
            if targets.is_empty() {
                app.status =
                    Some("mark sessions with space, or select one to delete".to_string());
            } else {
                app.confirm_delete = Some(targets);
            }
        }
        _ => {}
    }
    false
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn draw(f: &mut ratatui::Frame, app: &mut App) {
    match app.focus {
        Focus::Tree => draw_tree(f, app),
        Focus::Viewer => draw_viewer(f, app),
    }
    if app.input_mode == InputMode::Rename {
        draw_rename(f, app);
    }
    if app.help {
        draw_help(f, app.focus);
    }
    if app.confirm_delete.is_some() {
        draw_confirm(f, app);
    }
}

fn draw_viewer(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(area);

    let Some(v) = app.viewer.as_mut() else {
        return;
    };

    let inner_w = chunks[1].width.saturating_sub(2);
    let inner_h = chunks[1].height.saturating_sub(2);
    v.height = inner_h.max(1);
    v.ensure_wrapped(inner_w);
    if v.pending_end {
        // A session opens at its latest message, like resuming it would.
        v.scroll = v.max_scroll();
        v.pending_end = false;
    }

    let total = v.wrapped.len();
    let pct = if v.max_scroll() == 0 {
        100
    } else {
        (v.scroll * 100 / v.max_scroll()).min(100)
    };
    let head = Paragraph::new(Line::from(vec![
        Span::styled(
            v.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "   {}  ·  {} lines  ·  {}%{}{}",
                v.path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
                total,
                pct,
                if v.show_thinking { "  ·  thinking" } else { "" },
                if v.expand_tools { "  ·  tools expanded" } else { "" },
            ),
            Style::default().fg(META),
        ),
        // A failed `c` reports here, or the key would look like it did nothing.
        Span::raw(app.status.as_ref().map(|s| format!("  —  {}", s)).unwrap_or_default()),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("session  (esc/q: back, j/k: scroll, t: thinking, o: tool output, c: resume, f1: help)"),
    );
    f.render_widget(head, chunks[0]);

    let end = (v.scroll + v.height as usize).min(total);
    let visible: Vec<Line> = v.wrapped[v.scroll.min(total)..end].to_vec();
    let body = Paragraph::new(visible).block(Block::default().borders(Borders::ALL));
    f.render_widget(body, chunks[1]);
}

/// Truncate to `w` columns with an ellipsis, then pad out to exactly `w`.
fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        let mut out = s.to_string();
        out.push_str(&" ".repeat(w - n));
        out
    } else if w <= 1 {
        "…".chars().take(w).collect()
    } else {
        let mut out: String = s.chars().take(w - 1).collect();
        out.push('…');
        out
    }
}

fn compact_count(n: u32) -> String {
    if n < 10_000 {
        n.to_string()
    } else {
        format!("{:.0}k", n as f64 / 1000.0)
    }
}

/// Widths for the session-row grid. Directory rows ignore this entirely.
struct Cols {
    started: bool,
    branch: usize,
    title: usize,
    /// Width of the tree connector column, wider when subagent rows are present.
    conn: usize,
}

const AGE_W: usize = 4;
const STARTED_W: usize = 10;
const MSGS_W: usize = 5;
const CONN_W: usize = 4;

/// Extra connector width for subagent rows. One column, so a child's `├─` lands
/// directly beneath the `▾` of the session that spawned it.
const DEEP_W: usize = 1;

fn cols(width: u16, deep: bool) -> Cols {
    let w = width as usize;
    let conn = CONN_W + if deep { DEEP_W } else { 0 };
    let mut started = true;
    let mut branch = 16usize;
    loop {
        let fixed = AGE_W
            + 1
            + if started { STARTED_W + 1 } else { 0 }
            + conn
            + 1
            + MSGS_W
            + if branch > 0 { branch + 1 } else { 0 };
        let title = w.saturating_sub(fixed);
        if title >= 24 {
            return Cols { started, branch, title, conn };
        }
        if branch > 0 {
            branch = 0;
        } else if started {
            started = false;
        } else {
            return Cols { started, branch, title: title.max(8), conn };
        }
    }
}

/// A directory node: free-form full-width line, not tied to the session grid.
const MARK_W: usize = 2;

/// The two-column gutter every row starts with, showing the deletion mark.
fn mark_span(m: Mark) -> Span<'static> {
    let (sym, color) = match m {
        Mark::None => (" ", META),
        Mark::Some => ("◐", Color::LightYellow),
        Mark::All => ("✓", Color::LightRed),
        Mark::Implied => ("✓", META),
    };
    Span::styled(
        format!("{} ", sym),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

#[allow(clippy::too_many_arguments)] // one parameter per field the line renders
fn dir_line(
    path: &str,
    count: usize,
    subs: usize,
    msgs: u32,
    latest: SystemTime,
    folded: bool,
    missing: bool,
    mark: Mark,
    width: usize,
) -> Line<'static> {
    let meta = Style::default().fg(META);
    let marker = format!("{} ", if folded { "▸" } else { "▾" });
    // A project whose directory is gone: red, struck through, and said in words,
    // since not every terminal draws strikethrough.
    let gone = if missing { " (missing)" } else { "" };
    let path_style = if missing {
        Style::default()
            .fg(Color::LightRed)
            .add_modifier(Modifier::BOLD | Modifier::CROSSED_OUT)
    } else {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    };
    let stats = format!(
        "{} session{}{} · {} msg{} · ",
        count,
        if count == 1 { "" } else { "s" },
        if subs > 0 {
            format!(" + {} subagent{}", subs, if subs == 1 { "" } else { "s" })
        } else {
            String::new()
        },
        compact_count(msgs),
        if msgs == 1 { "" } else { "s" },
    );
    let age = format_age(latest);

    let used = marker.chars().count()
        + path.chars().count()
        + gone.chars().count()
        + stats.chars().count()
        + age.chars().count();
    let mut spans = vec![
        mark_span(mark),
        Span::styled(
            marker,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(path.to_string(), path_style),
    ];
    if missing {
        spans.push(Span::styled(gone, Style::default().fg(Color::LightRed)));
    }
    if used + 2 <= width {
        // A dim rule carries the eye from the path across to the stats.
        spans.push(Span::styled(
            format!(" {} ", "─".repeat(width - used - 2)),
            meta,
        ));
        spans.push(Span::styled(stats, meta));
        spans.push(Span::styled(age, Style::default().fg(age_color(latest))));
    } else {
        spans.push(Span::styled(format!("  {}", age), Style::default().fg(age_color(latest))));
    }
    Line::from(spans)
}

fn session_line(
    s: &Session,
    last: bool,
    depth: u8,
    kids: usize,
    folded: bool,
    mark: Mark,
    c: &Cols,
) -> Line<'static> {
    let dim = Style::default().fg(META);
    let mut spans = vec![
        mark_span(mark),
        Span::styled(
            format!("{:>w$} ", format_age(s.mtime), w = AGE_W),
            Style::default().fg(age_color(s.mtime)),
        ),
    ];
    if c.started {
        spans.push(Span::styled(
            format!("{} ", fit(&s.started_at, STARTED_W)),
            dim,
        ));
    }

    // A session with subagents is a foldable node; a plain one just gets a branch.
    let branch = if last { '└' } else { '├' };
    let knob = if kids > 0 {
        if folded { '▸' } else { '▾' }
    } else {
        '─'
    };
    let pad = c.conn.saturating_sub(CONN_W + if depth == 2 { DEEP_W } else { 0 });
    let indent = if depth == 2 { DEEP_W } else { 0 };
    spans.push(Span::styled(
        format!("{} {}{} {}", " ".repeat(indent), branch, knob, " ".repeat(pad)),
        if depth == 2 {
            dim
        } else if kids > 0 {
            Style::default().fg(Color::LightYellow)
        } else {
            dim
        },
    ));

    let title_style = if depth == 2 {
        Style::default().fg(META)
    } else {
        Style::default()
    };
    spans.push(Span::styled(fit(&s.title, c.title), title_style));
    spans.push(Span::styled(
        format!(" {:>w$}", compact_count(s.message_count), w = MSGS_W),
        dim,
    ));
    if c.branch > 0 {
        spans.push(Span::styled(
            format!(" {}", fit(&s.git_branch, c.branch)),
            dim,
        ));
    }
    Line::from(spans)
}

fn header_line(c: &Cols) -> Line<'static> {
    let st = Style::default()
        .fg(META)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let mut text = " ".repeat(MARK_W);
    text.push_str(&format!("{:>w$} ", "last", w = AGE_W));
    if c.started {
        text.push_str(&fit("started", STARTED_W));
        text.push(' ');
    }
    text.push_str(&" ".repeat(c.conn));
    text.push_str(&fit("title", c.title));
    text.push_str(&format!(" {:>w$}", "msgs", w = MSGS_W));
    if c.branch > 0 {
        text.push_str(&format!(" {}", fit("branch", c.branch)));
    }
    Line::from(Span::styled(text, st))
}

fn draw_tree(f: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(f.area());

    let mode_label = format!(
        "[{}|{}|{}] ",
        match app.match_mode {
            MatchMode::OnlyTitle => "title",
            MatchMode::FullText => "full",
        },
        match app.matching {
            Matching::Plain => "plain",
            Matching::Fuzzy => "fuzzy",
        },
        match app.scope {
            Scope::CurrentDir => "cwd",
            Scope::All => "all",
        }
    );
    let filtering = app.input_mode == InputMode::Filter;
    let filter_part = if filtering || !app.filter.is_empty() {
        format!("/{}", app.filter)
    } else {
        String::new()
    };
    let trailing = match &app.status {
        Some(s) => format!(" — {}", s),
        None if filtering => format!(" ({} matches, enter to accept)", app.match_count),
        None if !app.marked.is_empty() => {
            format!(" ({} matches, {} marked)", app.match_count, app.marked.len())
        }
        None => format!(" ({} matches)", app.match_count),
    };
    let header = Paragraph::new(format!("{}{}{}", mode_label, filter_part, trailing)).block(
        Block::default()
            .borders(Borders::ALL)
            .title("sessions  (/: filter, enter: open, hjkl: move, f1: help, q: quit)"),
    );
    f.render_widget(header, chunks[0]);

    // One border around a fixed column header plus the scrolling row list.
    let block = Block::default().borders(Borders::ALL);
    let inner = block.inner(chunks[1]);
    f.render_widget(block, chunks[1]);
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);
    app.view_height = parts[1].height.max(1);

    let deep = app
        .rows
        .iter()
        .any(|r| matches!(r, RowItem::Session { depth: 2, .. }));
    let c = cols(inner.width.saturating_sub(MARK_W as u16), deep);
    f.render_widget(Paragraph::new(header_line(&c)), parts[0]);

    let sessions = app.active_sessions();
    let marks = &*app;
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|item| {
            ListItem::new(match item {
                RowItem::Dir {
                    path,
                    count,
                    subs,
                    msgs,
                    latest,
                    folded,
                    missing,
                    children,
                } => dir_line(
                    path,
                    *count,
                    *subs,
                    *msgs,
                    *latest,
                    *folded,
                    *missing,
                    marks.dir_mark(children),
                    (inner.width as usize).saturating_sub(MARK_W),
                ),
                RowItem::Session {
                    idx,
                    last,
                    depth,
                    kids,
                    folded,
                } => {
                    let s = &sessions[*idx];
                    let mark = if marks.marked.contains(&s.file_path) {
                        Mark::All
                    } else if s
                        .parent
                        .as_ref()
                        .is_some_and(|p| marks.marked.contains(p))
                    {
                        // Deleting the parent removes its directory, subagents included.
                        Mark::Implied
                    } else {
                        Mark::None
                    };
                    session_line(s, *last, *depth, *kids, *folded, mark, &c)
                }
            })
        })
        .collect();

    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, parts[1], &mut app.list_state);

    if filtering && app.confirm_delete.is_none() && !app.help {
        let x = chunks[0].x
            + 1
            + mode_label.chars().count() as u16
            + filter_part.chars().count() as u16;
        f.set_cursor_position((x, chunks[0].y + 1));
    }
}

fn draw_rename(f: &mut ratatui::Frame, app: &App) {
    let w = ((f.area().width as u32 * 70 / 100) as u16).max(40).min(f.area().width);
    let h = 7u16.min(f.area().height);
    let area = Rect {
        x: f.area().x + f.area().width.saturating_sub(w) / 2,
        y: f.area().y + (f.area().height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    let field_w = w.saturating_sub(4) as usize;
    // Show the tail of a long title so the caret stays on screen.
    let typed: String = {
        let n = app.rename_buf.chars().count();
        if n > field_w {
            app.rename_buf.chars().skip(n - field_w).collect()
        } else {
            app.rename_buf.clone()
        }
    };

    let lines = vec![
        Line::from(Span::styled(
            format!(
                "  {}",
                app.rename_target
                    .as_ref()
                    .and_then(|p| p.file_stem())
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
            ),
            Style::default().fg(META),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                typed.clone(),
                Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  enter to save · esc to cancel".to_string(),
            Style::default().fg(META),
        )),
    ];

    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::LightCyan))
                .title("rename session"),
        ),
        area,
    );
    f.set_cursor_position((area.x + 3 + typed.chars().count() as u16, area.y + 3));
}

fn draw_help(f: &mut ratatui::Frame, focus: Focus) {
    let (entries, title) = match focus {
        Focus::Tree => (HELP_TREE, "help — session tree"),
        Focus::Viewer => (HELP_VIEWER, "help — session viewer"),
    };
    let lines: Vec<Line> = entries
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(
                    format!("  {:<24}", k),
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(*d),
            ])
        })
        .chain(std::iter::once(Line::from("")))
        .chain(std::iter::once(Line::from(Span::styled(
            "  any key closes this help".to_string(),
            Style::default().fg(META),
        ))))
        .collect();

    let h = (lines.len() as u16 + 2).min(f.area().height);
    let w = 62.min(f.area().width);
    let area = Rect {
        x: f.area().x + (f.area().width.saturating_sub(w)) / 2,
        y: f.area().y + (f.area().height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn draw_confirm(f: &mut ratatui::Frame, app: &App) {
    const LIST_MAX: usize = 10;
    let Some(paths) = app.confirm_delete.as_ref() else {
        return;
    };
    let with_dirs = paths.iter().filter(|p| p.with_extension("").is_dir()).count();
    let shown = paths.len().min(LIST_MAX);

    // More than one session goes at once: say so loudly, so a `d` on a directory
    // or a forgotten mark cannot be waved through as if it were a single delete.
    let many = paths.len() > 1;
    let w = ((f.area().width as u32 * 78 / 100) as u16).max(40).min(f.area().width);
    let text_w = w.saturating_sub(4) as usize;
    let alarm = Style::default()
        .fg(Color::White)
        .bg(Color::Red)
        .add_modifier(Modifier::BOLD);
    let loud = Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line> = if many {
        let banner = format!("⚠  PERMANENTLY DELETE {} SESSIONS  ⚠", paths.len());
        // The banner spans the whole inner width, border to border.
        let pad = (w.saturating_sub(2) as usize).saturating_sub(banner.chars().count());
        let mut v = vec![Line::from(Span::styled(
            format!(
                "{}{}{}",
                " ".repeat(pad / 2),
                banner,
                " ".repeat(pad - pad / 2)
            ),
            alarm,
        ))];
        // A directory delete, or marks that all sit in one project: name it.
        let mut projects: Vec<String> =
            paths.iter().filter_map(|p| app.title_owner(p)).collect();
        projects.sort();
        projects.dedup();
        if let [only] = projects.as_slice() {
            v.push(Line::from(vec![
                Span::styled("  all from ", loud),
                Span::styled(
                    fit(only, text_w.saturating_sub(11)),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
            ]));
        } else if projects.len() > 1 {
            v.push(Line::from(Span::styled(
                format!("  across {} projects", projects.len()),
                loud,
            )));
        }
        v.push(Line::from(""));
        v
    } else {
        vec![
            Line::from(Span::styled("Delete this session?", loud)),
            Line::from(""),
        ]
    };
    // Titles read far better than uuid paths; the short id disambiguates them.
    for p in paths.iter().take(shown) {
        let id: String = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .chars()
            .take(8)
            .collect();
        let title = app.title_for(p);
        lines.push(Line::from(vec![
            Span::styled(format!("  {} ", id), Style::default().fg(META)),
            Span::raw(fit(&title, text_w.saturating_sub(11))),
        ]));
    }
    if paths.len() > shown {
        lines.push(Line::from(Span::styled(
            format!("  … and {} more", paths.len() - shown),
            Style::default().fg(META),
        )));
    }
    if with_dirs > 0 {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "  {} of these also {} a directory of subagent transcripts,",
                with_dirs,
                if with_dirs == 1 { "has" } else { "have" }
            ),
            Style::default().fg(META),
        )));
    }
    if with_dirs > 0 {
        lines.push(Line::from(Span::styled(
            "  which is deleted with them.".to_string(),
            Style::default().fg(META),
        )));
    }
    lines.push(Line::from(""));
    if many {
        lines.push(Line::from(Span::styled("  This cannot be undone.", loud)));
        lines.push(Line::from(vec![
            Span::raw("  Press "),
            Span::styled("[y]/[enter]/[del]", loud),
            Span::raw(" to delete them all, [n]/[esc] to cancel."),
        ]));
    } else {
        lines.push(Line::from("  Press [y]/[enter]/[del] to confirm, [n]/[esc] to cancel."));
    }

    let h = (lines.len() as u16 + 2).min(f.area().height);
    let area = Rect {
        x: f.area().x + f.area().width.saturating_sub(w) / 2,
        y: f.area().y + (f.area().height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(
            if many {
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Thick)
                    .border_style(loud)
                    .title(Span::styled(
                        format!(" ⚠ DELETE {} SESSIONS ⚠ ", paths.len()),
                        alarm,
                    ))
            } else {
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red))
                    .title("confirm delete")
            },
        ),
        area,
    );
}


// ---------------------------------------------------------------------------
// Session transcript viewer
// ---------------------------------------------------------------------------

struct Viewer {
    title: String,
    path: PathBuf,
    logical: Vec<Line<'static>>,
    wrapped: Vec<Line<'static>>,
    wrap_width: u16,
    scroll: usize,
    height: u16,
    show_thinking: bool,
    expand_tools: bool,
    /// Jump to the bottom on the first draw, once wrapping knows the real height.
    pending_end: bool,
    /// Raw file text, kept so the toggles can re-render without touching disk.
    source: String,
}

impl Viewer {
    fn new(title: String, path: PathBuf, source: String) -> Self {
        let mut v = Viewer {
            title,
            path,
            logical: Vec::new(),
            wrapped: Vec::new(),
            wrap_width: 0,
            scroll: 0,
            height: 1,
            show_thinking: false,
            expand_tools: false,
            pending_end: true,
            source,
        };
        v.rebuild();
        v
    }

    fn rebuild(&mut self) {
        self.logical = render_transcript(&self.source, self.show_thinking, self.expand_tools);
        self.wrap_width = 0; // force re-wrap on next draw
    }

    fn ensure_wrapped(&mut self, width: u16) {
        if width != self.wrap_width {
            self.wrapped = self
                .logical
                .iter()
                .flat_map(|l| wrap_line(l, width as usize))
                .collect();
            self.wrap_width = width;
            self.clamp();
        }
    }

    fn max_scroll(&self) -> usize {
        self.wrapped.len().saturating_sub(self.height as usize)
    }

    fn clamp(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
    }

    fn scroll_by(&mut self, delta: isize) {
        let s = self.scroll as isize + delta;
        self.scroll = s.clamp(0, self.max_scroll() as isize) as usize;
    }
}

fn ts_hhmm(v: &serde_json::Value) -> String {
    v.get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|t| t.get(11..16).map(|s| s.to_string()))
        .unwrap_or_default()
}

/// One-line summary of a tool call's most interesting argument.
fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    let pick = |k: &str| input.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let raw = match name {
        "Bash" => pick("command").or_else(|| pick("description")),
        "Read" | "Write" | "NotebookEdit" => pick("file_path"),
        "Edit" => pick("file_path"),
        "Grep" => pick("pattern"),
        "Glob" => pick("pattern"),
        "Task" | "Agent" => pick("description").or_else(|| pick("prompt")),
        "WebFetch" => pick("url"),
        "WebSearch" => pick("query"),
        _ => None,
    }
    .unwrap_or_else(|| input.to_string());
    let one = raw.split('\n').next().unwrap_or("").trim().to_string();
    if one.chars().count() > 120 {
        format!("{}…", one.chars().take(120).collect::<String>())
    } else {
        one
    }
}

fn tool_result_text(c: &serde_json::Value) -> String {
    match c {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

const TOOL_PREVIEW_LINES: usize = 6;

/// Build the transcript for one session file, in file order.
fn render_transcript(source: &str, show_thinking: bool, expand_tools: bool) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let user_style = Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD);
    let asst_style = Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD);
    let meta = Style::default().fg(META);
    let dim = Style::default().fg(META).add_modifier(Modifier::ITALIC);

    let push_header = |out: &mut Vec<Line<'static>>, label: &str, st: Style, ts: String, side: bool| {
        if !out.is_empty() {
            out.push(Line::from(""));
        }
        let mut spans = vec![Span::styled(format!("▌ {}", label), st)];
        if side {
            spans.push(Span::styled("  (subagent)", meta));
        }
        if !ts.is_empty() {
            spans.push(Span::styled(format!("  {}", ts), meta));
        }
        out.push(Line::from(spans));
    };

    for raw in source.lines() {
        let v: serde_json::Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let side = v.get("isSidechain").and_then(|b| b.as_bool()) == Some(true);
        let is_meta = v.get("isMeta").and_then(|b| b.as_bool()) == Some(true);
        let content = v.get("message").and_then(|m| m.get("content"));

        match ty {
            "user" if !is_meta => match content {
                Some(serde_json::Value::String(s)) => {
                    let text = clean_prompt(s);
                    if text.is_empty() {
                        continue;
                    }
                    push_header(&mut out, "You", user_style, ts_hhmm(&v), side);
                    out.extend(md_to_lines(&text, "  "));
                }
                Some(serde_json::Value::Array(parts)) => {
                    for p in parts {
                        match p.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                let t = p.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                let text = clean_prompt(t);
                                if !text.is_empty() {
                                    push_header(&mut out, "You", user_style, ts_hhmm(&v), side);
                                    out.extend(md_to_lines(&text, "  "));
                                }
                            }
                            Some("tool_result") => {
                                let body = tool_result_text(
                                    p.get("content").unwrap_or(&serde_json::Value::Null),
                                );
                                let is_err = p
                                    .get("is_error")
                                    .and_then(|b| b.as_bool())
                                    == Some(true);
                                let lines: Vec<&str> = body.lines().collect();
                                let shown = if expand_tools {
                                    lines.len()
                                } else {
                                    lines.len().min(TOOL_PREVIEW_LINES)
                                };
                                let style = if is_err {
                                    Style::default().fg(Color::LightRed)
                                } else {
                                    meta
                                };
                                for l in lines.iter().take(shown) {
                                    out.push(Line::from(Span::styled(
                                        format!("    {}", l),
                                        style,
                                    )));
                                }
                                if lines.len() > shown {
                                    out.push(Line::from(Span::styled(
                                        format!(
                                            "    … {} more lines (o to expand)",
                                            lines.len() - shown
                                        ),
                                        dim,
                                    )));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            "assistant" => {
                if let Some(serde_json::Value::Array(parts)) = content {
                    let mut headed = false;
                    for p in parts {
                        match p.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                let t = p.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                if t.trim().is_empty() {
                                    continue;
                                }
                                if !headed {
                                    push_header(
                                        &mut out,
                                        "Claude",
                                        asst_style,
                                        ts_hhmm(&v),
                                        side,
                                    );
                                    headed = true;
                                }
                                out.extend(md_to_lines(t, "  "));
                            }
                            Some("thinking") if show_thinking => {
                                let t = p
                                    .get("thinking")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("");
                                if t.trim().is_empty() {
                                    continue;
                                }
                                if !headed {
                                    push_header(
                                        &mut out,
                                        "Claude",
                                        asst_style,
                                        ts_hhmm(&v),
                                        side,
                                    );
                                    headed = true;
                                }
                                out.push(Line::from(Span::styled(
                                    "  thinking".to_string(),
                                    dim,
                                )));
                                for l in t.lines() {
                                    out.push(Line::from(Span::styled(
                                        format!("  {}", l),
                                        dim,
                                    )));
                                }
                            }
                            Some("tool_use") => {
                                let name =
                                    p.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                                let input =
                                    p.get("input").unwrap_or(&serde_json::Value::Null);
                                out.push(Line::from(vec![
                                    Span::styled(
                                        format!("  ⚙ {} ", name),
                                        Style::default()
                                            .fg(Color::LightYellow)
                                            .add_modifier(Modifier::BOLD),
                                    ),
                                    Span::styled(tool_summary(name, input), meta),
                                ]));
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if out.is_empty() {
        out.push(Line::from(Span::styled(
            "  (no renderable messages in this session)".to_string(),
            dim,
        )));
    }
    out
}
// ---------------------------------------------------------------------------
// Markdown -> ratatui lines
// ---------------------------------------------------------------------------

/// Split text into tokens that are each either a run of spaces or a run of
/// non-spaces, so word wrapping can put breaks only between words.
fn split_keep_spaces(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_space: Option<bool> = None;
    for c in s.chars() {
        let is_space = c == ' ';
        if Some(is_space) != in_space {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            in_space = Some(is_space);
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// True for the span a list item starts with, so wrapping can hang-indent under it.
fn is_list_marker(c: &str) -> bool {
    c == "• " || c == "[x] " || c == "[ ] " || {
        let t = c.strip_suffix(". ");
        matches!(t, Some(d) if !d.is_empty() && d.chars().all(|ch| ch.is_ascii_digit()))
    }
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Wrap one logical line to `width`, preserving per-span styles and indenting
/// continuation lines to the original line's leading whitespace.
fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 || line_width(line) <= width {
        return vec![line.clone()];
    }
    // Continuation lines line up under the text, not under the bullet: count the
    // leading whitespace spans plus a list/task marker if one follows them.
    let mut indent_n = 0usize;
    for span in &line.spans {
        let c = span.content.as_ref();
        // Spaces and blockquote bars are gutter; keep consuming spans made only of them.
        let lead = c.chars().take_while(|ch| *ch == ' ' || *ch == '▏').count();
        indent_n += lead;
        if lead == c.chars().count() && !c.is_empty() {
            continue;
        }
        if lead == 0 && is_list_marker(c) {
            indent_n += c.chars().count();
        }
        break;
    }
    let indent_n = indent_n.min(width / 2);
    let indent = " ".repeat(indent_n);

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    let mut first = true;

    macro_rules! flush {
        () => {{
            let mut spans: Vec<Span<'static>> = Vec::new();
            if !first {
                spans.push(Span::raw(indent.clone()));
            }
            spans.append(&mut cur);
            out.push(Line::from(spans));
            first = false;
            cur_w = 0;
        }};
    }

    for span in &line.spans {
        let style = span.style;
        for word in split_keep_spaces(&span.content) {
            let avail = if first { width } else { width.saturating_sub(indent_n) }.max(1);
            let w = word.chars().count();
            if cur_w + w > avail && cur_w > 0 {
                flush!();
                if word.trim().is_empty() {
                    continue; // don't carry a space to the start of a new line
                }
            }
            if w > avail {
                let mut rest = word.clone();
                while rest.chars().count() > avail {
                    let head: String = rest.chars().take(avail).collect();
                    rest = rest.chars().skip(avail).collect();
                    cur.push(Span::styled(head, style));
                    flush!();
                }
                if !rest.is_empty() {
                    cur_w = rest.chars().count();
                    cur.push(Span::styled(rest, style));
                }
                continue;
            }
            cur_w += w;
            cur.push(Span::styled(word, style));
        }
    }
    if !cur.is_empty() || out.is_empty() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        if !first {
            spans.push(Span::raw(indent.clone()));
        }
        spans.append(&mut cur);
        out.push(Line::from(spans));
    }
    out
}

const CODE: Color = Color::Indexed(114);
const META: Color = Color::Indexed(244);

/// Render a markdown string to styled lines, every line prefixed with `indent`.
fn md_to_lines(src: &str, indent: &str) -> Vec<Line<'static>> {
    use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default();
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut quote_depth = 0usize;
    let mut in_code = false;
    let mut extra_indent = String::new();

    macro_rules! prefix {
        () => {{
            let mut p = String::from(indent);
            p.push_str(&extra_indent);
            for _ in 0..quote_depth {
                p.push_str("▏ ");
            }
            p
        }};
    }
    macro_rules! flush {
        () => {{
            if !cur.is_empty() {
                let mut spans = vec![Span::raw(prefix!())];
                spans.append(&mut cur);
                out.push(Line::from(spans));
            }
        }};
    }
    macro_rules! blank {
        () => {{
            let need = out.last().map(|l| line_width(l) != 0).unwrap_or(false);
            if need {
                out.push(Line::from(""));
            }
        }};
    }

    for ev in Parser::new_ext(src, opts) {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                flush!();
                blank!();
                let color = match level {
                    HeadingLevel::H1 => Color::LightMagenta,
                    HeadingLevel::H2 => Color::LightBlue,
                    _ => Color::LightCyan,
                };
                style = Style::default().fg(color).add_modifier(Modifier::BOLD);
            }
            Event::End(TagEnd::Heading(_)) => {
                flush!();
                style = Style::default();
                out.push(Line::from(""));
            }
            Event::Start(Tag::Paragraph) => flush!(),
            Event::End(TagEnd::Paragraph) => {
                flush!();
                blank!();
            }
            Event::Start(Tag::List(start)) => {
                flush!();
                list_stack.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                flush!();
                list_stack.pop();
                if list_stack.is_empty() {
                    blank!();
                }
            }
            Event::Start(Tag::Item) => {
                flush!();
                let depth = list_stack.len().saturating_sub(1);
                extra_indent = "  ".repeat(depth);
                let marker = match list_stack.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{}. ", n);
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                cur.push(Span::styled(marker, Style::default().fg(META)));
            }
            Event::End(TagEnd::Item) => {
                flush!();
                extra_indent.clear();
            }
            Event::Start(Tag::BlockQuote(_)) => {
                flush!();
                quote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush!();
                quote_depth = quote_depth.saturating_sub(1);
                blank!();
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                flush!();
                blank!();
                in_code = true;
                extra_indent = "  ".to_string();
                if let CodeBlockKind::Fenced(lang) = kind
                    && !lang.is_empty() {
                        out.push(Line::from(vec![
                            Span::raw(format!("{}  ", indent)),
                            Span::styled(lang.to_string(), Style::default().fg(META)),
                        ]));
                    }
                style = Style::default().fg(CODE);
            }
            Event::End(TagEnd::CodeBlock) => {
                flush!();
                in_code = false;
                extra_indent.clear();
                style = Style::default();
                blank!();
            }
            Event::Start(Tag::Emphasis) => style = style.add_modifier(Modifier::ITALIC),
            Event::End(TagEnd::Emphasis) => style = style.remove_modifier(Modifier::ITALIC),
            Event::Start(Tag::Strong) => style = style.add_modifier(Modifier::BOLD),
            Event::End(TagEnd::Strong) => style = style.remove_modifier(Modifier::BOLD),
            Event::Start(Tag::Strikethrough) => style = style.add_modifier(Modifier::CROSSED_OUT),
            Event::End(TagEnd::Strikethrough) => {
                style = style.remove_modifier(Modifier::CROSSED_OUT)
            }
            Event::Start(Tag::Link { .. }) => {
                style = style.fg(Color::LightBlue).add_modifier(Modifier::UNDERLINED)
            }
            Event::End(TagEnd::Link) => {
                style = Style::default();
            }
            Event::Text(t) => {
                if in_code {
                    // Code blocks keep their own newlines.
                    let mut parts = t.split('\n').peekable();
                    while let Some(p) = parts.next() {
                        if !p.is_empty() {
                            cur.push(Span::styled(p.to_string(), style));
                        }
                        if parts.peek().is_some() {
                            let mut spans = vec![Span::raw(prefix!())];
                            spans.append(&mut cur);
                            out.push(Line::from(spans));
                        }
                    }
                } else {
                    cur.push(Span::styled(t.to_string(), style));
                }
            }
            Event::Code(t) => cur.push(Span::styled(
                format!("`{}`", t),
                Style::default().fg(CODE),
            )),
            Event::SoftBreak => cur.push(Span::styled(" ", style)),
            Event::HardBreak => flush!(),
            Event::Rule => {
                flush!();
                out.push(Line::from(Span::styled(
                    format!("{}────────", indent),
                    Style::default().fg(META),
                )));
            }
            Event::TaskListMarker(done) => cur.push(Span::styled(
                if done { "[x] " } else { "[ ] " }.to_string(),
                Style::default().fg(META),
            )),
            _ => {}
        }
    }
    flush!();
    while matches!(out.last(), Some(l) if line_width(l) == 0) {
        out.pop();
    }
    out
}
























