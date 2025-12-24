mod format;

use crate::format::format_float;
use anyhow::{Context as _, Result, anyhow, ensure};
use eframe::egui::{Align, CentralPanel, Color32, Context, Frame, Key, Modal, Pos2, Ui, Vec2, vec2};
use eframe::egui::{KeyboardShortcut, Layout, Modifiers, TextEdit, ThemePreference, TopBottomPanel};
use eframe::{CreationContext, WebRunner};
use egui_snarl::ui::{PinInfo, PinPlacement, SnarlPin, SnarlStyle, SnarlViewer};
use egui_snarl::{InPin, InPinId, NodeId, OutPin, OutPinId, Snarl};
use meval::eval_str;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, LazyCell};
use std::{collections::HashMap, rc::Rc};
use wasm_bindgen::prelude::JsCast;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{Storage, window};

const THRESHOLD: f64 = 1E-9;
const MODAL_WIDTH: f32 = 800.;
const STORAGE_PREFIX: &str = "factory-balancer/";

#[derive(Serialize, Deserialize)]
enum NodeMeta {
    Resource(/** label */ String),
    Process(ProcessMeta),
}

#[derive(Serialize, Deserialize)]
struct ProcessMeta {
    label: String,
    capacity: String,
    activity: String,
    speed: String,
    consumes: Vec<String>,
    produces: Vec<String>,
}

struct ChartStats {
    nodes: HashMap<NodeId, NodeStats>,
}

enum NodeStats {
    Resource(ResourceStats),
    Process(/** valid */ bool),
}

#[derive(Default, Clone, Copy)]
struct ResourceStats {
    inc: f64,
    dec: f64,
    net: f64,
}

impl ProcessMeta {
    fn common_rate(&self) -> Option<f64> {
        let mut rate = eval_str(&self.capacity).ok()?;
        if !self.activity.is_empty() {
            rate = rate.min(eval_str(&self.activity).ok()?);
        }
        Some(rate * eval_str(&self.speed).ok()?)
    }
}

impl ChartStats {
    fn compute(chart: &Snarl<NodeMeta>) -> Self {
        let mut this = Self { nodes: HashMap::new() };
        for (node, meta) in chart.node_ids() {
            let NodeMeta::Process(meta) = &meta else { continue };
            let mut valid = false;
            if let Some(rate) = meta.common_rate() {
                valid = true;
                for (input, qty) in meta.consumes.iter().enumerate() {
                    let Ok([adj]) = <[OutPinId; 1]>::try_from(chart.in_pin(InPinId { node, input }).remotes) else {
                        valid = false;
                        continue;
                    };
                    let Ok(qty) = eval_str(qty) else {
                        valid = false;
                        continue;
                    };
                    let rate = rate * qty;
                    let stats = this.resource_mut(adj.node);
                    stats.dec += rate;
                    stats.net -= rate;
                }
                for (output, qty) in meta.produces.iter().enumerate() {
                    let Ok([adj]) = <[InPinId; 1]>::try_from(chart.out_pin(OutPinId { node, output }).remotes) else {
                        valid = false;
                        continue;
                    };
                    let Ok(qty) = eval_str(qty) else {
                        valid = false;
                        continue;
                    };
                    let rate = rate * qty;
                    let stats = this.resource_mut(adj.node);
                    stats.inc += rate;
                    stats.net += rate;
                }
            }
            this.nodes.insert(node, NodeStats::Process(valid));
        }
        this
    }

    fn resource_mut(&mut self, node: NodeId) -> &mut ResourceStats {
        let stats = self.nodes.entry(node).or_insert_with(|| NodeStats::Resource(<_>::default()));
        let NodeStats::Resource(stats) = stats else { unreachable!() };
        stats
    }

    fn resource(&self, node: NodeId) -> ResourceStats {
        if let Some(NodeStats::Resource(stats)) = self.nodes.get(&node) { *stats } else { <_>::default() }
    }
}

