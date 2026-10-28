// Card-style main menu: a centred window with a page rail and cards.
// The classic side panel (Gui::run_menu) stays available through `menu_classic`.
// Nothing here locks Gui: windows and toasts go through MenuOut.
use super::*;

#[derive(Default)]
pub(super) struct State {
    page: Page,
    search: String,
    gen: u32,
    was_open: bool,
    editor: Option<ConfigEditor>, // inline settings draft, created on first visit
    resize_from: Option<egui::Vec2>, // size when the pointer went down; only a user drag is saved
}

#[derive(Default, Copy, Clone, PartialEq, Eq, Hash)]
enum Page { #[default] Home, General, Graphics, Gameplay, Translation, Interface, Plugins, System }

impl Page {
    const ALL: [Page; 8] = [
        Page::Home, Page::General, Page::Graphics, Page::Gameplay,
        Page::Translation, Page::Interface, Page::Plugins, Page::System
    ];

    // FontAwesome 4.7 glyphs, already bundled
    fn icon(self) -> &'static str {
        match self {
            Page::Home => "\u{f015}",
            Page::General => "\u{f013}",
            Page::Graphics => "\u{f108}",
            Page::Gameplay => "\u{f11b}",
            Page::Translation => "\u{f1ab}",
            Page::Interface => "\u{f1fc}",
            Page::Plugins => "\u{f12e}",
            Page::System => "\u{f085}",
        }
    }

    fn title(self) -> Cow<'static, str> {
        match self {
            Page::Home => t!("card_menu.page_home"),
            Page::General => t!("config_editor.general_tab"),
            Page::Graphics => t!("config_editor.graphics_tab"),
            Page::Gameplay => t!("config_editor.gameplay_tab"),
            Page::Translation => t!("menu.translation_heading"),
            Page::Interface => t!("card_menu.page_interface"),
            Page::Plugins => t!("card_menu.page_plugins"),
            Page::System => t!("card_menu.page_system"),
        }
    }

    fn tab(self) -> Option<ConfigEditorTab> {
        match self {
            Page::General => Some(ConfigEditorTab::General),
            Page::Graphics => Some(ConfigEditorTab::Graphics),
            Page::Gameplay => Some(ConfigEditorTab::Gameplay),
            _ => None
        }
    }
}

fn font(family: &str, size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name(family.into()))
}

pub(super) fn run(gui: &mut Gui) {
    let ctx = gui.context.clone();
    let scale = get_scale(&ctx);
    let mut out = MenuOut::default();

    let st = &mut gui.card_menu;
    let opening = gui.show_menu && !st.was_open; // was_open is reset by hidden() once the menu is gone
    st.was_open = gui.show_menu;

    let req_w = REQUESTED_WIDTH.lock().ok().and_then(|mut l| l.take());
    // set_menu_width(w) means classic's panel width (plugin column = w - 16): add the rail, page and card margins, card stroke
    let chrome = 40.0 * scale + 32.0 * scale + 2.0;
    let screen = ctx.viewport_rect();
    // gen only changes on "Reset menu size": a new id drops egui's remembered size.
    // Each orientation gets its own id, so a rotation doesn't carry the other one's size over.
    let id = egui::Id::new("card_menu").with(gui.finalized_scale.to_bits()).with(st.gen).with(screen.width() > screen.height());
    let cfg = Hachimi::instance().config.load_full();

    match cfg.menu_layout {
        hachimi::MenuLayout::Centered => {
            let auto = egui::vec2(
                (screen.width() * 0.70).clamp(360.0 * scale, 560.0 * scale),
                (screen.height() * 0.86).clamp(240.0 * scale, 440.0 * scale)
            );
            let max = screen.size() - egui::Vec2::splat(16.0 * scale); // leaves room for the window frame
            let size = cfg.menu_size.map_or(auto, |[w, h]| egui::vec2(w, h) * scale).min(max);

            let mut window = egui::Window::new("")
                .id(id)
                .title_bar(false)
                .resizable(true)
                .constrain(true)
                .pivot(egui::Align2::CENTER_CENTER)
                .default_pos(screen.center())
                .default_size(size)
                .max_size(max)
                .min_width(req_w.map_or(200.0 * scale, |w| w - 16.0 + chrome));
            if opening {
                window = window.current_pos(screen.center()); // re-centre on every open
            }
            let mut open = gui.show_menu; // false: egui fades the window out
            let shown = window
                .open(&mut open)
                .show(&ctx, |ui| {
                    let content = ui.max_rect().size();
                    body(gui, ui, &mut out, id, scale);
                    content
                });

            // remember a user resize, in points at gui_scale 1
            if let Some(content) = shown.and_then(|r| r.inner) {
                if drag_ended(&mut gui.card_menu, ctx.is_using_pointer(), content) {
                    save_silent(&mut gui.card_menu, |c| c.menu_size = Some([content.x / scale, content.y / scale]));
                }
            }
        }
        layout => {
            let width = cfg.menu_dock_width.unwrap_or(260.0) * scale;
            let panel = if layout == hachimi::MenuLayout::DockLeft { egui::SidePanel::left(id) } else { egui::SidePanel::right(id) };
            let panel = match req_w {
                Some(w) => panel.exact_width(w + chrome), // plugin asked for a page width
                None => panel.default_width(width).width_range(200.0 * scale..=360.0 * scale)
            };
            // None while sliding, so the width is only read from the real panel
            let shown = panel.show_animated(&ctx, gui.show_menu, |ui| body(gui, ui, &mut out, id, scale));
            if let Some(r) = shown {
                let new_width = r.response.rect.width();
                if drag_ended(&mut gui.card_menu, ctx.is_using_pointer(), egui::vec2(new_width, 0.0)) {
                    save_silent(&mut gui.card_menu, |c| c.menu_dock_width = Some(new_width / scale));
                }
            }
        }
    }

    gui.menu_close_contract();
    out.apply(gui);
}

