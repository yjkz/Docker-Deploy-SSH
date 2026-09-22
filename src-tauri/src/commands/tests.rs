    use super::*;
    use crate::config::{save_config, AuthConfig, TransferMode};
    use crate::stack::{detect_image_env_drift, image_refs_with_env};

    // ===== 清理分析纯函数(第三批)=====

    #[test]
    fn test_parse_cleanup_ndjson_skips_non_json() {
        // stderr 混入(如 "WARNING: No swap limit support")不应让整节失败;
        // 以 `{` 开头但结构损坏的行单独报"解析失败"
        let out = "WARNING: No swap limit support\n{\"ID\":\"abc\",\"Repository\":\"x\"}\n\n{ broken json\n";
        let (items, warnings) = parse_cleanup_ndjson(out);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["ID"], "abc");
        // 提示行进 warnings;损坏的 JSON 行报解析失败
        assert!(warnings.iter().any(|w| w.contains("No swap limit")));
        assert!(warnings.iter().any(|w| w.contains("解析失败")));
    }

    #[test]
    fn test_parse_cleanup_ndjson_warning_cap() {
        // 提示行最多收集 3 条,防刷屏
        let out = "a\nb\nc\nd\ne\n";
        let (items, warnings) = parse_cleanup_ndjson(out);
        assert!(items.is_empty());
        assert_eq!(warnings.len(), 3);
    }

    #[test]
    fn test_image_repo_of() {
        assert_eq!(image_repo_of("myapp:latest"), "myapp");
        assert_eq!(image_repo_of("myapp"), "myapp");
        // registry:port/name 里的冒号不是 tag 分隔符
        assert_eq!(image_repo_of("registry:5000/app"), "registry:5000/app");
        assert_eq!(image_repo_of("registry:5000/app:v1"), "registry:5000/app");
        assert_eq!(image_repo_of("app@sha256:deadbeef"), "app");
        assert_eq!(image_repo_of(""), "");
    }

    #[test]
    fn test_is_date_tag() {
        assert!(is_date_tag("20260905-101010"));
        assert!(!is_date_tag("latest"));
        assert!(!is_date_tag("20260905"));
        assert!(!is_date_tag("2026090-1010100"));
        assert!(!is_date_tag("2026090x-101010"));
    }

    #[test]
    fn test_compose_image_repos() {
        let yaml = "\
services:
  web:
    image: myapp:latest
  db:
    image: registry:5000/postgres:16
  worker:
    image: myapp:latest
";
        let repos = compose_image_repos(yaml);
        assert!(repos.contains(&"myapp".to_string()));
        assert!(repos.contains(&"registry:5000/postgres".to_string()));
        // 去重:myapp 只出现一次
        assert_eq!(repos.iter().filter(|r| *r == "myapp").count(), 1);
        // 解析失败 → 空(调用方据此跳过标签清理,不误删)
        assert!(compose_image_repos("}{ not yaml").is_empty());
        assert!(compose_image_repos("services: {}").is_empty());
    }

    #[test]
    fn test_split_compose_dump() {
        let out = "==COMPOSE:/home/a/docker-compose.yml\nservices:\n  web:\n    image: x\n\n==COMPOSE:/home/b/docker-compose.yml\nservices:\n  db:\n    image: y\n\n";
        let parts = split_compose_dump(out);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].0, "/home/a/docker-compose.yml");
        assert!(parts[0].1.contains("image: x"));
        assert_eq!(parts[1].0, "/home/b/docker-compose.yml");
        assert!(parts[1].1.contains("image: y"));
    }

    #[test]
    fn test_project_dir_of_release() {
        assert_eq!(
            project_dir_of_release("/home/henghao/zetok/releases/20260905-101010"),
            "/home/henghao/zetok"
        );
        assert_eq!(
            project_dir_of_release("/home/henghao/zetok/releases/20260905-101010/"),
            "/home/henghao/zetok"
        );
        assert_eq!(project_dir_of_release("/tmp/x"), "");
    }

    #[test]
    fn test_parse_du_output() {
        // GNU du:大小与路径以制表符分隔
        let rows = parse_du_output("1.2G\t/home/a/proj\n340M\t/home/a/other\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], ("/home/a/proj".to_string(), "1.2G".to_string()));
        assert_eq!(rows[1], ("/home/a/other".to_string(), "340M".to_string()));
        // 退化为空格分隔
        let rows2 = parse_du_output("12K /home/a/b");
        assert_eq!(rows2[0], ("/home/a/b".to_string(), "12K".to_string()));
        // 空格分隔 + 路径含空格:size 必无空格,按首个空白切后其余整体为路径
        let rows3 = parse_du_output("512M /home/a b/my proj");
        assert_eq!(rows3[0], ("/home/a b/my proj".to_string(), "512M".to_string()));
        assert!(parse_du_output("").is_empty());
    }

    #[test]
    fn test_abs_path_lines() {
        let out = "/home/a/docker-compose.yml\nrunning\n… (共 12 行)\n/home/b/compose.yml\n";
        let lines = abs_path_lines(out);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "/home/a/docker-compose.yml");
    }

    #[test]
    fn test_cleanup_cmd_builders_quote() {
        // 含空格与单引号的路径必须被安全包裹
        let dirs = vec!["/home/a b/releases/20260905-101010".to_string()];
        assert_eq!(
            rm_release_dirs_cmd(&dirs),
            "rm -rf '/home/a b/releases/20260905-101010'"
        );
        let ids = vec!["sha256:abc".to_string()];
        assert_eq!(rmi_ids_cmd(&ids), "docker rmi 'sha256:abc'");
        assert_eq!(
            rm_volume_names_cmd(&["vol1".to_string()]),
            "docker volume rm 'vol1'"
        );
    }

    #[test]
    fn test_is_valid_release_dir_target() {
        // 合法:<root>/releases/<ts>(root 非空、ts 为纯目录名)
        assert!(is_valid_release_dir_target("/home/a/releases/20260905-101010"));
        assert!(is_valid_release_dir_target("/srv/app b/releases/20260905-101010"));
        assert!(is_valid_release_dir_target("/opt/x/releases/20260905-101010"));
        // 拒绝:非绝对路径 / 缺 releases 段 / ts 含子路径 / 逃逸
        assert!(!is_valid_release_dir_target("home/a/releases/2026"));
        assert!(!is_valid_release_dir_target("/home/a/20260905-101010"));
        assert!(!is_valid_release_dir_target("/home/a/releases/"));
        assert!(!is_valid_release_dir_target("/releases/2026")); // root 为空
        assert!(!is_valid_release_dir_target("/home/a/releases/x/y"));
        assert!(!is_valid_release_dir_target("/home/a/releases/../../etc"));
        assert!(!is_valid_release_dir_target("/home/a/releases/..\\x"));
        assert!(!is_valid_release_dir_target("/etc"));
        assert!(!is_valid_release_dir_target("/"));
    }

    #[test]
    fn test_cleanup_scan_compose_cmd_excludes_archives() {
        // 纯函数版本(第二十六批拆出):不读全局设置 —— 测试恒稳定,
        // 且能直接断言「自定义名字与深度」确实进入命令
        let cmd = cleanup_scan_compose_cmd_with("/home/henghao", &[], 4);
        assert!(cmd.contains("-maxdepth 4"));
        assert!(cmd.contains("'*/releases/*'"));
        assert!(cmd.contains("'*/.git/*'"));
        assert!(cmd.contains("/home/henghao"));
        assert!(cmd.contains("'docker-compose.yml'"), "空列表回退默认名: {}", cmd);

        // 自定义名与深度
        let custom = cleanup_scan_compose_cmd_with(
            "/srv/app",
            &["docker-compose.prod.yml".to_string()],
            6,
        );
        assert!(custom.contains("-maxdepth 6"), "{}", custom);
        assert!(custom.contains("'docker-compose.prod.yml'"), "{}", custom);
        assert!(!custom.contains("'docker-compose.yaml'"), "默认名不应混入: {}", custom);
    }

    // ---- 第七批:回滚明细批量读取 + 版本说明 ----

    #[test]
    fn test_cleanup_scan_releases_cmd_accepts_custom_names() {
        // A2(第二十七批):归档名放宽 —— 不再要求 `20*-*` 时间戳形态,
        // 自定义命名(如 prod-2026-09-18)也应被列出;排序退回 mtime
        // (与部署侧收尾裁剪 cleanup_releases_cmd 的 `ls -1dt` 同源)。
        let cmd = cleanup_scan_releases_cmd("/srv/app");
        assert!(cmd.contains("'/srv/app'"), "{}", cmd);
        assert!(cmd.contains("-path '*/releases'"), "{}", cmd);
        // 名字过滤必须消失(自定义命名归档否则不可见)
        assert!(!cmd.contains("-name"), "不应再按名字过滤: {}", cmd);
        assert!(!cmd.contains("20*-*"), "时间戳模式必须移除: {}", cmd);
        // mtime 倒序:每个 releases 目录一次 ls -1dt(不经 xargs —— 批拆分会让
        // 排序只在批内成立,全局顺序失真)
        assert!(cmd.contains("ls -1dt"), "需 mtime 倒序: {}", cmd);
        assert!(cmd.contains("while IFS= read -r"), "{}", cmd);
    }

    #[test]
    fn test_parse_release_lines_normalizes() {
        // 远端 `ls -1dt` 的目录条目带尾斜杠;解析须去尾斜杠、丢坏行、保序
        let out = "/srv/app/releases/prod-2026-09-18/\n\
                   \n\
                   not-a-path\n\
                   /srv/app/releases/20260905-101010/\n";
        let rows = parse_release_lines(out);
        assert_eq!(
            rows,
            vec![
                "/srv/app/releases/prod-2026-09-18".to_string(),
                "/srv/app/releases/20260905-101010".to_string(),
            ]
        );
        assert!(parse_release_lines("").is_empty());
    }

    #[test]
    fn test_releases_of_project_preserves_mtime_order_with_custom_names() {
        // 输入 = 远端 mtime 倒序(自定义命名使字典序与时间序分离):
        // 实现若退回按名字排序,本测试必红 —— 而「保留最新 N 个」会删错归档。
        let all = vec![
            "/srv/app/releases/prod-2026-09-18".to_string(), // 最新
            "/srv/app/releases/20260901-101010".to_string(),
            "/srv/app/releases/zzz-old-name-sorts-last".to_string(), // 名字最大、时间最旧
            "/srv/other/releases/20260905-101010".to_string(),      // 别的项目:剔除
        ];
        let got = releases_of_project(&all, "/srv/app");
        assert_eq!(
            got,
            vec![
                "/srv/app/releases/prod-2026-09-18",
                "/srv/app/releases/20260901-101010",
                "/srv/app/releases/zzz-old-name-sorts-last",
            ]
        );
        assert!(releases_of_project(&all, "/srv/none").is_empty());
    }

    #[test]
    fn test_releases_scan_cmd_markers_and_flags() {
        let candidates = vec!["/app/docker-compose.yml".to_string()];
        let with_images = releases_scan_cmd("/app", 50, &candidates, true);
        // mtime 倒序 + 截断 + 三类归档段 + compose 段 + 镜像段
        // (A2:归档名放宽后不能再按名字排序/截断,否则最新归档会被挤出前 N 条)
        assert!(with_images.contains("ls -1dt '/app/releases'/*/"));
        assert!(with_images.contains("| head -n 50"));
        assert!(!with_images.contains("sort -r"), "不得按名字排序: {}", with_images);
        assert!(with_images.contains("while IFS= read -r d"));
        assert!(with_images.contains("==RELEASE:%s"));
        assert!(with_images.contains("==MANIFEST:%s"));
        assert!(with_images.contains("==NOTES:%s"));
        assert!(with_images.contains("==COMPOSE:%s"));
        assert!(with_images.contains("==IMAGES"));
        assert!(with_images.contains("'/app/docker-compose.yml'"));
        // 归档内路径由远端循环变量拼接($d),不再逐归档展开字面路径
        assert!(with_images.contains("cat \"$d/manifest.json\""));
        assert!(with_images.contains("cat \"$d/release-notes.json\""));
        // include_images=false 时不带镜像段(list_releases 用)
        let no_images = releases_scan_cmd("/app", 100, &[], false);
        assert!(!no_images.contains("==IMAGES"));
        assert!(no_images.contains("head -n 100"));
    }

    #[test]
    fn test_parse_releases_dump_sections() {
        let candidates = vec!["/app/docker-compose.yml".to_string()];
        // 远端输出按归档分组:每个归档的 RELEASE/MANIFEST/NOTES 相邻
        let out = "==RELEASE:20260905-101010\n\
                   app.tar.gz\n\
                   manifest.json\n\
                   ==MANIFEST:20260905-101010\n\
                   {\"project\":\"app\",\"images\":[]}\n\
                   ==RELEASE:20260901-090000\n\
                   ==NOTES:20260901-090000\n\
                   {\"title\":\"v1\",\"body\":\"fix\",\"updatedAt\":\"t\"}\n\
                   ==COMPOSE:/app/docker-compose.yml\n\
                   services:\n  web:\n    image: app:latest\n\
                   ==IMAGES\n\
                   {\"Repository\":\"app\",\"Tag\":\"20260901-090000\"}\n";
        let dump = parse_releases_dump(out, &candidates);
        assert_eq!(dump.entries.len(), 2);
        assert_eq!(dump.entries[0].ts, "20260905-101010");
        assert_eq!(dump.entries[0].files, vec!["app.tar.gz", "manifest.json"]);
        assert!(dump.entries[0].manifest.contains("\"project\":\"app\""));
        assert!(dump.entries[0].notes.is_empty());
        // 第二个归档只有 NOTES 段(files/manifest 为空,不错位)
        assert_eq!(dump.entries[1].ts, "20260901-090000");
        assert!(dump.entries[1].files.is_empty());
        assert!(dump.entries[1].notes.contains("\"title\":\"v1\""));
        assert!(dump.compose_texts[0].contains("image: app:latest"));
        assert!(dump.images.contains("\"Repository\":\"app\""));
    }

    #[test]
    fn test_parse_releases_dump_empty_and_unknown() {
        let dump = parse_releases_dump("", &[]);
        assert!(dump.entries.is_empty());
        assert!(dump.images.is_empty());
        // 标记即条目:==RELEASE 后的清单行归入该条目,文件清单忽略空行;
        // ==IMAGES 段独立收集
        let dump = parse_releases_dump(
            "==RELEASE:99999999-999999\nstray.tar.gz\n\n==IMAGES\n{x}\n",
            &[],
        );
        assert_eq!(dump.entries.len(), 1);
        assert_eq!(dump.entries[0].ts, "99999999-999999");
        assert_eq!(dump.entries[0].files, vec!["stray.tar.gz"]);
        assert!(dump.images.contains("{x}"));
    }

    #[test]
    fn test_rollback_scan_cmd_sections() {
        let cmd = rollback_scan_cmd("/home/henghao");
        assert!(cmd.starts_with("[ -d '/home/henghao' ]"));
        assert!(cmd.contains("==ROOTEXISTS:1"));
        assert!(cmd.contains("==COMPOSEFILES"));
        assert!(cmd.contains("==RELEASES"));
        assert!(cmd.contains("==PSALL"));
        assert!(cmd.contains("docker ps -a --format '{{json .}}'"));
        // compose find 复用清理分析的拼装(深度 4,排除 releases/.git)
        assert!(cmd.contains("-maxdepth 4"));
        assert!(cmd.contains("'*/releases/*'"));
    }

    #[test]
    fn test_parse_rollback_scan_sections() {
        let out = "==ROOTEXISTS:1\n\
                   ==COMPOSEFILES\n\
                   /home/x/app/docker-compose.yml\n\
                   ==RELEASES\n\
                   /home/x/app/releases/20260905-101010\n\
                   ==PSALL\n\
                   {\"Names\":\"app\",\"Status\":\"Up 1 hour\",\"Labels\":{\"com.docker.compose.project.working_dir\":\"/home/x/app\"}}\n\
                   not-json\n";
        let scan = parse_rollback_scan(out);
        assert!(scan.root_exists);
        assert_eq!(scan.compose_paths, vec!["/home/x/app/docker-compose.yml"]);
        assert_eq!(
            scan.release_paths,
            vec!["/home/x/app/releases/20260905-101010"]
        );
        assert_eq!(scan.ps_items.len(), 1); // 坏行跳过
        assert_eq!(compose_working_dirs(&scan.ps_items), vec!["/home/x/app"]);
    }

    #[test]
    fn test_parse_rollback_scan_root_missing() {
        let scan = parse_rollback_scan("==ROOTEXISTS:0\n");
        assert!(!scan.root_exists);
    }

    #[test]
    fn test_running_container_count_exact_working_dir() {
        // A1(第二十七批):精确归属 —— 计数按 compose working_dir 标签匹配项目目录,
        // 不再按容器名包含目录名的近似。反例构造成本关键:同前缀的两个项目
        // (app / app-staging)在旧口径下会互相串数,新口径必须各归各的。
        let items: Vec<serde_json::Value> = vec![
            serde_json::json!({"Names":"app-web-1","Status":"Up 3 hours","Labels":{"com.docker.compose.project.working_dir":"/opt/app"}}),
            serde_json::json!({"Names":"app-db-1","Status":"Up 3 hours","Labels":{"com.docker.compose.project.working_dir":"/opt/app"}}),
            // 名字含 "app" 但归属 app-staging:旧口径会被算进 /opt/app
            serde_json::json!({"Names":"app-staging-web-1","Status":"Up 2 hours","Labels":{"com.docker.compose.project.working_dir":"/opt/app-staging"}}),
            // 已停止:不计
            serde_json::json!({"Names":"app-old-1","Status":"Exited (0) 2 days ago","Labels":{"com.docker.compose.project.working_dir":"/opt/app"}}),
            // 非 compose 容器(无标签):不计入任何项目
            serde_json::json!({"Names":"loose-container","Status":"Up 1 min"}),
        ];
        assert_eq!(running_container_count(&items, "/opt/app"), 2);
        assert_eq!(running_container_count(&items, "/opt/app-staging"), 1);
        assert_eq!(running_container_count(&items, "/opt/other"), 0);
        // 尾斜杠等价(扫描根可能带 / 结尾)
        assert_eq!(running_container_count(&items, "/opt/app/"), 2);
        // 空目录名不得匹配到任何容器
        assert_eq!(running_container_count(&items, ""), 0);
    }

    #[test]
    fn test_running_container_count_status_variants() {
        // Status 前缀判定:Up / up 混合大小写都算运行中;Restarting 不算
        let items: Vec<serde_json::Value> = vec![
            serde_json::json!({"Names":"a","Status":"up 5 seconds","Labels":{"com.docker.compose.project.working_dir":"/d"}}),
            serde_json::json!({"Names":"b","Status":"Restarting (1) 2 seconds ago","Labels":{"com.docker.compose.project.working_dir":"/d"}}),
            serde_json::json!({"Names":"c","Status":"Up 1 hour (healthy)","Labels":{"com.docker.compose.project.working_dir":"/d"}}),
        ];
        assert_eq!(running_container_count(&items, "/d"), 2);
    }

    #[test]
    fn test_compose_working_dirs_dedupe_and_skip_empty() {
        let items: Vec<serde_json::Value> = vec![
            serde_json::json!({"Labels": {"com.docker.compose.project.working_dir": "/a"}}),
            serde_json::json!({"Labels": {"com.docker.compose.project.working_dir": "/a"}}),
            serde_json::json!({"Names": "no-labels"}),
        ];
        assert_eq!(compose_working_dirs(&items), vec!["/a"]);
    }

    #[test]
    fn test_release_notes_write_cmd_atomic() {
        let cmd = release_notes_write_cmd("/app/releases/x/release-notes.json", "QUJD");
        // 与 .env 原子写同构:b64 进、tmp 带引号外 $$ 后缀、mv 原子替换到单引号包裹的目标
        assert!(cmd.starts_with("echo QUJD | base64 -d > "));
        assert!(cmd.contains("'.ddtmp.'$$"));
        assert!(cmd.contains("&& mv '"));
        assert!(cmd.ends_with(" '/app/releases/x/release-notes.json'"));
    }

    #[test]
    fn test_parse_release_notes_invalid() {
        assert!(parse_release_notes("").is_none());
        assert!(parse_release_notes("not json").is_none());
        assert!(parse_release_notes("{\"title\":\"缺 body\"}").is_none());
        let n = parse_release_notes(
            "{\"title\":\"v1\",\"body\":\"desc\",\"updatedAt\":\"2026-09-12T00:00:00Z\"}",
        );
        assert_eq!(n.unwrap().body, "desc");
    }

    #[test]
    fn test_cleanup_sections_has_any() {
        let empty = CleanupSections {
            images: true,
            containers: false,
            volumes: false,
            builder: false,
            image_ids: vec![],
            container_ids: vec![],
            volume_names: vec![],
            projects: vec![],
        };
        // 勾了节但没有任何目标 → 不可执行(防"勾了却没东西可删"的误报成功)
        assert!(!empty.has_any());
        let with_target = CleanupSections {
            images: true,
            containers: false,
            volumes: false,
            builder: false,
            image_ids: vec!["sha256:x".into()],
            container_ids: vec![],
            volume_names: vec![],
            projects: vec![],
        };
        assert!(with_target.has_any());
        let builder_only = CleanupSections {
            images: false,
            containers: false,
            volumes: false,
            builder: true,
            image_ids: vec![],
            container_ids: vec![],
            volume_names: vec![],
            projects: vec![],
        };
        assert!(builder_only.has_any());
    }

    #[test]
    fn test_effective_remote_dir_fallback_semantics() {
        // 第四批:项目级目录优先;未配置(旧配置)必须完全回落到服务器目录
        let server = ServerConfig {
            id: "s1".into(),
            name: "美国".into(),
            host: "1.2.3.4".into(),
            port: 22,
            username: "root".into(),
            auth: AuthConfig {
                auth_type: crate::config::AuthType::Password,
                key_path: None,
                password_enc: None,
                key_pass_enc: None,
            },
            remote_dir: "/home/henghao".into(),
            host_key_sha256: None,
            tags: Vec::new(),
        };
        let mk_project = |dir: Option<&str>| ProjectConfig {
            id: "p1".into(),
            name: "官网".into(),
            image_filter: String::new(),
            compose_file: "c.yml".into(),
            file_mappings: Vec::new(),
            service_overrides: Vec::new(),
            health_wait_secs: 0,
            pre_deploy_cmd: None,
            post_deploy_cmd: None,
            notify_webhook: None,
            source_compose_path: None,
            source_hash: None,
            remote_dir: dir.map(String::from),
            default_server_id: None,
            release_keep: None,
        };
        // 未配置 → 服务器目录(旧配置行为不变)
        assert_eq!(effective_remote_dir(&server, &mk_project(None)), "/home/henghao");
        // 空串/纯空白 → 同样回落(视为未配置)
        assert_eq!(effective_remote_dir(&server, &mk_project(Some(""))), "/home/henghao");
        assert_eq!(effective_remote_dir(&server, &mk_project(Some("   "))), "/home/henghao");
        // 配置了 → 用项目自己的目录
        assert_eq!(
            effective_remote_dir(&server, &mk_project(Some("/home/henghao/site"))),
            "/home/henghao/site"
        );
        // 前后空白被裁剪
        assert_eq!(
            effective_remote_dir(&server, &mk_project(Some("  /opt/app  "))),
            "/opt/app"
        );
    }

    #[test]
    fn test_project_config_legacy_json_defaults() {
        // 旧 projects.json(无 remote_dir / default_server_id)必须能反序列化,
        // 且两个新字段为 None(行为与历史一致)
        let legacy = r#"{
            "id": "p1", "name": "官网", "image_filter": "",
            "compose_file": "E:/cfg/stacks/u1/docker-compose.yml",
            "file_mappings": [],
            "service_overrides": [],
            "health_wait_secs": 0,
            "pre_deploy_cmd": null, "post_deploy_cmd": null, "notify_webhook": null
        }"#;
        let p: ProjectConfig = serde_json::from_str(legacy).unwrap();
        assert_eq!(p.remote_dir, None);
        assert_eq!(p.default_server_id, None);
        assert_eq!(p.source_compose_path, None);
        // 回落语义:旧配置取服务器目录
        let server = ServerConfig {
            id: "s1".into(),
            name: "s".into(),
            host: "h".into(),
            port: 22,
            username: "u".into(),
            auth: AuthConfig {
                auth_type: crate::config::AuthType::Password,
                key_path: None,
                password_enc: None,
                key_pass_enc: None,
            },
            remote_dir: "/opt/legacy".into(),
            host_key_sha256: None,
            tags: Vec::new(),
        };
        assert_eq!(effective_remote_dir(&server, &p), "/opt/legacy");
    }

    #[test]
    fn test_deploy_record_legacy_json_defaults() {
        // 旧 deployments.json(无 server_id/project_id)能反序列化,字段为 None
        let legacy = r#"{
            "ts": "2026-09-01 10:00:00", "mode": "stack",
            "server_name": "美国", "project_name": "官网",
            "images": ["a:1"], "success": true,
            "message": "部署完成", "duration_secs": 42
        }"#;
        let r: DeployRecord = serde_json::from_str(legacy).unwrap();
        assert_eq!(r.server_id, None);
        assert_eq!(r.project_id, None);
        assert_eq!(r.release_dir, None);
    }

    #[test]
    fn test_local_basename() {
        // Windows 与 POSIX 分隔符都支持,尾部斜杠去掉
        assert_eq!(local_basename("E:\\apps\\web"), Some("web".to_string()));
        assert_eq!(local_basename("/opt/data/"), Some("data".to_string()));
        assert_eq!(local_basename("C:/x/y/z.txt"), Some("z.txt".to_string()));
        assert_eq!(local_basename("web"), Some("web".to_string()));
        // 无法取名的输入 → None(调用方据此保持原行为)
        assert_eq!(local_basename(""), None);
        assert_eq!(local_basename("   "), None);
        assert_eq!(local_basename("/"), None);
    }

    #[test]
    fn test_source_content_hash_detects_change() {
        let dir = std::env::temp_dir().join(format!("dd-hash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let compose = dir.join("docker-compose.yml");
        std::fs::write(&compose, "services:\n  web:\n    image: a:1\n").unwrap();
        let h1 = source_content_hash(&compose).unwrap();
        // 同内容 → 同哈希(可重复比对)
        assert_eq!(h1, source_content_hash(&compose).unwrap());

        // 改 compose → 哈希变化
        std::fs::write(&compose, "services:\n  web:\n    image: a:2\n").unwrap();
        let h2 = source_content_hash(&compose).unwrap();
        assert_ne!(h1, h2);

        // 改 .env(compose 未动)→ 也要变化(插值结果会变)
        std::fs::write(&compose, "services:\n  web:\n    image: a:1\n").unwrap();
        std::fs::write(dir.join(".env"), "TAG=2\n").unwrap();
        let h3 = source_content_hash(&compose).unwrap();
        assert_ne!(h1, h3);

        // 源不存在 → Err(调用方按"无法比对"降级,不误判为已变更)
        assert!(source_content_hash(&dir.join("nope.yml")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_bundle_content_changed_ignores_compose_filename() {
        let dir = std::env::temp_dir().join(format!("dd-bundle-{}", uuid::Uuid::new_v4()));
        let src = dir.join("src");
        let dst = dir.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        let body = "services:\n  web:\n    image: a:1\n";

        // 源叫 compose.yml,副本叫 docker-compose.yml —— 内容相同不应判为变更
        // (这正是 source_content_hash 按文件名入哈希的盲区)
        std::fs::write(src.join("compose.yml"), body).unwrap();
        std::fs::write(dst.join("docker-compose.yml"), body).unwrap();
        assert!(!bundle_content_changed(&src.join("compose.yml"), &dst.join("docker-compose.yml")).unwrap());

        // compose 内容不同 → 变更
        std::fs::write(dst.join("docker-compose.yml"), "services:\n  web:\n    image: a:2\n").unwrap();
        assert!(bundle_content_changed(&src.join("compose.yml"), &dst.join("docker-compose.yml")).unwrap());

        // 副本缺失 → 需同步
        assert!(bundle_content_changed(&src.join("compose.yml"), &dst.join("nope.yml")).unwrap());

        // .env 变化 → 变更(即使 compose 相同)
        std::fs::write(dst.join("docker-compose.yml"), body).unwrap();
        assert!(!bundle_content_changed(&src.join("compose.yml"), &dst.join("docker-compose.yml")).unwrap());
        std::fs::write(src.join(".env"), "TAG=2\n").unwrap();
        assert!(bundle_content_changed(&src.join("compose.yml"), &dst.join("docker-compose.yml")).unwrap());
        std::fs::write(dst.join(".env"), "TAG=2\n").unwrap();
        assert!(!bundle_content_changed(&src.join("compose.yml"), &dst.join("docker-compose.yml")).unwrap());

        // override 增删 → 变更
        std::fs::write(src.join("docker-compose.override.yml"), "services: {}\n").unwrap();
        assert!(bundle_content_changed(&src.join("compose.yml"), &dst.join("docker-compose.yml")).unwrap());
        std::fs::write(dst.join("docker-compose.override.yml"), "services: {}\n").unwrap();
        assert!(!bundle_content_changed(&src.join("compose.yml"), &dst.join("docker-compose.yml")).unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_update_reports_changed_before_overwriting_baseline() {
        // 回归:update_project_from_source 曾先写新哈希再复核状态,导致
        // 无论是否真的同步了内容都报 unchanged(用户看到"无改动"但文件已更新)。
        // 这里验证判定所用的"更新前副本 vs 源"语义:内容确有差异时必须为 changed。
        let dir = std::env::temp_dir().join(format!("dd-upd-{}", uuid::Uuid::new_v4()));
        let src = dir.join("src");
        let dst = dir.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("docker-compose.yml"), "services:\n  web:\n    image: v2\n").unwrap();
        std::fs::write(dst.join("docker-compose.yml"), "services:\n  web:\n    image: v1\n").unwrap();
        // 更新前:内容不同 → 本次会带来改动(应报 changed)
        assert!(bundle_content_changed(&src.join("docker-compose.yml"), &dst.join("docker-compose.yml")).unwrap());
        // 模拟更新后(副本 == 源)→ 再比对为一致(基准已同步,后续不再报变更)
        std::fs::copy(src.join("docker-compose.yml"), dst.join("docker-compose.yml")).unwrap();
        assert!(!bundle_content_changed(&src.join("docker-compose.yml"), &dst.join("docker-compose.yml")).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_project_source_status_states() {
        let mk = |compose: &str, path: Option<&str>, hash: Option<&str>| ProjectConfig {
            id: "p".into(),
            name: "n".into(),
            image_filter: String::new(),
            compose_file: compose.into(),
            file_mappings: Vec::new(),
            service_overrides: Vec::new(),
            health_wait_secs: 0,
            pre_deploy_cmd: None,
            post_deploy_cmd: None,
            notify_webhook: None,
            source_compose_path: path.map(String::from),
            source_hash: hash.map(String::from),
            remote_dir: None,
            default_server_id: None,
            release_keep: None,
        };
        // 未绑定源 → unbound;手工项目(远端相对路径)与导入项目都算 unbound,
        // 但 imported 不同,前端据此决定是否提示补绑
        let manual = mk("docker-compose.yml", None, None);
        let st = project_source_status(&manual);
        assert_eq!(st.state, "unbound");
        assert!(!st.imported, "远端相对路径 = 手工项目");
        assert!(!st.bound);

        // 导入项目(compose 在 config/stacks 下)→ imported=true(unbound 时前端提示绑定)
        let imported_compose = crate::config::config_dir()
            .join("stacks")
            .join("test-uuid")
            .join("docker-compose.yml");
        let imported = mk(&imported_compose.to_string_lossy(), None, None);
        let st = project_source_status(&imported);
        assert_eq!(st.state, "unbound");
        assert!(st.imported, "配置目录下副本 = 导入项目");

        // 源不存在 → missing
        let missing = mk(
            "docker-compose.yml",
            Some("E:/definitely/not/here/docker-compose.yml"),
            Some("x"),
        );
        assert_eq!(project_source_status(&missing).state, "missing");

        let dir = std::env::temp_dir().join(format!("dd-src-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let compose = dir.join("docker-compose.yml");
        std::fs::write(&compose, "services: {}\n").unwrap();

        // 已绑定但无哈希(手工改配置)→ 以当前内容为基准,报 unchanged 而非 unknown
        let no_hash = mk("docker-compose.yml", Some(&compose.to_string_lossy()), None);
        let st = project_source_status(&no_hash);
        assert_eq!(st.state, "unchanged");
        assert!(st.bound);

        // 哈希一致 → unchanged;不一致 → changed
        let h = source_content_hash(&compose).unwrap();
        let same = mk("docker-compose.yml", Some(&compose.to_string_lossy()), Some(&h));
        assert_eq!(project_source_status(&same).state, "unchanged");
        let diff = mk(
            "docker-compose.yml",
            Some(&compose.to_string_lossy()),
            Some("deadbeef"),
        );
        assert_eq!(project_source_status(&diff).state, "changed");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ===== deploy_notify_text(通知中心挂点文案)=====

    #[test]
    fn test_deploy_notify_text_success() {
        let record = DeployRecord {
            ts: "2026-09-05 12:00:00".into(),
            mode: MODE_SINGLE.into(),
            server_name: "生产".into(),
            project_name: "博客".into(),
            images: vec![],
            success: true,
            message: "部署完成".into(),
            duration_secs: 42,
            release_dir: None,
            server_id: None,
            project_id: None,
        };
        let (title, body) = deploy_notify_text(true, "部署完成", &Some(record));
        assert_eq!(title, "部署成功");
        assert!(body.contains("博客"));
        assert!(body.contains("生产"));
        assert!(body.contains("部署完成"));
        assert!(body.contains("42"));
    }

    #[test]
    fn test_deploy_notify_text_failure_and_cancel() {
        let record = DeployRecord {
            ts: String::new(),
            mode: MODE_STACK.into(),
            server_name: "s".into(),
            project_name: "p".into(),
            images: vec![],
            success: false,
            message: crate::errors::cancelled(),
            duration_secs: 3,
            release_dir: None,
            server_id: None,
            project_id: None,
        };
        // 取消(错误码 canceled)→ cancel 标题;正文剥标记展示纯文案
        let (title, body) = deploy_notify_text(false, &crate::errors::cancelled(), &Some(record));
        assert_eq!(title, "部署已取消");
        assert!(body.contains(CANCELLED_MSG));
        assert!(!body.contains("dderr"), "正文不得带码标记: {}", body);
        // 普通失败 → failure 标题
        let (title, _) = deploy_notify_text(false, "健康检查未通过", &None);
        assert_eq!(title, "部署失败");
        // panic 路径(无记录)→ 兜底正文
        let (_, body) = deploy_notify_text(false, "部署过程发生内部错误", &None);
        assert!(body.contains("部署详情缺失"));
    }

    // ===== hook_failure_result(钩子失败映射)=====

    #[test]
    fn test_hook_failure_result_cancel_passthrough() {
        // 取消错误(码 canceled)原样透传(Pre/Post 同口径):包装成其他文案会让
        // 收尾的 cancel 判定失配,把取消误报为部署失败
        let cancelled = crate::errors::cancelled();
        assert_eq!(
            hook_failure_result(HookKind::Pre, &cancelled),
            Some(cancelled.clone())
        );
        assert_eq!(
            hook_failure_result(HookKind::Post, &cancelled),
            Some(cancelled)
        );
    }

    #[test]
    fn test_hook_failure_result_pre_wraps_post_swallows() {
        // Pre 普通失败 → 「执行失败,部署中止」;Post 普通失败 → None(仅告警)
        assert_eq!(
            hook_failure_result(HookKind::Pre, "exit code 1"),
            Some("部署前钩子执行失败,部署中止: exit code 1".to_string())
        );
        assert_eq!(hook_failure_result(HookKind::Post, "exit code 1"), None);
    }

    // ===== rollback_notify_text(回滚通知文案)=====

    #[test]
    fn test_rollback_notify_text_success() {
        let record = DeployRecord {
            ts: "2026-09-05 12:00:00".into(),
            mode: MODE_ROLLBACK.into(),
            server_name: "生产".into(),
            project_name: "博客".into(),
            images: vec![],
            success: true,
            message: "回滚到 20260905120000".into(),
            duration_secs: 7,
            release_dir: None,
            server_id: None,
            project_id: None,
        };
        let message = record.message.clone();
        let (title, body) = rollback_notify_text(true, &message, &Some(record));
        assert_eq!(title, "回滚成功");
        assert!(body.contains("博客"));
        // 目标 release ts 随「回滚到 …」消息进入正文
        assert!(body.contains("20260905120000"));
        assert!(body.contains("7"));
    }

    #[test]
    fn test_rollback_notify_text_failure_and_cancel() {
        // 取消(错误码 canceled)→ cancel 标题;普通失败 → failure 标题
        let (title, _) = rollback_notify_text(false, &crate::errors::cancelled(), &None);
        assert_eq!(title, "回滚已取消");
        let (title, body) = rollback_notify_text(false, "回滚目标发布目录不存在", &None);
        assert_eq!(title, "回滚失败");
        // 无记录(panic/早期失败)→ 兜底正文
        assert!(body.contains("回滚详情缺失"));
    }

    // ===== remote_join =====

    #[test]
    fn test_remote_join_simple() {
        assert_eq!(
            remote_join("/opt/app", "docker-compose.yml"),
            "/opt/app/docker-compose.yml"
        );
    }

    #[test]
    fn test_remote_join_nested_rel() {
        assert_eq!(
            remote_join("/opt/app", "sql/init.sql"),
            "/opt/app/sql/init.sql"
        );
    }

    #[test]
    fn test_remote_join_leading_slash_no_escape() {
        // rel 以 '/' 开头时视为相对 remote_dir 仍拼接,不提供绝对路径逃逸
        assert_eq!(remote_join("/opt/app", "/abs/x"), "/opt/app/abs/x");
    }

    #[test]
    fn test_remote_join_base_trailing_slash() {
        assert_eq!(remote_join("/opt/app/", "y"), "/opt/app/y");
        assert_eq!(remote_join("/opt/app//", "a/b"), "/opt/app/a/b");
    }

    #[test]
    fn test_remote_join_empty_rel() {
        assert_eq!(remote_join("/opt/app", ""), "/opt/app");
    }

    #[test]
    fn test_remote_join_empty_base() {
        assert_eq!(remote_join("", "a.txt"), "/a.txt");
        assert_eq!(remote_join("/", "a.txt"), "/a.txt");
    }

    // ===== split_remote_file =====

    #[test]
    fn test_split_remote_file() {
        assert_eq!(
            split_remote_file("/opt/app/docker-compose.yml"),
            ("/opt/app".to_string(), "docker-compose.yml".to_string())
        );
        assert_eq!(
            split_remote_file("a.txt"),
            (String::new(), "a.txt".to_string())
        );
    }

    // ===== shell_single_quote =====

    #[test]
    fn test_shell_single_quote() {
        assert_eq!(shell_single_quote("/opt/app"), "'/opt/app'");
        assert_eq!(shell_single_quote("/opt/a'b"), "'/opt/a'\\''b'");
    }

    // ===== 单镜像 compose 路径 =====

    #[test]
    fn test_is_windows_absolute_path() {
        assert!(is_windows_absolute_path(
            r"E:\github\Docker-Deploy-SSH\docker-compose.yml"
        ));
        assert!(is_windows_absolute_path(
            "E:/github/Docker-Deploy-SSH/docker-compose.yml"
        ));
        assert!(is_windows_absolute_path(r"\\server\share\docker-compose.yml"));
        assert!(!is_windows_absolute_path("docker-compose.yml"));
        assert!(!is_windows_absolute_path("/opt/app/docker-compose.yml"));
    }

    #[test]
    fn test_single_image_command_uses_remote_compose_path() {
        let local = r"E:\github\Docker-Deploy-SSH\config\stacks\id\docker-compose.yml";
        let remote = remote_compose_path("/home/henghao");
        let cmd = compose_up_cmd("/home/henghao", &remote, &[]);
        assert!(!cmd.contains(local));
        assert_eq!(
            cmd,
            "cd '/home/henghao' && docker compose -f '/home/henghao/docker-compose.yml' up -d"
        );
    }

    // ===== resolve_password =====

    #[test]
    fn test_resolve_password_key_auth() {
        // Key 认证:一律 None,不使用密码
        assert_eq!(resolve_password(&AuthType::Key, None, None).unwrap(), None);
        assert_eq!(
            resolve_password(&AuthType::Key, Some("ignored"), Some("enc")).unwrap(),
            None
        );
    }

    #[test]
    fn test_resolve_password_plain_takes_priority() {
        assert_eq!(
            resolve_password(&AuthType::Password, Some("pw"), None).unwrap(),
            Some("pw".to_string())
        );
        // 前端输入的明文优先于已保存密文
        assert_eq!(
            resolve_password(&AuthType::Password, Some("fresh"), Some("enc")).unwrap(),
            Some("fresh".to_string())
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_resolve_password_fallback_to_enc() {
        let enc = crate::crypto::dpapi_protect("saved-pw").unwrap();
        assert_eq!(
            resolve_password(&AuthType::Password, None, Some(&enc)).unwrap(),
            Some("saved-pw".to_string())
        );
        // 空明文视为未输入,回退到已保存密文
        assert_eq!(
            resolve_password(&AuthType::Password, Some(""), Some(&enc)).unwrap(),
            Some("saved-pw".to_string())
        );
    }

    #[test]
    fn test_resolve_password_missing() {
        // 密码认证但既无明文也无密文 → 报错
        assert!(resolve_password(&AuthType::Password, None, None).is_err());
    }

    #[test]
    fn test_resolve_password_bad_enc() {
        // 密文无效(base64 非法)→ 报错
        assert!(resolve_password(&AuthType::Password, None, Some("不是base64!!")).is_err());
    }

    // ===== resolve_key_passphrase(阶段三:加密私钥口令)=====

    /// 构造仅含 auth 的最小 ServerConfig(供 resolve_key_passphrase 测试)。
    #[allow(dead_code)]
    fn key_pass_cfg(key_pass_enc: Option<String>) -> ServerConfig {
        ServerConfig {
            id: "s1".into(),
            name: "n".into(),
            host: "1.2.3.4".into(),
            port: 22,
            username: "root".into(),
            auth: AuthConfig {
                auth_type: AuthType::Key,
                key_path: Some("C:/k".into()),
                password_enc: None,
                key_pass_enc,
            },
            remote_dir: "/opt/app".into(),
            host_key_sha256: None,
            tags: Vec::new(),
        }
    }

    #[test]
    fn test_resolve_key_passphrase_none_when_absent() {
        // 未配置 key_pass_enc(旧版配置/未加密私钥)→ None,不报错
        assert_eq!(resolve_key_passphrase(&key_pass_cfg(None)).unwrap(), None);
        // 空串按未配置处理
        assert_eq!(resolve_key_passphrase(&key_pass_cfg(Some(String::new()))).unwrap(), None);
    }

    #[cfg(windows)]
    #[test]
    fn test_resolve_key_passphrase_dpapi_roundtrip() {
        let enc = dpapi_protect("key-pass-123").unwrap();
        assert_eq!(
            resolve_key_passphrase(&key_pass_cfg(Some(enc))).unwrap(),
            Some("key-pass-123".to_string())
        );
    }

    #[test]
    fn test_resolve_key_passphrase_bad_enc() {
        // 密文无效(base64 非法)→ 报错
        assert!(resolve_key_passphrase(&key_pass_cfg(Some("不是base64!!".into()))).is_err());
    }

    // ===== import_compose =====

    #[test]
    fn test_import_compose_copies_parse_and_saves() {
        // DD_CONFIG_DIR 是进程级环境变量,与 config 层测试共用锁串行执行
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 源 compose(文件名故意不是 docker-compose.yml)+ 同目录 .env
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let source = src_dir.join("my-stack.yaml");
        std::fs::write(
            &source,
            "name: demo\nservices:\n  web:\n    build: ./web\n    image: ${IMAGE}:v1\n  db:\n    image: postgres:16\n",
        )
        .unwrap();
        std::fs::write(src_dir.join(".env"), "IMAGE=myapp\n").unwrap();

        let project =
            import_compose(source.to_string_lossy().to_string(), "测试栈".into()).unwrap();

        // 副本位于 config/stacks/<uuid>/docker-compose.yml,内容与源一致
        let copy = PathBuf::from(&project.compose_file);
        assert!(copy.is_file(), "compose 副本应存在: {}", project.compose_file);
        assert_eq!(copy.file_name().unwrap().to_string_lossy(), "docker-compose.yml");
        let stacks_dir = dir.join("config").join("stacks");
        assert_eq!(
            copy.parent().unwrap().parent().unwrap(),
            stacks_dir.as_path(),
            "副本应在 config/stacks/<uuid>/ 下"
        );
        assert_eq!(
            std::fs::read_to_string(&copy).unwrap(),
            std::fs::read_to_string(&source).unwrap(),
            "副本内容应与源一致(原样复制,不做插值)"
        );
        // 同目录 .env 一并复制
        assert!(copy.parent().unwrap().join(".env").is_file(), ".env 副本应存在");
        // origin.json 记录导入来源的原始父目录名(供副本路径解析默认镜像名兜底)
        let origin: crate::stack::StackOrigin = serde_json::from_str(
            &std::fs::read_to_string(copy.parent().unwrap().join("origin.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(origin.dir_name, "src", "origin.json 应记录源的父目录名");

        // service_overrides 取解析默认:web=Local(build),db=Pull(仅 image)
        assert_eq!(project.service_overrides.len(), 2);
        let web = project.service_overrides.iter().find(|o| o.service == "web").unwrap();
        assert_eq!(web.mode, TransferMode::Local);
        let db = project.service_overrides.iter().find(|o| o.service == "db").unwrap();
        assert_eq!(db.mode, TransferMode::Pull);

        // 返回的 compose_file 指向副本而非源路径;配置已保存
        assert_ne!(project.compose_file, source.to_string_lossy().to_string());
        let cfg = load_config().unwrap();
        assert!(cfg.projects.iter().any(|p| p.id == project.id && p.name == "测试栈"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_import_compose_missing_file() {
        let err = import_compose("Z:/definitely/not/compose.yml".into(), "x".into()).unwrap_err();
        assert!(err.contains("不存在"), "实际: {}", err);
    }

    #[test]
    fn test_import_compose_invalid_yaml() {
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
        let source = dir.join("bad.yml");
        std::fs::write(&source, "services: [unclosed\n").unwrap();
        let err = import_compose(source.to_string_lossy().to_string(), "x".into()).unwrap_err();
        std::fs::remove_dir_all(&dir).ok();
        // 解析失败不落盘:不应产生 stacks 目录
        assert!(err.contains("YAML"), "实际: {}", err);
        assert!(!dir.join("config").join("stacks").exists(), "解析失败不应创建栈目录");
    }

    #[test]
    fn test_import_compose_copies_override_files() {
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 源 compose + 同目录 override(合并后 web 的 image 以 override 为准)
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let source = src_dir.join("docker-compose.yml");
        std::fs::write(
            &source,
            "name: demo\nservices:\n  web:\n    build: ./web\n    image: myapp:1\n  db:\n    image: postgres:16\n",
        )
        .unwrap();
        std::fs::write(
            src_dir.join("compose.override.yaml"),
            "services:\n  web:\n    image: myapp:2\n",
        )
        .unwrap();
        // 非 override 命名的文件不应被复制
        std::fs::write(src_dir.join("other.yaml"), "services: {}\n").unwrap();

        let project =
            import_compose(source.to_string_lossy().to_string(), "override 栈".into()).unwrap();
        let copy_dir = PathBuf::from(&project.compose_file)
            .parent()
            .unwrap()
            .to_path_buf();

        // override 副本同名落在 stacks/<uuid>/ 下
        assert!(
            copy_dir.join("compose.override.yaml").is_file(),
            "override 副本应存在: {}",
            copy_dir.display()
        );
        assert!(
            !copy_dir.join("other.yaml").exists(),
            "非 override 文件不应被复制"
        );
        // 解析时已合并 override:web 的 image 以 override 为准(build 保留 → Local)
        let web = project.service_overrides.iter().find(|o| o.service == "web").unwrap();
        assert_eq!(web.mode, TransferMode::Local);
        let db = project.service_overrides.iter().find(|o| o.service == "db").unwrap();
        assert_eq!(db.mode, TransferMode::Pull);

        std::fs::remove_dir_all(&dir).ok();
    }

    // ===== 整栈部署:请求反序列化(前端契约,snake_case)=====

    /// 构造服务分类项的便捷函数。
    fn choice(service: &str, image: &str, mode: TransferMode) -> StackServiceChoice {
        StackServiceChoice {
            service: service.into(),
            image: image.into(),
            mode,
        }
    }

    #[test]
    fn test_stack_deploy_request_deserialize() {
        let json = r#"{
            "project_id": "p1",
            "server_id": "s1",
            "services": [
                {"service": "web", "image": "myapp:1", "mode": "Local"},
                {"service": "db", "image": "", "mode": "Pull"}
            ],
            "password_plain": null
        }"#;
        let req: StackDeployRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.project_id, "p1");
        assert_eq!(req.server_id, "s1");
        assert_eq!(req.services.len(), 2);
        assert_eq!(req.services[0].mode, TransferMode::Local);
        assert_eq!(req.services[1].mode, TransferMode::Pull);
        assert_eq!(req.services[1].image, "");
        assert_eq!(req.password_plain, None);
    }

    // ===== 步骤 1:validate_stack_choices =====

    #[test]
    fn test_validate_stack_choices_rejects_empty() {
        let err = validate_stack_choices(&[]).unwrap_err();
        assert!(err.contains("为空"), "实际: {}", err);
    }

    #[test]
    fn test_validate_stack_choices_local_image_required() {
        let services = vec![
            choice("web", "myapp:1", TransferMode::Local),
            choice("db", "   ", TransferMode::Local),
        ];
        let err = validate_stack_choices(&services).unwrap_err();
        assert!(err.contains("db"), "错误应含服务名: {}", err);
        assert!(err.contains("本地传输"), "实际: {}", err);
    }

    #[test]
    fn test_validate_stack_choices_ok_allows_empty_pull_image() {
        // Pull 类镜像为空合法(服务器自行拉取)
        let services = vec![
            choice("web", "myapp:1", TransferMode::Local),
            choice("db", "", TransferMode::Pull),
        ];
        validate_stack_choices(&services).unwrap();
    }

    // ===== 步骤 2:group_by_mode / sum_sizes =====

    #[test]
    fn test_group_by_mode() {
        let services = vec![
            choice("a", "a:1", TransferMode::Local),
            choice("b", "b:1", TransferMode::Pull),
            choice("c", "c:1", TransferMode::Local),
        ];
        let (local, pull) = group_by_mode(&services);
        let local_names: Vec<&str> = local.iter().map(|s| s.service.as_str()).collect();
        let pull_names: Vec<&str> = pull.iter().map(|s| s.service.as_str()).collect();
        assert_eq!(local_names, vec!["a", "c"]);
        assert_eq!(pull_names, vec!["b"]);
    }

    #[test]
    fn test_group_by_mode_all_pull() {
        // 全 Pull:local 为空,打包/装载/上传镜像包均跳过
        let services = vec![choice("db", "", TransferMode::Pull)];
        let (local, pull) = group_by_mode(&services);
        assert!(local.is_empty());
        assert_eq!(pull.len(), 1);
    }

    #[test]
    fn test_stack_record_images() {
        // 仅登记本地传输且镜像非空的服务,按服务顺序
        let services = vec![
            choice("web", "myapp:1", TransferMode::Local),
            choice("db", "", TransferMode::Pull),
            choice("cache", "redis:7", TransferMode::Local),
            choice("worker", "   ", TransferMode::Local),
        ];
        assert_eq!(
            stack_record_images(&services),
            vec!["myapp:1".to_string(), "redis:7".to_string()]
        );
        // 全 Pull → 空列表
        assert!(stack_record_images(&[choice("db", "", TransferMode::Pull)]).is_empty());
    }

    #[test]
    fn test_sum_sizes() {
        assert_eq!(sum_sizes(&[]), Some(0));
        assert_eq!(sum_sizes(&[Some(1), Some(2)]), Some(3));
        // 任一项未知 → 整体未知(跳过磁盘预检)
        assert_eq!(sum_sizes(&[Some(1), None, Some(2)]), None);
        // 溢出保护
        assert_eq!(sum_sizes(&[Some(u64::MAX), Some(1)]), None);
    }

    // ===== 步骤 3/4:releases 路径拼装 =====

    #[test]
    fn test_releases_dir() {
        assert_eq!(
            releases_dir("/opt/app", "20260829-101010"),
            "/opt/app/releases/20260829-101010"
        );
        // remote_dir 尾部斜杠被吸收
        assert_eq!(
            releases_dir("/opt/app/", "20260829-101010"),
            "/opt/app/releases/20260829-101010"
        );
    }

    #[test]
    fn test_remote_compose_path() {
        assert_eq!(
            remote_compose_path("/opt/app"),
            "/opt/app/docker-compose.yml"
        );
    }

    // ===== 步骤 5:compose_pull_cmd =====

    #[test]
    fn test_compose_pull_cmd() {
        assert_eq!(
            compose_pull_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &[],
                &["web".to_string(), "db".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' pull 'web' 'db'"
        );
        // 有 override:按检测顺序追加 -f(compose 后者覆盖前者)
        assert_eq!(
            compose_pull_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &["compose.override.yaml".to_string(), "docker-compose.override.yml".to_string()],
                &["web".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' -f 'compose.override.yaml' -f 'docker-compose.override.yml' pull 'web'"
        );
    }

    #[test]
    fn test_compose_pull_cmd_quotes_block_injection() {
        // 服务名内嵌单引号被 '\'' 转义,无法逃出引号注入额外命令
        assert_eq!(
            compose_pull_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &[],
                &["a'; rm -rf /".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' pull 'a'\\''; rm -rf /'"
        );
        // override 文件名同样转义
        assert_eq!(
            compose_pull_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &["o'.yaml".to_string()],
                &["web".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' -f 'o'\\''.yaml' pull 'web'"
        );
    }

    // ===== 步骤 6:compose_up_cmd =====

    #[test]
    fn test_compose_up_cmd() {
        assert_eq!(
            compose_up_cmd("/opt/app", "/opt/app/docker-compose.yml", &[]),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' up -d"
        );
        assert_eq!(
            compose_up_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &["compose.override.yaml".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' -f 'compose.override.yaml' up -d"
        );
    }

    // ===== Task 5:augment_pull_error 私有仓库认证提示 =====

    #[test]
    fn test_augment_pull_error_auth_variants() {
        for fragment in [
            "unauthorized: authentication required",
            "HTTP 401 Unauthorized",
            "denied: requested access to the resource is denied",
            "_ERROR: PERMISSION DENIED_",
        ] {
            let err = format!("远端命令执行失败(退出码 1): pull ({})", fragment);
            let msg = augment_pull_error(&err);
            assert!(
                msg.contains("检测到私有仓库认证问题,请先在服务器上 docker login 对应 registry"),
                "应追加登录提示: {}", msg
            );
            assert!(msg.starts_with(&err), "原错误应保留在前: {}", msg);
        }
    }

    #[test]
    fn test_augment_pull_error_other_failure_unchanged() {
        let err = "远端命令执行失败(退出码 1): pull (no such host)";
        assert_eq!(augment_pull_error(err), err);
        assert_eq!(augment_pull_error(""), "");
    }

    // ===== 收尾:cleanup_releases_cmd =====

    #[test]
    fn test_cleanup_releases_cmd() {
        // keep = 5(历史默认):tail -n +6 起删
        assert_eq!(
            cleanup_releases_cmd("/opt/app", 5),
            "ls -1dt '/opt/app/releases'/*/ | tail -n +6 | xargs -r rm -rf"
        );
        // 项目自定义保留数:tail 起点 = keep + 1
        assert_eq!(
            cleanup_releases_cmd("/opt/app", 2),
            "ls -1dt '/opt/app/releases'/*/ | tail -n +3 | xargs -r rm -rf"
        );
        // keep = 0:删除全部历史归档(tail -n +1)
        assert_eq!(
            cleanup_releases_cmd("/opt/app", 0),
            "ls -1dt '/opt/app/releases'/*/ | tail -n +1 | xargs -r rm -rf"
        );
        // 上限值不产生异常(tail 起点 = 51)
        assert_eq!(
            cleanup_releases_cmd("/opt/app", 50),
            "ls -1dt '/opt/app/releases'/*/ | tail -n +51 | xargs -r rm -rf"
        );
    }

    #[test]
    fn test_cleanup_releases_cmd_escapes_quote() {
        assert_eq!(
            cleanup_releases_cmd("/op't", 5),
            "ls -1dt '/op'\\''t/releases'/*/ | tail -n +6 | xargs -r rm -rf"
        );
    }

    #[test]
    fn test_release_keep_of_fallback_and_clamp() {
        let mk = |keep: Option<u32>| ProjectConfig {
            id: "p".into(),
            name: "n".into(),
            image_filter: String::new(),
            compose_file: "c.yml".into(),
            file_mappings: Vec::new(),
            service_overrides: Vec::new(),
            health_wait_secs: 0,
            pre_deploy_cmd: None,
            post_deploy_cmd: None,
            notify_webhook: None,
            source_compose_path: None,
            source_hash: None,
            remote_dir: None,
            default_server_id: None,
            release_keep: keep,
        };
        // 未配置(旧配置)→ 默认 5(历史行为不变)
        assert_eq!(crate::config::release_keep_of(&mk(None)), 5);
        // 显式值被尊重,含边界 0
        assert_eq!(crate::config::release_keep_of(&mk(Some(0))), 0);
        assert_eq!(crate::config::release_keep_of(&mk(Some(1))), 1);
        assert_eq!(crate::config::release_keep_of(&mk(Some(20))), 20);
        // 超上限被夹到 50(防手改配置写入过大值导致 tail 参数异常)
        assert_eq!(crate::config::release_keep_of(&mk(Some(999))), 50);
    }

    // ===== 单镜像步骤 5.2:docker_tag_cmd =====

    #[test]
    fn test_docker_tag_cmd() {
        assert_eq!(
            docker_tag_cmd("myapp:20260829-143000", "myapp:latest"),
            "docker tag 'myapp:20260829-143000' 'myapp:latest'"
        );
    }

    #[test]
    fn test_docker_tag_cmd_escapes_quote() {
        assert_eq!(
            docker_tag_cmd("my'app:20260829", "my'app:latest"),
            "docker tag 'my'\\''app:20260829' 'my'\\''app:latest'"
        );
    }

    // ===== Task 2:远端磁盘预检命令拼装 =====

    #[test]
    fn test_docker_root_cmd() {
        assert_eq!(docker_root_cmd(), "docker info -f '{{.DockerRootDir}}'");
    }

    #[test]
    fn test_df_free_gb_cmd() {
        assert_eq!(
            df_free_gb_cmd("/var/lib/docker"),
            "df -k '/var/lib/docker' | tail -1 | awk '{print $4}'"
        );
    }

    #[test]
    fn test_df_free_gb_cmd_escapes_quote() {
        // 路径内嵌单引号被 '\'' 转义,无法逃出引号注入额外命令
        assert_eq!(
            df_free_gb_cmd("/var/li'b"),
            "df -k '/var/li'\\''b' | tail -1 | awk '{print $4}'"
        );
    }

    #[test]
    fn test_parse_df_gb() {
        // 新口径 df -k(1K 块数)→ GB:30G = 31457280 KB
        assert_eq!(parse_df_gb("31457280\n"), Some(30.0));
        assert_eq!(parse_df_gb("  1048576 "), Some(1.0)); // 1G
        assert_eq!(parse_df_gb("524288"), Some(0.5)); // 0.5G
        // 兼容旧口径带 G 后缀(去后缀按 GB 直读)
        assert_eq!(parse_df_gb("30G\n"), Some(30.0));
        // 空输出 / 非数字 → None,调用方跳过预检
        assert_eq!(parse_df_gb(""), None);
        assert_eq!(parse_df_gb("   \n"), None);
        assert_eq!(parse_df_gb("N/A"), None);
    }

    // ===== Task 2:precheck_remote_disk 判定(Ok / None 跳过 / 不足)=====

    #[test]
    fn test_precheck_remote_disk_ok() {
        assert!(precheck_remote_disk(Some(20.0), 15 * 1024 * 1024 * 1024).is_ok());
        // 恰好等于需求(边界)也通过
        assert!(precheck_remote_disk(Some(15.0), 15 * 1024 * 1024 * 1024).is_ok());
        // 需求为 0(如全 Pull)恒通过
        assert!(precheck_remote_disk(Some(0.0), 0).is_ok());
    }

    #[test]
    fn test_precheck_remote_disk_none_skips() {
        // 无法获取剩余空间 → 跳过预检(告警由调用方负责)
        assert!(precheck_remote_disk(None, u64::MAX).is_ok());
    }

    #[test]
    fn test_precheck_remote_disk_insufficient() {
        let err = precheck_remote_disk(Some(10.0), 15 * 1024 * 1024 * 1024).unwrap_err();
        assert!(err.contains("磁盘剩余空间不足"), "实际: {}", err);
        assert!(err.contains("15.0"), "错误应含所需 GB: {}", err);
        assert!(err.contains("10.0"), "错误应含实际 GB: {}", err);
        assert!(err.contains("清理服务器磁盘"), "实际: {}", err);
    }

    // ===== Task 2:prune_cmd =====

    #[test]
    fn test_prune_cmd() {
        assert_eq!(
            prune_cmd(),
            "docker image prune -f; docker container prune -f"
        );
    }

    // ===== Task 4 修复轮:镜像包上传失败后同路径重试一次 =====

    #[test]
    fn test_upload_retry_failure_msg_keeps_both_errors() {
        // 重试失败时报错应同时携带两次失败信息,便于对照断点与失败原因
        let msg = upload_retry_failure_msg("SFTP 写入远端文件失败 (…)", "SSH 连接失败");
        assert!(msg.contains("重试仍失败"), "实际: {}", msg);
        assert!(msg.contains("SFTP 写入远端文件失败"), "应含重试错误: {}", msg);
        assert!(msg.contains("首次失败:SSH 连接失败"), "应含首次错误: {}", msg);
    }

    // ===== Task 3:钩子命令拼装 =====

    #[test]
    fn test_hook_cmd() {
        assert_eq!(
            hook_cmd("/opt/app", "docker image prune -f"),
            "cd '/opt/app' && ( docker image prune -f )"
        );
    }

    #[test]
    fn test_hook_cmd_escapes_remote_dir_quote() {
        // remote_dir 单引号转义;钩子命令是用户配置的可信复合命令,原样拼入
        // (支持 && / ; / 重定向,不整体加引号 —— 非防注入边界,见 hook_cmd 文档)
        assert_eq!(
            hook_cmd("/op't", "a && b; c > /tmp/log"),
            "cd '/op'\\''t' && ( a && b; c > /tmp/log )"
        );
    }

    // ===== Task 3:compose ps / logs 命令拼装 =====

    #[test]
    fn test_compose_ps_json_cmd() {
        assert_eq!(
            compose_ps_json_cmd("/opt/app", "/opt/app/docker-compose.yml", &[]),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' ps --all --format json"
        );
        // override 与 pull/up 同序追加 -f:override-only 服务也进入健康判定
        assert_eq!(
            compose_ps_json_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &["compose.override.yaml".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' -f 'compose.override.yaml' ps --all --format json"
        );
    }

    #[test]
    fn test_compose_ps_json_cmd_escapes_quote() {
        // 路径内嵌单引号被 '\'' 转义,无法逃出引号注入额外命令
        assert_eq!(
            compose_ps_json_cmd("/opt/app", "/op't.yml", &[]),
            "cd '/opt/app' && docker compose -f '/op'\\''t.yml' ps --all --format json"
        );
    }

    #[test]
    fn test_compose_logs_cmd() {
        assert_eq!(
            compose_logs_cmd("/opt/app", "./docker-compose.yml", &[]),
            "cd '/opt/app' && docker compose -f './docker-compose.yml' logs --tail 50"
        );
        assert_eq!(
            compose_logs_cmd(
                "/opt/app",
                "./docker-compose.yml",
                &["compose.override.yml".to_string()]
            ),
            "cd '/opt/app' && docker compose -f './docker-compose.yml' -f 'compose.override.yml' logs --tail 50"
        );
    }

    // ===== Task 3:health_verdict 判定 =====

    /// 构造一行 `compose ps --format json` 输出
    /// (health/exit_code 传 None 表示不带该字段)。
    fn ps_line(service: &str, state: &str, health: Option<&str>, exit_code: Option<i64>) -> String {
        let mut json = format!(r#"{{"Service":"{}","State":"{}""#, service, state);
        if let Some(h) = health {
            json.push_str(&format!(r#","Health":"{}""#, h));
        }
        if let Some(c) = exit_code {
            json.push_str(&format!(r#","ExitCode":{}"#, c));
        }
        json.push('}');
        json
    }

    /// String 行列表转 `&str` 切片(临时 String 需先绑定再借用)。
    fn as_lines(raw: &[String]) -> Vec<&str> {
        raw.iter().map(String::as_str).collect()
    }

    #[test]
    fn test_health_verdict_all_running_pass() {
        let raw = vec![
            ps_line("web", "running", None, None),
            ps_line("db", "running", Some(""), None),
            ps_line("cache", "running", Some("healthy"), Some(0)),
        ];
        assert_eq!(health_verdict(&as_lines(&raw)), HealthVerdict::Pass);
    }

    #[test]
    fn test_health_verdict_exited_fails_fast() {
        // exited 且无 ExitCode 字段(版本差异)→ 保守按失败(宁误报不漏报)
        let raw = vec![
            ps_line("web", "running", None, None),
            ps_line("db", "exited", None, None),
        ];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Unhealthy {
                service: "db".to_string(),
                state: "exited".to_string()
            }
        );
    }

    #[test]
    fn test_health_verdict_exited_nonzero_exit_code_unhealthy() {
        // exited 且 ExitCode≠0 → 立即失败,状态注明"非零退出"
        let raw = vec![
            ps_line("web", "running", None, None),
            ps_line("db", "exited", None, Some(1)),
        ];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Unhealthy {
                service: "db".to_string(),
                state: "exited(非零退出)".to_string()
            }
        );
    }

    #[test]
    fn test_health_verdict_exited_zero_exit_code_pending() {
        // exited 且 ExitCode==0(一次性服务正常退出)→ 不算失败,继续轮询,
        // pending 展示"已退出(退出码 0)"并置 exited_zero(预算耗尽时报错
        // 据此提示关闭健康检查)
        let raw = vec![
            ps_line("web", "running", None, None),
            ps_line("job", "exited", None, Some(0)),
        ];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Indeterminate {
                pending: Some(("job".to_string(), "已退出(退出码 0)".to_string())),
                exited_zero: true
            }
        );
    }

    #[test]
    fn test_health_verdict_exited_missing_exit_code_unhealthy() {
        // 仅 exited 容器且无 ExitCode 字段 → 保守按失败
        let raw = vec![ps_line("db", "exited", Some(""), None)];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Unhealthy {
                service: "db".to_string(),
                state: "exited".to_string()
            }
        );
    }

    #[test]
    fn test_health_verdict_restarting_and_dead_fail_fast() {
        for state in ["restarting", "dead"] {
            let raw = vec![ps_line("db", state, None, None)];
            assert_eq!(
                health_verdict(&as_lines(&raw)),
                HealthVerdict::Unhealthy {
                    service: "db".to_string(),
                    state: state.to_string()
                }
            );
        }
    }

    #[test]
    fn test_health_verdict_blank_or_garbage_indeterminate() {
        // 空行 / 全空白 / 非 JSON 输出(旧版 compose、警告行等)→ 无法判定,继续轮询
        assert_eq!(
            health_verdict(&[""]),
            HealthVerdict::Indeterminate { pending: None, exited_zero: false }
        );
        assert_eq!(
            health_verdict(&["   "]),
            HealthVerdict::Indeterminate { pending: None, exited_zero: false }
        );
        assert_eq!(
            health_verdict(&[r#"time="2026-08-30" level=warning msg="x""#]),
            HealthVerdict::Indeterminate { pending: None, exited_zero: false }
        );
    }

    #[test]
    fn test_health_verdict_health_three_states() {
        // 无 Health 字段(无 healthcheck)→ Pass
        let raw = vec![ps_line("web", "running", None, None)];
        assert_eq!(health_verdict(&as_lines(&raw)), HealthVerdict::Pass);
        // Health="healthy" → Pass
        let raw = vec![ps_line("web", "running", Some("healthy"), None)];
        assert_eq!(health_verdict(&as_lines(&raw)), HealthVerdict::Pass);
        // Health="starting" → 尚未就绪,继续轮询(pending 展示 Health)
        let raw = vec![ps_line("web", "running", Some("starting"), None)];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Indeterminate {
                pending: Some(("web".to_string(), "starting".to_string())),
                exited_zero: false
            }
        );
        // Health="unhealthy" → 未通过,继续轮询(预算耗尽时报错展示该状态)
        let raw = vec![ps_line("web", "running", Some("unhealthy"), None)];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Indeterminate {
                pending: Some(("web".to_string(), "unhealthy".to_string())),
                exited_zero: false
            }
        );
    }

    #[test]
    fn test_health_verdict_not_running_pending() {
        // 非终态且非 running(created/paused)→ 继续轮询,带出服务与容器状态
        let raw = vec![ps_line("web", "created", None, None)];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Indeterminate {
                pending: Some(("web".to_string(), "created".to_string())),
                exited_zero: false
            }
        );
    }

    #[test]
    fn test_health_verdict_json_array_format() {
        // 旧版 compose 一次性输出 JSON 数组 → 同样可解析
        let lines = vec![
            r#"[{"Service":"web","State":"running"},{"Service":"db","State":"running","Health":"healthy"}]"#,
        ];
        assert_eq!(health_verdict(&lines), HealthVerdict::Pass);
    }

    #[test]
    fn test_health_verdict_falls_back_to_name_field() {
        // 缺 Service 字段时回退容器 Name
        let lines = vec![r#"{"Name":"app-db-1","State":"exited"}"#];
        assert_eq!(
            health_verdict(&lines),
            HealthVerdict::Unhealthy {
                service: "app-db-1".to_string(),
                state: "exited".to_string()
            }
        );
    }

    #[test]
    fn test_health_verdict_health_object_status() {
        // Health 为嵌套对象时取其 Status 字段
        let lines = vec![r#"{"Service":"web","State":"running","Health":{"Status":"healthy"}}"#];
        assert_eq!(health_verdict(&lines), HealthVerdict::Pass);
    }

    // ===== Task 6:classify_change 部署变更分类(六例)=====

    #[test]
    fn test_classify_change_pull_mode_always_pull() {
        // 例 1:Pull 类一律 "Pull",不看本地/远端状态
        assert_eq!(
            classify_change(
                &TransferMode::Pull,
                true,
                Some("sha256:a"),
                Some("sha256:a"),
                Some("old:1")
            ),
            "Pull"
        );
        assert_eq!(
            classify_change(&TransferMode::Pull, false, None, None, None),
            "Pull"
        );
    }

    #[test]
    fn test_classify_change_local_image_missing_absent() {
        // 例 2:Local 且本地不存在该 repo:tag → "Absent"
        assert_eq!(
            classify_change(&TransferMode::Local, false, Some("sha256:a"), None, None),
            "Absent"
        );
        assert_eq!(
            classify_change(&TransferMode::Local, false, None, None, Some("old:1")),
            "Absent"
        );
    }

    #[test]
    fn test_classify_change_remote_missing_create() {
        // 例 3:远端无该镜像、也无现存容器 → 全新创建
        assert_eq!(
            classify_change(&TransferMode::Local, true, None, Some("sha256:abc"), None),
            "Create"
        );
    }

    #[test]
    fn test_classify_change_remote_missing_with_container_recreate() {
        // 例 4:远端无该镜像但有现存容器(旧版在跑)→ 重建
        assert_eq!(
            classify_change(
                &TransferMode::Local,
                true,
                None,
                Some("sha256:abc"),
                Some("myapp:old")
            ),
            "Recreate"
        );
    }

    #[test]
    fn test_classify_change_same_id_unchanged() {
        // 例 5:远端镜像 ID 与本地一致(容忍 sha256: 前缀与大小写差异)→ 不变
        assert_eq!(
            classify_change(
                &TransferMode::Local,
                true,
                Some("sha256:ABC123"),
                Some("abc123"),
                Some("myapp:1")
            ),
            "Unchanged"
        );
    }

    #[test]
    fn test_classify_change_different_id_recreate() {
        // 例 6:ID 不同 → 镜像已更新,重建
        assert_eq!(
            classify_change(
                &TransferMode::Local,
                true,
                Some("sha256:aaa"),
                Some("sha256:bbb"),
                Some("myapp:1")
            ),
            "Recreate"
        );
    }

    // ===== 智能传输:same_image_id 完整 ID 口径 =====

    #[test]
    fn test_same_image_id_full_64_hex_equal() {
        // 远端 --no-trunc 输出(sha256: 前缀 + 完整 64 位)vs 本地
        // image_id_by_ref 输出(剥前缀后的完整 64 位)→ 相等(跳过判定的主口径)
        let full = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(same_image_id(
            &format!("sha256:{}", full),
            full
        ));
        assert!(same_image_id(
            &format!("sha256:{}", full),
            &format!("sha256:{}", full)
        ));
    }

    #[test]
    fn test_same_image_id_case_and_prefix_mixed() {
        // 大小写差异、sha256: 前缀有无混合,均视为相等
        assert!(same_image_id("sha256:ABCDEF123456", "abcdef123456"));
        assert!(same_image_id("ABCDEF123456", "sha256:abcdef123456"));
        assert!(same_image_id("  sha256:abc123  ", "ABC123"));
    }

    #[test]
    fn test_same_image_id_truncated_vs_full_not_equal() {
        // 回归:12 位截断 ID(旧 REMOTE_IMAGES_CMD 口径)与完整 64 位 ID
        // 不相等 —— 跳过判定数据源必须用 REMOTE_IMAGES_CMD_FULL
        let full = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(!same_image_id("0123456789abcdef", full));
    }

    #[test]
    fn test_same_image_id_empty_not_equal() {
        // 任一为空视为不等(避免空串误判"未变化")
        assert!(!same_image_id("", "abc"));
        assert!(!same_image_id("sha256:", "abc"));
        assert!(!same_image_id("", ""));
    }

    // ===== 镜像 ID 查询命令与解析(迁移路径取 ID 的专用口径)=====

    #[test]
    fn test_docker_inspect_id_cmd_has_format_and_quotes() {
        // 回归:迁移路径曾用不带 --format 的 docker_inspect_cmd 取 ID,
        // 输出是 JSON 数组 → 解析恒失败 → 每个镜像都误报「源服务器上不存在」。
        let cmd = docker_inspect_id_cmd("myapp:latest");
        assert!(cmd.contains("--format"), "必须带 --format 才能取到 ID 单值");
        assert!(cmd.contains("{{.Id}}"), "格式串须为 {{{{.Id}}}}");
        assert!(cmd.contains("'myapp:latest'"), "引用须单引号包裹");
    }

    #[test]
    fn test_docker_inspect_id_cmd_quotes_escaped_for_injection() {
        // 引用含单引号时按 '\'' 转义,不产生注入面
        let cmd = docker_inspect_id_cmd("a'b");
        assert!(!cmd.contains("'a'b'"), "未转义的原样拼接会破坏引号");
        assert!(cmd.contains(r"'\''"), "单引号应转义为 '\\''");
    }

    #[test]
    fn test_parse_inspect_id_strips_prefix_and_trim() {
        let full = "sha256:0123456789abcdef";
        assert_eq!(parse_inspect_id(&format!("{}\n", full)), Some("0123456789abcdef".into()));
        assert_eq!(parse_inspect_id("sha256:abc"), Some("abc".into()));
        // 已无前缀原样返回(兼容不同 docker 版本输出)
        assert_eq!(parse_inspect_id("abc123"), Some("abc123".into()));
    }

    #[test]
    fn test_parse_inspect_id_rejects_json_shape_and_empty() {
        // 误用无 --format 的 inspect(JSON 数组)时必须返回 None,
        // 而不是把 "[\n    {\n        \"Id\": \"sha256:abcdef\",\n        \"RepoTags\": null\n    }\n]" 当镜像 ID 参与比较
        let json_output = "[\n    {\n        \"Id\": \"sha256:abcdef\",\n        \"RepoTags\": null\n    }\n]";
        assert_eq!(parse_inspect_id(json_output), None);
        assert_eq!(parse_inspect_id("{\"Id\": \"sha256:abc\"}"), None);
        assert_eq!(parse_inspect_id(""), None);
        assert_eq!(parse_inspect_id("   \n  "), None);
    }

    // ===== 项目级远程目录检测/创建(第六批)=====

    #[test]
    fn test_test_dir_cmd_quotes_and_escapes() {
        // 目录存在性检查命令:`test -d '<path>'`,路径单引号包裹防注入
        let cmd = test_dir_cmd("/home/henghao/site");
        assert_eq!(cmd, "test -d '/home/henghao/site'");
        // 含单引号与 shell 元字符时经 '\'' 转义,不留注入面
        let evil = test_dir_cmd("/tmp/a'; rm -rf /; echo '");
        assert!(!evil.contains("a'; rm"), "未转义的引号会闭合字符串: {}", evil);
        assert!(evil.contains(r"'\''"), "应转义为 '\\'': {}", evil);
        let dollar = test_dir_cmd("/tmp/x$(whoami)`id`");
        assert!(dollar.contains("'/tmp/x$(whoami)`id`'"), "{}", dollar);
    }

    #[test]
    fn test_mkdir_p_cmd_quotes_and_escapes() {
        // 创建目录命令:`mkdir -p '<path>'`
        let cmd = mkdir_p_cmd("/home/henghao/site");
        assert_eq!(cmd, "mkdir -p '/home/henghao/site'");
        let evil = mkdir_p_cmd("/tmp/a'; rm -rf /; echo '");
        assert!(!evil.contains("a'; rm"), "{}", evil);
        assert!(evil.contains(r"'\''"), "{}", evil);
    }


    // ===== Task 6:webhook 载荷序列化 =====

    #[test]
    fn test_webhook_payload_fields() {
        let record = DeployRecord {
            ts: "2026-08-29 10:00:00".into(),
            mode: MODE_STACK.into(),
            server_name: "生产服务器".into(),
            project_name: "博客".into(),
            images: vec!["web:1".into()],
            success: true,
            message: "部署完成".into(),
            duration_secs: 42,
            release_dir: None,
            server_id: None,
            project_id: None,
        };
        let v: serde_json::Value = serde_json::from_str(&webhook_payload(&record)).unwrap();
        assert_eq!(v["event"], "deploy");
        assert_eq!(v["success"], true);
        assert_eq!(v["message"], "部署完成");
        assert_eq!(v["server"], "生产服务器");
        assert_eq!(v["project"], "博客");
        assert_eq!(v["duration_secs"], 42);
        assert_eq!(v["ts"], "2026-08-29 10:00:00");

        // 失败记录同样携带完整字段(success=false)
        let mut failed = record.clone();
        failed.success = false;
        failed.message = "部署失败:连接超时".into();
        let v: serde_json::Value = serde_json::from_str(&webhook_payload(&failed)).unwrap();
        assert_eq!(v["success"], false);
        assert_eq!(v["message"], "部署失败:连接超时");
        assert_eq!(v["event"], "deploy");
    }

    // ===== Task 6:远端容器查询命令与解析 =====

    #[test]
    fn test_remote_dir_basename() {
        assert_eq!(remote_dir_basename("/opt/app"), "app");
        assert_eq!(remote_dir_basename("/opt/app/"), "app");
        assert_eq!(remote_dir_basename("app"), "app");
        assert_eq!(remote_dir_basename("/"), "");
        assert_eq!(remote_dir_basename(""), "");
    }

    #[test]
    fn test_compose_containers_cmd() {
        assert_eq!(
            compose_containers_cmd("/opt/app"),
            "docker ps -a --filter 'label=com.docker.compose.project=app' --format '{{json .}}'"
        );
        // 项目名(基名)内嵌单引号被 '\'' 转义,无法逃出引号注入额外命令
        assert_eq!(
            compose_containers_cmd("/op't"),
            "docker ps -a --filter 'label=com.docker.compose.project=op'\\''t' --format '{{json .}}'"
        );
    }

    #[test]
    fn test_parse_container_line() {
        let line = r#"{"Command":"nginx","CreatedAt":"2026-08-30 10:00:00 +0800 CST","ID":"abc123","Image":"myapp:1","Labels":"com.docker.compose.project=demo,com.docker.compose.service=web","Names":"demo-web-1","State":"running"}"#;
        assert_eq!(
            parse_container_line(line),
            Some(("web".to_string(), "myapp:1".to_string()))
        );
        // 无 compose 服务标签(手动 docker run 的容器)→ None
        assert_eq!(parse_container_line(r#"{"Image":"x","Labels":"foo=bar"}"#), None);
        // 非 JSON 行 → None
        assert_eq!(parse_container_line("oops"), None);
    }

    // ===== 修复轮:webhook URL 日志脱敏 =====

    #[test]
    fn test_url_host_for_log_sanitizes_token() {
        // query 内嵌 token(飞书/钉钉机器人地址形态):只留 host
        assert_eq!(
            url_host_for_log("https://open.feishu.cn/open-apis/bot/v2/hook?token=secret123"),
            "open.feishu.cn"
        );
        // userinfo 内嵌凭据:取 @ 之后的 host[:port]
        assert_eq!(
            url_host_for_log("https://user:pass@hook.example.com:8443/x"),
            "hook.example.com:8443"
        );
        // 无 path/query
        assert_eq!(url_host_for_log("http://example.com"), "example.com");
        // 解析不出 host(非 URL / 空 authority)→ 占位符
        assert_eq!(url_host_for_log("not-a-url"), "<unparseable-url>");
        assert_eq!(url_host_for_log("https://"), "<unparseable-url>");
        assert_eq!(url_host_for_log(""), "<unparseable-url>");
    }

    #[test]
    fn test_webhook_error_detail_no_url() {
        // Status 错误:只落状态码,不内嵌 URL(ureq 的 Display 会带响应 URL)
        let resp = ureq::Response::new(404, "Not Found", "body").unwrap();
        let detail = webhook_error_detail(&ureq::Error::Status(404, resp));
        assert_eq!(detail, "HTTP 状态码 404");
        assert!(!detail.contains("http"), "不应包含 URL: {}", detail);
    }

    // ===== 阶段六:部署断点续传(纯函数单测)=====

    /// 构造单镜像断点产物(测试便捷函数)。
    fn single_art() -> SingleResumeArtifacts {
        SingleResumeArtifacts {
            origin_ref: "myapp:latest".into(),
            repository: "myapp".into(),
            use_date_tag: true,
            image_ref: Some("myapp:20260906-101010".into()),
            tar_local: Some("C:\\Temp\\abc.tar.gz".into()),
            tar_name: Some("abc.tar.gz".into()),
            skip_unchanged: false,
        }
    }

    /// 构造整栈断点产物(测试便捷函数)。
    fn stack_art() -> StackResumeArtifacts {
        StackResumeArtifacts {
            services: vec![
                choice("web", "myapp:1", TransferMode::Local),
                choice("db", "", TransferMode::Pull),
            ],
            unchanged: vec![false],
            skip_unchanged: true,
            force_archive: false,
            release_ts: Some("20260906-101010".into()),
            files: vec!["a.tar.gz".into()],
            locals: vec!["C:\\Temp\\a.tar.gz".into()],
            images: vec!["myapp:1".into()],
        }
    }

    #[test]
    fn test_resume_step_label_single_and_stack() {
        // 单镜像 1..5:打标签/导出压缩/上传镜像/同步文件/服务器部署
        assert_eq!(resume_step_label(MODE_SINGLE, 1), "打标签");
        assert_eq!(resume_step_label(MODE_SINGLE, 2), "导出压缩");
        assert_eq!(resume_step_label(MODE_SINGLE, 3), "上传镜像");
        assert_eq!(resume_step_label(MODE_SINGLE, 4), "同步文件");
        assert_eq!(resume_step_label(MODE_SINGLE, 5), "服务器部署");
        // 整栈 1..6:分类确认/打包/上传/装载/拉取/启动
        assert_eq!(resume_step_label(MODE_STACK, 1), "分类确认");
        assert_eq!(resume_step_label(MODE_STACK, 2), "打包");
        assert_eq!(resume_step_label(MODE_STACK, 3), "上传");
        assert_eq!(resume_step_label(MODE_STACK, 4), "装载");
        assert_eq!(resume_step_label(MODE_STACK, 5), "拉取");
        assert_eq!(resume_step_label(MODE_STACK, 6), "启动");
        // 迁移(第二十六批):1..5 阶段级文案
        assert_eq!(resume_step_label(crate::config::MODE_MIGRATE, 1), "卷搬运");
        assert_eq!(resume_step_label(crate::config::MODE_MIGRATE, 2), "镜像搬运");
        assert_eq!(resume_step_label(crate::config::MODE_MIGRATE, 3), "compose 与归档搬运");
        assert_eq!(resume_step_label(crate::config::MODE_MIGRATE, 4), "目标启动");
        assert_eq!(resume_step_label(crate::config::MODE_MIGRATE, 5), "收尾");
        // 越界(成功后即清除断点,理论不可达)与未知模式兜底
        assert_eq!(resume_step_label(MODE_SINGLE, 6), "部署收尾");
        assert_eq!(resume_step_label(MODE_SINGLE, 0), "部署收尾");
        assert_eq!(resume_step_label("rollback", 1), "部署收尾");
    }

    #[test]
    fn test_single_resume_artifacts_serde_roundtrip() {
        // camelCase 序列化(存入 resume-deploy.json 的 artifacts 字段)→ 读回逐字段相等
        let art = single_art();
        let v = serde_json::to_value(&art).unwrap();
        assert_eq!(v["originRef"], "myapp:latest");
        assert_eq!(v["useDateTag"], true);
        assert_eq!(v["tarLocal"], "C:\\Temp\\abc.tar.gz");
        assert_eq!(v["tarName"], "abc.tar.gz");
        let back: SingleResumeArtifacts = serde_json::from_value(v).unwrap();
        assert_eq!(back, art);
    }

    #[test]
    fn test_stack_resume_artifacts_serde_roundtrip() {
        let art = stack_art();
        let v = serde_json::to_value(&art).unwrap();
        assert_eq!(v["releaseTs"], "20260906-101010");
        assert_eq!(v["skipUnchanged"], true);
        assert_eq!(v["forceArchive"], false);
        assert_eq!(v["files"][0], "a.tar.gz");
        assert_eq!(v["images"][0], "myapp:1");
        // services 内嵌 StackServiceChoice(snake_case 契约不变)
        assert_eq!(v["services"][0]["service"], "web");
        assert_eq!(v["services"][0]["mode"], "Local");
        let back: StackResumeArtifacts = serde_json::from_value(v).unwrap();
        assert_eq!(back, art);
    }

    #[test]
    fn test_parse_resume_artifacts_corrupt_is_none() {
        // 非 JSON 对象(数组/字符串)→ None,调用方按断点数据损坏报错
        assert!(parse_single_artifacts(&serde_json::json!([1, 2, 3])).is_none());
        assert!(parse_stack_artifacts(&serde_json::json!("垃圾")).is_none());
        // JSON 对象但字段全缺 → serde default 宽松解析为默认值;关键产物缺失
        // 由 resume_context_of 的字段级校验拦截(步骤号 > 产物完成度 → 报错)
        let lenient = parse_single_artifacts(&serde_json::json!({"nope": 1}));
        assert!(lenient.is_some());
        let cp = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_SINGLE),
            mode: MODE_SINGLE.into(),
            step_next: 2,
            ts: String::new(),
            server_id: "s1".into(),
            project_id: "p1".into(),
            server_name: "s".into(),
            project_name: "p".into(),
            artifacts: serde_json::json!({"nope": 1}),
        };
        assert!(resume_context_of(&cp).is_err(), "步骤 2 但缺镜像引用应判损坏");
        // 合法数据 → Some
        assert!(parse_single_artifacts(&serde_json::to_value(single_art()).unwrap()).is_some());
        assert!(parse_stack_artifacts(&serde_json::to_value(stack_art()).unwrap()).is_some());
    }

    #[test]
    fn test_resume_local_tars_by_mode() {
        // 单镜像:tar_local;整栈:locals 全部;未知模式:空
        let single_cp = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_SINGLE),
            mode: MODE_SINGLE.into(),
            step_next: 3,
            ts: "2026-09-06 10:00:00".into(),
            server_id: "s1".into(),
            project_id: "p1".into(),
            server_name: "服务器".into(),
            project_name: "项目".into(),
            artifacts: serde_json::to_value(single_art()).unwrap(),
        };
        let tars = resume_local_tars(&single_cp);
        assert_eq!(tars, vec![PathBuf::from("C:\\Temp\\abc.tar.gz")]);

        let stack_cp = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_STACK),
            mode: MODE_STACK.into(),
            step_next: 4,
            artifacts: serde_json::to_value(stack_art()).unwrap(),
            ..single_cp.clone()
        };
        let tars = resume_local_tars(&stack_cp);
        assert_eq!(tars, vec![PathBuf::from("C:\\Temp\\a.tar.gz")]);

        let unknown = ResumeCheckpoint { mode: "rollback".into(), ..stack_cp.clone() };
        assert!(resume_local_tars(&unknown).is_empty());
        // artifacts 损坏 → 空(清理尽力而为,不让放弃操作失败)
        let corrupt = ResumeCheckpoint {
            artifacts: serde_json::json!("垃圾"),
            ..stack_cp
        };
        assert!(resume_local_tars(&corrupt).is_empty());
    }

    #[test]
    fn test_resume_context_of_validates() {
        let base = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_SINGLE),
            mode: MODE_SINGLE.into(),
            step_next: 3,
            ts: String::new(),
            server_id: "s1".into(),
            project_id: "p1".into(),
            server_name: "s".into(),
            project_name: "p".into(),
            artifacts: serde_json::to_value(single_art()).unwrap(),
        };
        // 合法单镜像断点
        let ctx = resume_context_of(&base).unwrap();
        assert_eq!(ctx.step_next, 3);
        assert_eq!(ctx.key, base.key);
        assert_eq!(ctx.single.tar_name.as_deref(), Some("abc.tar.gz"));

        // 未知模式 → 报错
        let bad_mode = ResumeCheckpoint { mode: "x".into(), ..base.clone() };
        assert!(resume_context_of(&bad_mode).is_err());

        // 步骤号越界 → 报错
        let bad_step = ResumeCheckpoint { step_next: 6, ..base.clone() };
        assert!(resume_context_of(&bad_step).is_err());

        // 步骤 1 未完成(image_ref 缺失)合法;步骤 3 但缺镜像引用 → 报错
        let mut art = single_art();
        art.image_ref = None;
        let step1 = ResumeCheckpoint { step_next: 1, artifacts: serde_json::to_value(&art).unwrap(), ..base.clone() };
        assert!(resume_context_of(&step1).is_ok());
        let step3 = ResumeCheckpoint { step_next: 3, artifacts: serde_json::to_value(&art).unwrap(), ..base.clone() };
        assert!(resume_context_of(&step3).is_err());

        // 整栈:合法 + 产物列表不一致 → 报错
        let stack_base = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_STACK),
            mode: MODE_STACK.into(),
            step_next: 4,
            artifacts: serde_json::to_value(stack_art()).unwrap(),
            ..base
        };
        assert!(resume_context_of(&stack_base).is_ok());
        let mut art = stack_art();
        art.locals.clear();
        let mismatch = ResumeCheckpoint { artifacts: serde_json::to_value(&art).unwrap(), ..stack_base };
        assert!(resume_context_of(&mismatch).is_err());
    }

    #[test]
    fn test_resume_view_of_mapping() {
        let cp = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_STACK),
            mode: MODE_STACK.into(),
            step_next: 3,
            ts: "2026-09-06 10:00:00".into(),
            server_id: "s1".into(),
            project_id: "p1".into(),
            server_name: "生产".into(),
            project_name: "博客".into(),
            artifacts: serde_json::json!({}),
        };
        let view = resume_view_of(&cp);
        assert_eq!(view.key, "s1|p1|stack");
        assert_eq!(view.mode, "stack");
        assert_eq!(view.step_next, 3);
        assert_eq!(view.step_label, "上传");
        assert_eq!(view.ts, "2026-09-06 10:00:00");
        assert_eq!(view.server_name, "生产");
        assert_eq!(view.project_name, "博客");
        // camelCase 序列化(前端契约)
        let v = serde_json::to_value(&view).unwrap();
        assert!(v.get("stepNext").is_some());
        assert!(v.get("stepLabel").is_some());
        assert!(v.get("serverName").is_some());
    }

    #[test]
    fn test_temp_file_guard_drop_semantics() {
        // new:Drop 删除;keep(断点活跃):Drop 保留(由成功收尾/放弃显式清理)
        let p1 = std::env::temp_dir().join(format!("dd-guard-{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&p1, b"x").unwrap();
        {
            let _g = TempFileGuard::new(p1.clone());
        }
        assert!(!p1.exists(), "TempFileGuard::new 应在 Drop 时删除文件");

        let p2 = std::env::temp_dir().join(format!("dd-guard-{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&p2, b"x").unwrap();
        {
            let _g = TempFileGuard::keep(p2.clone());
        }
        assert!(p2.exists(), "TempFileGuard::keep 不应在 Drop 时删除文件");
        std::fs::remove_file(&p2).ok();
    }

    #[test]
    fn test_checkpoint_trim_cleans_dropped_local_tars() {
        // 断点表超上限时被裁掉的条目,其断点期本地 tar 必须一并清理。
        // 不清理会静默泄漏临时盘:那些 tar 既不在断点表里(无法经「放弃断点」
        // 回收),也不属于任何失败台(界面上看不到)。批量部署 N 台各写一条
        // 断点,超过 MAX_CHECKPOINTS 时必然触发。
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 建 MAX_CHECKPOINTS 条,每条带一个真实存在的本地 tar(ts 递增)
        let mut tars = Vec::new();
        for i in 0..crate::config::MAX_CHECKPOINTS {
            let tar = dir.join(format!("keep-{}.tar.gz", i));
            std::fs::write(&tar, b"x").unwrap();
            tars.push(tar.clone());
            let mut art = single_art();
            art.tar_local = Some(tar.to_string_lossy().to_string());
            let cp = ResumeCheckpoint {
                key: checkpoint_key(&format!("s{}", i), "p1", MODE_SINGLE),
                mode: MODE_SINGLE.into(),
                step_next: 3,
                ts: format!("2026-09-06 10:00:{:02}", i),
                server_id: format!("s{}", i),
                project_id: "p1".into(),
                server_name: "s".into(),
                project_name: "p".into(),
                artifacts: serde_json::to_value(&art).unwrap(),
            };
            crate::config::save_checkpoint(&cp).unwrap();
        }

        // 再写一条(超上限):最旧的 s0 被裁掉,其 tar 应被删除
        let overflow_tar = dir.join("overflow.tar.gz");
        std::fs::write(&overflow_tar, b"x").unwrap();
        let mut art = single_art();
        art.tar_local = Some(overflow_tar.to_string_lossy().to_string());
        let cp = ResumeCheckpoint {
            key: checkpoint_key("s99", "p1", MODE_SINGLE),
            mode: MODE_SINGLE.into(),
            step_next: 3,
            ts: "2026-09-06 11:00:00".into(),
            server_id: "s99".into(),
            project_id: "p1".into(),
            server_name: "s".into(),
            project_name: "p".into(),
            artifacts: serde_json::to_value(&art).unwrap(),
        };
        let dropped = crate::config::save_checkpoint(&cp).unwrap();

        // 返回值报告了被裁条目(config 层不解析 artifacts,交调用方清理)
        assert_eq!(dropped.len(), 1, "应只裁掉最旧的 1 条");
        assert_eq!(dropped[0].server_id, "s0");
        // 被裁条目的 tar 由 commands 层口径解析得到
        let dropped_tars = resume_local_tars(&dropped[0]);
        assert_eq!(dropped_tars, vec![tars[0].clone()]);
        // 履行清理(与实际调用点 checkpoint_save 同口径)
        for path in &dropped_tars {
            std::fs::remove_file(path).unwrap();
        }
        assert!(!tars[0].exists(), "被裁断点的本地 tar 应被清理");
        assert!(tars[1].exists(), "未超限的断点 tar 不应被误删");
        assert!(overflow_tar.exists(), "新写入断点的 tar 不应被删");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_checkpoint_cleanup_on_success_removes_tar_and_checkpoint() {
        // 成功收尾:断点删除 + 保留的本地 tar 删除;文件已不存在时静默
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        let key = checkpoint_key("s9", "p9", MODE_SINGLE);
        let cp = ResumeCheckpoint {
            key: key.clone(),
            mode: MODE_SINGLE.into(),
            step_next: 5,
            ts: "2026-09-06 10:00:00".into(),
            server_id: "s9".into(),
            project_id: "p9".into(),
            server_name: "s".into(),
            project_name: "p".into(),
            artifacts: serde_json::json!({}),
        };
        crate::config::save_checkpoint(&cp).unwrap();
        let tar = dir.join("left.tar.gz");
        std::fs::write(&tar, b"x").unwrap();

        checkpoint_cleanup_on_success(&key, &[tar.clone(), dir.join("already-gone.tar.gz")]);
        assert!(crate::config::load_resume_map().get(&key).is_none(), "断点应被清除");
        assert!(!tar.exists(), "保留的本地 tar 应被删除");

        std::fs::remove_dir_all(&dir).ok();
    }

    // ===== save_server_entry(密文 merge;get_config 只读视图配套)=====

    fn server_entry(id: &str, pass: Option<&str>, key_pass: Option<&str>, fp: Option<&str>) -> crate::config::ServerConfig {
        crate::config::ServerConfig {
            id: id.into(),
            name: "n".into(),
            host: "1.2.3.4".into(),
            port: 22,
            username: "root".into(),
            auth: crate::config::AuthConfig {
                auth_type: crate::config::AuthType::Password,
                key_path: None,
                password_enc: pass.map(|s| s.to_string()),
                key_pass_enc: key_pass.map(|s| s.to_string()),
            },
            remote_dir: "/opt/app".into(),
            host_key_sha256: fp.map(|s| s.to_string()),
            tags: Vec::new(),
        }
    }

    /// B3(第二十七批):`save_server_entry` 是服务器配置唯一写入口 ——
    /// 标签在此归一(trim/去空/去重保序/cap),脏输入不得落盘。
    #[test]
    fn test_save_server_entry_normalizes_tags() {
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-tags-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        let mut s = server_entry("s1", None, None, None);
        s.tags = vec![
            "  华东  ".into(),
            "".into(),
            "生产".into(),
            "华东".into(),   // 去重
            "   ".into(),
            "x".repeat(40),  // 超长 → 截断 24 字符
        ];
        save_server_entry(s).unwrap();
        let cfg = load_config().unwrap();
        assert_eq!(
            cfg.servers[0].tags,
            vec!["华东".to_string(), "生产".to_string(), "x".repeat(24)]
        );

        std::env::remove_var("DD_CONFIG_DIR");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_save_server_entry_merges_ciphertext() {
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-ssentry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 新增:密文/指纹以入参为准
        save_server_entry(server_entry("s1", Some("ENC-P"), Some("ENC-K"), Some("SHA256:fp"))).unwrap();
        let cfg = load_config().unwrap();
        assert_eq!(cfg.servers[0].auth.password_enc.as_deref(), Some("ENC-P"));
        assert_eq!(cfg.servers[0].auth.key_pass_enc.as_deref(), Some("ENC-K"));
        assert_eq!(cfg.servers[0].host_key_sha256.as_deref(), Some("SHA256:fp"));

        // 编辑但密文/指纹传 None(只读视图回写场景)→ merge 保留现有密文与指纹
        let mut edit = server_entry("s1", None, None, None);
        edit.name = "改名".into();
        save_server_entry(edit).unwrap();
        let cfg = load_config().unwrap();
        assert_eq!(cfg.servers[0].name, "改名");
        assert_eq!(cfg.servers[0].auth.password_enc.as_deref(), Some("ENC-P"), "密文应被 merge 保留");
        assert_eq!(cfg.servers[0].auth.key_pass_enc.as_deref(), Some("ENC-K"));
        assert_eq!(cfg.servers[0].host_key_sha256.as_deref(), Some("SHA256:fp"), "指纹应被 merge 保留");

        // 改密码:传入新密文 → 覆盖
        save_server_entry(server_entry("s1", Some("ENC-NEW"), None, None)).unwrap();
        let cfg = load_config().unwrap();
        assert_eq!(cfg.servers[0].auth.password_enc.as_deref(), Some("ENC-NEW"), "新密码应覆盖");
        assert_eq!(cfg.servers[0].auth.key_pass_enc.as_deref(), Some("ENC-K"), "私钥口令仍保留");

        std::fs::remove_dir_all(&dir).ok();
    }

    // ===== 哨兵防护(v6.1.1:防只读视图 "*" 落盘)=====

    #[test]
    fn test_save_config_cmd_restores_sentinel() {
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-sentinel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 磁盘落真实密文
        save_server_entry(server_entry("s1", Some("REAL-B64"), Some("REAL-KEY"), Some("SHA256:fp"))).unwrap();

        // 场景 A:get_config 只读视图(哨兵)整量回写 → 磁盘密文不被冲掉
        let mut view = get_config().unwrap();
        assert_eq!(view.servers[0].auth.password_enc.as_deref(), Some("*"), "视图应为哨兵");
        // 模拟前端改一处(改项目列表)后全量回写
        view.servers[0].name = "改名后".into();
        save_config_cmd(view).unwrap();
        let cfg = load_config().unwrap();
        assert_eq!(cfg.servers[0].name, "改名后");
        assert_eq!(cfg.servers[0].auth.password_enc.as_deref(), Some("REAL-B64"), "哨兵不得落盘,真实密文应保留");
        assert_eq!(cfg.servers[0].auth.key_pass_enc.as_deref(), Some("REAL-KEY"));

        // 场景 B:真实新密文(改密码)→ 正常覆盖
        let mut cfg2 = load_config().unwrap();
        cfg2.servers[0].auth.password_enc = Some("NEW-B64".into());
        save_config_cmd(cfg2).unwrap();
        assert_eq!(load_config().unwrap().servers[0].auth.password_enc.as_deref(), Some("NEW-B64"));

        // 场景 C:save_server_entry 收到哨兵 → 视同未改,沿用现值
        let mut edit = server_entry("s1", Some("*"), Some("*"), None);
        edit.name = "再改名".into();
        save_server_entry(edit).unwrap();
        let cfg3 = load_config().unwrap();
        assert_eq!(cfg3.servers[0].name, "再改名");
        assert_eq!(cfg3.servers[0].auth.password_enc.as_deref(), Some("NEW-B64"), "哨兵不得覆盖真实密文");

        // 场景 D:新增服务器带哨兵 → 落为 None(绝不写 "*")
        save_server_entry(server_entry("s2", Some("*"), None, None)).unwrap();
        let cfg4 = load_config().unwrap();
        let s2 = cfg4.servers.iter().find(|s| s.id == "s2").unwrap();
        assert_eq!(s2.auth.password_enc, None, "新增项的哨兵应落为 None");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_resolve_password_rejects_sentinel() {
        // 哨兵/损坏密文 → 明确可操作报错(而非晦涩的 base64 解码失败)
        let err = resolve_password(&AuthType::Password, None, Some("*")).unwrap_err();
        assert!(err.contains("重新输入登录密码"), "实际: {err}");
        let err2 = resolve_password(&AuthType::Password, None, Some("不是base64!!")).unwrap_err();
        assert!(err2.contains("重新输入登录密码"), "实际: {err2}");
        // 正常 base64 且明文优先时不受影响
        assert_eq!(resolve_password(&AuthType::Password, Some("plain"), Some("*")).unwrap(), Some("plain".to_string()));
    }

    #[test]
    fn test_notify_cipher_masked_and_restored() {
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-nsentinel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 磁盘落真实 SMTP 密文
        let mut cfg = load_config().unwrap();
        cfg.notify.email.password_enc = Some("SMTP-B64".into());
        save_config(&cfg).unwrap();

        // get_config 只读视图:notify 密文也应为哨兵(与 servers 同口径)
        let view = get_config().unwrap();
        assert_eq!(view.notify.email.password_enc.as_deref(), Some("*"), "notify 密文应脱敏");

        // 全量回写:哨兵还原为磁盘现值(不被冲掉)
        save_config_cmd(view).unwrap();
        let after = load_config().unwrap();
        assert_eq!(after.notify.email.password_enc.as_deref(), Some("SMTP-B64"), "notify 哨兵不得落盘");

        // 真实新值可覆盖
        let mut cfg2 = load_config().unwrap();
        cfg2.notify.email.password_enc = Some("SMTP-NEW".into());
        save_config_cmd(cfg2).unwrap();
        assert_eq!(load_config().unwrap().notify.email.password_enc.as_deref(), Some("SMTP-NEW"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_encrypt_password_rejects_sentinel() {
        // 第二十批 P2-5 回归:只读视图哨兵 "*" 不得被当「新密码明文」加密 ——
        // DPAPI("*") 不等于哨兵本身,restore_sentinel 不还原,真实密文会被
        // 静默覆盖(v6.1.1 同类事故的纵深防御)。
        let err = encrypt_password("*".to_string());
        assert!(err.is_err(), "哨兵明文必须被拒绝");
        let msg = err.unwrap_err();
        assert_eq!(
            crate::errors::code_of(&msg),
            Some(crate::errors::ErrCode::Input),
            "错误串应挂 input 码,实际: {}",
            msg
        );
        assert!(crate::errors::strip(&msg).contains("占位符"), "文案应说明占位符问题");

        // 对照:正常明文不受影响(DPAPI 同机往返)
        let enc = encrypt_password("正常密码123".to_string()).unwrap();
        assert!(!enc.is_empty() && enc != "*", "正常明文照常加密");
    }

    // ===== 连接路径凭据纪律守护(v6.3.1)=====

    /// 守护:生产代码中所有 `SshClient::connect` 调用必须传入**解析后的凭据**,
    /// 不得直传 `None, None`。
    ///
    /// 背景(v6.3.1 用户真机报告):`server_diagnose` 曾以 `connect(&server, None,
    /// None, ..)` 直连——加密私钥的服务器在诊断里 `load_secret_key(path, None)`
    /// 报「私钥已加密」,而「测试连接」经 `resolve_key_passphrase` 解析口令后正常,
    /// 症状即「测试连接通过、一键诊断失败」。这是第二条同类漏项(前有 RSA hash),
    /// 故把纪律固化为源码级断言:任何新增连接点直传 None 本测试即红。
    #[test]
    fn test_all_connect_sites_resolve_credentials() {
        let files = [
            ("src/commands/host_server.rs", include_str!("host_server.rs")),
            ("src/commands/deploy.rs", include_str!("deploy.rs")),
            ("src/commands/rollback.rs", include_str!("rollback.rs")),
            ("src/commands/cleanup.rs", include_str!("cleanup.rs")),
            ("src/commands/resume.rs", include_str!("resume.rs")),
            ("src/manage.rs", include_str!("../manage.rs")),
        ];
        let mut bad: Vec<String> = Vec::new();
        for (name, src) in files {
            for (i, line) in src.lines().enumerate() {
                if !line.contains("SshClient::connect(") {
                    continue;
                }
                // 合并续行(调用常跨行:connect(\n &server,\n password...,\n ...))
                let mut window = String::from(line);
                for l in src.lines().skip(i + 1).take(4) {
                    window.push(' ');
                    window.push_str(l);
                    if window.contains("Arc::") {
                        break; // 参数列表到 Arc 参数即完整
                    }
                }
                // 白名单:必须出现已解析凭据变量(as_deref 形式)
                if !window.contains("password.as_deref()") || !window.contains("key_pass.as_deref()") {
                    bad.push(format!("{}:{}", name, i + 1));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "以下 SshClient::connect 调用未传解析后的凭据(password/key_pass 的 as_deref);\
             诊断/部署等连接点必须先经 resolve_password + resolve_key_passphrase,直传 None 会导致:\
             密码服务器缺密码、加密私钥服务器缺口令。违规点: {:?}",
            bad
        );
    }

    // ===== manifest 镜像条目:ID 记录与旧归档兼容(v6.3.2)=====

    #[test]
    fn test_build_manifest_images_records_id() {
        let owned = [
            StackServiceChoice { service: "web".into(), image: "myapp:latest".into(), mode: TransferMode::Local },
            StackServiceChoice { service: "db".into(), image: "postgres:16".into(), mode: TransferMode::Local },
        ];
        let local: Vec<&StackServiceChoice> = owned.iter().collect();
        let ids = vec![Some("sha256:aaa".to_string()), None];
        let files = vec!["web.tar.gz".to_string()];
        let m = build_manifest_images(&local, &[false, true], &files, &ids);
        assert_eq!(m.len(), 2);
        // 打包项:id 记录 + 文件名按顺序消费
        assert_eq!(m[0].service, "web");
        assert_eq!(m[0].tag, "myapp:latest");
        assert_eq!(m[0].file.as_deref(), Some("web.tar.gz"));
        assert_eq!(m[0].id.as_deref(), Some("sha256:aaa"), "ID 应写入 manifest(对比按内容判变化)");
        // 跳过项:file 无、id 无(采集失败回退按 tag 比较)
        assert_eq!(m[1].service, "db");
        assert_eq!(m[1].file, None);
        assert_eq!(m[1].id, None);
    }

    // ===== 回滚可用性预检(第二十九批 R1)=====

    /// 构造 manifest 条目
    fn mimg(service: &str, tag: &str, file: Option<&str>, id: Option<&str>) -> ManifestImage {
        ManifestImage {
            service: service.into(),
            tag: tag.into(),
            file: file.map(String::from),
            id: id.map(String::from),
        }
    }

    #[test]
    fn test_rollback_precheck_archived_package() {
        // 归档内有包(本次变化):回滚用它,docker load 即可 —— 不依赖远端任何东西
        let images = vec![mimg("web", "myapp:20260919", Some("web.tar.gz"), Some("aaa"))];
        let remote: Vec<(String, String)> = vec![]; // 远端空也不影响
        let plan = plan_rollback(&images, &remote);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].source, RollbackImageSource::Archived);
        assert!(!plan[0].blocking, "有包可用不阻断");
    }

    #[test]
    fn test_rollback_precheck_skipped_but_present_by_id() {
        // 智能传输跳过的服务(file=null):manifest 记了 ID,远端按 ID 找得到
        // → 可直接用,**不需要重新上传包**(R1 的核心场景)
        let images = vec![mimg("db", "postgres:16", None, Some("sha256:bbb"))];
        let remote = vec![("postgres:16".to_string(), "sha256:bbb".to_string())];
        let plan = plan_rollback(&images, &remote);
        assert_eq!(plan[0].source, RollbackImageSource::RemoteById);
        assert!(!plan[0].blocking, "远端正有该 ID 镜像 → 可用");
    }

    #[test]
    fn test_rollback_precheck_query_failure_says_retry_not_missing() {
        // **审查发现**:远端镜像列表查询失败时,首版把「查不到」写成
        // 「镜像已不在服务器上(可能被清理)」—— 定性错误会诱导用户做出错误的
        // 恢复决策(重试即可 vs 必须重新部署)。两者都阻断(fail-closed),
        // 但文案必须区分。
        let images = vec![mimg("db", "postgres:16", None, Some("sha256:bbb"))];
        let plan = plan_rollback_full(&images, &[], &[], false);
        assert_eq!(plan[0].source, RollbackImageSource::Unknown, "查询失败归 Unknown");
        assert!(plan[0].blocking, "fail-closed:仍阻断");
        assert!(
            plan[0].detail.contains("无法查询") && plan[0].detail.contains("重试"),
            "应说「查不到,请重试」而非「镜像丢了」: {}",
            plan[0].detail
        );
        assert!(
            !plan[0].detail.contains("已不在服务器上"),
            "不得误报为镜像丢失: {}",
            plan[0].detail
        );
        // 对照:远端列表可得且确实没有该镜像 → 才是「不在服务器上」
        let plan2 = plan_rollback_full(&images, &[], &[], true);
        assert_eq!(plan2[0].source, RollbackImageSource::Missing);
        assert!(plan2[0].detail.contains("不在服务器上"), "{}", plan2[0].detail);
    }

    #[test]
    fn test_rollback_precheck_archived_package_missing_from_dir() {
        // **补丁审查发现的第二处漏洞**:首版只看 manifest 的 `file` 字段是否为
        // Some,就判「归档内有包 → 可用」。但 manifest 是**部署当时**写的记录,
        // 归档目录里的文件可能已被手工删除 / 部分清理(清理分析逐目录删除、
        // 或用户手工 rm)—— 此时 manifest 仍写着有包,而实际没有。
        //
        // 后果与第一处漏洞同类:预检说可用、`docker load` 阶段找不到文件
        // (装载循环按实际文件清单走)**静默跳过**,`up -d` 用当前镜像启动,
        // 界面报「回滚完成」。
        //
        // 正确判定:归档有包 **且** 该包真在目录的文件清单里。
        let images = vec![
            mimg("web", "myapp:1", Some("web.tar.gz"), Some("aaa")),      // 包在
            mimg("db", "postgres:16", Some("db.tar.gz"), Some("bbb")),    // 包**不在**目录里
        ];
        // 目录实际只有 web.tar.gz(db.tar.gz 被清理了)
        let actual_files = vec!["web.tar.gz".to_string(), "manifest.json".to_string()];
        let plan = plan_rollback_with_files(&images, &[], &actual_files);
        assert_eq!(plan[0].source, RollbackImageSource::Archived);
        assert!(!plan[0].blocking, "包确实在 → 可用");
        assert_eq!(
            plan[1].source,
            RollbackImageSource::Missing,
            "manifest 说在但目录里没有 → 不能算可用"
        );
        assert!(plan[1].blocking, "应阻断(否则 load 阶段静默跳过)");
        assert!(
            plan[1].detail.contains("归档") && plan[1].detail.contains("不在"),
            "文案应说明包不在归档里: {}",
            plan[1].detail
        );
    }

    #[test]
    fn test_rollback_precheck_id_present_but_tag_moved_away() {
        // **第二十九批补丁**发现:跳过服务的镜像 ID 仍在服务器上,但 compose
        // 期望的 tag 已被别的 ID 占用时,首版预检只看「ID 在不在」→ 判「可用」。
        // 这是错的:整栈回滚当时**没有 `docker tag` 步骤**,跳过服务没有包,
        // `up -d` 按 tag 找不到归档版本 → 静默沿用了新版本。
        //
        // **v6.12.0 修正**:执行链 up 前新增「按 ID 收敛」步骤(ID 还在就把
        // 标签指回,零拷贝),所以这种状态不再是「回不去」,而是可恢复的
        // 中间态 —— 归 `RemoteByIdTagMoved`(契约 source 串 `tagRestore`),
        // **不再阻断**,但 UI 必须显式展示「将自动指回」而非静默。
        // (真机案例:goodlaser-backend:latest 指向 265b2e14d9a6,归档 ID
        //  c313095267ee 仍挂在其它标签下 —— 旧行为报「回不去」,用户无出路。)
        let images = vec![mimg("db", "postgres:16", None, Some("sha256:bbb"))];
        // 远端:ID bbb 仍存在,但挂在别的 tag 下;postgres:16 已被 ccc 占
        let remote = vec![
            ("postgres:16".to_string(), "sha256:ccc".to_string()),
            ("postgres:16-prev".to_string(), "sha256:bbb".to_string()),
        ];
        let plan = plan_rollback(&images, &remote);
        assert_eq!(
            plan[0].source,
            RollbackImageSource::RemoteByIdTagMoved,
            "ID 在、tag 被占 → 可自动指回的中间态(v6.12.0 起不再判 Missing)"
        );
        assert!(!plan[0].blocking, "可指回 → 不阻断(否则用户真机场景无出路)");
        // 文案要点出「标签已指向别的镜像」+「将自动指回」
        assert!(
            plan[0].detail.contains("指回") && plan[0].detail.contains("其它的标签"),
            "文案应说明标签被占用且将自动指回: {}",
            plan[0].detail
        );
    }

    #[test]
    fn test_rollback_precheck_skipped_but_id_gone() {
        // 跳过且远端已无该 ID(被人手动删/清理过):阻断项,须让用户知道
        // 「这个服务回不去」—— 静默沿用当前镜像正是本批要根除的失效路径
        let images = vec![mimg("db", "postgres:16", None, Some("sha256:bbb"))];
        let remote = vec![("postgres:16".to_string(), "sha256:ccc".to_string())];
        let plan = plan_rollback(&images, &remote);
        assert_eq!(plan[0].source, RollbackImageSource::Missing);
        assert!(plan[0].blocking, "ID 不存在 → 阻断(用户确认后才继续)");
        // 提示要点出「同 tag 已被别的 ID 占用」(即版本已被覆盖)
        assert!(plan[0].detail.contains("覆盖") || plan[0].detail.contains("不一致"),
            "detail 应说明 tag 被占用: {}", plan[0].detail);
    }

    #[test]
    fn test_rollback_precheck_skipped_tag_absent_entirely() {
        // 跳过且远端连同 tag 都没有:同样是阻断项(镜像真的没了)
        let images = vec![mimg("db", "postgres:16", None, Some("sha256:bbb"))];
        let remote: Vec<(String, String)> = vec![];
        let plan = plan_rollback(&images, &remote);
        assert_eq!(plan[0].source, RollbackImageSource::Missing);
        assert!(plan[0].blocking);
    }

    #[test]
    fn test_rollback_precheck_legacy_manifest_without_id() {
        // 旧归档:file=null 且 id=null(本功能引入前的 manifest 没记 ID)
        // → 无从查证,保守标阻断并给出「重新部署一次以记录镜像 ID」的指引
        let images = vec![mimg("db", "postgres:16", None, None)];
        let remote = vec![("postgres:16".to_string(), "sha256:bbb".to_string())];
        let plan = plan_rollback(&images, &remote);
        assert_eq!(plan[0].source, RollbackImageSource::Unknown);
        assert!(plan[0].blocking, "旧归档无法核对,须显式告知");
        assert!(plan[0].detail.contains("重新部署"), "应给可执行指引: {}", plan[0].detail);
    }

    #[test]
    fn test_rollback_precheck_mixed_and_summary() {
        // 混合场景:1 个有包 + 1 个按 ID 命中 + 1 个丢失
        let images = vec![
            mimg("web", "myapp:20260919", Some("web.tar.gz"), Some("aaa")),
            mimg("cache", "redis:7", None, Some("sha256:ddd")),
            mimg("db", "postgres:16", None, Some("sha256:bbb")),
        ];
        let remote = vec![
            ("redis:7".to_string(), "sha256:ddd".to_string()),
            ("postgres:16".to_string(), "sha256:ccc".to_string()),
        ];
        let plan = plan_rollback(&images, &remote);
        assert_eq!(plan.len(), 3);
        assert_eq!(plan.iter().filter(|p| p.blocking).count(), 1, "仅 1 项阻断");
        // 计数汇总(供 UI 文案与阻断判定)
        let s = rollback_precheck_summary(&plan);
        assert_eq!(s.archived, 1);
        assert_eq!(s.remote_by_id, 1);
        assert_eq!(s.missing, 1);
        assert_eq!(s.unknown, 0);
        assert!(s.has_blocking(), "存在阻断项");
        // 全可用时不阻断
        let ok_plan = plan_rollback(&images[..2], &remote);
        assert!(!rollback_precheck_summary(&ok_plan).has_blocking());
    }

    #[test]
    fn test_rollback_precheck_id_matching_is_prefix_tolerant() {
        // ID 比较与既有 same_image_id 同口径:一方带 sha256: 前缀、一方不带也算同
        let images = vec![mimg("db", "postgres:16", None, Some("sha256:bbb"))];
        let remote = vec![("postgres:16".to_string(), "bbb".to_string())];
        let plan = plan_rollback(&images, &remote);
        assert_eq!(plan[0].source, RollbackImageSource::RemoteById,
            "前缀差异不应误判为丢失(与跨端比较同口径)");
    }

    #[test]
    fn test_archived_override_original_filter() {
        // R2:归档内 <名>.ddbak → 原名的识别规则(只认 compose override 形态)
        assert_eq!(
            archived_override_original("docker-compose.override.yml.ddbak"),
            Some("docker-compose.override.yml")
        );
        assert_eq!(
            archived_override_original("compose.prod.yaml.ddbak"),
            Some("compose.prod.yaml")
        );
        // 非 override 形态:不恢复(防误把别的 .ddbak 物写回项目目录)
        assert_eq!(archived_override_original("foo.ddbak"), None);
        assert_eq!(archived_override_original("docker-compose.yml.ddbak"), None,
            "base compose 不是 override(它走 compose 副本通道)");
        assert_eq!(archived_override_original("docker-compose.override.txt.ddbak"), None);
        assert_eq!(archived_override_original("docker-compose.override.yml"), None,
            "没有 .ddbak 后缀的不是归档副本");
    }

    #[test]
    fn test_rollback_precheck_block_message_lists_each_item() {
        // 阻断文案必须逐条列出「回不去」的服务 + 给出两条出路 —— 只报「N 项阻断」
        // 对用户无行动价值(这是本批要根除的「说不清」)
        let images = vec![
            mimg("web", "myapp:20260919", Some("web.tar.gz"), Some("aaa")),
            mimg("db", "postgres:16", None, Some("sha256:bbb")),
            mimg("old", "legacy:1", None, None),
        ];
        let remote = vec![("postgres:16".to_string(), "sha256:ccc".to_string())];
        let plan = plan_rollback(&images, &remote);
        let sum = rollback_precheck_summary(&plan);
        let msg = rollback_precheck_block_message(&plan, &sum);
        assert!(msg.contains("2 个服务无法回退"), "{}", msg);
        assert!(msg.contains("postgres:16"), "应点名缺失服务: {}", msg);
        assert!(msg.contains("legacy:1"), "应点名无法核对的服务: {}", msg);
        assert!(msg.contains("仍要回滚"), "应给出出路: {}", msg);
        // 可用服务不应出现在阻断清单里(避免噪音)
        assert!(!msg.contains("myapp:20260919"), "可用项不该混进阻断清单: {}", msg);
    }

    #[test]
    fn test_rollback_precheck_error_code_roundtrip() {
        // 前端按码分流(项目纪律):阻断错误必须可被 code_of 识别
        let tagged = crate::errors::tagged(crate::errors::ErrCode::RollbackPrecheck, "x");
        assert_eq!(
            crate::errors::code_of(&tagged),
            Some(crate::errors::ErrCode::RollbackPrecheck)
        );
        assert_eq!(crate::errors::ErrCode::RollbackPrecheck.as_str(), "rollback_precheck");
    }

    #[test]
    fn test_rollback_precheck_tag_moved_counts_and_no_block() {
        // 四态计数:tagRestore(可自动指回)单独一列,且**不计入阻断**
        let images = vec![
            mimg("web", "myapp:20260919", Some("web.tar.gz"), Some("aaa")),
            mimg("db", "postgres:16", None, Some("sha256:bbb")),
            mimg("cache", "redis:7", None, Some("sha256:ccc")),
        ];
        let remote = vec![
            ("postgres:16".to_string(), "sha256:bbb".to_string()),
            ("redis:7".to_string(), "sha256:zzz".to_string()),
            ("redis:7-old".to_string(), "sha256:ccc".to_string()),
        ];
        let plan = plan_rollback(&images, &remote);
        let s = rollback_precheck_summary(&plan);
        assert_eq!(s.archived, 1);
        assert_eq!(s.remote_by_id, 1);
        assert_eq!(s.tag_restore, 1, "tag 被占但 ID 在 → 计 tag_restore");
        assert_eq!(s.missing, 0);
        assert!(!s.has_blocking(), "可自动指回的服务不构成阻断");
        // 契约 source 串:前端按串渲染中间态
        assert_eq!(rollback_source_str(RollbackImageSource::RemoteByIdTagMoved), "tagRestore");
    }

    // ===== v6.12.0:按镜像 ID 收敛(plan_tag_convergence)=====

    #[test]
    fn test_tag_convergence_retags_when_tag_moved() {
        // 真机案例的治愈路径:ID 还在、tag 被占 → 出「指回」动作(零拷贝)
        let images = vec![mimg("backend", "goodlaser-backend:latest", None, Some("sha256:c313"))];
        let remote = vec![
            ("goodlaser-backend:latest".to_string(), "sha256:265b".to_string()),
            ("goodlaser-backend:20260901".to_string(), "sha256:c313".to_string()),
        ];
        let c = plan_tag_convergence(&images, &remote);
        assert_eq!(c.retag.len(), 1);
        assert_eq!(c.retag[0].0, "backend", "服务名(日志/历史按它归因)");
        assert_eq!(c.retag[0].1, "sha256:c313", "源 = 归档记录的镜像 ID");
        assert_eq!(c.retag[0].2, "goodlaser-backend:latest", "目标 = compose 期望的 tag");
        assert!(c.missing.is_empty());
        // 生成的命令用 ID 指回标签
        let cmd = docker_tag_cmd(&c.retag[0].1, &c.retag[0].2);
        assert!(cmd.starts_with("docker tag "), "{}", cmd);
        assert!(cmd.contains("c313") && cmd.contains("goodlaser-backend:latest"), "{}", cmd);
    }

    #[test]
    fn test_tag_convergence_noop_when_aligned() {
        // 已经对齐(tag 正指向归档 ID)→ 无动作,不该白白 retag
        let images = vec![mimg("db", "postgres:16", None, Some("sha256:bbb"))];
        let remote = vec![("postgres:16".to_string(), "sha256:bbb".to_string())];
        let c = plan_tag_convergence(&images, &remote);
        assert!(c.retag.is_empty(), "已对齐不产生动作");
        assert!(c.missing.is_empty());
    }

    #[test]
    fn test_tag_convergence_tag_absent_still_retags() {
        // tag 整个不存在(被删)但 ID 还在 → 直接建标签(同样零拷贝可救)
        let images = vec![mimg("db", "postgres:16", None, Some("sha256:bbb"))];
        let remote = vec![("other:1".to_string(), "sha256:bbb".to_string())];
        let c = plan_tag_convergence(&images, &remote);
        assert_eq!(c.retag.len(), 1, "ID 在就能建回标签");
        assert_eq!(c.retag[0].2, "postgres:16");
    }

    #[test]
    fn test_tag_convergence_missing_id_is_blocking_case() {
        // ID 真丢了 → 收敛不了,交给调用方阻断(这是唯一真正的「回不去」)
        let images = vec![mimg("db", "postgres:16", None, Some("sha256:bbb"))];
        let remote = vec![("postgres:16".to_string(), "sha256:ccc".to_string())];
        let c = plan_tag_convergence(&images, &remote);
        assert!(c.retag.is_empty());
        assert_eq!(c.missing.len(), 1);
        assert_eq!(c.missing[0].0, "db");
        assert_eq!(c.missing[0].1, "postgres:16");
    }

    #[test]
    fn test_tag_convergence_skips_archived_and_idless() {
        // 有包的服务(Archived)不参与 ID 收敛:包内标签以 load 为准,
        // 且 list 里可能查不到(尚未 load);无 ID 的旧归档同样跳过。
        let images = vec![
            mimg("web", "myapp:1", Some("web.tar.gz"), Some("aaa")),
            mimg("old", "legacy:1", None, None),
        ];
        let remote: Vec<(String, String)> = vec![];
        let c = plan_tag_convergence(&images, &remote);
        assert!(c.retag.is_empty(), "有包/无 ID 的服务不由 ID 收敛处理");
        assert!(c.missing.is_empty(), "不应误报缺失(有包走装载,无 ID 走原降级)");
    }

    #[test]
    fn test_tag_convergence_prefix_tolerant() {
        // 与既有 same_image_id 同口径:一方带 sha256: 前缀一方不带仍算同
        let images = vec![mimg("db", "postgres:16", None, Some("bbb"))];
        let remote = vec![("postgres:16".to_string(), "sha256:bbb".to_string())];
        let c = plan_tag_convergence(&images, &remote);
        assert!(c.retag.is_empty(), "前缀差异不应触发多余的 retag");
        assert!(c.missing.is_empty());
    }

    #[test]
    fn test_tag_convergence_query_failed_all_missing() {
        // 远端列表不可得(查询失败)时不能当成「ID 都在」—— 调用方用
        // remote_available=false 表达,收敛计划应把全部项交回由调用方判定
        // (保守:missing 列出,文案与预检同口径「重试」而非「丢了」)。
        let images = vec![mimg("db", "postgres:16", None, Some("sha256:bbb"))];
        let c = plan_tag_convergence(&images, &[]);
        assert_eq!(c.missing.len(), 1, "查不到列表 → 不能静默放行");
        assert!(c.retag.is_empty());
    }

    // ===== v6.12.0:清单驱动装载(select_packages)=====

    #[test]
    fn test_select_packages_only_manifest_recorded() {
        // 目录里有 manifest 未记录的 tar → 必须跳过(第二十九批遗留的
        // 反向校验缺口:此前按 ls 全量装载,多余包会覆盖标签且无提示)
        let images = vec![
            mimg("web", "myapp:1", Some("web.tar.gz"), Some("aaa")),
            mimg("db", "postgres:16", None, Some("bbb")),
        ];
        let actual = vec![
            "web.tar.gz".to_string(),
            "stray-old.tar.gz".to_string(), // 目录里的多余包(人工放入/残留)
            "manifest.json".to_string(),
        ];
        let sel = select_packages(&images, &actual);
        assert_eq!(sel.to_load, vec!["web.tar.gz".to_string()], "只装载清单记录的包");
        assert_eq!(sel.unrecorded, vec!["stray-old.tar.gz".to_string()], "多余包单列并告警");
        assert!(sel.recorded_missing.is_empty());
    }

    #[test]
    fn test_select_packages_reports_recorded_missing() {
        // manifest 记录的包不在目录里 → 单列(预检已阻断;执行侧防御性再报一次)
        let images = vec![mimg("web", "myapp:1", Some("web.tar.gz"), Some("aaa"))];
        let actual = vec!["manifest.json".to_string()];
        let sel = select_packages(&images, &actual);
        assert!(sel.to_load.is_empty());
        assert_eq!(sel.recorded_missing, vec!["web.tar.gz".to_string()]);
    }

    #[test]
    fn test_select_packages_no_manifest_loads_nothing() {
        // 无清单(旧归档)时不能盲装全部:调用方按「无清单降级」处理。
        // 空 images(无 manifest) → to_load 空、unrecorded 列出全部目录包,
        // 由调用方决定是否降级为「按包恢复」(现状行为)。
        let actual = vec!["a.tar.gz".to_string(), "b.tar.gz".to_string()];
        let sel = select_packages(&[], &actual);
        assert!(sel.to_load.is_empty());
        assert_eq!(sel.unrecorded.len(), 2, "无清单时所有包都属未记录,交由调用方决策");
    }

    #[test]
    fn test_select_packages_skips_non_tar_directory_entries() {
        // 目录清单里的非包文件(compose 副本 / override .ddbak)不参与
        let images = vec![mimg("web", "myapp:1", Some("web.tar.gz"), Some("aaa"))];
        let actual = vec![
            "web.tar.gz".to_string(),
            "docker-compose.yml".to_string(),
            "compose.override.yml.ddbak".to_string(),
        ];
        let sel = select_packages(&images, &actual);
        assert_eq!(sel.to_load, vec!["web.tar.gz".to_string()]);
        assert!(sel.unrecorded.is_empty(), "非包文件不该进告警清单");
    }

    // ===== v6.12.0:插值漂移检测(manifest 的 tag 即部署时插值结果)=====

    #[test]
    fn test_image_env_drift_detected_and_attributed() {
        use std::collections::HashMap;
        // 归档 compose 里 image 用 ${TAG} 引用;manifest 记录的是部署当时插值出的 tag
        let compose = "services:\n  web:\n    image: ${REG}/app:${TAG}\n  db:\n    image: postgres:16\n";
        let mut env = HashMap::new();
        env.insert("REG".to_string(), "registry.example.com".to_string());
        env.insert("TAG".to_string(), "v2".to_string()); // 部署后被改过(v1 → v2)
        let refs = image_refs_with_env(compose, &[], &env).expect("compose 应可解析");
        // manifest 记录部署当时的引用:.../app:v1
        let manifest = vec![
            ("web".to_string(), "registry.example.com/app:v1".to_string()),
            ("db".to_string(), "postgres:16".to_string()),
        ];
        let drift = detect_image_env_drift(&refs, &manifest);
        assert_eq!(drift.len(), 1, "只有引用变量的服务会被判定漂移: {:?}", drift);
        assert_eq!(drift[0].service, "web");
        assert_eq!(drift[0].expected, "registry.example.com/app:v1");
        assert_eq!(drift[0].resolved, "registry.example.com/app:v2");
    }

    #[test]
    fn test_image_env_no_drift_when_unchanged() {
        use std::collections::HashMap;
        let compose = "services:\n  web:\n    image: ${REG}/app:${TAG}\n";
        let mut env = HashMap::new();
        env.insert("REG".to_string(), "registry.example.com".to_string());
        env.insert("TAG".to_string(), "v1".to_string());
        let refs = image_refs_with_env(compose, &[], &env).unwrap();
        let manifest = vec![("web".to_string(), "registry.example.com/app:v1".to_string())];
        assert!(detect_image_env_drift(&refs, &manifest).is_empty(), "值未变不该报漂移");
    }

    #[test]
    fn test_image_env_drift_ignores_services_without_vars() {
        use std::collections::HashMap;
        // 不引用变量的 image 与 .env 无关 → 永不漂移(避免噪音)
        let compose = "services:\n  db:\n    image: postgres:16\n";
        let refs = image_refs_with_env(compose, &[], &HashMap::new()).unwrap();
        let manifest = vec![("db".to_string(), "postgres:16".to_string())];
        assert!(detect_image_env_drift(&refs, &manifest).is_empty());
        // 即便 manifest 记的 tag 与 compose 解析结果不同,也不算漂移(非变量所致,
        // 属 compose/manifest 不一致,由执行链其它检查覆盖)
        let manifest2 = vec![("db".to_string(), "postgres:17".to_string())];
        assert!(detect_image_env_drift(&refs, &manifest2).is_empty(), "无变量不参与漂移判定");
    }

    #[test]
    fn test_image_env_drift_sees_override_image_override() {
        use std::collections::HashMap;
        // override 改写 image 时,漂移检测必须看到 override 后的版本(与解析同源)
        let compose = "services:\n  web:\n    image: ${REG}/app:${TAG}\n";
        let ov_text = "services:\n  web:\n    image: ${REG}/app:edge\n";
        let mut env = HashMap::new();
        env.insert("REG".to_string(), "r.io".to_string());
        env.insert("TAG".to_string(), "v1".to_string());
        let refs = image_refs_with_env(compose, &[ov_text.to_string()], &env).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].resolved, "r.io/app:edge", "override 优先(与部署解析一致)");
        let manifest = vec![("web".to_string(), "r.io/app:v1".to_string())];
        let drift = detect_image_env_drift(&refs, &manifest);
        assert_eq!(drift.len(), 1, "override 后的引用与 manifest 不符 → 漂移");
    }

    #[test]
    fn test_image_env_drift_unset_var_changes_resolution() {
        use std::collections::HashMap;
        // 「变量在部署时定义了、后来被删」同样改变插值结果 → 必须报漂移
        let compose = "services:\n  web:\n    image: myapp:${TAG}\n";
        let refs = image_refs_with_env(compose, &[], &HashMap::new()).unwrap();
        let manifest = vec![("web".to_string(), "myapp:v1".to_string())];
        let drift = detect_image_env_drift(&refs, &manifest);
        assert_eq!(drift.len(), 1, "变量消失同样让引用漂移: {:?}", drift);
        assert_eq!(drift[0].resolved, "myapp:", "未定义 → 插值为空(与 run 时同口径)");
    }

    #[test]
    fn test_image_refs_with_env_reports_has_vars() {
        use std::collections::HashMap;
        let compose = "services:\n  a:\n    image: plain:1\n  b:\n    image: ${X}:1\n";
        let mut env = HashMap::new();
        env.insert("X".to_string(), "reg".to_string());
        let refs = image_refs_with_env(compose, &[], &env).unwrap();
        assert_eq!(refs.len(), 2);
        assert!(!refs[0].has_vars, "纯字面量 → 无变量");
        assert!(refs[1].has_vars);
        assert_eq!(refs[0].raw, "plain:1");
    }

    // ===== v6.12.0:up 后运行镜像校验(容器实际 ID)=====

    #[test]
    fn test_container_mismatch_detection() {
        // up 后按容器实际镜像 ID 核对(兜 .env 漂移 / compose 未重建等残余路径)
        let expected = vec![
            ("web".to_string(), "sha256:aaa".to_string()),
            ("db".to_string(), "sha256:bbb".to_string()),
        ];
        // web 跑对了;db 实际跑的是 ccc(≠ 期望 bbb)
        let actual = vec![
            ("web".to_string(), "sha256:aaa".to_string()),
            ("db".to_string(), "sha256:ccc".to_string()),
        ];
        let m = select_container_image_mismatches(&expected, &actual);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].0, "db");
        assert_eq!(m[0].1, "sha256:bbb", "期望值");
        assert_eq!(m[0].2, "sha256:ccc", "实际值");
    }

    #[test]
    fn test_container_mismatch_missing_container_counts_too() {
        // 服务没有对应容器(up 失败/被 scale 0)→ 同样算不一致(不能静默)
        let expected = vec![
            ("web".to_string(), "sha256:aaa".to_string()),
            ("worker".to_string(), "sha256:bbb".to_string()),
        ];
        let actual = vec![("web".to_string(), "sha256:aaa".to_string())];
        let m = select_container_image_mismatches(&expected, &actual);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].0, "worker");
        assert!(m[0].2.is_empty(), "无容器 → 实际值空串");
    }

    #[test]
    fn test_container_mismatch_prefix_tolerant() {
        // 与全仓 ID 比较同口径:sha256: 前缀差异不算不一致
        let expected = vec![("web".to_string(), "aaa".to_string())];
        let actual = vec![("web".to_string(), "sha256:aaa".to_string())];
        assert!(select_container_image_mismatches(&expected, &actual).is_empty());
    }

    #[test]
    fn test_parse_compose_service_images_single_roundtrip() {
        // up 后校验的批量解析:一次 docker inspect 拿全部容器的 (服务, 镜像 ID)
        // (逐服务 ps+inspect 会 2×N 次往返;批量只 2 次)
        let out = "web|sha256:aaa\n db |sha256:bbb\n\nignored-no-pipe\n|sha256:ccc\n";
        let v = parse_compose_service_images(out);
        assert_eq!(v.len(), 2, "无管道行 / 服务名空的行跳过: {:?}", v);
        assert_eq!(v[0], ("web".to_string(), "sha256:aaa".to_string()));
        assert_eq!(v[1], ("db".to_string(), "sha256:bbb".to_string()));
    }

    #[test]
    fn test_verify_expected_table_skips_idless() {
        // up 后校验的期望表:只取有 ID 的服务(旧归档/采集失败无从比对)
        let images = vec![
            mimg("web", "myapp:1", Some("web.tar.gz"), Some("aaa")),
            mimg("old", "legacy:1", None, None),
            mimg("db", "postgres:16", None, Some("sha256:bbb")),
        ];
        let expected = expected_images_from_manifest(&images);
        assert_eq!(expected.len(), 2, "无 ID 的服务不入期望表: {:?}", expected);
        assert_eq!(expected[0], ("web".to_string(), "aaa".to_string()));
        assert_eq!(expected[1], ("db".to_string(), "sha256:bbb".to_string()));
    }

    #[test]
    fn test_compose_service_image_template_parses() {
        // **真机回归(v6.12.0 首发缺陷)**:`{{.Config.Labels."com.docker.compose.service"}}`
        // 不是合法 Go 模板(字段链只接受标识符;带点键要用 `index`)—— docker CLI
        // 在连接 daemon **之前**解析模板,直接 `template parsing error: bad character
        // U+0022` 且退出码 64,up 后校验每次部署都失败(只告警不误判成败,但校验
        // 形同虚设;真机报错原文:"docker inspect 查询失败(退出码 64)")。
        let template = COMPOSE_SERVICE_IMAGE_TEMPLATE;

        // 形态断言(零依赖兜底:CI / 无 docker CLI 环境同样能挡住退回点号写法)
        assert!(
            template.contains("index .Config.Labels"),
            "带点的标签键必须用 index 读取: {}",
            template
        );
        assert!(
            !template.contains("Labels.\""),
            "点号直取带点键不是合法 Go 模板(真机退出码 64): {}",
            template
        );

        // 真机验证:让本机 docker CLI 实际解析一次模板。解析先于连接 daemon,
        // 故**无 daemon 也能验证**(连接失败 = 已通过解析;失败信息不含模板错误);
        // docker CLI 不存在 → 跳过(与 docker:: 真机测试同口径)。
        let out = std::process::Command::new("docker")
            .args(["inspect", "--format", template, "dd-selftest-no-such-object"])
            .output();
        let Ok(out) = out else {
            println!("本机无 docker CLI,跳过模板真机解析");
            return;
        };
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !text.contains("template parsing error") && !text.contains("bad character"),
            "docker CLI 解析模板失败,up 后校验会整条失效: {}",
            text.trim()
        );
    }

    #[test]
    fn test_manifest_image_legacy_json_without_id() {
        // 旧归档(本字段引入前)的 manifest.json 无 id 字段 → serde default 兼容 None
        let legacy = r#"{"project":"app","ts":"20260101-000000","compose_copy":"docker-compose.yml",
            "images":[{"service":"web","tag":"app:latest","file":"app.tar.gz"}]}"#;
        let m = parse_release_manifest(legacy).expect("旧 manifest 应可解析");
        assert_eq!(m.images.len(), 1);
        assert_eq!(m.images[0].id, None, "旧归档 id 应为 None(对比回退按 tag)");
        // 新归档带 id 可往返
        let fresh = r#"{"project":"app","ts":"20260101-000000","compose_copy":"docker-compose.yml",
            "images":[{"service":"web","tag":"app:latest","file":"app.tar.gz","id":"sha256:bbb"}]}"#;
        let m2 = parse_release_manifest(fresh).expect("新 manifest 应可解析");
        assert_eq!(m2.images[0].id.as_deref(), Some("sha256:bbb"));
    }

    // ===== 远程操作互斥(第二十二批)=====

    /// 合并为单一测试:互斥位是**进程级静态量**,两个独立 #[test] 会被 cargo
    /// 并行调度而互撞(串行用例持位期间并发用例的线程全部被拒 → won=0;
    /// 调度延迟又可能让失败线程二次成功 → won>1),首版实测 flaky。
    /// 并发段另用「获胜者等待其余线程全部完成尝试」消除第二个获取窗口
    /// (否则被延迟调度的线程可能在取胜者释放后二次成功)。
    #[test]
    fn test_remote_op_guard_serial_and_concurrent() {
        // --- 串行语义:先取成功,持有期间第二次取被拒;Drop 后可再次取 ---
        let g1 = acquire_remote_op().expect("首次获取应成功");
        let denied = acquire_remote_op();
        assert!(denied.is_err(), "持有期间第二次获取必须被拒");
        assert!(
            denied.unwrap_err().contains("已有远程操作进行中"),
            "拒绝文案应面向用户且可识别"
        );
        assert!(REMOTE_OP_IN_FLIGHT.load(Ordering::SeqCst));
        drop(g1);
        assert!(
            !REMOTE_OP_IN_FLIGHT.load(Ordering::SeqCst),
            "Drop 应释放互斥位"
        );

        // --- 并发语义:8 线程争抢,恰好一方获胜(RAII + compare_exchange) ---
        const N: usize = 8;
        let attempts_done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..N {
            let done = std::sync::Arc::clone(&attempts_done);
            handles.push(std::thread::spawn(move || {
                let guard = acquire_remote_op();
                let won = guard.is_ok();
                if won {
                    // 持位等待其余 N-1 个线程完成各自的一次尝试再释放,
                    // 保证此后不会有任何成功路径(确定性:恰一胜)
                    while done.load(Ordering::SeqCst) < N - 1 {
                        std::thread::yield_now();
                    }
                }
                done.fetch_add(1, Ordering::SeqCst);
                drop(guard);
                won
            }));
        }
        let won = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|w| *w)
            .count();
        assert_eq!(won, 1, "8 线程并发争抢应恰好一方获胜");
        assert!(
            !REMOTE_OP_IN_FLIGHT.load(Ordering::SeqCst),
            "全部线程结束后互斥位应释放(无泄漏)"
        );
    }

    // ===== compose 覆盖文件参数拼装(第二十二批补测)=====

    #[test]
    fn test_compose_file_flags_single_and_overrides() {
        // 单文件:仅 -f,无 override 时不追加
        assert_eq!(compose_file_flags("docker-compose.yml", &[]), "-f 'docker-compose.yml'");
        // 带 override:按序追加(与 load 顺序一致,后者覆盖前者)
        assert_eq!(
            compose_file_flags(
                "docker-compose.yml",
                &["compose.override.yml".to_string(), "docker-compose.override.yml".to_string()]
            ),
            "-f 'docker-compose.yml' -f 'compose.override.yml' -f 'docker-compose.override.yml'"
        );
        // 单引号注入:经 shell_single_quote 转义无损(`'\''` 序列)
        let escaped = compose_file_flags("a'b.yml", &[]);
        assert_eq!(
            escaped,
            r#"-f 'a'\''b.yml'"#,
            "路径内单引号必须以 shell 转义序列包裹"
        );
    }

    #[test]
    fn test_compose_override_names_detects_existing_files() {
        // 构造临时目录:只有两个 override 实际存在,其余候选不返回
        let dir = std::env::temp_dir().join(format!("ddovr-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("compose.override.yml"), "services: {}\n").unwrap();
        std::fs::write(dir.join("compose.override.yaml"), "services: {}\n").unwrap();
        let compose = dir.join("docker-compose.yml");
        std::fs::write(&compose, "services: {}\n").unwrap();

        let names = compose_override_names(compose.to_str().unwrap());
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(names.len(), 2, "只返回存在的 override: {:?}", names);
        assert!(names.contains(&"compose.override.yml".to_string()));
        assert!(names.contains(&"compose.override.yaml".to_string()));
    }