fn resource_rate_excl_process(chart: &Snarl<NodeMeta>, r: NodeId, p: NodeId) -> f64 {
    let mut result = 0.;
    'outer: for (node, meta) in chart.node_ids() {
        let false = node == p else { continue };
        let NodeMeta::Process(meta) = &meta else { continue };
        let rate = LazyCell::new(|| meta.common_rate());
        for (input, qty) in meta.consumes.iter().enumerate() {
            let Ok([adj]) = <[OutPinId; 1]>::try_from(chart.in_pin(InPinId { node, input }).remotes) else { continue };
            let true = adj.node == r else { continue };
            let Ok(qty) = eval_str(qty) else { continue };
            let Some(rate) = *rate else { continue 'outer };
            result -= rate * qty;
        }
        for (output, qty) in meta.produces.iter().enumerate() {
            let Ok([adj]) = <[InPinId; 1]>::try_from(chart.out_pin(OutPinId { node, output }).remotes) else { continue };
            let true = adj.node == r else { continue };
            let Ok(qty) = eval_str(qty) else { continue };
            let Some(rate) = *rate else { continue 'outer };
            result += rate * qty;
        }
    }
    result
}

fn fit_activity_to_input(chart: &Snarl<NodeMeta>, pin: InPinId) -> Option<f64> {
    let NodeMeta::Process(meta) = &chart[pin.node] else { unreachable!() };
    let speed = eval_str(&meta.speed).ok()?;
    let qty = eval_str(&meta.consumes[pin.input]).ok()?;
    let [r] = <[OutPinId; 1]>::try_from(chart.in_pin(pin).remotes).ok()?;
    let resource_rate = resource_rate_excl_process(chart, r.node, pin.node);
    Some(resource_rate / (speed * qty))
}

fn fit_activity_to_output(chart: &Snarl<NodeMeta>, pin: OutPinId) -> Option<f64> {
    let NodeMeta::Process(meta) = &chart[pin.node] else { unreachable!() };
    let speed = eval_str(&meta.speed).ok()?;
    let qty = eval_str(&meta.produces[pin.output]).ok()?;
    let [r] = <[InPinId; 1]>::try_from(chart.out_pin(pin).remotes).ok()?;
    let resource_rate = resource_rate_excl_process(chart, r.node, pin.node);
    Some(-resource_rate / (speed * qty))
}

/// Return whether to retain.
type ModalBox = Box<dyn FnMut(&mut App, &Context) -> bool>;

enum Action {
    None,
    AddConsume(NodeId),
    AddProduce(NodeId),
    RemoveConsume(InPinId),
    RemoveProduce(OutPinId),
    FitActivityToInput(InPinId),
    FitActivityToOutput(OutPinId),
}

struct ChartViewer {
    action: Action,
    stats: ChartStats,
}

fn prepare_small_button(ui: &mut Ui) {
    let spacing = &mut ui.style_mut().spacing;
    spacing.button_padding = Vec2::ZERO;
    spacing.item_spacing = vec2(1., 0.);
}

impl SnarlViewer<NodeMeta> for ChartViewer {
    fn connect(&mut self, from: &OutPin, to: &InPin, chart: &mut Snarl<NodeMeta>) {
        match (&chart[from.id.node], &chart[to.id.node]) {
            (NodeMeta::Resource(_), NodeMeta::Resource(_)) => return,
            (NodeMeta::Process(_), NodeMeta::Process(_)) => return,
            (NodeMeta::Resource(_), NodeMeta::Process(_)) => {
                let true = to.remotes.is_empty() else { return };
            }
            (NodeMeta::Process(_), NodeMeta::Resource(_)) => {
                let true = from.remotes.is_empty() else { return };
            }
        }
        chart.connect(from.id, to.id);
    }

    fn title(&mut self, meta: &NodeMeta) -> String {
        match meta {
            NodeMeta::Resource(label) => label.clone(),
            NodeMeta::Process(meta) => meta.label.clone(),
        }
    }

    fn show_header(&mut self, node: NodeId, _: &[InPin], _: &[OutPin], ui: &mut Ui, chart: &mut Snarl<NodeMeta>) {
        let (width, label) = match &mut chart[node] {
            NodeMeta::Resource(label) => (80., label),
            NodeMeta::Process(meta) => {
                let mut width = 108.;
                (!meta.consumes.is_empty()).then(|| width += 36.);
                (!meta.produces.is_empty()).then(|| width += 36.);
                (width, &mut meta.label)
            }
        };
        ui.set_width(width);
        TextEdit::singleline(label).desired_width(f32::INFINITY).show(ui);
    }

    fn node_frame(&mut self, mut frame: Frame, node: NodeId, _: &[InPin], _: &[OutPin], _: &Snarl<NodeMeta>) -> Frame {
        let Some(stats) = self.stats.nodes.get(&node) else { return frame };
        match stats {
            NodeStats::Process(valid) => _ = (!valid).then(|| frame.fill = Color32::DARK_RED),
            NodeStats::Resource(stats) => {
                if stats.net < -THRESHOLD {
                    frame.fill = Color32::from_rgb(160, 80, 0);
                } else if stats.net > THRESHOLD {
                    frame.fill = Color32::DARK_GREEN;
                }
            }
        }
        frame
    }

    fn has_body(&mut self, _: &NodeMeta) -> bool { true }
    fn show_body(&mut self, node: NodeId, _: &[InPin], _: &[OutPin], ui: &mut Ui, chart: &mut Snarl<NodeMeta>) {
        match &mut chart[node] {
            NodeMeta::Resource(_) => {
                ui.set_width(72.);
                let stats = self.stats.resource(node);
                let inc = format_float(stats.inc, THRESHOLD);
                let dec = format_float(stats.dec, THRESHOLD);
                let net = format_float(stats.net, THRESHOLD);
                ui.vertical_centered(|ui| ui.label(format!("➕ {inc}\n➖ {dec}\nNet {net}")));
            }
            NodeMeta::Process(meta) => {
                ui.set_width(100.);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label("Cap");
                        TextEdit::singleline(&mut meta.capacity).desired_width(f32::INFINITY).show(ui);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Act");
                        TextEdit::singleline(&mut meta.activity).desired_width(f32::INFINITY).show(ui);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Spd");
                        TextEdit::singleline(&mut meta.speed).desired_width(f32::INFINITY).show(ui);
                    });
                    ui.horizontal(|ui| {
                        prepare_small_button(ui);
                        ui.small_button("➕").clicked().then(|| self.action = Action::AddConsume(node));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.small_button("➕").clicked().then(|| self.action = Action::AddProduce(node));
                        });
                    });
                });
            }
        }
    }

    fn inputs(&mut self, meta: &NodeMeta) -> usize {
        match meta {
            NodeMeta::Resource(_) => 1,
            NodeMeta::Process(meta) => meta.consumes.len(),
        }
    }

    fn show_input(&mut self, pin: &InPin, ui: &mut Ui, chart: &mut Snarl<NodeMeta>) -> impl SnarlPin + 'static {
        if let NodeMeta::Process(meta) = &mut chart[pin.id.node] {
            ui.vertical(|ui| {
                TextEdit::singleline(&mut meta.consumes[pin.id.input]).desired_width(20.).show(ui);
                ui.horizontal(|ui| {
                    prepare_small_button(ui);
                    ui.small_button("✖").clicked().then(|| self.action = Action::RemoveConsume(pin.id));
                    ui.small_button("➡").clicked().then(|| self.action = Action::FitActivityToInput(pin.id));
                });
            });
        }
        PinInfo::square()
    }

    fn outputs(&mut self, meta: &NodeMeta) -> usize {
        match meta {
            NodeMeta::Resource(_) => 1,
            NodeMeta::Process(meta) => meta.produces.len(),
        }
    }

    fn show_output(&mut self, pin: &OutPin, ui: &mut Ui, chart: &mut Snarl<NodeMeta>) -> impl SnarlPin + 'static {
        if let NodeMeta::Process(meta) = &mut chart[pin.id.node] {
            ui.set_width(30.);
            ui.vertical(|ui| {
                TextEdit::singleline(&mut meta.produces[pin.id.output]).desired_width(20.).show(ui);
                ui.horizontal(|ui| {
                    prepare_small_button(ui);
                    ui.small_button("⬅").clicked().then(|| self.action = Action::FitActivityToOutput(pin.id));
                    ui.small_button("✖").clicked().then(|| self.action = Action::RemoveProduce(pin.id));
                });
            });
        }
        PinInfo::square()
    }

    fn has_graph_menu(&mut self, _: Pos2, _: &mut Snarl<NodeMeta>) -> bool { true }
    fn show_graph_menu(&mut self, pos: Pos2, ui: &mut Ui, chart: &mut Snarl<NodeMeta>) {
        ui.button("New Resource").clicked().then(|| _ = chart.insert_node(pos, NodeMeta::Resource(String::new())));
        ui.button("New Process").clicked().then(|| {
            let meta = ProcessMeta {
                label: String::new(),
                capacity: "1".to_owned(),
                activity: String::new(),
                speed: "1".to_owned(),
                consumes: vec!["1".to_owned()],
                produces: vec!["1".to_owned()],
            };
            chart.insert_node(pos, NodeMeta::Process(meta));
        });
    }

    fn has_node_menu(&mut self, _: &NodeMeta) -> bool { true }
    fn show_node_menu(&mut self, node: NodeId, _: &[InPin], _: &[OutPin], ui: &mut Ui, chart: &mut Snarl<NodeMeta>) {
        ui.button("Delete").clicked().then(|| chart.remove_node(node));
    }
}

