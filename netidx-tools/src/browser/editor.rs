use super::FromGui;
use futures::channel::mpsc;
use glib::clone;
use gtk::{self, prelude::*};
use log::warn;
use netidx::{chars::Chars, subscriber::Value, path::Path};
use netidx_protocols::view;
use std::{cell::RefCell, rc::Rc, result};

type OnChange = Rc<dyn Fn(view::Widget)>;

struct KindWrap {
    root: gtk::Box,
}

impl KindWrap {
    fn new(on_change: OnChange, spec: view::Widget) -> KindWrap {
        let kinds =
            ["Table", "Label", "Button", "Toggle", "Selector", "Entry", "Box", "Grid"];
        let kind = gtk::ComboBoxText::new();
        for k in &kinds {
            kind.append_text(k);
        }
        kind.set_active_id(Some(match spec {
            view::Widget::Table(_) => "Table",
            view::Widget::Label(_) => "Label",
            view::Widget::Button(_) => "Button",
            view::Widget::Toggle(_) => "Toggle",
            view::Widget::Selector(_) => "Selector",
            view::Widget::Entry(_) => "Entry",
            view::Widget::Box(_) => "Box",
            view::Widget::Grid(_) => "Grid",
        }));
        let widget = RefCell::new(Widget::new(on_change.clone(), spec));
        let root = gtk::Box::new(gtk::Orientation::Vertical, 5);
        root.add(&kind);
        {
            let wr = widget.borrow();
            root.add(wr.root());
        }
        kind.connect_changed(clone!(@weak root => move |c| match c.get_active_id() {
            Some(s) if &*s == "Table" => {
                let mut wr = widget.borrow_mut();
                root.remove(wr.root());
                *wr = Widget::Table(Table::new(on_change.clone(), Path::from("/")));
                root.add(wr.root());
                on_change(view::Widget::Table(Path::from("/")));
            }
            Some(s) if &*s == "Label" => {
                let s = Value::String(Chars::from("static label"));
                let spec = view::Source::Constant(s);
                let mut wr = widget.borrow_mut();
                root.remove(wr.root());
                *wr = Widget::Label(Label::new(on_change.clone(), spec.clone()));
                root.add(wr.root());
                on_change(view::Widget::Label(spec));
            }
            Some(s) if &*s == "Button" => todo!(),
            Some(s) if &*s == "Toggle" => todo!(),
            Some(s) if &*s == "Selector" => todo!(),
            Some(s) if &*s == "Entry" => todo!(),
            Some(s) if &*s == "Box" => todo!(),
            Some(s) if &*s == "Grid" => todo!(),
            None => (), // CR estokes: hmmm
            _ => unreachable!(),
        }));
        KindWrap { root }
    }

    fn root(&self) -> &gtk::Widget {
        self.root.upcast_ref()
    }
}

struct BoxChild {
    expand: gtk::CheckButton,
    fill: gtk::CheckButton,
    padding: gtk::Entry,
    halign: gtk::ComboBoxText,
    valign: gtk::ComboBoxText,
    kind: gtk::ComboBoxText,
    delete: gtk::Button,
    child: Widget,
}

struct GridChild {
    row: usize,
    col: usize,
    spec: Rc<view::Grid>,
    parent: OnChange,
    id: usize,
    halign: gtk::ComboBoxText,
    valign: gtk::ComboBoxText,
    delete: gtk::Button,
    kind: gtk::ComboBoxText,
    child: Widget,
}

struct GridRow {
    row: usize,
    spec: Rc<view::Grid>,
    parent: OnChange,
    revealer: gtk::Revealer,
    container: gtk::Box,
    delete: gtk::Button,
    add: gtk::Button,
    contents: Vec<GridChild>,
}

struct Table {
    path: gtk::Entry,
}

impl Table {
    fn new(on_change: OnChange, path: Path) -> Self {
        let entry = gtk::Entry::new();
        entry.set_text(&*path);
        entry.connect_activate(move |e| {
            let path = Path::from(String::from(&*e.get_text()));
            on_change(view::Widget::Table(path))
        });
        Table { path: entry }
    }

