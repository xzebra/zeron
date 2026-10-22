//! Offscreen Edit Project dialog evidence: footer, palette, custom color and
//! the icon browser. `cargo run --locked -p zeron-ui --example
//! edit-project-fixture --features appshots-fixture -- <output dir>`
use gpui::{AppContext, AsyncApp, Bounds, WindowBounds, WindowOptions, px, size};
use std::{path::PathBuf, time::Duration};
use zeron_ui::*;

async fn pause(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(400))
        .await;
}

fn capture(
    window: gpui::AnyWindowHandle,
    cx: &mut AsyncApp,
    output: &std::path::Path,
    name: &str,
) -> anyhow::Result<()> {
    window.update(cx, |_, w, cx| {
        w.draw(cx).clear();
        w.render_to_image()?
            .save(output.join(format!("{name}.png")))?;
        Ok(())
    })?
}

const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect x="1" y="1" width="22" height="22" rx="6" fill="#668cf5"/><path d="M7 6h11v3h-8v3h6v3h-6v4H7Z" fill="white"/></svg>"##;

fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _guard = runtime.enter();
    let output = PathBuf::from(std::env::args().nth(1).expect("output directory"));
    std::fs::create_dir_all(&output)?;
    let temp = tempfile::tempdir()?;
    let data = temp.path().to_path_buf();
    let project = data.join("fieldnotes");
    let images = [
        "apps/web/public/favicon.svg",
        "apps/web/public/apple-touch-icon.png",
        "assets/brand/logo-wordmark.png",
        "assets/icons/close.svg",
        "docs/screenshots/onboarding-flow-step-two.png",
        "design/hero.jpg",
    ];
    for path in images {
        let path = project.join(path);
        std::fs::create_dir_all(path.parent().unwrap())?;
        if path.extension().is_some_and(|e| e == "svg") {
            std::fs::write(&path, SVG)?;
        } else {
            let mut image = image::RgbaImage::new(32, 32);
            for (x, y, pixel) in image.enumerate_pixels_mut() {
                *pixel = image::Rgba([(x * 8) as u8, 120, (y * 8) as u8, 255]);
            }
            image.save_with_format(&path, image::ImageFormat::Png)?;
        }
    }
    gpui_platform::application()
        .with_assets(icons::Assets)
        .run(move |cx| {
            gpui_tokio::init(cx);
            gpui_base::init(cx);
            let mut settings = settings::UiSettings::default();
            settings.surface = zeron_theme::SurfacePreference::Opaque;
            settings.save(&data).unwrap();
            settings::init(settings.clone(), data.clone(), cx);
            let fonts = typography::register_fonts(cx);
            typography::init(
                settings.ui_font_family.clone(),
                settings.ui_font_size,
                settings.terminal_font_family.clone(),
                settings.terminal_font_size,
                settings.code_font_family.clone(),
                settings.code_font_size,
                fonts,
                cx,
            );
            theme_library::init(data.clone(), cx);
            appearance::init(
                appearance::AppearanceMode::Dark,
                settings.theme_selection,
                settings.accent,
                settings.surface,
                cx,
            );
            history::init(
                settings.git_history_columns,
                settings.git_history_column_widths,
                settings.git_history_column_order,
                settings.git_history_author_display,
                cx,
            );
            composer::init(cx, settings.composer_send_behavior);
            terminal::panel::init(cx);
            app_menus::init(cx);
            let state = cx.new(|_| {
                let mut s = state::AppState::new();
                s.connection = zeron_proto::view::ConnectionStatus::Ready;
                s.workspace_scope = Some(zeron_proto::WorkspaceScope::Local);
                s.local_device_id = Some("local".into());
                s.devices = vec![serde_json::from_value(serde_json::json!({"id":"local","name":"This device","platform":std::env::consts::OS,"lastSeenAt":null})).unwrap()];
                s.chats_synced = true;
                s.spaces_synced = true;
                s.spaces = vec![serde_json::from_value(serde_json::json!({"id":"project","deviceId":"local","path":project,"name":"Fieldnotes","createdAt":"2026-09-08T00:00:00Z"})).unwrap()];
                s
            });
            let boot = EngineBootConfig {
                data_dir: data.clone(),
                ipc_port: 0,
                edge_url: String::new(),
                edge_token: None,
                org_id: None,
                workos_client_id: None,
                default_harness: HarnessId::ClaudeCode,
            };
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            gpui::point(px(12.), px(30.)),
                            size(px(760.), px(860.)),
                        ))),
                        ..Default::default()
                    },
                    |_, cx| cx.new(|cx| shell::Shell::new(state.clone(), boot, cx)),
                )
                .unwrap();
            cx.spawn(async move |cx| {
                let run = async {
                    pause(cx).await;
                    window.update(cx, |shell, _, cx| shell.fixture_edit_project("project", cx))?;
                    pause(cx).await;
                    capture(window.into(), cx, &output, "1-dialog")?;
                    window.update(cx, |shell, _, cx| {
                        shell.fixture_edit_project_hex("#ff5a1f", cx)
                    })?;
                    window.update(cx, |shell, _, cx| {
                        shell.fixture_edit_project_browse(
                            images.iter().map(|path| path.to_string()).collect(),
                            cx,
                        )
                    })?;
                    pause(cx).await;
                    pause(cx).await;
                    capture(window.into(), cx, &output, "2-browser-custom-color")?;
                    window.update(cx, |_, _, cx| {
                        appearance::set_mode(appearance::AppearanceMode::Light, cx)
                    })?;
                    pause(cx).await;
                    capture(window.into(), cx, &output, "3-browser-light")?;
                    window.update(cx, |shell, _, cx| shell.fixture_edit_project_pick(0, cx))?;
                    pause(cx).await;
                    pause(cx).await;
                    capture(window.into(), cx, &output, "4-picked-light")?;
                    anyhow::Ok(())
                };
                if let Err(error) = run.await {
                    eprintln!("edit project fixture failed: {error:?}");
                    std::process::exit(1);
                }
                cx.update(|cx| cx.quit());
            })
            .detach();
            cx.activate(true);
        });
    Ok(())
}