struct App {
    style: SnarlStyle,
    chart: Snarl<NodeMeta>,
    modal: Option<ModalBox>,
    storage: Option<Storage>,
    storage_key: String,
}

impl App {
    fn alert(&mut self, msg: String) {
        self.modal = Some(Box::new(move |_, ctx| {
            let resp = Modal::new("alert".into()).show(ctx, |ui| {
                ui.set_max_width(MODAL_WIDTH);
                ui.label(&msg);
            });
            !resp.should_close()
        }));
    }

    fn show_storage_key_list(&mut self, mut keys: Vec<String>) {
        self.modal = Some(Box::new(move |app, ctx| {
            enum Action<'a> {
                None,
                Load(&'a String),
                Delete(usize),
            }
            let mut action = Action::None;
            let resp = Modal::new("storage_key_list".into()).show(ctx, |ui| {
                ui.set_max_width(MODAL_WIDTH);
                let false = keys.is_empty() else { return drop(ui.label("(Empty)")) };
                for (i, key) in keys.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.button("✖").clicked().then(|| action = Action::Delete(i));
                        ui.button(key).clicked().then(|| action = Action::Load(key));
                    });
                }
            });
            match action {
                Action::None => (),
                Action::Load(key) => {
                    app.storage_key.clone_from(key);
                    app.load_from_storage();
                    return false;
                }
                Action::Delete(i) => {
                    let key = format!("{STORAGE_PREFIX}{}", keys.remove(i));
                    if let Err(e) = app.storage.as_ref().unwrap().remove_item(&key) {
                        app.alert(format!("{e:?}"));
                    }
                }
            }
            !resp.should_close()
        }));
    }

    fn load_from_storage(&mut self) {
        if let Err(e) = (|| -> Result<()> {
            let storage = self.storage.as_ref().unwrap();
            if self.storage_key.is_empty() {
                let len = storage.length().map_err(|e| anyhow!("{e:?}"))?;
                let mut keys = Vec::new();
                for i in 0..len {
                    let key = storage.key(i).ok().flatten().context("Failed to list storage keys")?;
                    let Some(key) = key.strip_prefix(STORAGE_PREFIX) else { continue };
                    let false = key.is_empty() else { continue };
                    keys.push(key.to_owned());
                }
                return Ok(self.show_storage_key_list(keys));
            }
            let key = format!("{STORAGE_PREFIX}{}", self.storage_key);
            let data = storage.get_item(&key).ok().flatten().context("Item not found")?;
            Ok(self.chart = ron::from_str(&data)?)
        })() {
            self.alert(format!("{e:?}"));
        }
    }

    fn save_to_storage(&mut self) {
        if let Err(e) = (|| -> Result<()> {
            ensure!(!self.storage_key.is_empty(), "Storage key shouldn't be empty");
            let data = ron::to_string(&self.chart)?;
            let key = format!("{STORAGE_PREFIX}{}", self.storage_key);
            self.storage.as_ref().unwrap().set_item(&key, &data).map_err(|e| anyhow!("{e:?}"))
        })() {
            self.alert(format!("{e:?}"));
        }
    }

    fn load_from_clipboard(&mut self, ctx: Context) {
        let data = JsFuture::from(window().unwrap().navigator().clipboard().read_text());
        let slot = Rc::new(Cell::new(None::<Result<String>>));
        let weak = Rc::downgrade(&slot);
        self.modal = Some(Box::new(move |app, ctx| {
            let Some(data) = slot.take() else {
                Modal::new("wait_for_clipboard".into()).show(ctx, |ui| ui.label("Waiting for clipboard"));
                return true;
            };
            if let Err(e) = (|| -> Result<()> { Ok(app.chart = ron::from_str(&data?)?) })() {
                app.alert(format!("{e:?}"));
            }
            false
        }));
        spawn_local(async move {
            let data = data.await;
            let Some(slot) = weak.upgrade() else { return };
            slot.set(Some(data.map_err(|e| anyhow!("{e:?}")).map(|x| x.as_string().context("Not a string")).flatten()));
            ctx.request_repaint();
        });
    }

    fn save_to_clipboard(&mut self) {
        match ron::to_string(&self.chart) {
            Ok(data) => drop(window().unwrap().navigator().clipboard().write_text(&data)),
            Err(e) => self.alert(e.to_string()),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _: &mut eframe::Frame) {
        TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.button("Source").clicked().then(|| {
                    if let Err(e) = window().unwrap().open_with_url_and_target("https://github.com/cyb0124/factory-balancer/", "_blank") {
                        self.alert(format!("{e:?}"));
                    }
                });
                ui.separator();
                ui.label("Browser Storage:");
                if self.storage.is_some() {
                    TextEdit::singleline(&mut self.storage_key).desired_width(120.).show(ui);
                    ui.button("Load").clicked().then(|| self.load_from_storage());
                    (ui.button("Save").clicked() || ui.input_mut(|x| x.consume_shortcut(&KeyboardShortcut::new(Modifiers::CTRL, Key::S))))
                        .then(|| self.save_to_storage());
                } else {
                    ui.label("(not available)");
                }
                ui.separator();
                ui.label("Clipboard:");
                ui.button("Load").clicked().then(|| self.load_from_clipboard(ctx.clone()));
                ui.button("Save").clicked().then(|| self.save_to_clipboard());
            });
        });
        CentralPanel::default().show(ctx, |ui| {
            let stats = ChartStats::compute(&self.chart);
            let mut viewer = ChartViewer { action: Action::None, stats };
            self.chart.show(&mut viewer, &self.style, (), ui);
            match viewer.action {
                Action::None => (),
                Action::AddConsume(node) => {
                    let NodeMeta::Process(meta) = &mut self.chart[node] else { unreachable!() };
                    meta.consumes.push("1".to_owned());
                }
                Action::AddProduce(node) => {
                    let NodeMeta::Process(meta) = &mut self.chart[node] else { unreachable!() };
                    meta.produces.push("1".to_owned());
                }
                Action::RemoveConsume(pin) => {
                    let NodeMeta::Process(meta) = &mut self.chart[pin.node] else { unreachable!() };
                    let old_len = meta.consumes.len();
                    meta.consumes.remove(pin.input);
                    self.chart.drop_inputs(pin);
                    for i in pin.input + 1..old_len {
                        let old = InPinId { node: pin.node, input: i };
                        let new = InPinId { node: pin.node, input: i - 1 };
                        self.chart.in_pin(old).remotes.into_iter().for_each(|far| _ = self.chart.connect(far, new));
                    }
                }
                Action::RemoveProduce(pin) => {
                    let NodeMeta::Process(meta) = &mut self.chart[pin.node] else { unreachable!() };
                    let old_len = meta.produces.len();
                    meta.produces.remove(pin.output);
                    self.chart.drop_outputs(pin);
                    for i in pin.output + 1..old_len {
                        let old = OutPinId { node: pin.node, output: i };
                        let new = OutPinId { node: pin.node, output: i - 1 };
                        self.chart.out_pin(old).remotes.into_iter().for_each(|far| _ = self.chart.connect(new, far));
                    }
                }
                Action::FitActivityToInput(pin) => {
                    if let Some(activity) = fit_activity_to_input(&self.chart, pin) {
                        let NodeMeta::Process(meta) = &mut self.chart[pin.node] else { unreachable!() };
                        meta.activity = activity.to_string();
                    } else {
                        self.alert("Failed to compute".to_owned());
                    }
                }
                Action::FitActivityToOutput(pin) => {
                    if let Some(activity) = fit_activity_to_output(&self.chart, pin) {
                        let NodeMeta::Process(meta) = &mut self.chart[pin.node] else { unreachable!() };
                        meta.activity = activity.to_string();
                    } else {
                        self.alert("Failed to compute".to_owned());
                    }
                }
            }
        });
        if let Some(mut modal) = self.modal.take() {
            modal(self, ctx).then(|| self.modal = Some(modal));
        }
    }
}

fn make_app(cc: &CreationContext) -> App {
    cc.egui_ctx.set_theme(ThemePreference::Dark);
    let style = SnarlStyle {
        header_drag_space: Some(Vec2::ZERO),
        collapsible: Some(false),
        wire_width: Some(3.),
        pin_placement: Some(PinPlacement::Edge),
        ..<_>::default()
    };
    App { style, chart: Snarl::new(), modal: None, storage: window().unwrap().local_storage().ok().flatten(), storage_key: String::new() }
}

fn main() {
    let canvas = window().unwrap().document().unwrap().get_element_by_id("main").unwrap().unchecked_into();
    spawn_local(async { WebRunner::new().start(canvas, <_>::default(), Box::new(|cc| Ok(Box::new(make_app(cc))))).await.unwrap() });
}
