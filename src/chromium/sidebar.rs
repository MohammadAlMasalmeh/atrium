//! Native sidebar list (ATRIUM_NATIVE_CHROME=1): favorites, the current space's pinned
//! pages and tabs, and recent chats in a source-list NSTableView on the window glass.
//!
//! The shell still owns spaces, pins and favorites. It sends this module a model after
//! every render and runs the actions sent back (`atriumShell.sidebar`); tab order and
//! closing go straight to the manager. AppKit calls the data source synchronously from
//! `reloadData`, so state is only borrowed briefly and never across AppKit calls.
use super::{Bridge, Event};
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObjectProtocol, ProtocolObject, Sel},
    sel, AnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSAccessibility, NSApplication, NSBezelStyle, NSButton, NSButtonType, NSColor, NSControl, NSControlSize,
    NSControlStateValueOff, NSControlStateValueOn, NSControlTextEditingDelegate, NSDragOperation, NSDraggingInfo,
    NSEvent, NSEventModifierFlags, NSFont, NSFontWeightSemibold, NSImage, NSImageScaling, NSImageView, NSMenu,
    NSMenuItem, NSPasteboard, NSPasteboardItem, NSPasteboardTypeString, NSPasteboardWriting, NSProgressIndicator,
    NSProgressIndicatorStyle, NSResponder, NSScrollView, NSStackView, NSStackViewDistribution, NSTableCellView,
    NSTableColumn, NSTableColumnResizingOptions, NSTableView, NSTableViewDataSource, NSTableViewDelegate,
    NSTableViewDropOperation, NSTableViewRowSizeStyle, NSTableViewStyle, NSTextField, NSTrackingArea,
    NSTrackingAreaOptions, NSUserInterfaceLayoutOrientation, NSView, NSAutoresizingMaskOptions,
};
use objc2_foundation::{NSArray, NSData, NSIndexSet, NSInteger, NSMutableIndexSet, NSObject, NSPoint, NSRect, NSSize, NSString};
use serde::Deserialize;
use serde_json::{json, Value as Json};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    sync::mpsc,
    time::Duration,
};

#[derive(Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub(super) struct Model {
    favs: Vec<Fav>,
    space: Space,
    spaces: Vec<SpaceRef>,
    pinned: Vec<Pin>,
    new_tab_active: bool,
    tabs: Vec<TabRow>,
    chats: Vec<Chat>,
}
#[derive(Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
struct Fav {
    url: String,
    title: String,
    icon: String,
    active: bool,
}
#[derive(Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
struct Space {
    id: u64,
    name: String,
    color: String,
}
#[derive(Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
struct SpaceRef {
    id: u64,
    name: String,
}
#[derive(Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
struct Pin {
    url: String,
    title: String,
    icon: String,
    live: bool,
    active: bool,
}
#[derive(Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
struct TabRow {
    id: u64,
    title: String,
    url: String,
    icon: String,
    loading: bool,
    suspended: bool,
    active: bool,
    split: bool,
    agent: bool,
}
#[derive(Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
struct Chat {
    id: String,
    title: String,
    busy: bool,
    active: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Row {
    Favs,
    SpaceHeader,
    Pin(usize),
    NewTab,
    Tab(usize),
    ChatsHeader,
    Chat(usize),
}

fn rows(m: &Model) -> Vec<Row> {
    let mut rows = Vec::new();
    if !m.favs.is_empty() {
        rows.push(Row::Favs);
    }
    rows.push(Row::SpaceHeader);
    rows.extend((0..m.pinned.len()).map(Row::Pin));
    rows.push(Row::NewTab);
    rows.extend((0..m.tabs.len()).map(Row::Tab));
    rows.push(Row::ChatsHeader);
    rows.extend((0..m.chats.len()).map(Row::Chat));
    rows
}

/// Whether `row` draws the same in both models (rows already match one to one).
fn same(a: &Model, b: &Model, row: Row) -> bool {
    match row {
        Row::Favs => a.favs == b.favs,
        Row::SpaceHeader => a.space == b.space,
        Row::Pin(i) => a.pinned[i] == b.pinned[i],
        Row::NewTab => a.new_tab_active == b.new_tab_active,
        Row::Tab(i) => a.tabs[i] == b.tabs[i],
        Row::ChatsHeader => true,
        Row::Chat(i) => a.chats[i] == b.chats[i],
    }
}

fn selected(m: &Model, row: Row) -> bool {
    match row {
        Row::Pin(i) => m.pinned[i].active,
        Row::NewTab => m.new_tab_active,
        Row::Tab(i) => m.tabs[i].active,
        Row::Chat(i) => m.chats[i].active,
        _ => false,
    }
}

const ROW_TYPE: &str = "com.atrium.sidebar-row";
const FAVS_PER_LINE: usize = 4;

struct Sidebar {
    scroll: Retained<NSScrollView>,
    table: Retained<SidebarTable>,
    source: Retained<Source>,
    model: Model,
    rows: Vec<Row>,
    icons: HashMap<String, Retained<NSImage>>,
    requested: HashSet<String>,
    fetch: mpsc::Sender<String>,
    placed: bool,
}

thread_local! {
    static SIDEBAR: RefCell<Option<Sidebar>> = const { RefCell::new(None) };
}

fn with<T>(f: impl FnOnce(&mut Sidebar) -> T) -> Option<T> {
    SIDEBAR.with(|s| s.borrow_mut().as_mut().map(f))
}

fn model_row(row: NSInteger) -> Option<(Model, Row)> {
    with(|s| usize::try_from(row).ok().and_then(|r| s.rows.get(r).copied()).map(|r| (s.model.clone(), r))).flatten()
}

fn send(command: Json) {
    if let Some(bridge) = with(|s| s.source.ivars().bridge.clone()) {
        bridge.send(Event::Command(command));
    }
}

/// An action the shell runs (it owns spaces, pins and favorites).
fn shell_action(action: Json) {
    send(json!({"cmd": "sidebarAction", "action": action}));
}

define_class!(
    /// Adds middle-click close and per-row context menus to a standard table.
    #[unsafe(super(NSTableView, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    struct SidebarTable;

    impl SidebarTable {
        #[unsafe(method_id(menuForEvent:))]
        fn menu_for_event(&self, event: &NSEvent) -> Option<Retained<NSMenu>> {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            menu_for_row(self.rowAtPoint(point))
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            if event.buttonNumber() == 2 {
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                middle_click(self.rowAtPoint(point));
            } else {
                unsafe { msg_send![super(self), otherMouseUp: event] }
            }
        }
    }
);

struct SourceIvars {
    bridge: Bridge,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = SourceIvars]
    struct Source;

