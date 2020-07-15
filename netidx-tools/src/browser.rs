use futures::{channel::mpsc, prelude::*, select_biased};
use gdk::{keys, EventKey};
use gio::prelude::*;
use glib::{self, clone, prelude::*, signal::Inhibit, subclass::prelude::*};
use gtk::{
    prelude::*, Adjustment, Align, Application, ApplicationWindow, Box as GtkBox,
    CellLayout, CellRenderer, CellRendererText, Label, ListStore, Orientation, PackType,
    ScrolledWindow, SelectionMode, SortColumn, StateFlags, TreeIter, TreeModel, TreePath,
    TreeStore, TreeView, TreeViewColumn, TreeViewColumnSizing,
};
use log::{debug, error, info, warn};
use netidx::{
    chars::Chars,
    config::Config,
    path::Path,
    pool::Pooled,
    resolver::{Auth, Table},
    subscriber::{Dval, SubId, Subscriber, Value},
};
use std::{
    cell::{Cell, RefCell},
    cmp::{max, Ordering},
    collections::{HashMap, HashSet},
    iter,
    rc::Rc,
    thread,
    time::Duration,
};
use tokio::{
    runtime::Runtime,
    time::{self, Instant},
};

type Batch = Pooled<Vec<(SubId, Value)>>;

#[derive(Debug, Clone)]
enum ToGui {
    Table(Subscriber, Path, Table),
    Batch(Batch),
    Refresh,
}

#[derive(Debug, Clone)]
enum FromGui {
    Navigate(Path),
}

struct Subscription {
    sub: Dval,
    row: TreeIter,
    col: u32,
}

struct NetidxTable {
    root: GtkBox,
    view: TreeView,
    store: ListStore,
    by_id: Rc<RefCell<HashMap<SubId, Subscription>>>,
    update_subscriptions: Rc<dyn Fn()>,
}