// Called instead of run() while the card menu isn't drawn (fully closed, input released, or classic chosen):
// like closing the settings window, drops the draft and undoes a live language preview.
pub(super) fn hidden(st: &mut State) {
    if let Some(editor) = st.editor.take() {
        editor.on_close();
    }
    st.was_open = false;
    st.resize_from = None;
}

// True once when the pointer is released after the size changed while it was held, i.e. a user drag.
// Growth from content, plugin width requests or screen capping happens without a drag and is never saved.
fn drag_ended(st: &mut State, using_pointer: bool, size: egui::Vec2) -> bool {
    if using_pointer {
        st.resize_from.get_or_insert(size);
        false
    } else {
        st.resize_from.take().is_some_and(|from| (size - from).length() > 1.0)
    }
}

fn body(gui: &mut Gui, ui: &mut egui::Ui, out: &mut MenuOut, id: egui::Id, scale: f32) {
    // menu-local fonts; the rest of the GUI keeps the global Proportional styles
    for (style, font_id) in ui.style_mut().text_styles.iter_mut() {
        match style {
            egui::TextStyle::Heading => font_id.family = egui::FontFamily::Name("display".into()),
            egui::TextStyle::Body | egui::TextStyle::Button | egui::TextStyle::Small =>
                font_id.family = egui::FontFamily::Name("strong".into()),
            _ => {}
        }
    }

    let rail_right = Hachimi::instance().config.load().menu_rail_right;
    let has_plugins = !get_plugin_menu_items().is_empty() || !get_plugin_menu_sections().is_empty();
    if gui.card_menu.page == Page::Plugins && !has_plugins {
        gui.card_menu.page = Page::Home;
    }
    // a search always lands on the settings grid, which then searches every tab
    let page = if gui.card_menu.search.is_empty() { gui.card_menu.page } else { Page::General };

    let rail = if rail_right { egui::SidePanel::right(id.with("rail")) } else { egui::SidePanel::left(id.with("rail")) };
    rail.exact_width(40.0 * scale)
        .resizable(false)
        .frame(egui::Frame::NONE.fill(ui.visuals().panel_fill))
        .show_inside(ui, |ui| rail_ui(ui, &mut gui.card_menu, has_plugins, rail_right, scale));

    egui::TopBottomPanel::top(id.with("header"))
        .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric((8.0 * scale) as i8, (4.0 * scale) as i8)))
        .show_inside(ui, |ui| header(gui, ui, out, scale));

    if let Some(tab) = page.tab() {
        sync_editor(&mut gui.card_menu);
        egui::TopBottomPanel::bottom(id.with("footer"))
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric((8.0 * scale) as i8, (4.0 * scale) as i8)))
            .show_inside(ui, |ui| footer(&mut gui.card_menu, ui, out, tab));
    }

    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.inner_margin(egui::Margin::same((8.0 * scale) as i8)))
        .show_inside(ui, |ui| {
            if let Ok(mut w) = PREV_MENU_WIDTH.lock() {
                // classic meaning: plugin column + 16; the column here is the card's inner width
                *w = ui.available_width() - 16.0 * scale - 2.0 + 16.0;
            }
            egui::ScrollArea::vertical()
                .id_salt(page)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    page_ui(gui, ui, out, page, scale);

                    #[cfg(target_os = "android")]
                    {
                        let padding = ime_scroll_padding(ui.ctx());
                        if padding > 0.0 && page.tab().is_none() { // options_page adds its own padding
                            ui.add_space(padding);
                        }
                    }
                });
        });
}

