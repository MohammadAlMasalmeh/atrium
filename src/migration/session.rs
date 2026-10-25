//! Chrome's last session ("SNSS" command log in `<profile>/Sessions/Session_*`).
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{bail, ensure, Result};
use serde::Serialize;

const SET_TAB_WINDOW: u8 = 0;
const SET_TAB_INDEX_IN_WINDOW: u8 = 2;
const PRUNED_FROM_BACK: u8 = 5;
const UPDATE_TAB_NAVIGATION: u8 = 6;
const SET_SELECTED_NAVIGATION_INDEX: u8 = 7;
const SET_SELECTED_TAB_IN_INDEX: u8 = 8;
const SET_WINDOW_TYPE: u8 = 9;
const PRUNED_FROM_FRONT: u8 = 11;
const SET_PINNED_STATE: u8 = 12;
const TAB_CLOSED: u8 = 16;
const WINDOW_CLOSED: u8 = 17;
const SET_ACTIVE_WINDOW: u8 = 20;
const PRUNED: u8 = 24;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SessionTab {
    pub url: String,
    pub title: String,
    pub window: i32,
    pub pinned: bool,
    pub selected: bool,
}

#[derive(Default)]
struct Tab {
    window: Option<i32>,
    index: i32,
    navigations: BTreeMap<i32, (String, String)>,
    selected: Option<i32>,
    pinned: bool,
}

/// The newest session file. Older Chrome versions used "Current Session".
pub fn latest(profile: &Path) -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(profile.join("Sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("Session_")) && p.is_file())
        .collect();
    files.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().trim_start_matches("Session_").parse::<u64>().unwrap_or(0)));
    files.pop().or_else(|| Some(profile.join("Current Session")).filter(|p| p.is_file()))
}

fn i32_at(b: &[u8], at: usize) -> Option<i32> {
    b.get(at..at + 4).map(|s| i32::from_le_bytes(s.try_into().unwrap()))
}

/// base::Pickle: a 4-byte payload size header, then 4-byte-aligned fields.
struct Pickle<'a> {
    data: &'a [u8],
    at: usize,
}
impl<'a> Pickle<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 4 }
    }
    fn int(&mut self) -> Option<i32> {
        let v = i32_at(self.data, self.at)?;
        self.at += 4;
        Some(v)
    }
    fn bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        let v = self.data.get(self.at..self.at.checked_add(len)?)?;
        self.at += len.div_ceil(4) * 4;
        Some(v)
    }
    fn string(&mut self) -> Option<String> {
        let len = usize::try_from(self.int()?).ok()?;
        Some(String::from_utf8_lossy(self.bytes(len)?).into_owned())
    }
    fn string16(&mut self) -> Option<String> {
        let len = usize::try_from(self.int()?).ok()?;
        let units: Vec<u16> = self.bytes(len.checked_mul(2)?)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Some(String::from_utf16_lossy(&units))
    }
}