impl NetidxTable {
    fn new(
        subscriber: Subscriber,
        base_path: Path,
        mut descriptor: Table,
        updates: mpsc::Sender<Pooled<Vec<(SubId, Value)>>>,
        from_gui: mpsc::UnboundedSender<FromGui>,
    ) -> NetidxTable {
        let view = TreeView::new();
        let tablewin = ScrolledWindow::new(None::<&Adjustment>, None::<&Adjustment>);
        let root = GtkBox::new(Orientation::Vertical, 5);
        let selected_path = Label::new(None);
        selected_path.set_halign(Align::Start);
        selected_path.set_margin_start(5);
        tablewin.add(&view);
        root.add(&tablewin);
        root.set_child_packing(&tablewin, true, true, 1, PackType::Start);
        root.set_child_packing(&selected_path, false, false, 1, PackType::End);
        root.add(&selected_path);
        selected_path.set_selectable(true);
        selected_path.set_single_line_mode(true);
        let nrows = descriptor.rows.len();
        descriptor.rows.sort();
        descriptor.cols.sort_by_key(|(p, _)| p.clone());
        descriptor.cols.retain(|(_, i)| i.0 >= (nrows / 2) as u64);
        view.get_selection().set_mode(SelectionMode::None);
        let vector_mode = descriptor.cols.len() == 0;
        let column_types = if vector_mode {
            vec![String::static_type(); 2]
        } else {
            (0..descriptor.cols.len() + 1)
                .into_iter()
                .map(|_| String::static_type())
                .collect::<Vec<_>>()
        };
        let store = ListStore::new(&column_types);
        for row in descriptor.rows.iter() {
            let row_name = Path::basename(row).unwrap_or("").to_value();
            let row = store.append();
            store.set_value(&row, 0, &row_name.to_value());
        }
        for col in 1..descriptor.cols.len() + 1 {
            let col = col as u32;
            let f = move |m: &TreeModel, r0: &TreeIter, r1: &TreeIter| -> Ordering {
                let v0_v = m.get_value(r0, col as i32);
                let v1_v = m.get_value(r1, col as i32);
                let v0_r = v0_v.get::<&str>();
                let v1_r = v1_v.get::<&str>();
                match (v0_r, v1_r) {
                    (Err(_), Err(_)) => Ordering::Equal,
                    (Err(_), _) => Ordering::Greater,
                    (_, Err(_)) => Ordering::Less,
                    (Ok(None), Ok(None)) => Ordering::Equal,
                    (Ok(None), _) => Ordering::Less,
                    (_, Ok(None)) => Ordering::Greater,
                    (Ok(Some(v0)), Ok(Some(v1))) => {
                        match (v0.parse::<f64>(), v1.parse::<f64>()) {
                            (Ok(v0f), Ok(v1f)) => {
                                v0f.partial_cmp(&v1f).unwrap_or(Ordering::Equal)
                            }
                            (_, _) => v0.cmp(v1),
                        }
                    }
                }
            };
            store.set_sort_func(SortColumn::Index(col), f);
        }
        let descriptor = Rc::new(descriptor);
        let by_id: Rc<RefCell<HashMap<SubId, Subscription>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let style = view.get_style_context();
        let focus_column: Rc<RefCell<Option<TreeViewColumn>>> =
            Rc::new(RefCell::new(None));
        let focus_row: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let cursor_changed = Rc::new(clone!(
            @weak focus_column, @weak focus_row, @weak store,
            @weak selected_path, @strong base_path =>
            move |v: &TreeView| {
                let (p, c) = v.get_cursor();
                let row_name = match p {
                    None => None,
                    Some(p) => match store.get_iter(&p) {
                        None => None,
                        Some(i) => Some(store.get_value(&i, 0))
                    }
                };
                let path = match row_name {
                    None => Path::from(""),
                    Some(row_name) => match row_name.get::<&str>().ok().flatten() {
                        None => Path::from(""),
                        Some(row_name) => {
                            *focus_column.borrow_mut() = c.clone();
                            *focus_row.borrow_mut() = Some(String::from(row_name));
                            let col_name = if vector_mode {
                                None
                            } else if v.get_column(0) == c {
                                None
                            } else {
                                c.as_ref().and_then(|c| c.get_title())
                            };
                            match col_name {
                                None => base_path.append(row_name),
                                Some(col_name) =>
                                    base_path.append(row_name).append(col_name.as_str()),
                            }
                        }
                    }
                };
                selected_path.set_label(&*path);
                v.columns_autosize();
                let (mut start, end) = match v.get_visible_range() {
                    None => return,
                    Some((s, e)) => (s, e)
                };
                while start <= end {
                    if let Some(i) = store.get_iter(&start) {
                        store.row_changed(&start, &i);
                    }
                    start.next();
                }
            }
        ));
        let update_subscriptions = Rc::new({
            let base_path = base_path.clone();
            let view = view.downgrade();
            let store = store.downgrade();
            let by_id = by_id.clone();
            let cursor_changed = Rc::clone(&cursor_changed);
            let descriptor = Rc::clone(&descriptor);
            let subscribed: RefCell<HashMap<String, HashSet<u32>>> =
                RefCell::new(HashMap::new());
            move || {
                let view = match view.upgrade() {
                    None => return,
                    Some(view) => view,
                };
                let store = match store.upgrade() {
                    None => return,
                    Some(store) => store,
                };
                let ncols = if vector_mode { 1 } else { descriptor.cols.len() };
                let (mut start, mut end) = match view.get_visible_range() {
                    None => return,
                    Some((s, e)) => (s, e),
                };
                for _ in 0..50 {
                    start.prev();
                    end.next();
                }
                let mut setval = Vec::new();
                let sort_column = match store.get_sort_column_id() {
                    None | Some((SortColumn::Default, _)) => None,
                    Some((SortColumn::Index(c), _)) => {
                        if c == 0 {
                            None
                        } else {
                            Some(c)
                        }
                    }
                };
                // unsubscribe invisible rows
                by_id.borrow_mut().retain(|_, v| match store.get_path(&v.row) {
                    None => false,
                    Some(p) => {
                        let visible =
                            (p >= start && p <= end) || (Some(v.col) == sort_column);
                        if !visible {
                            let row_name_v = store.get_value(&v.row, 0);
                            if let Ok(Some(row_name)) = row_name_v.get::<&str>() {
                                let mut sub = subscribed.borrow_mut();
                                match sub.get_mut(row_name) {
                                    None => (),
                                    Some(set) => {
                                        set.remove(&v.col);
                                        if set.is_empty() {
                                            sub.remove(row_name);
                                        }
                                    }
                                }
                            }
                            setval.push((v.row.clone(), v.col, None));
                        }
                        visible
                    }
                });
                let mut maybe_subscribe_col =
                    |row: &TreeIter, row_name: &str, id: u32| {
                        let mut subs = subscribed.borrow_mut();
                        if !subs.get(row_name).map(|s| s.contains(&id)).unwrap_or(false) {
                            subs.entry(row_name.into())
                                .or_insert_with(HashSet::new)
                                .insert(id);
                            setval.push((row.clone(), id, Some("#subscribe")));
                            let p = base_path.append(row_name);
                            let p = if vector_mode {
                                p
                            } else {
                                p.append(&descriptor.cols[(id - 1) as usize].0)
                            };
                            let s = subscriber.durable_subscribe(p);
                            s.updates(true, updates.clone());
                            by_id.borrow_mut().insert(
                                s.id(),
                                Subscription { sub: s, row: row.clone(), col: id as u32 },
                            );
                        }
                    };
                // subscribe to all the visible rows
                while start < end {
                    if let Some(row) = store.get_iter(&start) {
                        let row_name_v = store.get_value(&row, 0);
                        if let Ok(Some(row_name)) = row_name_v.get::<&str>() {
                            for col in 0..ncols {
                                maybe_subscribe_col(&row, row_name, (col + 1) as u32);
                            }
                        }
                    }
                    start.next();
                }
                // subscribe to all rows in the sort column
                if let Some(id) = sort_column {
                    if let Some(row) = store.get_iter_first() {
                        loop {
                            let row_name_v = store.get_value(&row, 0);
                            if let Ok(Some(row_name)) = row_name_v.get::<&str>() {
                                maybe_subscribe_col(&row, row_name, id);
                            }
                            if !store.iter_next(&row) {
                                break;
                            }
                        }
                    }
                }
                for (row, id, val) in setval {
                    store.set_value(&row, id, &val.to_value());
                }
                cursor_changed(&view);
            }
        });
        view.append_column(&{
            let column = TreeViewColumn::new();
            let cell = CellRendererText::new();
            column.pack_start(&cell, true);
            column.set_title("name");
            column.add_attribute(&cell, "text", 0);
            column.set_sort_column_id(0);
            column.set_sizing(TreeViewColumnSizing::Fixed);
            column
        });
        for col in 0..(if vector_mode { 1 } else { descriptor.cols.len() }) {
            let id = (col + 1) as i32;
            let column = TreeViewColumn::new();
            let cell = CellRendererText::new();
            column.pack_start(&cell, true);
            TreeViewColumnExt::set_cell_data_func(
                &column,
                &cell,
                Some(Box::new({
                    let focus_column = Rc::clone(&focus_column);
                    let focus_row = Rc::clone(&focus_row);
                    let style = style.clone();
                    move |c: &TreeViewColumn,
                          cr: &CellRenderer,
                          s: &TreeModel,
                          i: &TreeIter| {
                        let cr = cr.clone().downcast::<CellRendererText>().unwrap();
                        let rn_v = s.get_value(i, 0);
                        let rn = rn_v.get::<&str>();
                        if let Ok(Some(v)) = s.get_value(i, id).get::<&str>() {
                            cr.set_property_text(Some(v));
                            match (&*focus_column.borrow(), &*focus_row.borrow(), rn) {
                                (Some(fc), Some(fr), Ok(Some(rn)))
                                    if fc == c && fr.as_str() == rn =>
                                {
                                    let fg = style.get_color(StateFlags::SELECTED);
                                    let bg =
                                        style.get_background_color(StateFlags::SELECTED);
                                    cr.set_property_cell_background_rgba(Some(&bg));
                                    cr.set_property_foreground_rgba(Some(&fg));
                                }
                                _ => {
                                    cr.set_property_cell_background(None);
                                    cr.set_property_foreground(None);
                                }
                            }
                        }
                    }
                })),
            );
            column.set_title(if vector_mode {
                "value"
            } else {
                descriptor.cols[col].0.as_ref()
            });
            column.set_sort_column_id(id);
            column.set_sizing(TreeViewColumnSizing::Fixed);
            view.append_column(&column);
        }
        view.set_fixed_height_mode(true);
        view.set_model(Some(&store));
        store.connect_sort_column_changed({
            let update_subscriptions = Rc::clone(&update_subscriptions);
            move |_| update_subscriptions()
        });
        view.connect_row_activated(clone!(
            @weak store, @strong base_path, @strong from_gui => move |_view, path, _col| {
                if let Some(row) = store.get_iter(&path) {
                    let row_name = store.get_value(&row, 0);
                    if let Ok(Some(row_name)) = row_name.get::<&str>() {
                        let path = base_path.append(row_name);
                        let _ = from_gui.unbounded_send(FromGui::Navigate(path));
                    }
                }
        }));
        view.connect_key_press_event(clone!(
            @strong base_path, @strong from_gui, @weak view, @weak focus_column,
            @weak selected_path =>
            @default-return Inhibit(false), move |_, key| {
                if key.get_keyval() == keys::constants::BackSpace {
                    let path = Path::dirname(&base_path).unwrap_or("/");
                    let m = FromGui::Navigate(Path::from(String::from(path)));
                    let _ = from_gui.unbounded_send(m);
                }
                if key.get_keyval() == keys::constants::Escape {
                    // unset the focus
                    view.set_cursor::<TreeViewColumn>(&TreePath::new(), None, false);
                    *focus_column.borrow_mut() = None;
                    *focus_row.borrow_mut() = None;
                    selected_path.set_label("");
                }
                Inhibit(false)
        }));
        view.connect_cursor_changed({
            let cursor_changed = Rc::clone(&cursor_changed);
            move |v| cursor_changed(v)
        });
        tablewin.get_vadjustment().map(|va| {
            let f = Rc::clone(&update_subscriptions);
            va.connect_value_changed(move |_| f());
        });
        NetidxTable { root, view, store, by_id, update_subscriptions }
    }
}