    impl Source {
        #[unsafe(method(rowClicked:))]
        fn row_clicked(&self, table: &NSTableView) {
            let split = NSApplication::sharedApplication(MainThreadMarker::from(self))
                .currentEvent()
                .is_some_and(|e| e.modifierFlags().contains(NSEventModifierFlags::Option));
            activate_row(table.clickedRow(), split);
        }
        #[unsafe(method(favClicked:))]
        fn fav_clicked(&self, sender: &NSButton) {
            shell_action(json!({"action": "fav", "i": sender.tag()}));
        }
        #[unsafe(method(closeTab:))]
        fn close_tab(&self, sender: &AnyObject) {
            send(json!({"cmd": "close", "id": tag(sender)}));
        }
        #[unsafe(method(closeOtherTabs:))]
        fn close_other_tabs(&self, sender: &NSMenuItem) {
            let keep = sender.tag() as u64;
            for id in with(|s| s.model.tabs.iter().map(|t| t.id).filter(|id| *id != keep).collect::<Vec<_>>()).unwrap_or_default() {
                send(json!({"cmd": "close", "id": id}));
            }
        }
        #[unsafe(method(splitTab:))]
        fn split_tab(&self, sender: &NSMenuItem) {
            shell_action(json!({"action": "tab", "id": sender.tag(), "split": true}));
        }
        #[unsafe(method(duplicateTab:))]
        fn duplicate_tab(&self, sender: &NSMenuItem) {
            if let Some(url) = tab_url(sender.tag() as u64) {
                send(json!({"cmd": "newTab", "url": url}));
            }
        }
        #[unsafe(method(copyTabLink:))]
        fn copy_tab_link(&self, sender: &NSMenuItem) {
            if let Some(url) = tab_url(sender.tag() as u64) {
                copy_text(&url);
            }
        }
        #[unsafe(method(pinTab:))]
        fn pin_tab(&self, sender: &NSMenuItem) {
            shell_action(json!({"action": "pinTab", "id": sender.tag()}));
        }
        #[unsafe(method(moveTabToSpace:))]
        fn move_tab_to_space(&self, sender: &NSMenuItem) {
            let tag = sender.tag() as u64;
            shell_action(json!({"action": "moveToSpace", "id": tag >> 16, "space": tag & 0xffff}));
        }
        #[unsafe(method(openPin:))]
        fn open_pin(&self, sender: &NSMenuItem) {
            shell_action(json!({"action": "pin", "i": sender.tag()}));
        }
        #[unsafe(method(copyPinLink:))]
        fn copy_pin_link(&self, sender: &NSMenuItem) {
            if let Some(url) = with(|s| s.model.pinned.get(sender.tag() as usize).map(|p| p.url.clone())).flatten() {
                copy_text(&url);
            }
        }
        #[unsafe(method(unpin:))]
        fn unpin(&self, sender: &AnyObject) {
            shell_action(json!({"action": "unpin", "i": tag(sender)}));
        }
        #[unsafe(method(removeFavorite:))]
        fn remove_favorite(&self, sender: &NSMenuItem) {
            shell_action(json!({"action": "favRemove", "i": sender.tag()}));
        }
        #[unsafe(method(openChat:))]
        fn open_chat(&self, sender: &NSMenuItem) {
            if let Some(id) = with(|s| s.model.chats.get(sender.tag() as usize).map(|c| c.id.clone())).flatten() {
                shell_action(json!({"action": "chat", "id": id}));
            }
        }
        #[unsafe(method(newChat:))]
        fn new_chat(&self, _sender: &AnyObject) {
            shell_action(json!({"action": "newChat"}));
        }
        #[unsafe(method(renameSpace:))]
        fn rename_space(&self, _sender: &NSMenuItem) {
            begin_rename();
        }
        #[unsafe(method(spaceRenamed:))]
        fn space_renamed(&self, field: &NSTextField) {
            shell_action(json!({"action": "renameSpace", "name": field.stringValue().to_string()}));
            field.setEditable(false);
            if let Some(window) = field.window() {
                window.makeFirstResponder(None);
            }
        }
    }

