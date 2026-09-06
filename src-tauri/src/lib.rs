pub mod commands;
pub mod config;
pub mod config_io;
pub mod crypto;
pub mod docker;
pub mod history;
pub mod manage;
pub mod manage_exec;
pub mod manage_stacks;
pub mod manage_stats;
pub mod notify;
pub mod ssh;
pub mod stack;
pub mod update;

use tauri_plugin_log::{Target, TargetKind};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    // 文件/目录选择对话框插件(前端 __TAURI__.dialog 经 dialog:default 权限调用)
    .plugin(tauri_plugin_dialog::init())
    // 通知中心:本地系统桌面通知(后端 Rust 侧经 NotificationExt 直接调用,
    // 不走 IPC;capabilities 的 notification:default 权限备前端通道使用)
    .plugin(tauri_plugin_notification::init())
    .manage(commands::DeployState::default())
    .manage(manage_stats::StatsState::default())
    .manage(manage_exec::ExecState::default())
    .invoke_handler(tauri::generate_handler![
      commands::get_config,
      commands::save_config_cmd,
      commands::retrust_host_key,
      config_io::config_export_file,
      config_io::config_import_file,
      config_io::config_wipe,
      notify::notify_get_config,
      notify::notify_save_config,
      notify::notify_test_desktop,
      notify::notify_test_email,
      commands::host_check,
      commands::start_docker,
      commands::list_images,
      commands::encrypt_password,
      commands::test_server,
      commands::server_env_check,
      commands::install_server_docker,
      commands::create_remote_dir,
      commands::prune_server,
      commands::deploy,
      commands::cancel_deploy,
      commands::deploy_stack,
      commands::deploy_batch,
      commands::cleanup_preview,
      commands::cleanup_execute,
      commands::deploy_resume_status,
      commands::deploy_resume_start,
      commands::deploy_resume_discard,
      commands::import_compose,
      commands::parse_compose,
      commands::preview_compose,
      commands::preview_stack_changes,
      commands::rollback_list_releases,
      commands::rollback_list_tags,
      commands::rollback_execute_stack,
      commands::rollback_execute_single,
      commands::get_history,
      manage::manage_list_servers,
      manage::manage_overview,
      manage::manage_list_containers,
      manage::manage_container_inspect,
      manage::manage_container_action,
      manage::manage_container_logs,
      manage::manage_list_images,
      manage::manage_image_pull,
      manage::manage_image_remove,
      manage::manage_image_tag,
      manage::manage_list_volumes,
      manage::manage_volume_inspect,
      manage::manage_volume_create,
      manage::manage_volume_remove,
      manage::manage_list_networks,
      manage::manage_network_inspect,
      manage::manage_network_create,
      manage::manage_network_remove,
      manage::manage_network_connect,
      manage::manage_network_disconnect,
      manage_stacks::manage_list_stacks,
      manage_stacks::manage_stack_action,
      manage_stacks::manage_stack_ps,
      manage_stacks::manage_stack_logs,
      manage_stacks::manage_stack_env_read,
      manage_stacks::manage_stack_env_save,
      manage_stats::manage_stats_start,
      manage_stats::manage_stats_stop,
      manage_exec::manage_exec_start,
      manage_exec::manage_exec_write,
      manage_exec::manage_exec_resize,
      manage_exec::manage_exec_stop,
      update::update_check,
      config::app_settings_get,
      config::app_settings_set,
      update::open_external,
    ])
    .setup(|app| {
      // 日志(不限 debug 构建,release 同样记录,便于现场排查):
      // - 文件:应用文件夹 logs/app.log(与 config/ 同级的便携布局),
      //   由 tauri-plugin-log 的 Folder target 追加写入(目录自动创建,
      //   超过 max_file_size 自动按日期轮转);所有构建均启用;
      // - Stdout:仅 debug 构建启用。release 是 windows_subsystem="windows"
      //   的 GUI 进程,无控制台句柄,写 stdout 会失败并 panic,故不注册。
      let log_dir = crate::config::app_dir().join("logs");
      let mut targets = vec![Target::new(TargetKind::Folder {
        path: log_dir,
        file_name: Some("app".into()),
      })];
      if cfg!(debug_assertions) {
        targets.push(Target::new(TargetKind::Stdout));
      }
      app.handle().plugin(
        tauri_plugin_log::Builder::default()
          .level(log::LevelFilter::Info)
          .targets(targets)
          .build(),
      )?;
      // 桌面端附加能力(UPGRADE-PLAN 阶段四):系统托盘 + 主窗口关闭拦截
      #[cfg(desktop)]
      setup_desktop(app)?;
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}

/// 桌面端附加能力(UPGRADE-PLAN 阶段四「桌面体验」):
/// 1. 常驻系统托盘:图标 + 菜单(显示主窗口/退出);左键单击显示并聚焦主窗口,
///    菜单走右键弹出(show_menu_on_left_click 关闭左键弹菜单)。
/// 2. 主窗口关闭拦截:按 settings.json 的 closeToTray 决定「隐藏到托盘」或
///    「默认关闭退出」。
///
/// API 依据(registry tauri-2.11.5 源码):TrayIconBuilder(src/tray/mod.rs,
/// icon/menu/show_menu_on_left_click/on_menu_event/on_tray_icon_event/build)、
/// TrayIconEvent::Click{button,button_state}(同文件)、Menu::new /
/// MenuItem::with_id / Menu::append_items(src/menu/menu.rs、normal.rs)、
/// MenuEvent::id()(src/menu/mod.rs)、WindowEvent::CloseRequested{api} 与
/// CloseRequestApi::prevent_close(src/app.rs)、AppHandle::exit(src/app.rs)、
/// WebviewWindow::on_window_event / show / set_focus / unminimize。
#[cfg(desktop)]
fn setup_desktop(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    use tauri::Manager;

    // 托盘菜单:显示主窗口 / 退出(with_id 显式指定 id 供事件分发)
    let show_item = MenuItem::with_id(app, "show-main", "显示主窗口", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let tray_menu = Menu::new(app)?;
    tray_menu.append_items(&[&show_item, &quit_item])?;

    let mut tray_builder = TrayIconBuilder::with_id("main-tray")
      .menu(&tray_menu)
      // 左键单击留给「显示主窗口」,菜单改由右键弹出(默认 true 会左键弹菜单)
      .show_menu_on_left_click(false)
      .tooltip("DockerDeploy SSH");
    // 图标用应用默认窗口图标(tauri.conf.json bundle.icon 编译期内嵌;
    // 缺失时不设图标,托盘退化为系统占位图标,不 panic)
    if let Some(icon) = app.default_window_icon().cloned() {
      tray_builder = tray_builder.icon(icon);
    }
    tray_builder
      .on_menu_event(|app, event| {
        // 菜单事件是全局监听,按菜单项 id 分发
        match event.id().as_ref() {
          "show-main" => show_main_window(app),
          "quit" => app.exit(0),
          _ => {}
        }
      })
      .on_tray_icon_event(|tray, event| {
        // 左键单击(松开时)→ 显示并聚焦主窗口
        if let TrayIconEvent::Click {
          button: MouseButton::Left,
          button_state: MouseButtonState::Up,
          ..
        } = event
        {
          show_main_window(tray.app_handle());
        }
      })
      .build(app)?;

    // 主窗口关闭拦截:每次关闭事件现读 settings.json(而非缓存),
    // 保证前端改完设置无需重启即生效;closeToTray=false 不拦截(默认关闭,
    // 最后一个窗口关闭即退出进程)
    if let Some(window) = app.get_webview_window("main") {
      let win = window.clone();
      window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
          if crate::config::load_app_settings().close_to_tray {
            // 阻止默认关闭,隐藏窗口;部署任务在后台继续运行
            api.prevent_close();
            let _ = win.hide();
          }
        }
      });
    }
    Ok(())
}

/// 显示并聚焦主窗口(托盘左键单击与菜单「显示主窗口」共用):
/// 取消最小化 + 显示 + 聚焦,覆盖「最小化到任务栏」「隐藏到托盘」两种状态。
#[cfg(desktop)]
fn show_main_window(app: &tauri::AppHandle) {
  use tauri::Manager;
  if let Some(window) = app.get_webview_window("main") {
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
  }
}
