//! The remembered arrangement, written so a restart can pick it back up.
//!
//! HyprWin does not persist the arrangement in the config: the config is a
//! deliberate start, hand-edited, while a session is machine-written memory.
//! Keeping them apart also means a reload never mistakes a saved session for a
//! user's intent, and clearing the file is a one-delete way to start fresh.
//!
//! An id is assigned per run and an HWND does not survive one, so a window is
//! remembered by what identifies it across processes — executable, class and
//! title — and a monitor by its 1-based, primary-first index, the same number
//! `layouts["N"]` and `status.json` already use. This module owns the document's
//! shape, the write and the two pure transforms a restore needs (matching a
//! window, re-keying a saved tree); `wm` fills one in and applies it.

use crate::layout::{DwindleTree, Node, Rect};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::Path;

/// The document's format. A file written by another version is ignored rather
/// than misread, which is the same reason a stale field is not trusted.
pub const SESSION_VERSION: u32 = 1;

/// One monitor's active workspace and layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorSession {
    /// 1-based, primary first.
    pub index: usize,
    pub active_workspace: u32,
    pub layout: String,
}

/// One remembered window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSession {
    /// The id the window had in the run that saved the session. Parked trees
    /// reference windows by this id, so a restore maps it onto the id this run
    /// assigns the window it matches.
    pub id: usize,
    pub exe: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub title: String,
    /// 1-based monitor index, primary first.
    pub monitor: usize,
    pub workspace: u32,
    #[serde(default)]
    pub floating: bool,
    #[serde(default)]
    pub fullscreen: bool,
    /// A floating window's rectangle. Tiled geometry is the layout's business,
    /// so it is recomputed rather than restored; a floating window has no layout
    /// slot to recompute from, so its rectangle is the only record of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<Rect>,
}

/// A dwindle tree parked for one workspace of one monitor.
///
/// The leaves are keyed by an id from the run that saved them, so a restore
/// re-keys them through the window records; a slot whose windows did not come
/// back cannot be restored and is dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceTree {
    pub monitor: usize,
    pub workspace: u32,
    pub tree: DwindleTree,
}

/// A master-layout ratio remembered for one workspace.
///
/// The master layout keeps its split in the config rather than in a tree, so a
/// resize there has nowhere else to be remembered. It is per-workspace, not
/// per-monitor, matching how `master_ratio_for` looks it up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MasterRatio {
    pub workspace: u32,
    pub ratio: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    #[serde(default)]
    pub monitors: Vec<MonitorSession>,
    #[serde(default)]
    pub windows: Vec<WindowSession>,
    #[serde(default)]
    pub trees: Vec<WorkspaceTree>,
    #[serde(default)]
    pub master_ratios: Vec<MasterRatio>,
}

impl Default for Session {
    fn default() -> Self {
        Session {
            version: SESSION_VERSION,
            monitors: Vec::new(),
            windows: Vec::new(),
            trees: Vec::new(),
            master_ratios: Vec::new(),
        }
    }
}

impl Session {
    /// Read a session file, or `None` when there is not a usable one.
    ///
    /// A missing file, unreadable bytes, invalid JSON or a version from another
    /// run all mean the same thing to a caller: there is nothing to restore, so
    /// start from the config. A damaged file is not an error worth stopping for.
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let session: Session = serde_json::from_str(&text).ok()?;
        if session.version != SESSION_VERSION {
            return None;
        }
        Some(session)
    }

    /// Write the session, replacing any previous one.
    ///
    /// The write goes to a sibling temporary file that is then renamed over the
    /// target, so a crash mid-write leaves the previous session intact rather
    /// than a half-written one that will not parse.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let text = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Take the record that best identifies a window, removing it.
    ///
    /// A window is matched by executable first, then refined by class and title,
    /// because a title is often the document name and can change between runs
    /// while the class stays put. The record is consumed so two windows of the
    /// same program are restored one-to-one instead of both claiming the first
    /// record. An empty executable cannot identify anything, so it never matches.
    pub fn take_window(&mut self, exe: &str, class: &str, title: &str) -> Option<WindowSession> {
        if exe.is_empty() {
            return None;
        }
        let exact = self
            .windows
            .iter()
            .position(|w| w.exe.eq_ignore_ascii_case(exe) && w.class == class && w.title == title);
        let same_class = self
            .windows
            .iter()
            .position(|w| w.exe.eq_ignore_ascii_case(exe) && w.class == class);
        let same_exe = self
            .windows
            .iter()
            .position(|w| w.exe.eq_ignore_ascii_case(exe));
        let idx = exact.or(same_class).or(same_exe)?;
        Some(self.windows.remove(idx))
    }
}