    unsafe impl NSObjectProtocol for Source {}

    unsafe impl NSTableViewDataSource for Source {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> NSInteger {
            with(|s| s.rows.len() as NSInteger).unwrap_or(0)
        }

        #[unsafe(method_id(tableView:pasteboardWriterForRow:))]
        fn writer_for_row(&self, _table: &NSTableView, row: NSInteger) -> Option<Retained<ProtocolObject<dyn NSPasteboardWriting>>> {
            pasteboard_writer(row)
        }

        #[unsafe(method(tableView:validateDrop:proposedRow:proposedDropOperation:))]
        fn validate_drop(
            &self,
            table: &NSTableView,
            info: &ProtocolObject<dyn NSDraggingInfo>,
            row: NSInteger,
            _op: NSTableViewDropOperation,
        ) -> NSDragOperation {
            match dragged_row(info).and_then(|from| drop_action(from, row)) {
                Some(_) => {
                    table.setDropRow_dropOperation(row, NSTableViewDropOperation::Above);
                    NSDragOperation::Move
                }
                None => NSDragOperation::None,
            }
        }

        #[unsafe(method(tableView:acceptDrop:row:dropOperation:))]
        fn accept_drop(
            &self,
            _table: &NSTableView,
            info: &ProtocolObject<dyn NSDraggingInfo>,
            row: NSInteger,
            _op: NSTableViewDropOperation,
        ) -> bool {
            dragged_row(info).is_some_and(|from| drop_rows(from, row))
        }
    }

    unsafe impl NSControlTextEditingDelegate for Source {}

    unsafe impl NSTableViewDelegate for Source {
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn view_for_row(&self, table: &NSTableView, _column: Option<&NSTableColumn>, row: NSInteger) -> Option<Retained<NSView>> {
            model_row(row).map(|(model, r)| cell(table, &model, r, self))
        }

        #[unsafe(method(tableView:heightOfRow:))]
        fn height_of_row(&self, _table: &NSTableView, row: NSInteger) -> f64 {
            match model_row(row) {
                Some((m, Row::Favs)) => m.favs.len().div_ceil(FAVS_PER_LINE) as f64 * 46.0 + 4.0,
                Some((_, Row::SpaceHeader | Row::ChatsHeader)) => 30.0,
                _ => 30.0,
            }
        }

        #[unsafe(method(tableView:shouldSelectRow:))]
        fn should_select_row(&self, _table: &NSTableView, row: NSInteger) -> bool {
            matches!(model_row(row), Some((_, Row::Pin(_) | Row::NewTab | Row::Tab(_) | Row::Chat(_))))
        }
    }
);

struct HoverIvars {
    close: RefCell<Option<Retained<NSButton>>>,
    keep_close: Cell<bool>,
}

define_class!(
    /// A row that shows its close button while the pointer is over it, like Safari's sidebar.
    #[unsafe(super(NSTableCellView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = HoverIvars]
    struct HoverCell;

    impl HoverCell {
        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            if let Some(close) = self.ivars().close.borrow().as_ref() {
                close.setHidden(false);
            }
        }
        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            if let Some(close) = self.ivars().close.borrow().as_ref() {
                close.setHidden(!self.ivars().keep_close.get());
            }
        }
    }
);

fn pasteboard_writer(row: NSInteger) -> Option<Retained<ProtocolObject<dyn NSPasteboardWriting>>> {
    let (_, r) = model_row(row)?;
    if !matches!(r, Row::Tab(_) | Row::Pin(_)) {
        return None;
    }
    let item = NSPasteboardItem::new();
    item.setString_forType(&NSString::from_str(&row.to_string()), &NSString::from_str(ROW_TYPE));
    Some(ProtocolObject::from_retained(item))
}

fn tag(sender: &AnyObject) -> NSInteger {
    unsafe { msg_send![sender, tag] }
}

fn tab_url(id: u64) -> Option<String> {
    with(|s| s.model.tabs.iter().find(|t| t.id == id).map(|t| t.url.clone())).flatten()
}

fn copy_text(text: &str) {
    let board = NSPasteboard::generalPasteboard();
    board.clearContents();
    board.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString });
}

fn activate_row(row: NSInteger, split: bool) {
    let Some((m, r)) = model_row(row) else { return };
    match r {
        Row::Pin(i) => shell_action(json!({"action": "pin", "i": i})),
        Row::NewTab => shell_action(json!({"action": "newTab"})),
        Row::Tab(i) => shell_action(json!({"action": "tab", "id": m.tabs[i].id, "split": split})),
        Row::Chat(i) => shell_action(json!({"action": "chat", "id": m.chats[i].id})),
        Row::Favs | Row::SpaceHeader | Row::ChatsHeader => {}
    }
}

fn middle_click(row: NSInteger) {
    if let Some((m, Row::Tab(i))) = model_row(row) {
        send(json!({"cmd": "close", "id": m.tabs[i].id}));
    }
}