fn rail_ui(ui: &mut egui::Ui, st: &mut State, has_plugins: bool, right: bool, scale: f32) {
    let mut selected_y = None;
    // scrolls only when the window is shorter than the rail (large gui_scale)
    egui::ScrollArea::vertical()
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| ui.vertical_centered(|ui| {
            ui.add_space(4.0 * scale);
            ui.spacing_mut().item_spacing.y = 4.0 * scale;
            for p in Page::ALL {
                if p == Page::Plugins && !has_plugins { continue; }
                let res = ui.add_sized(
                    [30.0 * scale, 30.0 * scale],
                    egui::Button::selectable(st.page == p, egui::RichText::new(p.icon()).size(16.0 * scale))
                ).on_hover_text(p.title());
                if res.clicked() {
                    st.page = p;
                    st.search.clear();
                }
                if st.page == p {
                    selected_y = Some(res.rect.center().y);
                }
            }
        }));

    // accent bar on the inner edge, sliding between entries
    if let Some(y) = selected_y {
        let y = ui.ctx().animate_value_with_time(ui.id().with("rail_bar"), y, 0.12);
        let r = ui.max_rect();
        let x = if right { r.left() } else { r.right() - 3.0 * scale };
        ui.painter().rect_filled(
            egui::Rect::from_min_size(egui::pos2(x, y - 10.0 * scale), egui::vec2(3.0 * scale, 20.0 * scale)),
            1.5 * scale,
            ui.visuals().widgets.active.bg_fill
        );
    }
}

// Two rows, and the title and version truncate: a row wider than the header would grow the docked panel or the window.
fn header(gui: &mut Gui, ui: &mut egui::Ui, out: &mut MenuOut, scale: f32) {
    ui.horizontal(|ui| {
        ui.add(Gui::icon(ui.ctx()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("\u{f00d}").on_hover_text(t!("menu.close_menu")).clicked() {
                gui.show_menu = false;
                gui.menu_anim_time = None;
            }
            if ui.button("\u{f29c}").on_hover_text(t!("about.title")).clicked() {
                out.window = Some(Box::new(AboutWindow::new()));
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(egui::RichText::new(t!("hachimi")).font(font("display", 15.0 * scale))).truncate());
                ui.add(egui::Label::new(egui::RichText::new(env!("HACHIMI_DISPLAY_VERSION")).weak().small()).truncate());
            });
        });
    });
    ui.horizontal(|ui| {
        // run_chip hard-codes the Proportional font, so the FPS chip is a plain frame
        egui::Frame::new()
            .fill(ui.visuals().extreme_bg_color)
            .corner_radius(3.0 * scale)
            .inner_margin(egui::Margin::symmetric((5.0 * scale) as i8, (2.0 * scale) as i8))
            .show(ui, |ui| ui.label(egui::RichText::new(&gui.fps_text).font(font("numeric", 10.0 * scale))));
        let _search_res = ui.add(
            egui::TextEdit::singleline(&mut gui.card_menu.search)
                .hint_text(t!("search_filter"))
                .desired_width(ui.available_width())
        );
        #[cfg(target_os = "android")]
        handle_android_keyboard(&_search_res, &mut gui.card_menu.search);
    });
}