async fn netidx_main(
    cfg: Config,
    auth: Auth,
    mut updates: mpsc::Receiver<Batch>,
    mut to_gui: mpsc::Sender<ToGui>,
    mut from_gui: mpsc::UnboundedReceiver<FromGui>,
) {
    let subscriber = Subscriber::new(cfg, auth).expect("failed to create subscriber");
    let resolver = subscriber.resolver();
    let mut refresh = time::interval(Duration::from_secs(1)).fuse();
    loop {
        select_biased! {
            _ = refresh.next() => match to_gui.send(ToGui::Refresh).await {
                Ok(()) => (),
                Err(e) => break
            },
            b = updates.next() => if let Some(batch) = b {
                match to_gui.send(ToGui::Batch(batch)).await {
                    Ok(()) => (),
                    Err(e) => break
                }
            },
            m = from_gui.next() => match m {
                None => break,
                Some(FromGui::Navigate(path)) => {
                    let table = match resolver.table(path.clone()).await {
                        Ok(table) => table,
                        Err(e) => {
                            error!("can't load path {}", e);
                            continue
                        }
                    };
                    let m = ToGui::Table(subscriber.clone(), path, table);
                    match to_gui.send(m).await {
                        Err(_) => break,
                        Ok(()) => ()
                    }
                }
            }
        }
    }
}