fn dragged_row(info: &ProtocolObject<dyn NSDraggingInfo>) -> Option<NSInteger> {
    let board = info.draggingPasteboard();
    board.stringForType(&NSString::from_str(ROW_TYPE))?.to_string().parse().ok()
}

/// What dropping row `from` above row `to` does, if anything.
fn drop_action(from: NSInteger, to: NSInteger) -> Option<Json> {
    let (m, rows) = with(|s| (s.model.clone(), s.rows.clone()))?;
    let source = rows.get(usize::try_from(from).ok()?).copied()?;
    let to = usize::try_from(to).ok()?;
    let first_pin = rows.iter().position(|r| *r == Row::SpaceHeader)? + 1;
    let new_tab = rows.iter().position(|r| *r == Row::NewTab)?;
    let first_tab = new_tab + 1;
    let pins = first_pin..=new_tab;
    let tabs = first_tab..=first_tab + m.tabs.len();
    match source {
        Row::Tab(i) if tabs.contains(&to) => {
            let j = to - first_tab;
            if j == i || j == i + 1 {
                return None;
            }
            Some(json!({"cmd": "moveTab", "id": m.tabs[i].id, "before": m.tabs.get(j).map(|t| t.id)}))
        }
        Row::Tab(i) if pins.contains(&to) => {
            Some(json!({"cmd": "sidebarAction", "action": {"action": "pinTab", "id": m.tabs[i].id}}))
        }
        Row::Pin(i) if pins.contains(&to) => {
            let j = to - first_pin;
            if j == i || j == i + 1 {
                return None;
            }
            let to = if j > i { j - 1 } else { j };
            Some(json!({"cmd": "sidebarAction", "action": {"action": "pinMove", "from": i, "to": to}}))
        }
        _ => None,
    }
}

fn drop_rows(from: NSInteger, to: NSInteger) -> bool {
    match drop_action(from, to) {
        Some(command) => {
            send(command);
            true
        }
        None => false,
    }
}

fn menu_item(menu: &NSMenu, title: &str, action: Sel, tag: NSInteger, target: &Source) -> Retained<NSMenuItem> {
    let mtm = MainThreadMarker::from(target);
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(title), Some(action), &NSString::new())
    };
    unsafe { item.setTarget(Some(target)) };
    item.setTag(tag);
    menu.addItem(&item);
    item
}

fn menu_for_row(row: NSInteger) -> Option<Retained<NSMenu>> {
    let (m, r) = model_row(row)?;
    let source = with(|s| s.source.clone())?;
    let mtm = MainThreadMarker::from(&*source);
    let menu = NSMenu::new(mtm);
    let separator = || menu.addItem(&NSMenuItem::separatorItem(mtm));
    match r {
        Row::Tab(i) => {
            let t = &m.tabs[i];
            let id = t.id as NSInteger;
            if !t.active {
                menu_item(&menu, "Open in Split View", sel!(splitTab:), id, &source);
            }
            menu_item(&menu, "Duplicate", sel!(duplicateTab:), id, &source);
            menu_item(&menu, "Copy Link", sel!(copyTabLink:), id, &source);
            menu_item(&menu, "Pin Tab", sel!(pinTab:), id, &source);
            let others: Vec<_> = m.spaces.iter().filter(|s| s.id != m.space.id).collect();
            if !others.is_empty() {
                let parent = menu_item(&menu, "Move to Space", sel!(moveTabToSpace:), 0, &source);
                let sub = NSMenu::new(mtm);
                for space in others {
                    menu_item(&sub, &space.name, sel!(moveTabToSpace:), ((t.id << 16) | space.id) as NSInteger, &source);
                }
                parent.setSubmenu(Some(&sub));
            }
            separator();
            menu_item(&menu, "Close Tab", sel!(closeTab:), id, &source);
            if m.tabs.len() > 1 {
                menu_item(&menu, "Close Other Tabs", sel!(closeOtherTabs:), id, &source);
            }
        }
        Row::Pin(i) => {
            menu_item(&menu, "Open", sel!(openPin:), i as NSInteger, &source);
            menu_item(&menu, "Copy Link", sel!(copyPinLink:), i as NSInteger, &source);
            separator();
            menu_item(&menu, "Unpin", sel!(unpin:), i as NSInteger, &source);
        }
        Row::Chat(i) => {
            menu_item(&menu, "Open Chat", sel!(openChat:), i as NSInteger, &source);
        }
        Row::SpaceHeader => {
            menu_item(&menu, "Rename Space…", sel!(renameSpace:), 0, &source);
        }
        Row::ChatsHeader => {
            menu_item(&menu, "New Chat", sel!(newChat:), 0, &source);
        }
        Row::Favs | Row::NewTab => return None,
    }
    Some(menu)
}

fn fav_menu(i: usize, source: &Source) -> Retained<NSMenu> {
    let menu = NSMenu::new(MainThreadMarker::from(source));
    menu_item(&menu, "Remove from Favorites", sel!(removeFavorite:), i as NSInteger, source);
    menu
}