/// Re-key a saved tree's leaf ids through `map`, dropping anything unmapped.
///
/// A leaf is the id a window had when the session was saved; `map` turns that
/// into the id it has now. A slot with no window back — it was closed between
/// runs — has no mapping, and a split with a missing branch has no meaning, so
/// the whole slot is dropped rather than left with a hole in it.
pub fn rewrite_tree(node: &Node, map: &HashMap<usize, usize>) -> Option<Node> {
    match node {
        Node::Leaf { id } => map.get(id).map(|new| Node::Leaf { id: *new }),
        Node::Split {
            horizontal,
            ratio,
            a,
            b,
        } => Some(Node::Split {
            horizontal: *horizontal,
            ratio: *ratio,
            a: Box::new(rewrite_tree(a, map)?),
            b: Box::new(rewrite_tree(b, map)?),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(exe: &str, class: &str, title: &str, workspace: u32) -> WindowSession {
        WindowSession {
            id: 0,
            exe: exe.to_string(),
            class: class.to_string(),
            title: title.to_string(),
            monitor: 1,
            workspace,
            floating: false,
            fullscreen: false,
            rect: None,
        }
    }

    #[test]
    fn a_window_is_matched_by_exe_then_refined_by_class_and_title() {
        let mut s = Session::default();
        s.windows
            .push(win("notepad.exe", "Notepad", "a.txt - Notepad", 2));
        s.windows
            .push(win("notepad.exe", "Notepad", "b.txt - Notepad", 3));

        // An exact title wins over the earlier same-exe record.
        let hit = s
            .take_window("notepad.exe", "Notepad", "b.txt - Notepad")
            .expect("an exact match must be found");
        assert_eq!(hit.workspace, 3);
        // Consumed, so the other record still answers for the remaining window.
        let other = s
            .take_window("notepad.exe", "Notepad", "a.txt - Notepad")
            .expect("the second record is still there");
        assert_eq!(other.workspace, 2);
        assert!(s.windows.is_empty());
    }

    #[test]
    fn a_changed_title_falls_back_to_the_exe_and_class() {
        let mut s = Session::default();
        s.windows
            .push(win("code.exe", "Chrome_WidgetWin_1", "old.rs - Code", 4));
        let hit = s
            .take_window("code.exe", "Chrome_WidgetWin_1", "new.rs - Code")
            .expect("a retitled window of the same program must still match");
        assert_eq!(hit.workspace, 4);
    }

    #[test]
    fn a_window_with_no_executable_matches_nothing() {
        let mut s = Session::default();
        s.windows.push(win("", "Notepad", "x", 1));
        assert!(s.take_window("", "Notepad", "x").is_none());
        assert!(
            s.take_window("notepad.exe", "Notepad", "x").is_none(),
            "an empty exe must not act as a wildcard either"
        );
        assert_eq!(s.windows.len(), 1, "the record is left alone");
    }

    #[test]
    fn matching_ignores_the_case_of_the_executable() {
        let mut s = Session::default();
        s.windows.push(win("Notepad.exe", "Notepad", "x", 1));
        assert!(s.take_window("notepad.EXE", "Notepad", "x").is_some());
    }

    #[test]
    fn each_record_is_handed_out_once() {
        let mut s = Session::default();
        s.windows.push(win("notepad.exe", "Notepad", "x", 5));
        assert!(s.take_window("notepad.exe", "Notepad", "other").is_some());
        assert!(
            s.take_window("notepad.exe", "Notepad", "other").is_none(),
            "a second window must not claim the same record"
        );
    }

    #[test]
    fn a_tree_is_rekeyed_and_dropped_when_a_window_is_missing() {
        let tree = Node::Split {
            horizontal: true,
            ratio: 0.4,
            a: Box::new(Node::Leaf { id: 10 }),
            b: Box::new(Node::Split {
                horizontal: false,
                ratio: 0.7,
                a: Box::new(Node::Leaf { id: 11 }),
                b: Box::new(Node::Leaf { id: 12 }),
            }),
        };
        let mut map = HashMap::new();
        map.insert(10, 1);
        map.insert(11, 2);
        map.insert(12, 3);
        let rewritten = rewrite_tree(&tree, &map).expect("every leaf mapped");
        assert_eq!(
            rewritten,
            Node::Split {
                horizontal: true,
                ratio: 0.4,
                a: Box::new(Node::Leaf { id: 1 }),
                b: Box::new(Node::Split {
                    horizontal: false,
                    ratio: 0.7,
                    a: Box::new(Node::Leaf { id: 2 }),
                    b: Box::new(Node::Leaf { id: 3 }),
                }),
            }
        );

        // Drop one window and the whole subtree that held it goes with it.
        map.remove(&12);
        assert!(
            rewrite_tree(&tree, &map).is_none(),
            "a split with a missing branch has no meaning"
        );
        map.remove(&10);
        assert!(rewrite_tree(&Node::Leaf { id: 10 }, &map).is_none());
    }

    #[test]
    fn the_document_round_trips_through_json() {
        let mut s = Session::default();
        s.monitors.push(MonitorSession {
            index: 1,
            active_workspace: 3,
            layout: "master".to_string(),
        });
        s.windows.push(WindowSession {
            rect: Some(Rect::new(10, 20, 300, 400)),
            ..win("notepad.exe", "Notepad", "x", 3)
        });
        s.trees.push(WorkspaceTree {
            monitor: 1,
            workspace: 3,
            tree: DwindleTree {
                root: Some(Node::Leaf { id: 0 }),
            },
        });
        let json = serde_json::to_string(&s).expect("serialises");
        let back: Session = serde_json::from_str(&json).expect("parses");
        assert_eq!(s, back);
    }

    #[test]
    fn a_session_from_another_version_is_ignored() {
        let dir = std::env::temp_dir().join("hyprwin-session-test-version");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        let mut s = Session::default();
        s.windows.push(win("notepad.exe", "Notepad", "x", 1));
        s.write(&path).expect("writes");
        assert!(Session::load(&path).is_some(), "a same-version file loads");

        s.version = SESSION_VERSION + 1;
        s.write(&path).expect("writes");
        assert!(
            Session::load(&path).is_none(),
            "a newer format must be ignored rather than misread"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_or_damaged_file_restores_nothing() {
        let dir = std::env::temp_dir().join("hyprwin-session-test-damaged");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        let _ = std::fs::remove_file(&path);
        assert!(Session::load(&path).is_none());
        std::fs::write(&path, "{ not json").unwrap();
        assert!(Session::load(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }
}
