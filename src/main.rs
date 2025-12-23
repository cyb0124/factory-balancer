#![feature(try_blocks)]

mod format;

use crate::format::format_float;
use eframe::egui::{Align, CentralPanel, Color32, Context, Frame, Key, Modal, Pos2, Ui, Vec2, vec2};
use eframe::egui::{KeyboardShortcut, Layout, Modifiers, TextEdit, ThemePreference, TopBottomPanel};
use eframe::{CreationContext, NativeOptions, run_native};
use egui_file_dialog::{DialogState, FileDialog};
use egui_snarl::ui::{PinInfo, PinPlacement, SnarlPin, SnarlStyle, SnarlViewer};
use egui_snarl::{InPin, InPinId, NodeId, OutPin, OutPinId, Snarl};
use meval::eval_str;
use serde::{Deserialize, Serialize};
use std::fs::{read_to_string, write};
use std::{cell::LazyCell, collections::HashMap};

const THRESHOLD: f64 = 1E-9;

#[derive(Serialize, Deserialize)]
enum NodeMeta {
    Resource(/** label */ String),
    Process(ProcessMeta),
}

#[derive(Serialize, Deserialize)]
struct ProcessMeta {
    label: String,
    activity: String,
    capacity: String,
    speed: String,
    consumes: Vec<String>,
    produces: Vec<String>,
}

struct ChartStats {
    nodes: HashMap<NodeId, NodeStats>,
}

enum NodeStats {
    Resource(/** rate */ f64),
    Process(/** valid */ bool),
}

impl ProcessMeta {
    fn common_rate(&self) -> Option<f64> {
        let mut activity = eval_str(&self.activity).ok()?;
        let speed = eval_str(&self.speed).ok()?;
        if !self.capacity.is_empty() {
            let capacity = eval_str(&self.capacity).ok()?;
            activity = activity.min(capacity);
        }
        Some(speed * activity.max(0.))
    }
}

impl ChartStats {
    fn compute(chart: &Snarl<NodeMeta>) -> Self {
        let mut this = Self { nodes: HashMap::new() };
        for (node, meta) in chart.node_ids() {
            let NodeMeta::Process(meta) = &meta else { continue };
            let mut adjs = Vec::with_capacity(meta.consumes.len() + meta.produces.len());
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
                    adjs.push((adj.node, -rate * qty));
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
                    adjs.push((adj.node, rate * qty));
                }
            }
            this.nodes.insert(node, NodeStats::Process(valid));
            for (node, rate) in adjs {
                let NodeStats::Resource(total) = this.nodes.entry(node).or_insert_with(|| NodeStats::Resource(0.)) else { unreachable!() };
                *total += rate;
            }
        }
        this
    }

    fn resource_rate(&self, node: NodeId) -> f64 { if let Some(NodeStats::Resource(rate)) = self.nodes.get(&node) { *rate } else { 0. } }
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

enum DeferredAction {
    None,
    AddConsume(NodeId),
    AddProduce(NodeId),
    RemoveConsume(InPinId),
    RemoveProduce(OutPinId),
    FitActivityToInput(InPinId),
    FitActivityToOutput(OutPinId),
}