/// Starts editing the space name in place (Rename Space…).
fn begin_rename() {
    let Some((table, row)) = with(|s| (s.table.clone(), s.rows.iter().position(|r| *r == Row::SpaceHeader))) else { return };
    let Some(row) = row else { return };
    let Some(view) = table.viewAtColumn_row_makeIfNecessary(0, row as NSInteger, true) else { return };
    let Some(cell) = view.downcast_ref::<NSTableCellView>() else { return };
    let Some(field) = (unsafe { cell.textField() }) else { return };
    field.setEditable(true);
    if let Some(window) = field.window() {
        window.makeFirstResponder(Some(&field));
    }
}

fn frame(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn symbol(name: &str, label: &str) -> Retained<NSImage> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), Some(&NSString::from_str(label)))
        .expect("SF Symbol")
}

/// A copy drawn at `points` square; cached images are shared between rows.
fn sized(image: &NSImage, points: f64) -> Retained<NSImage> {
    let copy: Retained<NSImage> = unsafe { msg_send![image, copy] };
    copy.setSize(NSSize::new(points, points));
    copy
}

fn color(hex: &str) -> Retained<NSColor> {
    let v = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0x0a84ff);
    let c = |shift: u32| ((v >> shift) & 0xff) as f64 / 255.0;
    NSColor::colorWithSRGBRed_green_blue_alpha(c(16), c(8), c(0), 1.0)
}

/// The cached favicon, or a globe while it loads (and for sites without one).
fn icon(url: &str) -> Retained<NSImage> {
    let cached = with(|s| {
        if let Some(image) = s.icons.get(url) {
            return Some(image.clone());
        }
        if (url.starts_with("https://") || url.starts_with("http://")) && s.requested.insert(url.to_string()) {
            let _ = s.fetch.send(url.to_string());
        }
        None
    })
    .flatten();
    cached.unwrap_or_else(|| symbol("globe", "Website"))
}

fn label(text: &str, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    field.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingTail);
    field.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    field
}

fn small_button(symbol_name: &str, title: &str, action: Sel, tag: NSInteger, target: &Source) -> Retained<NSButton> {
    let mtm = MainThreadMarker::from(target);
    let button = unsafe { NSButton::buttonWithImage_target_action(&symbol(symbol_name, title), Some(target), Some(action), mtm) };
    button.setBezelStyle(NSBezelStyle::AccessoryBarAction);
    button.setShowsBorderOnlyWhileMouseInside(true);
    button.setControlSize(NSControlSize::Small);
    button.setToolTip(Some(&NSString::from_str(title)));
    button.setTag(tag);
    button
}

fn cell(table: &NSTableView, m: &Model, row: Row, source: &Source) -> Retained<NSView> {
    let mtm = MainThreadMarker::from(source);
    let width = table.tableColumns().firstObject().map_or(200.0, |c| c.width());
    match row {
        Row::Favs => favs_cell(m, width, source),
        Row::SpaceHeader => header_cell(&m.space.name, Some(&m.space.color), width, None, source),
        Row::ChatsHeader => header_cell("Chats", None, width, Some(small_button("square.and.pencil", "New Chat (⌘N)", sel!(newChat:), 0, source)), source),
        Row::Pin(i) => {
            let p = &m.pinned[i];
            let close = small_button("xmark", "Unpin", sel!(unpin:), i as NSInteger, source);
            item_cell(width, mtm, Item { title: &p.title, tooltip: &p.url, image: Some(icon(&p.icon)), dim: !p.live, close: Some(close), keep_close: p.active, ..Item::default() })
        }
        Row::NewTab => item_cell(width, mtm, Item { title: "New Tab", tooltip: "New Tab (⌘T)", image: Some(symbol("plus", "New Tab")), dim: true, ..Item::default() }),
        Row::Tab(i) => {
            let t = &m.tabs[i];
            let close = small_button("xmark", "Close Tab (⌘W)", sel!(closeTab:), t.id as NSInteger, source);
            let mut badges = Vec::new();
            if t.split {
                badges.push(symbol("rectangle.split.2x1", "In split view"));
            }
            if t.agent {
                badges.push(symbol("circle.fill", "The agent is working here"));
            }
            item_cell(width, mtm, Item {
                title: &t.title,
                tooltip: &t.url,
                image: (!t.loading).then(|| icon(&t.icon)),
                spinning: t.loading,
                dim: t.suspended,
                close: Some(close),
                keep_close: t.active,
                badges,
            })
        }
        Row::Chat(i) => {
            let c = &m.chats[i];
            item_cell(width, mtm, Item {
                title: &c.title,
                tooltip: &c.title,
                image: (!c.busy).then(|| symbol("bubble.left", "Chat")),
                spinning: c.busy,
                ..Item::default()
            })
        }
    }
}

#[derive(Default)]
struct Item<'a> {
    title: &'a str,
    tooltip: &'a str,
    image: Option<Retained<NSImage>>,
    spinning: bool,
    dim: bool,
    close: Option<Retained<NSButton>>,
    keep_close: bool,
    badges: Vec<Retained<NSImage>>,
}

