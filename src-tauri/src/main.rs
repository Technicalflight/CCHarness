// CCHarness — Tauri 2 desktop app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent_tools;
mod auxmemo;
mod bench;
mod chat;
mod commands;
mod confidence;
mod config;
mod divergence;
mod git_panel;
mod guard;
mod importer;
mod mcp;
mod memvector;
mod prefix;
mod privacy;
mod sessions;
mod skillhub;
mod skills;
mod spill;
mod sysprompt;
mod types_rs;
mod update;
mod urlguard;
mod worktree;

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager,
};

fn show_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        // close-button behavior comes from settings.close_action:
        //   "quit" → close normally; "tray" → hide, tray keeps us alive;
        //   "ask" (default) → intercept and let the frontend dialog decide.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if commands::is_force_quit() {
                    return; // user already confirmed a real exit
                }
                let app = window.app_handle();
                let action = config::load(&app.state::<commands::AppState>().data_dir)
                    .settings
                    .close_action;
                match action.as_str() {
                    "quit" => {} // fall through — close normally
                    "tray" => {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                    _ => {
                        api.prevent_close();
                        let _ = window.emit("close-ask", ());
                    }
                }
            }
        })
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .expect("无法确定应用数据目录");
            std::fs::create_dir_all(&data_dir).ok();
            app.manage(commands::AppState::new(data_dir));

            // heal a window that a previous session left partially off-screen
            // (e.g. a geometry restore landing on a maximized window): clamp
            // its position so at least ~120px of it stays on the monitor
            if let Some(win) = app.get_webview_window("main") {
                if let (Ok(pos), Ok(outer), Ok(Some(mon))) =
                    (win.outer_position(), win.outer_size(), win.current_monitor())
                {
                    let mp = mon.position();
                    let ms = mon.size();
                    let min_visible = 120;
                    let x = pos
                        .x
                        .clamp(mp.x - (outer.width as i32 - min_visible), mp.x + ms.width as i32 - min_visible);
                    let y = pos
                        .y
                        .clamp(mp.y - (outer.height as i32 - min_visible), mp.y + ms.height as i32 - min_visible);
                    if (x, y) != (pos.x, pos.y) {
                        let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
                    }
                }
            }

            // system tray: left click restores, right click shows the menu
            let show = MenuItem::with_id(app, "show", "显示主界面", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().expect("no default icon").clone())
                .tooltip("CCHarness")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, ev| match ev.id.as_ref() {
                    "show" => show_main(app),
                    "quit" => commands::request_quit(app),
                    _ => {}
                })
                .on_tray_icon_event(|tray, ev| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = ev
                    {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::save_config,
            update::check_update,
            commands::ccswitch_import,
            commands::hide_to_tray,
            commands::app_quit,
            commands::test_provider,
            commands::fetch_models,
            commands::list_sessions,
            commands::create_session,
            commands::delete_session,
            commands::rename_session,
            commands::update_bindings,
            commands::set_workspace,
            commands::set_permission_mode,
            commands::get_session_messages,
            commands::attachment_data,
            commands::get_telemetry,
            commands::get_global_stats,
            commands::export_session,
            commands::get_app_data_dir,
            commands::open_data_dir,
            commands::window_minimize,
            commands::window_toggle_maximize,
            commands::window_close,
            commands::send_message,
            commands::arena_send,
            commands::group_send,
            bench::bench_cases_default,
            bench::bench_history,
            bench::bench_run,
            commands::stop_generation,
            commands::resolve_approval,
            commands::rollback_session,
            commands::compact_session,
            commands::get_session_compaction,
            commands::compact_estimate,
            commands::get_todos,
            commands::mcp_status,
            commands::mcp_test,
            commands::get_skills,
            commands::delete_skill,
            commands::clear_session,
            commands::skillhub_list,
            commands::skillhub_install,
            commands::skillhub_plugins,
            commands::skillhub_plugin_install,
            commands::enhance_prompt,
            commands::get_aux_stats,
            commands::set_workflow_mode,
            commands::get_workflow_mode,
            commands::goal_set,
            commands::goal_get,
            commands::wiki_generate,
            commands::session_digest,
            commands::session_change_lines,
            commands::open_backup_dir,
            commands::privacy_log_tail,
            commands::privacy_log_clear,
            commands::goal_status,
            commands::goal_clear,
            commands::sm_get,
            commands::sm_set,
            commands::list_session_writes,
            commands::get_write_diff,
            commands::set_session_pinned,
            commands::set_session_archived,
            commands::branch_session,
            commands::search_workspace_files,
            commands::read_workspace_file,
            commands::list_workspace_dir,
            commands::open_external,
            commands::import_scan,
            commands::import_session,
            commands::wt_start,
            commands::wt_info,
            commands::wt_diff,
            commands::wt_merge,
            commands::wt_discard,
            git_panel::git_overview,
            git_panel::git_stage,
            git_panel::git_stage_all,
            git_panel::git_unstage,
            git_panel::git_discard,
            git_panel::git_commit,
            git_panel::git_file_diff,
            git_panel::git_branches,
            git_panel::git_switch,
            git_panel::git_remotes,
            git_panel::git_remote_add,
            git_panel::git_remote_remove,
            git_panel::git_push,
            git_panel::git_pull,
            git_panel::git_fetch,
        ])
        .run(tauri::generate_context!())
        .expect("CCHarness 启动失败");
}