fn page_ui(gui: &mut Gui, ui: &mut egui::Ui, out: &mut MenuOut, page: Page, scale: f32) {
    if let Some(tab) = page.tab() {
        ui.label(egui::RichText::new(t!("card_menu.settings_desc")).weak().small());
        if let Some(editor) = gui.card_menu.editor.as_mut() {
            let mut config = std::mem::take(&mut editor.config);
            editor.options_page(ui, &mut config, tab, scale, 16.0 * scale);
            editor.config = config;
        }
        return;
    }

    match page {
        Page::General | Page::Graphics | Page::Gameplay => {} // handled above
        Page::Home => {
            card(ui, &t!("card_menu.status_title"), &t!("card_menu.status_desc"), |ui| {
                Gui::menu_stats(ui, &gui.fps_text);
                ui.label(egui::RichText::new(t!("about.fork_notice")).weak().small());
            });
            card(ui, &t!("card_menu.quick_title"), &t!("card_menu.quick_desc"), |ui| {
                #[cfg(target_os = "windows")]
                Gui::menu_quick_graphics(ui, &mut gui.menu_fps_value, &mut gui.menu_vsync_value);
                #[cfg(not(target_os = "windows"))]
                Gui::menu_quick_graphics(ui, &mut gui.menu_fps_value);
            });
            card(ui, &t!("card_menu.presets_title"), &t!("card_menu.presets_desc"), |ui| presets_card(&mut gui.card_menu, ui, out));
            card(ui, &t!("card_menu.auto_skip_title"), &t!("card_menu.auto_skip_desc"), |ui| auto_skip_card(&mut gui.card_menu, ui));
            card(ui, &t!("menu.config_heading"), &t!("card_menu.config_desc"), |ui| Gui::menu_config_buttons(ui, out));
        }
        Page::Translation => {
            card(ui, &t!("menu.translation_heading"), &t!("card_menu.translation_desc"), |ui| Gui::menu_translation_buttons(ui, out));
        }
        Page::Interface => {
            card(ui, &t!("card_menu.layout_title"), &t!("card_menu.layout_desc"), |ui| layout_card(&mut gui.card_menu, ui));
            card(ui, &t!("theme_editor.title"), &t!("card_menu.theme_desc"), |ui| {
                if ui.button(t!("open")).clicked() {
                    out.window = Some(Box::new(ThemeEditorWindow::new()));
                }
            });
            card(ui, &t!("menu.shortcuts_heading"), &t!("card_menu.shortcuts_desc"), |ui| Gui::menu_shortcut_buttons(ui, out));
        }
        Page::Plugins => {
            card(ui, &t!("card_menu.page_plugins"), &t!("card_menu.plugins_desc"), |ui| Gui::menu_plugin_entries(ui, scale, None));
        }
        Page::System => {
            card(ui, &t!("menu.danger_zone_heading"), &t!("menu.danger_zone_warning"), |ui| Gui::menu_danger_buttons(ui, out));
            card(ui, &t!("about.title"), &t!("card_menu.about_desc"), |ui| {
                ui.label(t!("about.fork_notice"));
                ui.horizontal_wrapped(|ui| {
                    if ui.button(t!("about.title")).clicked() {
                        out.window = Some(Box::new(AboutWindow::new()));
                    }
                    // same gate as the classic button
                    if crate::core::updater::self_update_enabled() && ui.button(t!("menu.check_for_updates")).clicked() {
                        Hachimi::instance().updater.clone().check_for_updates(|_| {});
                    }
                    if ui.button(t!("about.view_license")).clicked() {
                        out.window = Some(Box::new(LicenseWindow::new()));
                    }
                });
            });
        }
    }
}