fn item_cell(width: f64, mtm: MainThreadMarker, item: Item) -> Retained<NSView> {
    let height = 30.0;
    let cell: Retained<HoverCell> = unsafe {
        msg_send![super(HoverCell::alloc(mtm).set_ivars(HoverIvars { close: RefCell::new(None), keep_close: Cell::new(item.keep_close) })), initWithFrame: frame(0.0, 0.0, width, height)]
    };
    cell.setToolTip(Some(&NSString::from_str(item.tooltip)));
    if item.spinning {
        let spinner = NSProgressIndicator::new(mtm);
        spinner.setStyle(NSProgressIndicatorStyle::Spinning);
        spinner.setControlSize(NSControlSize::Small);
        spinner.setFrame(frame(6.0, 7.0, 16.0, 16.0));
        unsafe { spinner.startAnimation(None) };
        cell.addSubview(&spinner);
    } else if let Some(image) = item.image {
        let view = NSImageView::imageViewWithImage(&image, mtm);
        view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
        view.setFrame(frame(6.0, 7.0, 16.0, 16.0));
        if item.dim {
            view.setAlphaValue(0.55);
        }
        unsafe { cell.setImageView(Some(&view)) };
        cell.addSubview(&view);
    }
    let mut trailing = width - 4.0;
    if let Some(close) = &item.close {
        trailing -= 22.0;
        close.setFrame(frame(trailing, 5.0, 22.0, 20.0));
        close.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        close.setHidden(!item.keep_close);
        cell.addSubview(close);
    }
    for badge in item.badges.iter().rev() {
        trailing -= 18.0;
        let view = NSImageView::imageViewWithImage(badge, mtm);
        view.setFrame(frame(trailing, 9.0, 12.0, 12.0));
        view.setContentTintColor(Some(&NSColor::controlAccentColor()));
        view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        cell.addSubview(&view);
    }
    let text = label(item.title, mtm);
    if item.dim {
        text.setTextColor(Some(&NSColor::secondaryLabelColor()));
    }
    let x = 30.0;
    text.setFrame(frame(x, 6.0, (trailing - x - 2.0).max(10.0), 18.0));
    text.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    unsafe { cell.setTextField(Some(&text)) };
    cell.addSubview(&text);
    *cell.ivars().close.borrow_mut() = item.close;

    let tracking = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            NSRect::ZERO,
            NSTrackingAreaOptions::MouseEnteredAndExited | NSTrackingAreaOptions::ActiveAlways | NSTrackingAreaOptions::InVisibleRect,
            Some(&cell),
            None,
        )
    };
    cell.addTrackingArea(&tracking);
    Retained::into_super(Retained::into_super(cell))
}

fn header_cell(title: &str, dot: Option<&str>, width: f64, accessory: Option<Retained<NSButton>>, source: &Source) -> Retained<NSView> {
    let mtm = MainThreadMarker::from(source);
    let cell: Retained<NSTableCellView> = unsafe { msg_send![NSTableCellView::alloc(mtm), initWithFrame: frame(0.0, 0.0, width, 30.0)] };
    let mut x = 8.0;
    if let Some(hex) = dot {
        let view = NSImageView::imageViewWithImage(&symbol("circle.fill", "Space color"), mtm);
        view.setContentTintColor(Some(&color(hex)));
        view.setFrame(frame(x, 11.0, 8.0, 8.0));
        cell.addSubview(&view);
        x += 14.0;
    }
    let text = label(title, mtm);
    text.setFont(Some(&NSFont::systemFontOfSize_weight(11.0, unsafe { NSFontWeightSemibold })));
    text.setTextColor(Some(&NSColor::secondaryLabelColor()));
    let right = if accessory.is_some() { width - 30.0 } else { width - 4.0 };
    text.setFrame(frame(x, 7.0, right - x, 16.0));
    text.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    if dot.is_some() {
        unsafe {
            text.setTarget(Some(source));
            text.setAction(Some(sel!(spaceRenamed:)));
        }
        text.setAccessibilityLabel(Some(&NSString::from_str("Space name")));
    }
    unsafe { cell.setTextField(Some(&text)) };
    cell.addSubview(&text);
    if let Some(button) = accessory {
        button.setFrame(frame(width - 28.0, 5.0, 24.0, 20.0));
        button.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        cell.addSubview(&button);
    }
    Retained::into_super(cell)
}

fn favs_cell(m: &Model, width: f64, source: &Source) -> Retained<NSView> {
    let mtm = MainThreadMarker::from(source);
    let lines = NSStackView::new(mtm);
    lines.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    lines.setSpacing(6.0);
    lines.setDistribution(NSStackViewDistribution::FillEqually);
    for (n, chunk) in m.favs.chunks(FAVS_PER_LINE).enumerate() {
        let line = NSStackView::new(mtm);
        line.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        line.setSpacing(6.0);
        line.setDistribution(NSStackViewDistribution::FillEqually);
        for (k, fav) in chunk.iter().enumerate() {
            let i = n * FAVS_PER_LINE + k;
            let button = unsafe { NSButton::buttonWithImage_target_action(&sized(&icon(&fav.icon), 20.0), Some(source), Some(sel!(favClicked:)), mtm) };
            button.setBezelStyle(NSBezelStyle::FlexiblePush);
            button.setButtonType(NSButtonType::PushOnPushOff);
            button.setState(if fav.active { NSControlStateValueOn } else { NSControlStateValueOff });
            button.setTag(i as NSInteger);
            button.setToolTip(Some(&NSString::from_str(&fav.title)));
            button.setAccessibilityLabel(Some(&NSString::from_str(&fav.title)));
            unsafe { button.setMenu(Some(&fav_menu(i, source))) };
            line.addArrangedSubview(&button);
        }
        // Keep a short last line's tiles the same size as full ones.
        for _ in chunk.len()..FAVS_PER_LINE {
            line.addArrangedSubview(&NSView::new(mtm));
        }
        lines.addArrangedSubview(&line);
    }
    let height = m.favs.len().div_ceil(FAVS_PER_LINE) as f64 * 46.0 + 4.0;
    lines.setFrame(frame(2.0, 2.0, width - 4.0, height - 8.0));
    lines.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    let cell = NSView::initWithFrame(NSView::alloc(mtm), frame(0.0, 0.0, width, height));
    cell.addSubview(&lines);
    cell
}

