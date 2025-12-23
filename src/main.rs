mod format;

use eframe::egui::{CentralPanel, Context, Key, Modal, Pos2, ThemePreference, Ui};
use eframe::{CreationContext, Frame, NativeOptions, run_native};
use egui_snarl::ui::{PinInfo, SnarlPin, SnarlStyle, SnarlViewer};
use egui_snarl::{InPin, NodeId, OutPin, Snarl};
use std::{collections::HashMap, mem::take};

use crate::format::format_float;

enum NodeMeta {
    Resource(/** label */ String),
    Process(ProcessMeta),
}

struct ProcessMeta {
    label: String,
    speed: String,
    count: String,
    consumes: Vec<String>,
    produces: Vec<String>,
}

struct ChartStats {
    nodes: HashMap<NodeId, NodeStats>,
}

enum NodeStats {
    Resource(/** rate */ f64),
}

impl ChartStats {
    fn compute(chart: &Snarl<NodeMeta>) -> Self {
        let mut this = Self { nodes: HashMap::new() };
        for (node, meta) in chart.node_ids() {
            let NodeMeta::Process(meta) = &meta else { continue };
            // TODO:
        }
        this
    }
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
    fn title(&mut self, meta: &NodeMeta) -> String {
        match meta {
            NodeMeta::Resource(label) => label.clone(),
            NodeMeta::Process(x) => x.label.clone(),
        }
    }

    fn has_body(&mut self, _: &NodeMeta) -> bool { true }
    fn show_body(&mut self, node: NodeId, inputs: &[InPin], outputs: &[OutPin], ui: &mut Ui, chart: &mut Snarl<NodeMeta>) {
        match &chart[node] {
            NodeMeta::Resource(_) => {
                let rate = if let Some(NodeStats::Resource(rate)) = self.stats.nodes.get(&node) { *rate } else { 0. };
                ui.label(format_float(rate));
            }
            NodeMeta::Process(meta) => {}
        }
    }

    fn inputs(&mut self, meta: &NodeMeta) -> usize {
        match meta {
            NodeMeta::Resource(_) => 1,
            NodeMeta::Process(meta) => meta.consumes.len(),
        }
    }

    fn outputs(&mut self, meta: &NodeMeta) -> usize {
        match meta {
            NodeMeta::Resource(_) => 1,
            NodeMeta::Process(meta) => meta.produces.len(),
        }
    }

    fn show_input(&mut self, pin: &InPin, ui: &mut Ui, chart: &mut Snarl<NodeMeta>) -> impl SnarlPin + 'static { PinInfo::square() }
    fn show_output(&mut self, pin: &OutPin, ui: &mut Ui, chart: &mut Snarl<NodeMeta>) -> impl SnarlPin + 'static { PinInfo::square() }

    fn has_graph_menu(&mut self, _: Pos2, _: &mut Snarl<NodeMeta>) -> bool { true }
    fn show_graph_menu(&mut self, pos: Pos2, ui: &mut Ui, _: &mut Snarl<NodeMeta>) {
        ui.button("New Resource").clicked().then(|| {
            *self.modal = Some(make_input_text_modal("Label: ", move |app, label| _ = app.chart.insert_node(pos, NodeMeta::Resource(label))))
        });
        ui.button("New Process").clicked().then(|| {
            *self.modal = Some(make_input_text_modal("Label: ", move |app, label| {
                let meta = ProcessMeta {
                    label,
                    speed: "1".to_owned(),
                    count: "1".to_owned(),
                    consumes: vec!["1".to_owned()],
                    produces: vec!["1".to_owned()],
                };
                app.chart.insert_node(pos, NodeMeta::Process(meta));
            }))
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