// Applied and saved at once.
fn layout_card(st: &mut State, ui: &mut egui::Ui) {
    let cfg = Hachimi::instance().config.load_full();

    ui.horizontal(|ui| {
        ui.label(t!("card_menu.menu_position"));
        let mut layout = cfg.menu_layout;
        if Gui::run_combo(ui, "menu_layout", &mut layout, &[
            (hachimi::MenuLayout::Centered, &t!("card_menu.layout_centered")),
            (hachimi::MenuLayout::DockLeft, &t!("card_menu.layout_dock_left")),
            (hachimi::MenuLayout::DockRight, &t!("card_menu.layout_dock_right"))
        ]) {
            save_silent(st, |c| c.menu_layout = layout);
        }
    });

    let mut rail_right = cfg.menu_rail_right;
    if ui.checkbox(&mut rail_right, t!("card_menu.rail_right")).changed() {
        save_silent(st, |c| c.menu_rail_right = rail_right);
    }

    let mut classic = cfg.menu_classic;
    if ui.checkbox(&mut classic, t!("config_editor.menu_classic")).changed() {
        save_silent(st, |c| c.menu_classic = classic);
    }

    if ui.button(t!("card_menu.reset_size")).clicked() {
        save_silent(st, |c| {
            c.menu_size = None;
            c.menu_dock_width = None;
        });
        st.gen = st.gen.wrapping_add(1);
    }
}

// Saved at once. The hooks read these keys when the game next sets things up, so nothing is pushed live.
// ponytail: overwrites the user's own values with no undo; the settings pages or Restore defaults bring them back.
fn presets_card(st: &mut State, ui: &mut egui::Ui, out: &mut MenuOut) {
    ui.horizontal_wrapped(|ui| {
        if ui.button(t!("card_menu.preset_low")).on_hover_text(t!("card_menu.preset_low_desc")).clicked() {
            // lowest game quality; every mod-side extra back to the game's own value
            save_silent(st, |c| {
                c.graphics_quality = GraphicsQuality::Toon1280;
                c.msaa = MsaaQuality::Disabled;
                c.aniso_level = AnisoLevel::Default;
                c.shadow_resolution = ShadowResolution::_512;
                c.shadow_distance = 0.0;
                c.soft_shadows = false;
                c.force_chara_shadows = false;
                c.render_scale = 1.0; // ponytail: below 1.0 would also blur the UI (set_ResolutionScale2D reads it), so only supersampling is dropped
                c.virtual_res_mult = 1.0;
            });
            out.notification = Some(t!("notification.config_saved"));
        }
        if ui.button(t!("card_menu.preset_cloth")).on_hover_text(t!("card_menu.preset_cloth_desc")).clicked() {
            save_silent(st, |c| {
                c.physics_update_mode = Some(SpringUpdateMode::Mode60FPS); // the uncap flag only applies in this mode (CySpringNative.rs)
                c.cyspring_mono_uncap_frame_scale = true;
            });
            out.notification = Some(t!("notification.config_saved"));
        }
    });
}

// Applied and saved at once; the risky ones carry their warning inline.
fn auto_skip_card(st: &mut State, ui: &mut egui::Ui) {
    let cfg = Hachimi::instance().config.load_full();
    for (label, desc, risky, get, set) in AUTO_SKIP_ROWS {
        let mut value = get(&cfg);
        let checkbox = egui::Checkbox::new(&mut value, t!(label));
        if ui.add_enabled(auto_skip_row_enabled(label, &cfg), checkbox).on_hover_text(t!(desc)).changed() {
            save_silent(st, |c| set(c, value));
        }
        if risky {
            ui.label(egui::RichText::new(t!(desc)).small().color(ui.visuals().warn_fg_color));
        }
    }

    // not a skip, but the same "career goes faster" switch; the speed slider stays in Graphics
    let mut flash = cfg.ui_animation_scale_flash;
    if ui.checkbox(&mut flash, t!("config_editor.ui_animation_scale_flash")).on_hover_text(t!("config_editor.ui_animation_scale_flash_desc")).changed() {
        save_silent(st, |c| c.ui_animation_scale_flash = flash);
    }
    ui.label(egui::RichText::new(t!("config_editor.ui_animation_scale_flash_desc")).small().color(ui.visuals().warn_fg_color));
}

// Lazily creates the settings draft and re-syncs it when the global config changes, as ConfigEditor::run does.
fn sync_editor(st: &mut State) {
    let editor = st.editor.get_or_insert_with(ConfigEditor::new);
    let global = Hachimi::instance().config.load();
    let ptr = Arc::as_ptr(&global) as usize;
    if ptr != editor.last_ptr_config {
        editor.config = (**global).clone();
        editor.last_ptr_config = ptr;
    }
    editor.poll_hachifonts();
    editor.search_term.clone_from(&st.search);
}