pub fn parse(bytes: &[u8]) -> Result<Vec<SessionTab>> {
    ensure!(bytes.len() >= 8 && &bytes[..4] == b"SNSS", "This is not a Chrome session file.");
    match i32_at(bytes, 4) {
        Some(1 | 3) => {}
        Some(2 | 4) => bail!("This Chrome session file is encrypted and cannot be imported."),
        _ => bail!("This Chrome session file version is not supported."),
    }
    let mut tabs: HashMap<i32, Tab> = HashMap::new();
    let mut window_order: Vec<i32> = Vec::new();
    let mut closed_windows = HashSet::new();
    let mut other_windows = HashSet::new();
    let mut selected_tab_index: HashMap<i32, i32> = HashMap::new();
    let mut at = 8;
    while at + 2 <= bytes.len() {
        let size = u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize;
        at += 2;
        if size == 0 || at + size > bytes.len() {
            break;
        }
        let id = bytes[at];
        let p = &bytes[at + 1..at + size];
        at += size;
        let (a, b) = (i32_at(p, 0), i32_at(p, 4));
        match id {
            SET_TAB_WINDOW => {
                if let (Some(window), Some(tab)) = (a, b) {
                    tabs.entry(tab).or_default().window = Some(window);
                    if !window_order.contains(&window) {
                        window_order.push(window);
                    }
                }
            }
            SET_TAB_INDEX_IN_WINDOW => {
                if let (Some(tab), Some(index)) = (a, b) {
                    tabs.entry(tab).or_default().index = index;
                }
            }
            UPDATE_TAB_NAVIGATION => {
                let mut pickle = Pickle::new(p);
                let parsed = (|| Some((pickle.int()?, pickle.int()?, pickle.string()?, pickle.string16().unwrap_or_default())))();
                if let Some((tab, index, url, title)) = parsed {
                    tabs.entry(tab).or_default().navigations.insert(index, (url, title));
                }
            }
            SET_SELECTED_NAVIGATION_INDEX => {
                if let (Some(tab), Some(index)) = (a, b) {
                    tabs.entry(tab).or_default().selected = Some(index);
                }
            }
            SET_SELECTED_TAB_IN_INDEX => {
                if let (Some(window), Some(index)) = (a, b) {
                    selected_tab_index.insert(window, index);
                }
            }
            SET_WINDOW_TYPE => {
                // 0 is a normal browser window; popups, apps, and DevTools are not restored.
                if let (Some(window), Some(kind)) = (a, b) {
                    if kind != 0 {
                        other_windows.insert(window);
                    } else {
                        other_windows.remove(&window);
                    }
                }
            }
            PRUNED_FROM_BACK => {
                if let (Some(tab), Some(index)) = (a, b) {
                    tabs.entry(tab).or_default().navigations.retain(|i, _| *i < index);
                }
            }
            PRUNED_FROM_FRONT => {
                if let (Some(tab), Some(count)) = (a, b) {
                    let t = tabs.entry(tab).or_default();
                    t.navigations = std::mem::take(&mut t.navigations).into_iter().filter(|(i, _)| *i >= count).map(|(i, v)| (i - count, v)).collect();
                    t.selected = t.selected.map(|s| (s - count).max(0));
                }
            }
            PRUNED => {
                if let (Some(tab), Some(index), Some(count)) = (a, b, i32_at(p, 8)) {
                    let t = tabs.entry(tab).or_default();
                    t.navigations = std::mem::take(&mut t.navigations).into_iter().filter(|(i, _)| *i < index || *i >= index + count)
                        .map(|(i, v)| (if i >= index + count { i - count } else { i }, v)).collect();
                }
            }
            SET_PINNED_STATE => {
                if let (Some(tab), Some(&pinned)) = (a, p.get(4)) {
                    tabs.entry(tab).or_default().pinned = pinned != 0;
                }
            }
            TAB_CLOSED => {
                if let Some(tab) = a {
                    tabs.remove(&tab);
                }
            }
            WINDOW_CLOSED => {
                if let Some(window) = a {
                    closed_windows.insert(window);
                }
            }
            SET_ACTIVE_WINDOW => {}
            _ => {}
        }
    }
    let mut out = Vec::new();
    for window in window_order {
        if closed_windows.contains(&window) || other_windows.contains(&window) {
            continue;
        }
        let mut in_window: Vec<&Tab> = tabs.values().filter(|t| t.window == Some(window)).collect();
        in_window.sort_by_key(|t| t.index);
        let selected = selected_tab_index.get(&window).copied();
        for (position, tab) in in_window.into_iter().enumerate() {
            let current = tab.selected.and_then(|i| tab.navigations.get(&i)).or_else(|| tab.navigations.values().last());
            if let Some((url, title)) = current {
                out.push(SessionTab { url: url.clone(), title: title.clone(), window, pinned: tab.pinned, selected: selected == Some(position as i32) });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn command(id: u8, payload: &[u8]) -> Vec<u8> {
        [((payload.len() + 1) as u16).to_le_bytes().to_vec(), vec![id], payload.to_vec()].concat()
    }
    fn ints(values: &[i32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }
    fn pad(mut v: Vec<u8>) -> Vec<u8> {
        v.resize(v.len().div_ceil(4) * 4, 0);
        v
    }
    pub fn navigation(tab: i32, index: i32, url: &str, title: &str) -> Vec<u8> {
        let title16: Vec<u8> = title.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let body = [ints(&[tab, index, url.len() as i32]), pad(url.as_bytes().to_vec()), ints(&[title.encode_utf16().count() as i32]), pad(title16), ints(&[0, 0])].concat();
        command(UPDATE_TAB_NAVIGATION, &[(body.len() as u32).to_le_bytes().to_vec(), body].concat())
    }

    #[test]
    fn restores_open_tabs_in_window_order_with_current_navigation() {
        let mut file = b"SNSS".to_vec();
        file.extend(3i32.to_le_bytes());
        for c in [
            command(SET_TAB_WINDOW, &ints(&[1, 10])),
            command(SET_TAB_INDEX_IN_WINDOW, &ints(&[10, 1])),
            navigation(10, 0, "https://a.test/old", "Old"),
            navigation(10, 1, "https://a.test/new", "Ünïcode"),
            command(SET_SELECTED_NAVIGATION_INDEX, &ints(&[10, 1])),
            command(SET_TAB_WINDOW, &ints(&[1, 11])),
            command(SET_TAB_INDEX_IN_WINDOW, &ints(&[11, 0])),
            navigation(11, 0, "https://b.test/", "B"),
            command(SET_PINNED_STATE, &[ints(&[11]), vec![1, 0, 0, 0]].concat()),
            command(SET_SELECTED_TAB_IN_INDEX, &ints(&[1, 1])),
            command(SET_TAB_WINDOW, &ints(&[1, 12])),
            navigation(12, 0, "https://closed.test/", "Closed"),
            command(TAB_CLOSED, &[ints(&[12, 0]), vec![0; 8]].concat()),
            command(SET_TAB_WINDOW, &ints(&[2, 20])),
            navigation(20, 0, "https://gone.test/", "Gone"),
            command(WINDOW_CLOSED, &[ints(&[2, 0]), vec![0; 8]].concat()),
        ] {
            file.extend(c);
        }
        let tabs = parse(&file).unwrap();
        assert_eq!(tabs.iter().map(|t| t.url.as_str()).collect::<Vec<_>>(), ["https://b.test/", "https://a.test/new"]);
        assert!(tabs[0].pinned && !tabs[0].selected);
        assert!(tabs[1].selected);
        assert_eq!(tabs[1].title, "Ünïcode");
        assert!(parse(b"SNSS\x02\0\0\0").is_err());
        assert!(parse(b"nope").is_err());
        // Truncated files keep the commands read so far.
        assert_eq!(parse(&file[..file.len() - 3]).unwrap().len(), 3);
    }
}