/// Builds the list (hidden until the shell reports where its sidebar list is).
pub(super) fn install(parent: &NSView, bridge: Bridge) {
    let mtm = MainThreadMarker::from(parent);
    let fetch = favicon_fetcher(bridge.clone());
    let source: Retained<Source> = unsafe { msg_send![super(Source::alloc(mtm).set_ivars(SourceIvars { bridge })), init] };

    let table: Retained<SidebarTable> = unsafe { msg_send![SidebarTable::alloc(mtm), initWithFrame: NSRect::ZERO] };
    let column = NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &NSString::from_str("main"));
    column.setResizingMask(NSTableColumnResizingOptions::AutoresizingMask);
    table.addTableColumn(&column);
    table.setHeaderView(None);
    table.setStyle(NSTableViewStyle::SourceList);
    table.setRowSizeStyle(NSTableViewRowSizeStyle::Custom);
    table.setBackgroundColor(&NSColor::clearColor());
    table.setIntercellSpacing(NSSize::new(0.0, 2.0));
    table.setAllowsMultipleSelection(true);
    table.setAllowsEmptySelection(true);
    table.setColumnAutoresizingStyle(objc2_app_kit::NSTableViewColumnAutoresizingStyle::FirstColumnOnlyAutoresizingStyle);
    table.setAccessibilityLabel(Some(&NSString::from_str("Tabs")));
    table.registerForDraggedTypes(&NSArray::from_retained_slice(&[NSString::from_str(ROW_TYPE)]));
    table.setDraggingSourceOperationMask_forLocal(NSDragOperation::Move, true);
    unsafe {
        table.setDataSource(Some(ProtocolObject::from_ref(&*source)));
        table.setDelegate(Some(ProtocolObject::from_ref(&*source)));
        table.setTarget(Some(&source));
        table.setAction(Some(sel!(rowClicked:)));
    }

    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame(10.0, 80.0, 232.0, 400.0));
    scroll.setDrawsBackground(false);
    scroll.setHasVerticalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setDocumentView(Some(&table));
    scroll.setHidden(true);
    table.sizeLastColumnToFit();
    parent.addSubview(&scroll);

    SIDEBAR.with(|s| {
        *s.borrow_mut() = Some(Sidebar {
            scroll,
            table,
            source,
            model: Model::default(),
            rows: Vec::new(),
            icons: HashMap::new(),
            requested: HashSet::new(),
            fetch,
            placed: false,
        })
    });
}

/// One background thread fetches favicons, so the UI thread never waits on the network.
fn favicon_fetcher(bridge: Bridge) -> mpsc::Sender<String> {
    let (tx, rx) = mpsc::channel::<String>();
    let spawned = std::thread::Builder::new().name("favicons".into()).spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(10)).build() else { return };
        while let Ok(url) = rx.recv() {
            let body = rt.block_on(async { client.get(&url).send().await?.error_for_status()?.bytes().await });
            if let Ok(bytes) = body {
                bridge.send(Event::Icon(url, bytes.to_vec()));
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("favicon thread: {e}");
    }
    tx
}

pub(super) fn icon_loaded(url: &str, bytes: &[u8]) {
    let data = NSData::with_bytes(bytes);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else { return };
    let Some((table, rows)) = with(|s| {
        s.icons.insert(url.to_string(), image);
        let m = &s.model;
        let rows: Vec<usize> = s
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| match **r {
                Row::Favs => m.favs.iter().any(|f| f.icon == url),
                Row::Pin(i) => m.pinned[i].icon == url,
                Row::Tab(i) => m.tabs[i].icon == url,
                _ => false,
            })
            .map(|(i, _)| i)
            .collect();
        (s.table.clone(), rows)
    }) else {
        return;
    };
    reload_rows(&table, &rows);
}

fn reload_rows(table: &NSTableView, rows: &[usize]) {
    if rows.is_empty() {
        return;
    }
    let set = NSMutableIndexSet::new();
    for r in rows {
        set.addIndex(*r);
    }
    table.reloadDataForRowIndexes_columnIndexes(&set, &NSIndexSet::indexSetWithIndex(0));
}