    fn root(&self) -> &gtk::Widget {
        self.path.upcast_ref()
    }
}

struct Label {
    source: gtk::Entry,
}

impl Label {
    fn new(on_change: OnChange, spec: view::Source) -> Self {
        let source = gtk::Entry::new();
        source.set_text(&serde_json::to_string(&spec).unwrap());
        source.connect_activate(move |e| {
            let txt = e.get_text();
            match serde_json::from_str::<view::Source>(&*txt) {
                Err(e) => warn!("invalid source: {}, {}", txt, e),
                Ok(src) => on_change(view::Widget::Label(src)),
            }
        });
        Label { source }
    }

    fn root(&self) -> &gtk::Widget {
        self.source.upcast_ref()
    }
}

struct Button {
    parent: OnChange,
    enabled: gtk::Entry,
    label: gtk::Entry,
    source: gtk::Entry,
    sink: gtk::Entry,
}

struct Toggle {
    parent: OnChange,
    enabled: gtk::Entry,
    source: gtk::Entry,
    sink: gtk::Entry,
}

struct Selector {
    parent: OnChange,
    enabled: gtk::Entry,
    choices: gtk::Entry,
    source: gtk::Entry,
    sink: gtk::Entry,
}

struct Entry {
    parent: OnChange,
    enabled: gtk::Entry,
    visible: gtk::Entry,
    source: gtk::Entry,
    sink: gtk::Entry,
}

struct Box {
    parent: OnChange,
    direction: gtk::ComboBoxText,
    revealer: gtk::Revealer,
    container: gtk::Box,
    children: Vec<BoxChild>,
}

struct Grid {
    parent: OnChange,
    homogeneous_columns: gtk::CheckButton,
    homogeneous_rows: gtk::CheckButton,
    column_spacing: gtk::Entry,
    row_spacing: gtk::Entry,
    revealer: gtk::Revealer,
    container: gtk::Box,
    children: Vec<GridRow>,
}

enum Widget {
    Table(Table),
    Label(Label),
    Button(Button),
    Toggle(Toggle),
    Selector(Selector),
    Entry(Entry),
    Box(Box),
    Grid(Grid),
}

impl Widget {
    fn new(on_change: OnChange, spec: view::Widget) -> Self {
        match spec {
            view::Widget::Table(path) => Widget::Table(Table::new(on_change, path)),
            view::Widget::Label(src) => Widget::Label(Label::new(on_change, src)),
            view::Widget::Button(_) => todo!(),
            view::Widget::Toggle(_) => todo!(),
            view::Widget::Selector(_) => todo!(),
            view::Widget::Entry(_) => todo!(),
            view::Widget::Box(_) => todo!(),
            view::Widget::Grid(_) => todo!(),
        }
    }

    fn root(&self) -> &gtk::Widget {
        match self {
            Widget::Table(w) => w.root(),
            Widget::Label(w) => w.root(),
            Widget::Button(_) => todo!(),
            Widget::Toggle(_) => todo!(),
            Widget::Selector(_) => todo!(),
            Widget::Entry(_) => todo!(),
            Widget::Box(_) => todo!(),
            Widget::Grid(_) => todo!(),
        }
    }
}

pub(super) struct Editor {
    root: KindWrap,
}

impl Editor {
    pub(super) fn new(
        mut from_gui: mpsc::UnboundedSender<FromGui>,
        path: Path,
        spec: view::View,
    ) -> Editor {
        let spec = Rc::new(RefCell::new(spec));
        let on_change = Rc::new({
            let spec = Rc::clone(&spec);
            move |s: view::Widget| {
                spec.borrow_mut().root = s;
                let m = FromGui::Render(path.clone(), spec.borrow().clone());
                let _: result::Result<_, _> = from_gui.unbounded_send(m);
            }
        });
        let root = spec.borrow().root.clone();
        Editor { root: KindWrap::new(on_change, root) }
    }

    pub(super) fn root(&self) -> &gtk::Widget {
        self.root.root()
    }
}
