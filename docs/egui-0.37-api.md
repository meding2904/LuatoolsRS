# egui / eframe 0.37 API cheat-sheet (verified against the crate source)

This version differs substantially from 0.2x/0.3x. Verify anything not listed here against
`~/.cargo/registry/src/index.crates.io-*/egui-0.37.0/` and `eframe-0.37.0/`.

## App trait

```rust
impl eframe::App for MyApp {
    // required; root Ui, no margin/background
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) { /* ... */ }

    // optional; context-only pass, no painting
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {}
}
```

`update(ctx, frame)` no longer exists.

## Startup

```rust
let options = eframe::NativeOptions {
    viewport: egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([960.0, 640.0])
        .with_app_id("luatools"),
    ..Default::default()
};
eframe::run_native(
    "LuaTools",
    options,
    Box::new(|cc| {
        // cc.egui_ctx: egui::Context
        Ok(Box::new(MyApp::new()))
    }),
)?;
// eframe::Result = Result<(), eframe::Error>
```

## Panels (0.37: `Panel`, not `SidePanel`/`TopBottomPanel`)

```rust
egui::Panel::top("titlebar").show(ui, |ui| {});
egui::Panel::left("nav")
    .exact_size(208.0)       // or .default_size / .min_size / .max_size / .size_range
    .resizable(false)
    .frame(egui::Frame::new().fill(PANEL))
    .show(ui, |ui| {});
egui::CentralPanel::default()        // or ::no_frame(), ::default_margins()
    .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::same(24)))
    .show(ui, |ui| {});
```

Panels take `&mut Ui`, not `&Context`. Add them outermost first; `CentralPanel` last.

## Frame / Margin / CornerRadius

```rust
egui::Frame::new()                 // Frame::NONE also exists
    .fill(Color32::from_rgb(20, 23, 29))
    .stroke(egui::Stroke::new(1.0, STROKE))
    .corner_radius(egui::CornerRadius::same(12))   // u8
    .inner_margin(egui::Margin::same(16))          // i8, also ::symmetric(x, y)
    .outer_margin(egui::Margin::same(0))
    .show(ui, |ui| {});
```

`Rounding` is renamed `CornerRadius`; `rounding()` is `corner_radius()`.

## Styling

```rust
let mut visuals = egui::Visuals::dark();
visuals.panel_fill = BG;
visuals.window_fill = PANEL;
visuals.override_text_color = Some(TEXT);
ctx.set_visuals(visuals);                       // or set_visuals_of(Theme::Dark, v)
ctx.set_theme(egui::ThemePreference::Dark);

let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
style.spacing.item_spacing = egui::vec2(8.0, 8.0);
style.spacing.button_padding = egui::vec2(12.0, 6.0);
ctx.set_style_of(egui::Theme::Dark, style);
```

`ctx.style()` / `ctx.set_style()` were removed in favour of `*_of(theme, ...)`.

## Widgets

```rust
ui.label(impl Into<WidgetText>);
ui.heading(impl Into<RichText>);
ui.button("text");                       // takes impl IntoAtoms (0.37 "Atoms" API)
ui.checkbox(&mut bool, "text");
ui.selectable_value(&mut value, candidate, "text");
ui.selectable_label(selected, "text");
ui.add(egui::Button::new(egui::RichText::new("Download").color(BG))
    .fill(ACCENT)
    .corner_radius(egui::CornerRadius::same(8))
    .stroke(egui::Stroke::NONE));
ui.add(egui::TextEdit::singleline(&mut self.query).hint_text("Search").desired_width(280.0));
ui.add(egui::ProgressBar::new(0.4).desired_height(6.0));
ui.add(egui::Slider::new(&mut value, 0.0..=100.0).text("label"));
ui.add(egui::DragValue::new(&mut value));
ui.spinner();
ui.separator();
```

## Layout

```rust
ui.horizontal(|ui| {});
ui.vertical(|ui| {});
ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {});
ui.add_sized([ui.available_width(), 34.0], widget);
ui.allocate_ui_with_layout(size, layout, |ui| {});
ui.columns(3, |cols| { cols[0].label("a"); });
ui.scope(|ui| {});
ui.add_space(8.0);
ui.available_width();
```

## Containers

```rust
egui::ScrollArea::vertical().show(ui, |ui| {});
egui::Modal::new(egui::Id::unique()).show(ui.ctx(), |ui| {});   // Id::new is deprecated
egui::Window::new("title").show(ui.ctx(), |ui| {});
egui::Popup::from_response(&response).show(|ui| {});
egui::Popup::menu(&button_response).show(|ui| {});
egui::ComboBox::from_label("x").show_ui(ui, |ui| {});
```

`Modal::show` takes `&Context`.

## Text / images / misc

```rust
egui::RichText::new("x").size(13.5).color(MUTED).strong().monospace();
egui::FontId::proportional(16.0);
egui::Image::new(egui::include_image!("../assets/logo.png"));
ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
ui.ctx().open_url(egui::OpenUrl::new_tab("https://lua.tools"));
ui.ctx().copy_text("clipboard".to_owned());
```

## Gotchas

- `Margin` and `CornerRadius` integer constructors take `i8` / `u8`.
- `ui.label` takes `impl Into<WidgetText>`; `ui.button` takes `impl IntoAtoms` — a `&str`, `String` or
  `RichText` all work.
- `ctx.input(|i| ...)` still exists; `ctx.request_repaint()` still exists.
- `Id::new` is deprecated: use `Id::unique()` or `IdSalt::new`.
