pub mod commands;
pub mod config;
pub mod crypto;
pub mod docker;
pub mod ssh;

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
      commands::deploy,
      commands::cancel_deploy,
    ])
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