fn footer(st: &mut State, ui: &mut egui::Ui, out: &mut MenuOut, tab: ConfigEditorTab) {
    let Some(editor) = st.editor.as_mut() else { return };
    ui.horizontal_wrapped(|ui| {
        if ui.button(t!("save")).clicked() {
            editor.save();
        }
        if ui.button(t!("card_menu.revert")).clicked() {
            editor.config = (**Hachimi::instance().config.load()).clone();
            editor.on_close(); // undoes a live language preview
        }
        if ui.button(t!("config_editor.restore_defaults")).clicked() {
            editor.restore_defaults();
        }
        if ui.button(t!("card_menu.open_in_window")).clicked() {
            let mut window = ConfigEditor::new();
            window.current_tab = tab;
            out.window = Some(Box::new(window));
        }
    });
}

fn card(ui: &mut egui::Ui, title: &str, desc: &str, add: impl FnOnce(&mut egui::Ui)) {
    let scale = get_scale(ui.ctx());
    let rounding = Hachimi::instance().config.load().ui_window_rounding;
    egui::Frame::new()
        .fill(egui::Color32::from_white_alpha(6))
        .stroke(egui::Stroke::new(1.0_f32, ui.visuals().widgets.active.bg_fill.gamma_multiply(0.3))) // accent, via apply_theme
        .corner_radius(0.6 * rounding * scale)
        .inner_margin(egui::Margin::symmetric((8.0 * scale) as i8, (7.0 * scale) as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(title).font(font("strong", 11.5 * scale)));
            ui.label(egui::RichText::new(desc).weak().small());
            ui.add_space(4.0 * scale);
            add(ui);
        });
    ui.add_space(6.0 * scale);
}

// Core save path (hachimi.rs save_config): no toast, no translation-updater restart.
fn save_silent(st: &mut State, f: impl Fn(&mut hachimi::Config)) {
    let hachimi = Hachimi::instance();
    let mut config = (**hachimi.config.load()).clone();
    f(&mut config);
    if let Err(e) = hachimi.save_config(&config) {
        error!("{}", e);
    }
    hachimi.config.store(Arc::new(config));

    // apply the same change to an open settings draft and mark it in sync, so its unsaved edits survive
    if let Some(editor) = st.editor.as_mut() {
        f(&mut editor.config);
        editor.last_ptr_config = Arc::as_ptr(&hachimi.config.load()) as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Loads the real font definitions through epaint itself (FontsImpl::new parses every font with
    // ab_glyph::FontRef::try_from_slice_and_index and panics on a bad file), then checks that each embedded
    // variable font maps its own glyphs and that its family still falls back to the icon and CJK fonts.
    #[test]
    fn only_user_drags_are_saved() {
        let mut st = State::default();
        let (a, b) = (egui::vec2(400.0, 300.0), egui::vec2(500.0, 300.0));
        assert!(!drag_ended(&mut st, false, b), "growth without a drag");
        assert!(!drag_ended(&mut st, true, a));
        assert!(!drag_ended(&mut st, true, b), "still dragging");
        assert!(drag_ended(&mut st, false, b), "drag released");
        assert!(!drag_ended(&mut st, false, b), "saved once");
        assert!(!drag_ended(&mut st, true, b));
        assert!(!drag_ended(&mut st, false, b), "click without a size change");
    }

    #[test]
    fn card_menu_fonts_load() {
        const FAMILIES: [&str; 3] = ["display", "strong", "numeric"];
        let id = |name: &str| egui::FontId::new(14.0, egui::FontFamily::Name(name.into()));

        let mut fonts = Gui::get_font_definitions();
        for family in FAMILIES {
            let own = fonts.families[&egui::FontFamily::Name(family.into())][0].clone();
            fonts.families.insert(egui::FontFamily::Name(format!("{family}_only").into()), vec![own]);
        }
        let ctx = egui::Context::default();
        ctx.set_fonts(fonts);
        let _ = ctx.run(Default::default(), |ctx| ctx.fonts_mut(|f| {
            for family in FAMILIES {
                assert!(f.has_glyphs(&id(&format!("{family}_only")), "Hachimi Edge 0123456789"), "{family}: own glyphs");
                assert!(f.has_glyphs(&id(family), "\u{f015}\u{3042}"), "{family}: icon/CJK fallback");
            }
        }));
    }
}