fn run_netidx(
    cfg: Config,
    auth: Auth,
    updates: mpsc::Receiver<Batch>,
    to_gui: mpsc::Sender<ToGui>,
    from_gui: mpsc::UnboundedReceiver<FromGui>,
) {
    thread::spawn(move || {
        let mut rt = Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(netidx_main(cfg, auth, updates, to_gui, from_gui));
    });
}

fn run_gui(
    app: &Application,
    updates: mpsc::Sender<Batch>,
    mut to_gui: mpsc::Receiver<ToGui>,
    from_gui: mpsc::UnboundedSender<FromGui>,
) {
    let main_context = glib::MainContext::default();
    let app = app.clone();
    let window = ApplicationWindow::new(&app);
    window.set_title("Netidx browser");
    window.set_default_size(800, 600);
    window.show_all();
    main_context.spawn_local(async move {
        let mut current: Option<NetidxTable> = None;
        let mut changed: HashMap<SubId, (TreeIter, u32, Value)> = HashMap::new();
        while let Some(m) = to_gui.next().await {
            match m {
                ToGui::Refresh => {
                    if let Some(t) = &mut current {
                        for (id, (row, col, v)) in changed.drain() {
                            t.store.set_value(&row, col, &format!("{}", v).to_value());
                        }
                        t.view.columns_autosize();
                        (t.update_subscriptions)();
                    }
                }
                ToGui::Batch(mut b) => {
                    if let Some(t) = &mut current {
                        let subs = t.by_id.borrow();
                        for (id, v) in b.drain(..) {
                            if let Some(sub) = subs.get(&id) {
                                changed.insert(id, (sub.row.clone(), sub.col, v));
                            }
                        }
                    }
                }
                ToGui::Table(subscriber, path, table) => {
                    if let Some(cur) = current.take() {
                        window.remove(&cur.root);
                        cur.view.set_model(None::<&ListStore>);
                    }
                    changed.clear();
                    window.set_title(&format!("Netidx Browser {}", path));
                    let cur = NetidxTable::new(
                        subscriber,
                        path,
                        table,
                        updates.clone(),
                        from_gui.clone(),
                    );
                    window.add(&cur.root);
                    window.show_all();
                    current = Some(cur);
                }
            }
        }
    })
}

pub(crate) fn run(cfg: Config, auth: Auth, path: Path) {
    let application = Application::new(Some("org.netidx.browser"), Default::default())
        .expect("failed to initialize GTK application");
    application.connect_activate(move |app| {
        let (tx_updates, rx_updates) = mpsc::channel(2);
        let (tx_to_gui, rx_to_gui) = mpsc::channel(2);
        let (tx_from_gui, rx_from_gui) = mpsc::unbounded();
        // navigate to the initial location
        tx_from_gui.unbounded_send(FromGui::Navigate(path.clone())).unwrap();
        run_netidx(cfg.clone(), auth.clone(), rx_updates, tx_to_gui, rx_from_gui);
        run_gui(app, tx_updates, rx_to_gui, tx_from_gui)
    });
    application.run(&[]);
}
