mod format;

use eframe::egui::{CentralPanel, Context, Key, Modal, Pos2, TextEdit, ThemePreference, Ui};
use eframe::{CreationContext, Frame, NativeOptions, run_native};
use egui_snarl::ui::{PinInfo, SnarlPin, SnarlStyle, SnarlViewer};
use egui_snarl::{InPin, InPinId, NodeId, OutPin, OutPinId, Snarl};
use meval::eval_str;
use std::{collections::HashMap, mem::take};

use crate::format::format_float;

enum NodeMeta {
    Resource(/** label */ String),
    Process(ProcessMeta),
}

struct ProcessMeta {
    label: String,
    count: String,
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

impl ChartStats {
    fn compute(chart: &Snarl<NodeMeta>) -> Self {
        let mut this = Self { nodes: HashMap::new() };
        for (node, meta) in chart.node_ids() {
            let NodeMeta::Process(meta) = &meta else { continue };
            let mut adjs = Vec::with_capacity(meta.consumes.len() + meta.produces.len());
            let mut valid = false;
            'valid: {
                let Ok(count) = eval_str(&meta.count) else { break 'valid };
                let Ok(speed) = eval_str(&meta.speed) else { break 'valid };
                let mult = speed * count;
                for (input, qty) in meta.consumes.iter().enumerate() {
                    let Ok([adj]) = <[OutPinId; 1]>::try_from(chart.in_pin(InPinId { node, input }).remotes) else { break 'valid };
                    let Ok(qty) = eval_str(qty) else { break 'valid };
                    adjs.push((adj.node, -mult * qty));
                }
                for (output, qty) in meta.produces.iter().enumerate() {
                    let Ok([adj]) = <[InPinId; 1]>::try_from(chart.out_pin(OutPinId { node, output }).remotes) else { break 'valid };
                    let Ok(qty) = eval_str(qty) else { break 'valid };
                    adjs.push((adj.node, mult * qty));
                }
                valid = true;
            }
            this.nodes.insert(node, NodeStats::Process(valid));
            if valid {
                for (node, rate) in adjs {
                    let NodeStats::Resource(total) = this.nodes.entry(node).or_insert_with(|| NodeStats::Resource(0.)) else { unreachable!() };
                    *total += rate;
                }
            }
        }
        this
    }

    fn resource_rate(&self, node: NodeId) -> f64 { if let Some(NodeStats::Resource(rate)) = self.nodes.get(&node) { *rate } else { 0. } }
}

/// Return whether to retain.
type ModalBox = Box<dyn FnMut(&mut App, &mut Ui) -> bool>;

struct ChartViewer<'a> {
    modal: &'a mut Option<ModalBox>,
    stats: ChartStats,
}

fn make_input_text_modal(prompt: &'static str, submit: impl Fn(&mut App, String) + 'static) -> ModalBox {
    let mut text = String::new();
    Box::new(move |app, ui| {
        let resp = ui.horizontal(|ui| {
            ui.label(prompt);
            ui.text_edit_singleline(&mut text)
        });
        if resp.inner.lost_focus() && ui.input(|x| x.key_pressed(Key::Enter)) {
            submit(app, take(&mut text));
            return false;
        }
        !ui.input(|x| x.key_pressed(Key::Escape))
    })
}

impl SnarlViewer<NodeMeta> for ChartViewer<'_> {
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
        let label = match &mut chart[node] {
            NodeMeta::Resource(label) => label,
            NodeMeta::Process(meta) => &mut meta.label,
        };
        TextEdit::singleline(label).desired_width(80.).show(ui);
    }

    fn has_body(&mut self, _: &NodeMeta) -> bool { true }
    fn show_body(&mut self, node: NodeId, inputs: &[InPin], outputs: &[OutPin], ui: &mut Ui, chart: &mut Snarl<NodeMeta>) {
        match &mut chart[node] {
            NodeMeta::Resource(_) => {
                ui.label(format_float(self.stats.resource_rate(node)));
            }
            NodeMeta::Process(meta) => {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label("Count: ");
                        TextEdit::singleline(&mut meta.count).desired_width(50.).show(ui);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Speed: ");
                        TextEdit::singleline(&mut meta.speed).desired_width(50.).show(ui);
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
            TextEdit::singleline(&mut meta.consumes[pin.id.input]).desired_width(20.).show(ui);
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
            TextEdit::singleline(&mut meta.produces[pin.id.output]).desired_width(20.).show(ui);
        }
        PinInfo::square()
    }

    fn has_graph_menu(&mut self, _: Pos2, _: &mut Snarl<NodeMeta>) -> bool { true }
    fn show_graph_menu(&mut self, pos: Pos2, ui: &mut Ui, chart: &mut Snarl<NodeMeta>) {
        ui.button("New Resource").clicked().then(|| _ = chart.insert_node(pos, NodeMeta::Resource(String::new())));
        ui.button("New Process").clicked().then(|| {
            let meta = ProcessMeta {
                label: String::new(),
                count: "1".to_owned(),
                speed: "1".to_owned(),
                consumes: vec!["1".to_owned()],
                produces: vec!["1".to_owned()],
            };
            chart.insert_node(pos, NodeMeta::Process(meta));
        });
    }
}

struct App {
    style: SnarlStyle,
    chart: Snarl<NodeMeta>,
    modal: Option<ModalBox>,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _: &mut Frame) {
        CentralPanel::default().show(ctx, |ui| {
            let stats = ChartStats::compute(&self.chart);
            self.chart.show(&mut ChartViewer { modal: &mut self.modal, stats }, &self.style, (), ui);
        });
        if let Some(mut modal) = self.modal.take() {
            Modal::new("modal".into()).show(ctx, |ui| {
                modal(self, ui).then(|| self.modal = Some(modal));
            });
        }
    }
}

fn make_app(cc: &CreationContext) -> App {
    cc.egui_ctx.set_theme(ThemePreference::Dark);
    App { style: SnarlStyle::new(), chart: Snarl::new(), modal: None }
}

fn main() {
    let mut opts = NativeOptions::default();
    opts.viewport.icon = Some(<_>::default());
    run_native("factor", opts, Box::new(|cc| Ok(Box::new(make_app(cc))))).unwrap();
}