struct ChartViewer {
    action: DeferredAction,
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
            NodeStats::Resource(rate) => {
                if *rate < -THRESHOLD {
                    frame.fill = Color32::from_rgb(160, 80, 0);
                } else if *rate > THRESHOLD {
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
                let rate = match self.stats.resource_rate(node) {
                    0. => "0".to_owned(),
                    x if x.abs() < THRESHOLD => "≈0".to_owned(),
                    x => format_float(x),
                };
                ui.vertical_centered(|ui| ui.label(rate));
            }
            NodeMeta::Process(meta) => {
                ui.set_width(100.);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label("Act");
                        TextEdit::singleline(&mut meta.activity).desired_width(f32::INFINITY).show(ui);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Cap");
                        TextEdit::singleline(&mut meta.capacity).desired_width(f32::INFINITY).show(ui);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Spd");
                        TextEdit::singleline(&mut meta.speed).desired_width(f32::INFINITY).show(ui);
                    });
                    ui.horizontal(|ui| {
                        prepare_small_button(ui);
                        ui.small_button("➕").clicked().then(|| self.action = DeferredAction::AddConsume(node));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.small_button("➕").clicked().then(|| self.action = DeferredAction::AddProduce(node));
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
                    ui.small_button("✖").clicked().then(|| self.action = DeferredAction::RemoveConsume(pin.id));
                    ui.small_button("➡").clicked().then(|| self.action = DeferredAction::FitActivityToInput(pin.id));
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
                    ui.small_button("⬅").clicked().then(|| self.action = DeferredAction::FitActivityToOutput(pin.id));
                    ui.small_button("✖").clicked().then(|| self.action = DeferredAction::RemoveProduce(pin.id));
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
                activity: "1".to_owned(),
                capacity: String::new(),
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
    path: String,
}

impl App {
    fn alert(&mut self, msg: String) {
        self.modal = Some(Box::new(move |_, ctx| {
            let resp = Modal::new("alert".into()).show(ctx, |ui| _ = ui.label(&msg));
            !resp.should_close()
        }));
    }

    fn do_load(&mut self) {
        match try {
            let data = read_to_string(&self.path).map_err(|e| e.to_string())?;
            ron::from_str(&data).map_err(|e| e.to_string())?
        } {
            Ok(chart) => self.chart = chart,
            Err(e) => self.alert(e),
        }
    }

    fn do_save(&mut self) {
        if let Err(e) = try {
            let data = ron::to_string(&self.chart).map_err(|e| e.to_string())?;
            write(&self.path, data).map_err(|e| e.to_string())?
        } {
            self.alert(e);
        }
    }

    fn pick_file(&mut self, save: bool, then: impl Fn(&mut App) + 'static) {
        let (name, ext) = ("Plain Text", "txt");
        let mut dialog = FileDialog::new().add_file_filter_extensions(name, vec![ext]);
        if save {
            dialog = dialog.add_save_extension(name, ext).default_save_extension(name);
            dialog.save_file();
        } else {
            dialog.pick_file();
        }
        self.modal = Some(Box::new(move |app, ctx| {
            dialog.update(ctx);
            if let Some(picked) = dialog.take_picked() {
                app.path = picked.to_string_lossy().into_owned();
                then(app);
            }
            matches!(dialog.state(), DialogState::Open)
        }));
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _: &mut eframe::Frame) {
        TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.text_edit_singleline(&mut self.path);
                (ui.button("Load").clicked() || ui.input_mut(|x| x.consume_shortcut(&KeyboardShortcut::new(Modifiers::CTRL, Key::R)))).then(|| {
                    if self.path.is_empty() {
                        self.pick_file(false, |app| app.do_load());
                    } else {
                        self.do_load();
                    }
                });
                (ui.button("Save").clicked() || ui.input_mut(|x| x.consume_shortcut(&KeyboardShortcut::new(Modifiers::CTRL, Key::S)))).then(|| {
                    if self.path.is_empty() {
                        self.pick_file(true, |app| app.do_save());
                    } else {
                        self.do_save();
                    }
                });
            });
        });
        CentralPanel::default().show(ctx, |ui| {
            let stats = ChartStats::compute(&self.chart);
            let mut viewer = ChartViewer { action: DeferredAction::None, stats };
            self.chart.show(&mut viewer, &self.style, (), ui);
            match viewer.action {
                DeferredAction::None => (),
                DeferredAction::AddConsume(node) => {
                    let NodeMeta::Process(meta) = &mut self.chart[node] else { unreachable!() };
                    meta.consumes.push("1".to_owned());
                }
                DeferredAction::AddProduce(node) => {
                    let NodeMeta::Process(meta) = &mut self.chart[node] else { unreachable!() };
                    meta.produces.push("1".to_owned());
                }
                DeferredAction::RemoveConsume(pin) => {
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
                DeferredAction::RemoveProduce(pin) => {
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
                DeferredAction::FitActivityToInput(pin) => {
                    if let Some(activity) = fit_activity_to_input(&self.chart, pin) {
                        let NodeMeta::Process(meta) = &mut self.chart[pin.node] else { unreachable!() };
                        meta.activity = activity.to_string();
                    } else {
                        self.alert("Failed to compute".to_owned());
                    }
                }
                DeferredAction::FitActivityToOutput(pin) => {
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
    App { style, chart: Snarl::new(), modal: None, path: String::new() }
}

fn main() {
    let mut opts = NativeOptions::default();
    opts.viewport.icon = Some(<_>::default());
    run_native("factor", opts, Box::new(|cc| Ok(Box::new(make_app(cc))))).unwrap();
}