pub(super) fn set_model(model: Model) {
    let Some((table, structural, changed)) = with(|s| {
        let rows = rows(&model);
        let structural = rows != s.rows;
        let changed: Vec<usize> = if structural {
            Vec::new()
        } else {
            rows.iter().enumerate().filter(|(_, r)| !same(&s.model, &model, **r)).map(|(i, _)| i).collect()
        };
        s.model = model;
        s.rows = rows;
        (s.table.clone(), structural, changed)
    }) else {
        return;
    };
    if structural {
        table.reloadData();
    } else {
        reload_rows(&table, &changed);
        // Row heights only change with the favorites row, which `rows` treats as structural.
    }
    sync_selection(&table);
}

fn sync_selection(table: &NSTableView) {
    let Some(want) = with(|s| s.rows.iter().enumerate().filter(|(_, r)| selected(&s.model, **r)).map(|(i, _)| i).collect::<Vec<_>>()) else {
        return;
    };
    let set = NSMutableIndexSet::new();
    for r in &want {
        set.addIndex(*r);
    }
    table.selectRowIndexes_byExtendingSelection(&set, false);
}

/// Where the shell's (hidden) sidebar list is, in window coordinates from the top left.
pub(super) fn place(x: f64, y: f64, w: f64, h: f64) {
    let Some(scroll) = with(|s| {
        s.placed = true;
        s.scroll.clone()
    }) else {
        return;
    };
    scroll.setFrame(frame(x, y, w, h));
}

pub(super) fn set_visible(visible: bool) {
    if let Some((scroll, placed)) = with(|s| (s.scroll.clone(), s.placed)) {
        scroll.setHidden(!(visible && placed));
    }
}

/// Test hook: drives the same code paths as clicks, middle clicks, context menu items
/// (by title, `Parent/Child` for submenus), drags, favorites and renaming.
pub(super) fn debug_action(action: &str, row: i64, arg: &Json) -> Result<(), String> {
    let row = row as NSInteger;
    match action {
        "click" => activate_row(row, arg["split"] == true),
        "middle" => middle_click(row),
        "menu" => {
            let path = arg["item"].as_str().ok_or("menu needs an item title")?;
            let mut menu = menu_for_row(row).ok_or("row has no menu")?;
            let mut parts = path.split('/').peekable();
            while let Some(title) = parts.next() {
                let index = menu.indexOfItemWithTitle(&NSString::from_str(title));
                if index < 0 {
                    return Err(format!("no menu item {title:?}"));
                }
                if parts.peek().is_some() {
                    menu = menu.itemAtIndex(index).and_then(|i| i.submenu()).ok_or("not a submenu")?;
                } else {
                    menu.performActionForItemAtIndex(index);
                }
            }
        }
        "drag" => {
            let to = arg["to"].as_i64().ok_or("drag needs a target row")? as NSInteger;
            if !drop_rows(row, to) {
                return Err("drop refused".into());
            }
        }
        "fav" => shell_action(json!({"action": "fav", "i": row})),
        "rename" => {
            begin_rename();
            let name = arg["name"].as_str().unwrap_or("");
            let (table, header) = with(|s| (s.table.clone(), s.rows.iter().position(|r| *r == Row::SpaceHeader))).ok_or("no sidebar")?;
            let view = table.viewAtColumn_row_makeIfNecessary(0, header.ok_or("no space header")? as NSInteger, true).ok_or("no header view")?;
            let field = view.downcast_ref::<NSTableCellView>().and_then(|c| unsafe { c.textField() }).ok_or("no name field")?;
            if !field.isEditable() {
                return Err("name field is not editable".into());
            }
            field.setStringValue(&NSString::from_str(name));
            unsafe { field.sendAction_to(field.action(), field.target().as_deref()) };
        }
        other => return Err(format!("unknown sidebar action {other}")),
    }
    Ok(())
}

/// The rows as drawn, for tests.
pub(super) fn describe() -> Option<Json> {
    let (table, scroll, rows) = with(|s| (s.table.clone(), s.scroll.clone(), s.rows.clone()))?;
    let selected = table.selectedRowIndexes();
    let items: Vec<Json> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let view = table.viewAtColumn_row_makeIfNecessary(0, i as NSInteger, true);
            let field = view.as_ref().and_then(|v| v.downcast_ref::<NSTableCellView>().and_then(|c| unsafe { c.textField() }));
            let title = field.as_ref().map(|f| f.stringValue().to_string());
            let dim = field.as_ref().and_then(|f| f.textColor()).is_some_and(|c| c.isEqual(Some(&NSColor::secondaryLabelColor())));
            let spinning = view.as_ref().is_some_and(|v| v.subviews().iter().any(|s| s.isKindOfClass(NSProgressIndicator::class())));
            json!({
                "row": i,
                "kind": format!("{r:?}").split('(').next().unwrap_or_default().to_string(),
                "title": title,
                "selected": selected.containsIndex(i),
                "spinning": spinning,
                "dim": dim,
            })
        })
        .collect();
    let f = scroll.frame();
    Some(json!({
        "visible": !scroll.isHidden(),
        "frame": {"x": f.origin.x, "y": f.origin.y, "w": f.size.width, "h": f.size.height},
        "icons_cached": with(|s| s.icons.len()).unwrap_or(0),
        "rows": items,
    }))
}
