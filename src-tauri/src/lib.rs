pub mod commands;
pub mod config;
pub mod crypto;
pub mod docker;
pub mod history;
pub mod ssh;
pub mod stack;

use tauri_plugin_log::{Target, TargetKind};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .manage(commands::DeployState::default())
    .invoke_handler(tauri::generate_handler![
      commands::get_config,
      commands::save_config_cmd,
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
      commands::import_compose,
      commands::parse_compose,
      commands::preview_compose,
      commands::get_history,
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
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
